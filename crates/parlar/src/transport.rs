//! The connection between parlar's binaries and parlard: a unix socket under `$XDG_RUNTIME_DIR`
//! on Unix, a named pipe (`\\.\pipe\parlar-<user>`) on Windows, open to this user only.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};

/// The read half of a connection, as the daemon sees it.
pub type ReadHalf = Pin<Box<dyn AsyncRead + Send>>;
/// The write half of a connection, as the daemon sees it.
pub type WriteHalf = Pin<Box<dyn AsyncWrite + Send>>;

/// `$PARLAR_SOCKET`, else the platform default: the socket path on Unix, the pipe name on
/// Windows.
pub fn endpoint() -> PathBuf {
    if let Some(p) = std::env::var_os("PARLAR_SOCKET") {
        return PathBuf::from(p);
    }
    default_endpoint()
}

#[cfg(unix)]
fn default_endpoint() -> PathBuf {
    // harnesses may start MCP servers with a scrubbed environment, so XDG_RUNTIME_DIR can be
    // missing even on a systemd desktop; /run/user/<uid> is where it would point. macOS has
    // neither, so the socket goes under the user's temp folder, which is per user there.
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let run = PathBuf::from(format!("/run/user/{}", uid()));
            run.is_dir().then_some(run)
        })
        .unwrap_or_else(|| std::env::temp_dir().join(format!("parlar-{}", uid())));
    base.join("parlar").join("parlar.sock")
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid takes nothing and cannot fail
    unsafe { getuid() }
}

#[cfg(windows)]
fn default_endpoint() -> PathBuf {
    // one pipe per user; the pipe's own permissions keep other users out. The name comes from the
    // account, not %USERNAME%: harnesses start MCP servers with a trimmed environment
    let user = win::user_name().or_else(|| std::env::var("USERNAME").ok()).unwrap_or_else(|| "user".into());
    let user = user.to_lowercase();
    let user: String = user.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    PathBuf::from(format!(r"\\.\pipe\parlar-{user}"))
}

/// A blocking connection for hooks, the MCP server and ctl. Reads happen on a thread of the
/// client's (see `client.rs`), so this needs no read timeout of its own.
#[cfg(unix)]
pub type Conn = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type Conn = std::fs::File;

#[cfg(unix)]
pub fn connect() -> io::Result<Conn> {
    let s = Conn::connect(endpoint())?;
    s.set_write_timeout(Some(std::time::Duration::from_millis(500)))?;
    Ok(s)
}

/// A pipe whose every instance is taken answers "busy" for a moment; the daemon opens the next
/// instance as soon as one is taken, so a few short retries cover it.
#[cfg(windows)]
pub fn connect() -> io::Result<Conn> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = endpoint();
    for _ in 0..20 {
        match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            r => return r,
        }
    }
    Err(io::Error::other("parlar pipe busy"))
}

pub fn try_clone(c: &Conn) -> io::Result<Conn> {
    c.try_clone()
}

/// parlard's side: accepts connections and hands each over as two halves.
pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    name: PathBuf,
    #[cfg(windows)]
    next: tokio::sync::Mutex<tokio::net::windows::named_pipe::NamedPipeServer>,
}

impl Listener {
    /// Listen at `path`. Fails when another parlard already answers there.
    pub async fn bind(path: &Path) -> anyhow::Result<Listener> {
        bind_at(path).await
    }

    pub async fn accept(&self) -> io::Result<(ReadHalf, WriteHalf)> {
        accept_on(self).await
    }
}

#[cfg(unix)]
async fn bind_at(path: &Path) -> anyhow::Result<Listener> {
    use anyhow::Context;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        // only lock down a directory that is ours, never a shared one like /tmp
        if dir.file_name().is_some_and(|n| n == "parlar") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    if path.exists() {
        if Conn::connect(path).is_ok() {
            anyhow::bail!("parlard is already running on {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let inner = tokio::net::UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    Ok(Listener { inner })
}

#[cfg(unix)]
async fn accept_on(l: &Listener) -> io::Result<(ReadHalf, WriteHalf)> {
    let (s, _) = l.inner.accept().await?;
    let (rd, wr) = s.into_split();
    Ok((Box::pin(rd), Box::pin(wr)))
}

#[cfg(windows)]
async fn bind_at(path: &Path) -> anyhow::Result<Listener> {
    // the first instance claims the name: a second parlard fails here instead of sharing it
    let first = win::server(path, true)
        .map_err(|e| anyhow::anyhow!("parlard is already running on {} (or the pipe is taken): {e}", path.display()))?;
    Ok(Listener { name: path.to_path_buf(), next: tokio::sync::Mutex::new(first) })
}

#[cfg(windows)]
async fn accept_on(l: &Listener) -> io::Result<(ReadHalf, WriteHalf)> {
    let mut next = l.next.lock().await;
    next.connect().await?;
    // open the next instance before handing this one over, so a client never finds no pipe
    let fresh = win::server(&l.name, false)?;
    let taken = std::mem::replace(&mut *next, fresh);
    let (rd, wr) = tokio::io::split(taken);
    Ok((Box::pin(rd), Box::pin(wr)))
}

#[cfg(windows)]
mod win {
    use std::io;
    use std::path::Path;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    /// The account this process runs as.
    pub fn user_name() -> Option<String> {
        let mut buf = [0u16; 257];
        let mut len = buf.len() as u32;
        // SAFETY: the buffer and its length in UTF-16 units describe the same memory
        let ok = unsafe { windows_sys::Win32::System::WindowsProgramming::GetUserNameW(buf.as_mut_ptr(), &mut len) };
        // the length includes the terminating NUL
        (ok != 0 && len > 1).then(|| String::from_utf16_lossy(&buf[..len as usize - 1]))
    }

    /// Full access for the pipe's owner (this user) and SYSTEM, nothing for anyone else. The
    /// default descriptor would let every local account read it.
    const SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";

    pub fn server(name: &Path, first: bool) -> io::Result<NamedPipeServer> {
        let wide: Vec<u16> = SDDL.encode_utf16().chain([0]).collect();
        let mut sd = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string in, a LocalAlloc'd descriptor out, freed below
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: attrs and the descriptor it points at live until the call returns
        let r =
            unsafe { opts.create_with_security_attributes_raw(name, (&mut attrs as *mut SECURITY_ATTRIBUTES).cast()) };
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW
        unsafe { LocalFree(sd) };
        r
    }
}

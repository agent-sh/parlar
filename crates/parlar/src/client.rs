//! Blocking client for hooks, the MCP server and ctl. No async runtime: a hook runs on every tool
//! call and must start and finish in a few milliseconds.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::proto::{Origin, Request, Response};
use crate::transport::{self, Conn};

/// Where parlard listens: the socket path on Unix, the pipe name on Windows.
pub fn socket_path() -> PathBuf {
    transport::endpoint()
}

/// One connection to parlard. All I/O on it happens on one thread of the client's: a request is
/// handed over, written, and its reply read there, so a call can wait with a timeout on any
/// platform (Windows pipes have no read timeout), and a read and a write never overlap (a pipe
/// opened for synchronous I/O runs them one at a time, so a pending read would block the next
/// write forever). A call that times out leaves the connection unusable: its late reply would
/// otherwise answer the next call.
pub struct Client {
    tx: mpsc::Sender<Option<Vec<u8>>>,
    rx: mpsc::Receiver<std::io::Result<String>>,
    broken: bool,
}

impl Client {
    /// None when parlard is not running. Callers treat that as voice mode off.
    pub fn connect() -> Option<Client> {
        Client::from_stream(transport::connect().ok()?).ok()
    }

    pub fn from_stream(s: Conn) -> Result<Client> {
        let (req_tx, req_rx) = mpsc::channel::<Option<Vec<u8>>>();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("parlar-client".into()).spawn(move || {
            let mut wr = match transport::try_clone(&s) {
                Ok(w) => w,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let mut rd = BufReader::new(s);
            // Some(bytes): write a request, then read its reply. None: read one more line.
            while let Ok(req) = req_rx.recv() {
                if let Some(bytes) = req
                    && let Err(e) = wr.write_all(&bytes)
                {
                    let _ = tx.send(Err(e));
                    return;
                }
                let mut line = String::new();
                let r = match rd.read_line(&mut line) {
                    Ok(0) => {
                        Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "parlard closed the connection"))
                    }
                    Ok(_) => Ok(line),
                    Err(e) => Err(e),
                };
                let end = r.is_err();
                if tx.send(r).is_err() || end {
                    return;
                }
            }
        })?;
        Ok(Client { tx: req_tx, rx, broken: false })
    }

    pub fn call(&mut self, req: &Request, timeout: Option<Duration>) -> Result<Response> {
        anyhow::ensure!(!self.broken, "connection to parlard timed out earlier");
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        self.tx.send(Some(line)).ok().context("parlard connection closed")?;
        let buf = self.recv(timeout)?;
        Ok(serde_json::from_str(&buf)?)
    }

    pub fn read_line(&mut self) -> Result<String> {
        self.tx.send(None).ok().context("parlard connection closed")?;
        self.recv(None)
    }

    fn recv(&mut self, timeout: Option<Duration>) -> Result<String> {
        let got = match timeout {
            Some(t) => self.rx.recv_timeout(t).map_err(|e| {
                self.broken = true;
                anyhow::anyhow!("read from parlard: {e}")
            })?,
            None => self.rx.recv().context("read from parlard")?,
        };
        got.context("read from parlard")
    }
}

/// Ancestor pids of this process, nearest first, stopping before pid 1.
pub fn ancestors() -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = std::process::id();
    for _ in 0..32 {
        let Some(ppid) = parent_of(pid) else { break };
        if ppid <= 1 {
            break;
        }
        out.push(ppid);
        pid = ppid;
    }
    out
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // the comm field is parenthesized and may contain spaces; ppid is the second field after it
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split(' ').nth(1)?.parse().ok()
}

/// Shells harnesses run hook commands through. Windows names drop ".exe".
const SHELLS: &[&str] =
    &["sh", "bash", "zsh", "dash", "fish", "ksh", "mksh", "tcsh", "csh", "busybox", "cmd", "powershell", "pwsh"];

#[cfg(target_os = "linux")]
fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Whether a process still runs, for dropping sessions whose harness is gone.
#[cfg(target_os = "linux")]
pub fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// macOS has no /proc: the parent pid and the executable name come from sysctl's kinfo_proc,
/// and liveness from kill(pid, 0).
#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    mac::info(pid).map(|(ppid, _)| ppid)
}

#[cfg(target_os = "macos")]
fn comm(pid: u32) -> String {
    mac::info(pid).map(|(_, name)| name).unwrap_or_default()
}

#[cfg(target_os = "macos")]
pub fn alive(pid: u32) -> bool {
    mac::alive(pid)
}

#[cfg(target_os = "macos")]
mod mac {
    unsafe extern "C" {
        fn sysctl(
            name: *const i32,
            namelen: u32,
            oldp: *mut core::ffi::c_void,
            oldlenp: *mut usize,
            newp: *const core::ffi::c_void,
            newlen: usize,
        ) -> i32;
        fn kill(pid: i32, sig: i32) -> i32;
        fn __error() -> *mut i32;
    }
    const CTL_KERN: i32 = 1;
    const KERN_PROC: i32 = 14;
    const KERN_PROC_PID: i32 = 1;
    const ESRCH: i32 = 3;
    /// Offsets into struct kinfo_proc on 64-bit macOS: p_comm (16 bytes) in extern_proc, and
    /// e_ppid in eproc. Stable ABI, unchanged since 10.5.
    const KINFO_SIZE: usize = 648;
    const P_COMM: usize = 243;
    const E_PPID: usize = 560;

    /// The parent pid and the executable name (as the kernel keeps it, 16 bytes at most).
    pub fn info(pid: u32) -> Option<(u32, String)> {
        let mib = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid as i32];
        let mut buf = [0u8; KINFO_SIZE];
        let mut len = KINFO_SIZE;
        // SAFETY: a 4-element mib and a buffer with its length; sysctl writes at most len bytes
        let r = unsafe { sysctl(mib.as_ptr(), 4, buf.as_mut_ptr().cast(), &mut len, std::ptr::null(), 0) };
        if r != 0 || len < E_PPID + 4 {
            return None;
        }
        let name_end = buf[P_COMM..P_COMM + 16].iter().position(|&c| c == 0).unwrap_or(16);
        let name = String::from_utf8_lossy(&buf[P_COMM..P_COMM + name_end]).into_owned();
        let ppid = u32::from_ne_bytes(buf[E_PPID..E_PPID + 4].try_into().ok()?);
        Some((ppid, name))
    }

    pub fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 checks for the process without sending anything
        unsafe { kill(pid as i32, 0) == 0 || *__error() != ESRCH }
    }
}

#[cfg(windows)]
fn parent_of(pid: u32) -> Option<u32> {
    win::process(pid).map(|(ppid, _)| ppid)
}

#[cfg(windows)]
fn comm(pid: u32) -> String {
    win::process(pid).map(|(_, name)| name).unwrap_or_default()
}

#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    win::alive(pid)
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, STILL_ACTIVE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    /// The parent pid and the lowercase executable name without ".exe".
    pub fn process(pid: u32) -> Option<(u32, String)> {
        // SAFETY: a snapshot handle walked with a properly sized entry, then closed
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut found = None;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                if e.th32ProcessID == pid {
                    let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
                    let name = name.strip_suffix(".exe").unwrap_or(&name).to_string();
                    found = Some((e.th32ParentProcessID, name));
                    break;
                }
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
            found
        }
    }

    pub fn alive(pid: u32) -> bool {
        // SAFETY: the handle is checked and closed; the exit code is written to a local
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }
}

/// Origin of a hook or ctl command: the harness is the nearest ancestor that is not a shell
/// (harnesses run hook commands through `sh -c` or `$SHELL -lc`).
pub fn origin(session: Option<String>) -> Origin {
    let pids = ancestors();
    let harness_pid = pids.iter().copied().find(|&p| !SHELLS.contains(&comm(p).as_str()));
    Origin { session, pids, harness_pid, mcp: false, ..Default::default() }
}

/// Origin of the MCP server: the harness is its nearest ancestor that is not a shell. On Linux that
/// is the parent; on Windows a harness starts the plugin's `parlar.cmd` through cmd.exe, so the
/// parent is a shell there.
pub fn mcp_origin(session: Option<String>) -> Origin {
    let pids = ancestors();
    let harness_pid = pids.iter().copied().find(|&p| !SHELLS.contains(&comm(p).as_str()));
    Origin { session, harness_pid, pids, mcp: true, ..Default::default() }
}

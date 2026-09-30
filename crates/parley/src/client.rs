//! Blocking client for hooks, the MCP server and ctl. No async runtime: a hook runs on every tool
//! call and must start and finish in a few milliseconds.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::proto::{Origin, Request, Response};

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("PARLEY_SOCKET") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("parley-{}", unsafe_uid())));
    base.join("parley").join("parley.sock")
}

fn unsafe_uid() -> u32 {
    // std has no getuid; /proc/self is owned by the caller's uid
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

pub struct Client {
    rd: BufReader<UnixStream>,
    wr: UnixStream,
}

impl Client {
    /// None when parleyd is not running. Callers treat that as voice mode off.
    pub fn connect() -> Option<Client> {
        let s = UnixStream::connect(socket_path()).ok()?;
        s.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
        let wr = s.try_clone().ok()?;
        Some(Client { rd: BufReader::new(s), wr })
    }

    pub fn from_stream(s: UnixStream) -> Result<Client> {
        let wr = s.try_clone()?;
        Ok(Client { rd: BufReader::new(s), wr })
    }

    pub fn call(&mut self, req: &Request, timeout: Option<Duration>) -> Result<Response> {
        self.rd.get_ref().set_read_timeout(timeout)?;
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        self.wr.write_all(&line)?;
        let mut buf = String::new();
        let n = self.rd.read_line(&mut buf).context("read from parleyd")?;
        anyhow::ensure!(n > 0, "parleyd closed the connection");
        Ok(serde_json::from_str(&buf)?)
    }

    pub fn read_line(&mut self) -> Result<String> {
        self.rd.get_ref().set_read_timeout(None)?;
        let mut buf = String::new();
        let n = self.rd.read_line(&mut buf)?;
        anyhow::ensure!(n > 0, "parleyd closed the connection");
        Ok(buf)
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

fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // the comm field is parenthesized and may contain spaces; ppid is the second field after it
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split(' ').nth(1)?.parse().ok()
}

pub fn origin(session: Option<String>) -> Origin {
    Origin { session, pids: ancestors() }
}

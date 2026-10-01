//! Where parlar keeps its files: the XDG directories on Linux, `%APPDATA%` and `%LOCALAPPDATA%`
//! on Windows. Each returns parlar's own folder inside the base.

use std::path::PathBuf;

fn xdg(var: &str, home_rel: &str, windows_var: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(var).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if cfg!(windows)
        && let Some(p) = std::env::var_os(windows_var).filter(|p| !p.is_empty())
    {
        return PathBuf::from(p);
    }
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(home_rel)
}

/// Settings and device choices: `$XDG_CONFIG_HOME/parlar`, `%APPDATA%\parlar`.
pub fn config() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config", "APPDATA").join("parlar")
}

/// Conversation state kept across restarts: `$XDG_STATE_HOME/parlar`, `%LOCALAPPDATA%\parlar`.
pub fn state() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state", "LOCALAPPDATA").join("parlar")
}

/// Models and the speech runtime: `$XDG_DATA_HOME/parlar`, `%LOCALAPPDATA%\parlar`.
pub fn data() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share", "LOCALAPPDATA").join("parlar")
}

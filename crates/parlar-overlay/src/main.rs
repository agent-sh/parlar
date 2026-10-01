//! parlar's floating voice indicator for Windows and macOS. A borderless window above every
//! other, drawn from one pixel buffer (render.rs) through GDI or Core Graphics. Click to stop or start the conversation, drag to move, right-click for the
//! menu. No GUI framework: the binary stays small and starts at login with parlard.

// a windowed program: no console window when it starts
#![cfg_attr(windows, windows_subsystem = "windows")]

// both are only driven by the Windows window; the swarm's tests run everywhere
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod link;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod render;
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
mod swarm;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::run()
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    mac::run()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn main() {
    eprintln!("parlar-overlay is the Windows and macOS indicator; on GNOME the indicator is the shell extension");
    std::process::exit(2);
}

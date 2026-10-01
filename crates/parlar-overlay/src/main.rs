//! parlar's floating voice indicator for Windows. A layered, topmost, borderless Win32 window
//! drawn with GDI. Click to stop or start the conversation, drag to move, right-click for the
//! menu. No GUI framework: the binary stays small and starts at login with parlard.

// both are only driven by the Windows window; the swarm's tests run everywhere
#[cfg_attr(not(windows), allow(dead_code))]
mod link;
#[cfg_attr(not(windows), allow(dead_code))]
mod swarm;

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("parlar-overlay is the Windows indicator; on GNOME the indicator is the shell extension");
    std::process::exit(2);
}

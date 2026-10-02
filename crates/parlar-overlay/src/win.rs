//! The Win32 side: a layered topmost window that shows the swarm, takes clicks and drags, and
//! pops a menu. The swarm is drawn into a 32-bit premultiplied bitmap and pushed with
//! UpdateLayeredWindow, so the window has no background and per-pixel alpha.

use std::cell::RefCell;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Result, bail};
use parlar::proto::{Phase, Request, Response, Ui};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
    CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, ReleaseDC, SelectObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::link::{self, Event};
use crate::swarm::{Mode, Swarm};

/// The window's size in device pixels at 100%; scaled by the monitor's DPI.
const SIZE_96: i32 = 104;
const FRAME_MS: u32 = 33;
/// How long a lost daemon shows as connecting before it shows as stopped.
const CONNECTING_GRACE_S: f32 = 8.0;
const TIMER_FRAME: usize = 1;
const TIMER_POLL: usize = 2;
/// Posted after a click, so the toggle (which may show a message box) runs on its own message
/// rather than inside the button-up handling.
const WM_PARLAR_TOGGLE: u32 = WM_APP + 1;
const CLASS: &str = "parlar-overlay";

// menu command ids
const CMD_TOGGLE: usize = 1;
const CMD_MUTE: usize = 2;
const CMD_VOICE_OFF: usize = 3;
const CMD_QUIT: usize = 4;
const CMD_FOCUS: usize = 100;
const CMD_INPUT: usize = 200;
const CMD_OUTPUT: usize = 300;

struct Overlay {
    hwnd: HWND,
    swarm: Swarm,
    rx: mpsc::Receiver<Event>,
    connected: bool,
    lost_at: f32,
    phase: Phase,
    muted: bool,
    voice_off: bool,
    user: f32,
    agent: f32,
    start: Instant,
    last: f32,
    ticking: bool,
    drag: Option<(POINT, POINT, bool)>,
    size: i32,
    /// Menu choices by command id: session ids, input and output device ids.
    menu_focus: Vec<String>,
    menu_inputs: Vec<String>,
    menu_outputs: Vec<String>,
}

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub fn run() -> Result<()> {
    // SAFETY: plain Win32 calls with valid arguments; the window procedure only touches the
    // thread-local overlay from this thread
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            bail!("register window class: {}", std::io::Error::last_os_error());
        }
        let (x, y, size) = initial_position();
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            wide("parlar").as_ptr(),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            size,
            size,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            bail!("create window: {}", std::io::Error::last_os_error());
        }
        let (tx, rx) = mpsc::channel();
        link::subscribe(tx);
        let start = Instant::now();
        let mut ov = Overlay {
            hwnd,
            swarm: Swarm::new(),
            rx,
            connected: false,
            lost_at: 0.0,
            phase: Phase::Connecting,
            muted: false,
            voice_off: false,
            user: 0.0,
            agent: 0.0,
            start,
            last: 0.0,
            ticking: false,
            drag: None,
            size,
            menu_focus: Vec::new(),
            menu_inputs: Vec::new(),
            menu_outputs: Vec::new(),
        };
        ov.wake();
        OVERLAY.with(|o| *o.borrow_mut() = Some(ov));
        // events from the link thread are pulled on a timer: Win32 messages must come from the
        // window's own thread
        SetTimer(hwnd, TIMER_POLL, 50, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// The saved position, clamped to a monitor, else the top right of the primary one.
fn initial_position() -> (i32, i32, i32) {
    // SAFETY: querying screen metrics has no preconditions
    let (vx, vy, vw, vh, pw) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
            GetSystemMetrics(SM_CXSCREEN),
        )
    };
    let dpi = unsafe { GetDpiForSystem() } as i32;
    let size = SIZE_96 * dpi / 96;
    let saved = std::fs::read_to_string(position_path()).ok().and_then(|s| serde_json::from_str::<(i32, i32)>(&s).ok());
    // the top right of the primary monitor, or the saved spot kept within the whole desktop (any
    // monitor, including ones left of or above the primary, which have negative coordinates)
    let (x, y) = saved.unwrap_or((pw - size - 40, 60));
    (x.clamp(vx, (vx + vw - size).max(vx)), y.clamp(vy, (vy + vh - size).max(vy)), size)
}

fn position_path() -> std::path::PathBuf {
    parlar::dirs::config().join("indicator.json")
}

fn save_position(hwnd: HWND) {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: a valid window handle and an out pointer
    if unsafe { GetWindowRect(hwnd, &mut r) } != 0 {
        let p = position_path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, format!("[{},{}]", r.left, r.top));
    }
}

impl Overlay {
    fn now(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    fn mode(&self) -> Mode {
        if !self.connected {
            return if self.now() - self.lost_at < CONNECTING_GRACE_S { Mode::Connecting } else { Mode::Stopped };
        }
        if self.muted && matches!(self.phase, Phase::Ready | Phase::Listening) {
            return Mode::Muted;
        }
        match self.phase {
            Phase::Stopped => Mode::Stopped,
            Phase::Connecting => Mode::Connecting,
            Phase::Ready => Mode::Idle,
            Phase::Listening => Mode::Listening,
            Phase::Working => Mode::Thinking,
            Phase::Speaking => Mode::Speaking,
            Phase::Interrupting => Mode::Both,
        }
    }

    fn wake(&mut self) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        self.last = self.now();
        // SAFETY: a valid window handle
        unsafe { SetTimer(self.hwnd, TIMER_FRAME, FRAME_MS, None) };
    }

    fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Connected(on) => {
                    self.connected = on;
                    if !on {
                        self.lost_at = self.now();
                        self.user = 0.0;
                        self.agent = 0.0;
                    }
                }
                Event::Ui(Ui::Phase { phase, mic_muted, voice_off }) => {
                    self.phase = phase;
                    self.muted = mic_muted;
                    self.voice_off = voice_off;
                }
                Event::Ui(Ui::Levels { user, agent }) => {
                    self.user = self.user.max(user);
                    self.agent = self.agent.max(agent);
                }
                Event::Ui(Ui::Tool { ok, .. }) => self.swarm.flare(!ok),
                Event::Ui(Ui::Caption { .. } | Ui::Notice { .. }) => {}
            }
            self.wake();
        }
        // connecting turns into stopped without an event
        if !self.connected && !self.ticking && self.now() - self.lost_at >= CONNECTING_GRACE_S {
            self.wake();
        }
    }

    fn tick(&mut self) {
        let t = self.now();
        let dt = (t - self.last).min(0.05);
        self.last = t;
        let mode = self.mode();
        self.swarm.set_mode(mode, t);
        self.swarm.step(dt, t, self.user, self.agent, self.voice_off);
        // levels arrive only while there is sound; let them fall between events
        self.user *= 0.85;
        self.agent *= 0.85;
        self.paint();
        if !self.swarm.busy() {
            self.ticking = false;
            // SAFETY: a valid window handle
            unsafe { KillTimer(self.hwnd, TIMER_FRAME) };
        }
    }

    /// Draw the swarm into a premultiplied BGRA bitmap and hand it to the layered window.
    fn paint(&self) {
        let w = self.size as usize;
        let px = crate::render::render(&self.swarm, w, self.muted && self.connected);
        // SAFETY: GDI objects created and released in order; the DIB has exactly w*w*4 bytes
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let mem = CreateCompatibleDC(screen);
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(w as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let bmp: HBITMAP = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if !bmp.is_null() && !bits.is_null() {
                std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
                let old = SelectObject(mem, bmp);
                let size = SIZE { cx: w as i32, cy: w as i32 };
                let src = POINT { x: 0, y: 0 };
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                UpdateLayeredWindow(
                    self.hwnd,
                    screen,
                    std::ptr::null(),
                    &size,
                    mem,
                    &src,
                    0 as COLORREF,
                    &blend,
                    ULW_ALPHA,
                );
                SelectObject(mem, old);
                DeleteObject(bmp);
            }
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
    }

    fn toggle(&self) {
        if !self.connected {
            notify("parlard is not running. Start it with: parlard service");
            return;
        }
        if self.phase != Phase::Stopped {
            send(link::set(|s| s.active = Some(false)));
            return;
        }
        match link::request(&Request::State) {
            Some(Response::State(st)) if st.sessions.iter().any(|x| x.focused) => {
                send(link::set(|s| s.active = Some(true)));
            }
            Some(_) => notify("Pick a session first: run /parlar:talk in it, or choose one under Talk to."),
            None => notify("parlard is not running."),
        }
    }

    fn menu(&mut self, x: i32, y: i32) {
        // SAFETY: menu handles created here are destroyed below; the window is valid
        unsafe {
            let m = CreatePopupMenu();
            let add = |m: HMENU, id: usize, text: &str, checked: bool, enabled: bool| {
                let mut flags = MF_STRING;
                if checked {
                    flags |= MF_CHECKED;
                }
                if !enabled {
                    flags |= MF_GRAYED;
                }
                AppendMenuW(m, flags, id, wide(text).as_ptr());
            };
            let sep = |m: HMENU| {
                AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            };
            self.menu_focus.clear();
            self.menu_inputs.clear();
            self.menu_outputs.clear();
            if !self.connected {
                add(m, 0, "parlard is not running", false, false);
            } else {
                if let Some(Response::State(st)) = link::request(&Request::State) {
                    let sessions: Vec<_> = st.sessions.iter().filter(|s| s.session.is_some()).collect();
                    if !sessions.is_empty() {
                        add(m, 0, "Talk to", false, false);
                        for s in sessions {
                            let folder = std::path::Path::new(&s.cwd)
                                .file_name()
                                .map(|f| f.to_string_lossy().into_owned())
                                .filter(|f| !f.is_empty())
                                .unwrap_or_else(|| "session".into());
                            let id = CMD_FOCUS + self.menu_focus.len();
                            self.menu_focus.push(s.session.clone().unwrap_or_default());
                            add(m, id, &format!("    {folder} ({})", harness_name(s.harness)), s.focused, true);
                        }
                        sep(m);
                    }
                }
                if let Some(Response::Devices { inputs, outputs }) = link::request(&Request::Devices) {
                    add(m, 0, "Input", false, false);
                    for d in inputs {
                        let id = CMD_INPUT + self.menu_inputs.len();
                        self.menu_inputs.push(d.id);
                        add(m, id, &format!("    {}", d.name), d.current, true);
                    }
                    add(m, 0, "Output", false, false);
                    for d in outputs {
                        let id = CMD_OUTPUT + self.menu_outputs.len();
                        self.menu_outputs.push(d.id);
                        add(m, id, &format!("    {}", d.name), d.current, true);
                    }
                    sep(m);
                }
                add(m, CMD_MUTE, "Mute mic", self.muted, true);
                add(m, CMD_VOICE_OFF, "Voice off (text only)", self.voice_off, true);
                sep(m);
                let stopped = self.phase == Phase::Stopped;
                add(m, CMD_TOGGLE, if stopped { "Start conversation" } else { "Stop conversation" }, false, true);
            }
            sep(m);
            add(m, CMD_QUIT, "Close indicator", false, true);
            // a popup menu from a non-activated window needs the window in front to dismiss right
            SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, x, y, 0, self.hwnd, std::ptr::null()) as usize;
            DestroyMenu(m);
            self.command(cmd);
        }
    }

    fn command(&mut self, cmd: usize) {
        match cmd {
            CMD_TOGGLE => self.toggle(),
            CMD_MUTE => send(link::set(|s| s.mic_muted = Some(!self.muted))),
            CMD_VOICE_OFF => send(link::set(|s| s.voice_off = Some(!self.voice_off))),
            // posted, so the window goes down after the overlay is back in its cell
            // SAFETY: a valid window handle
            CMD_QUIT => unsafe {
                PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
            },
            c if c >= CMD_OUTPUT => {
                if let Some(id) = self.menu_outputs.get(c - CMD_OUTPUT).cloned() {
                    send(link::set(|s| s.output = Some(id)));
                }
            }
            c if c >= CMD_INPUT => {
                if let Some(id) = self.menu_inputs.get(c - CMD_INPUT).cloned() {
                    send(link::set(|s| s.input = Some(id)));
                }
            }
            c if c >= CMD_FOCUS => {
                if let Some(id) = self.menu_focus.get(c - CMD_FOCUS).cloned() {
                    send(link::set(|s| s.focus = Some(id)));
                }
            }
            _ => {}
        }
    }
}

fn harness_name(h: parlar::proto::Harness) -> &'static str {
    match h {
        parlar::proto::Harness::Claude => "claude",
        parlar::proto::Harness::Codex => "codex",
        parlar::proto::Harness::Other => "other",
    }
}

fn send(r: Option<Response>) {
    match r {
        Some(Response::Error { message }) => notify(&message),
        None => notify("parlard is not running."),
        _ => {}
    }
}

fn notify(text: &str) {
    // SAFETY: a simple message box with NUL-terminated strings
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(text).as_ptr(),
            wide("parlar").as_ptr(),
            MB_OK | MB_ICONINFORMATION | MB_TOPMOST,
        );
    }
}

fn cursor() -> POINT {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: an out pointer
    unsafe { GetCursorPos(&mut p) };
    p
}

/// What an input message asks for, decided under a short borrow and done after it ends: every
/// Win32 call here can call back into this procedure on the same thread (SetWindowPos,
/// ReleaseCapture and DestroyWindow send messages; TrackPopupMenu and MessageBoxW run message
/// loops), and a borrow held across one would panic.
enum Act {
    None,
    Capture,
    Move(i32, i32),
    Release { toggle: bool, save: bool },
    Menu,
    Toggle,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let act = OVERLAY.with(|o| {
        let mut o = o.borrow_mut();
        let ov = o.as_mut()?;
        Some(match msg {
            WM_TIMER => {
                match wp {
                    TIMER_FRAME => ov.tick(),
                    TIMER_POLL => ov.poll(),
                    _ => {}
                }
                Act::None
            }
            WM_LBUTTONDOWN => {
                let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                // SAFETY: a valid window handle and an out pointer; GetWindowRect sends nothing
                unsafe { GetWindowRect(hwnd, &mut r) };
                ov.drag = Some((cursor(), POINT { x: r.left, y: r.top }, false));
                Act::Capture
            }
            WM_MOUSEMOVE => match ov.drag.as_mut() {
                Some((from, origin, moved)) => {
                    let c = cursor();
                    let (dx, dy) = (c.x - from.x, c.y - from.y);
                    if dx.abs() + dy.abs() > 4 {
                        *moved = true;
                    }
                    if *moved { Act::Move(origin.x + dx, origin.y + dy) } else { Act::None }
                }
                None => Act::None,
            },
            WM_LBUTTONUP => match ov.drag.take() {
                Some((_, _, moved)) => Act::Release { toggle: !moved, save: moved },
                None => Act::None,
            },
            WM_RBUTTONUP => Act::Menu,
            WM_PARLAR_TOGGLE => Act::Toggle,
            WM_DESTROY => {
                // SAFETY: ends the message loop; posts, does not send
                unsafe { PostQuitMessage(0) };
                Act::None
            }
            _ => return None,
        })
    });
    let Some(act) = act else {
        // SAFETY: default handling for everything else
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    };
    // SAFETY: valid handles; the overlay is not borrowed here
    unsafe {
        match act {
            Act::None => {}
            Act::Capture => {
                SetCapture(hwnd);
            }
            Act::Move(x, y) => {
                SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
            }
            Act::Release { toggle, save } => {
                ReleaseCapture();
                if save {
                    save_position(hwnd);
                }
                if toggle {
                    PostMessageW(hwnd, WM_PARLAR_TOGGLE, 0, 0);
                }
            }
            Act::Menu | Act::Toggle => {
                // the menu and the toggle run message loops: take the overlay out of its cell
                // for the duration, so the re-entrant timers find nothing to borrow
                let taken = OVERLAY.with(|o| o.borrow_mut().take());
                if let Some(mut ov) = taken {
                    if matches!(act, Act::Menu) {
                        let c = cursor();
                        ov.menu(c.x, c.y);
                    } else {
                        ov.toggle();
                    }
                    OVERLAY.with(|o| *o.borrow_mut() = Some(ov));
                }
            }
        }
    }
    0
}

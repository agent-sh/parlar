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
/// Posted by the input handlers: the menu and the toggle run message loops (TrackPopupMenu,
/// MessageBox), so they must run outside the window procedure's borrow of the overlay.
const WM_PARLAR_MENU: u32 = WM_APP + 1;
const WM_PARLAR_TOGGLE: u32 = WM_APP + 2;
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
    // SAFETY: querying monitor metrics has no preconditions
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let dpi = unsafe { GetDpiForSystem() } as i32;
    let size = SIZE_96 * dpi / 96;
    let saved = std::fs::read_to_string(position_path()).ok().and_then(|s| serde_json::from_str::<(i32, i32)>(&s).ok());
    let (x, y) = saved.unwrap_or((sw - size - 40, 60));
    (x.clamp(0, (sw - size).max(0)), y.clamp(0, (sh - size).max(0)), size)
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
                Event::Ui(Ui::Caption { .. }) => {}
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
        let mut px = vec![0u8; w * w * 4];
        let c = w as f32 / 2.0;
        let u = w as f32 * 0.42;
        let scale = w as f32 / 104.0;
        // a pocket of night behind the fireflies so they read on light windows too
        let depth = if self.swarm.mode() == Mode::Stopped { 0.18 } else { 0.42 };
        for yy in 0..w {
            for xx in 0..w {
                let d = (((xx as f32 - c).powi(2) + (yy as f32 - c).powi(2)).sqrt() / c).min(1.0);
                let a = if d < 0.7 { depth * (1.0 - 0.4 * d / 0.7) } else { depth * 0.6 * (1.0 - (d - 0.7) / 0.3) };
                let i = (yy * w + xx) * 4;
                px[i] = (0.09 * 255.0 * a) as u8;
                px[i + 1] = (0.06 * 255.0 * a) as u8;
                px[i + 2] = (0.05 * 255.0 * a) as u8;
                px[i + 3] = (255.0 * a) as u8;
            }
        }
        let muted_bar = self.muted && self.connected;
        for p in &self.swarm.p {
            if p.a < 0.005 {
                continue;
            }
            let (fx, fy) = (c + p.x * u, c + p.y * u);
            let col = Swarm::color(p);
            let halo = (4.5 + p.flare * 12.0) * scale;
            let (x0, x1) = (((fx - halo).floor() as i32).max(0), ((fx + halo).ceil() as i32).min(w as i32 - 1));
            let (y0, y1) = (((fy - halo).floor() as i32).max(0), ((fy + halo).ceil() as i32).min(w as i32 - 1));
            for yy in y0..=y1 {
                for xx in x0..=x1 {
                    let r = ((xx as f32 - fx).powi(2) + (yy as f32 - fy).powi(2)).sqrt() / halo;
                    if r >= 1.0 {
                        continue;
                    }
                    // the radial gradient: full at the center, 45% at a quarter, zero at the edge
                    let g = if r < 0.25 { 1.0 - 0.55 * r / 0.25 } else { 0.45 * (1.0 - (r - 0.25) / 0.75) };
                    let a = p.a * g;
                    let i = ((yy as usize) * w + xx as usize) * 4;
                    // additive, like the shell's ADD operator, premultiplied
                    for (k, ch) in [2usize, 1, 0].into_iter().enumerate() {
                        px[i + ch] = (px[i + ch] as f32 + col[k] * a).min(255.0) as u8;
                    }
                    px[i + 3] = (px[i + 3] as f32 + 255.0 * a * 0.6).min(255.0) as u8;
                }
            }
        }
        if muted_bar {
            let y = (c + u * 0.86) as usize;
            for yy in y.saturating_sub(1)..=(y + 1).min(w - 1) {
                for xx in (c - u * 0.25) as usize..=(c + u * 0.25) as usize {
                    let i = (yy * w + xx) * 4;
                    px[i] = 59;
                    px[i + 1] = 67;
                    px[i + 2] = 191;
                    px[i + 3] = 191;
                }
            }
        }
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
            // SAFETY: a valid window handle
            CMD_QUIT => unsafe {
                DestroyWindow(self.hwnd);
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

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = OVERLAY.with(|o| {
        let mut o = o.borrow_mut();
        let Some(ov) = o.as_mut() else { return false };
        match msg {
            WM_TIMER => {
                match wp {
                    TIMER_FRAME => ov.tick(),
                    TIMER_POLL => ov.poll(),
                    _ => {}
                }
                true
            }
            WM_LBUTTONDOWN => {
                let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                // SAFETY: valid handles
                unsafe {
                    GetWindowRect(hwnd, &mut r);
                    SetCapture(hwnd);
                }
                ov.drag = Some((cursor(), POINT { x: r.left, y: r.top }, false));
                true
            }
            WM_MOUSEMOVE => {
                if let Some((from, origin, moved)) = ov.drag.as_mut() {
                    let c = cursor();
                    let (dx, dy) = (c.x - from.x, c.y - from.y);
                    if dx.abs() + dy.abs() > 4 {
                        *moved = true;
                    }
                    if *moved {
                        // SAFETY: a valid window handle
                        unsafe {
                            SetWindowPos(
                                hwnd,
                                HWND_TOPMOST,
                                origin.x + dx,
                                origin.y + dy,
                                0,
                                0,
                                SWP_NOSIZE | SWP_NOACTIVATE,
                            )
                        };
                    }
                }
                true
            }
            WM_LBUTTONUP => {
                // SAFETY: capture was taken on the press
                unsafe { ReleaseCapture() };
                if let Some((_, _, moved)) = ov.drag.take() {
                    if moved {
                        save_position(hwnd);
                    } else {
                        // SAFETY: a valid window handle
                        unsafe { PostMessageW(hwnd, WM_PARLAR_TOGGLE, 0, 0) };
                    }
                }
                true
            }
            WM_RBUTTONUP => {
                // SAFETY: a valid window handle
                unsafe { PostMessageW(hwnd, WM_PARLAR_MENU, 0, 0) };
                true
            }
            WM_DESTROY => {
                // SAFETY: ends the message loop
                unsafe { PostQuitMessage(0) };
                true
            }
            _ => false,
        }
    });
    if handled {
        return 0;
    }
    // the menu and the toggle re-enter this procedure through their own message loops, so they
    // take the overlay out of its cell for the duration and put it back after
    match msg {
        WM_PARLAR_MENU | WM_PARLAR_TOGGLE => {
            let taken = OVERLAY.with(|o| o.borrow_mut().take());
            if let Some(mut ov) = taken {
                if msg == WM_PARLAR_MENU {
                    let c = cursor();
                    ov.menu(c.x, c.y);
                } else {
                    ov.toggle();
                }
                OVERLAY.with(|o| *o.borrow_mut() = Some(ov));
            }
            0
        }
        // SAFETY: default handling for everything else
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

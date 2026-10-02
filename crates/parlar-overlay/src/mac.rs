//! The macOS side: a borderless, floating NSWindow with a view that shows the rendered swarm as
//! an NSImage, takes clicks and drags, and pops an NSMenu. One timer drives both the parlard
//! event poll and the frames. Everything runs on the main thread, as AppKit requires.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSCompositingOperation,
    NSControlStateValueOn, NSEvent, NSFloatingWindowLevel, NSImage, NSMenu, NSMenuItem, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{NSObjectProtocol, NSString, NSTimer};
use parlar::proto::{Phase, Request, Response, Ui};

use crate::link::{self, Event};
use crate::render::render;
use crate::swarm::{Mode, Swarm};

/// The window's size in points.
const SIZE: f64 = 104.0;
const FRAME_S: f64 = 0.033;
/// How long a lost daemon shows as connecting before it shows as stopped.
const CONNECTING_GRACE_S: f32 = 8.0;

// menu tags
const TAG_TOGGLE: isize = 1;
const TAG_MUTE: isize = 2;
const TAG_VOICE_OFF: isize = 3;
const TAG_QUIT: isize = 4;
const TAG_FOCUS: isize = 100;
const TAG_INPUT: isize = 200;
const TAG_OUTPUT: isize = 300;

/// Everything the indicator knows, owned by the view.
struct State {
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
    /// Pixel size of the rendered image: points times the backing scale.
    px: usize,
    /// Menu choices by tag: session ids, input and output device ids.
    menu_focus: Vec<String>,
    menu_inputs: Vec<String>,
    menu_outputs: Vec<String>,
}

struct Ivars {
    state: RefCell<State>,
    /// A press and where the window was, until the release; `moved` once it became a drag.
    drag: Cell<Option<(CGPoint, CGPoint, bool)>>,
    image: RefCell<Option<Retained<NSImage>>>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond the main thread; no Drop impl.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ParlarSwarmView"]
    #[ivars = Ivars]
    struct SwarmView;

    impl SwarmView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: CGRect) {
            if let Some(img) = self.ivars().image.borrow().as_ref() {
                let b = self.bounds();
                img.drawInRect_fromRect_operation_fraction(
                    b,
                    CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0)),
                    NSCompositingOperation::SourceOver,
                    1.0,
                );
            }
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            self.on_tick();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let Some(win) = self.window() else { return };
            let at = NSEvent::mouseLocation();
            let origin = win.frame().origin;
            self.ivars().drag.set(Some((at, origin, false)));
            let _ = event;
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, _event: &NSEvent) {
            let Some(win) = self.window() else { return };
            let Some((from, origin, moved)) = self.ivars().drag.get() else { return };
            let at = NSEvent::mouseLocation();
            let (dx, dy) = (at.x - from.x, at.y - from.y);
            let moved = moved || dx.abs() + dy.abs() > 4.0;
            self.ivars().drag.set(Some((from, origin, moved)));
            if moved {
                win.setFrameOrigin(CGPoint::new(origin.x + dx, origin.y + dy));
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            let Some((_, _, moved)) = self.ivars().drag.take() else { return };
            if moved {
                if let Some(win) = self.window() {
                    save_position(win.frame().origin);
                }
            } else {
                self.toggle();
            }
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.menu(event);
        }

        #[unsafe(method(menuAction:))]
        fn menu_action(&self, sender: &NSMenuItem) {
            self.command(sender.tag());
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }

    unsafe impl NSObjectProtocol for SwarmView {}
);

impl SwarmView {
    fn new(mtm: MainThreadMarker, frame: CGRect, state: State) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars {
            state: RefCell::new(state),
            drag: Cell::new(None),
            image: RefCell::new(None),
        });
        // SAFETY: NSView's designated initializer
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn on_tick(&self) {
        let (repaint, busy) = {
            let mut st = self.ivars().state.borrow_mut();
            st.poll();
            let now = st.now();
            let dt = (now - st.last).min(0.05);
            st.last = now;
            let mode = st.mode();
            let (user, agent, voice_off) = (st.user, st.agent, st.voice_off);
            st.swarm.set_mode(mode, now);
            st.swarm.step(dt, now, user, agent, voice_off);
            // levels arrive only while there is sound; let them fall between events
            st.user *= 0.85;
            st.agent *= 0.85;
            let busy = st.swarm.busy();
            let px = st.px;
            let buf = render(&st.swarm, px, st.muted && st.connected);
            (image_from_bgra(&buf, px), busy)
        };
        if let Some(img) = repaint {
            *self.ivars().image.borrow_mut() = Some(img);
            self.setNeedsDisplay(true);
        }
        // nothing moves: the next tick costs only the poll
        let _ = busy;
    }

    fn toggle(&self) {
        let (connected, phase) = {
            let st = self.ivars().state.borrow();
            (st.connected, st.phase)
        };
        if !connected {
            notify("parlard is not running. Start it with: parlard service");
            return;
        }
        if phase != Phase::Stopped {
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

    fn menu(&self, event: &NSEvent) {
        let mtm = MainThreadMarker::from(self);
        let menu = NSMenu::new(mtm);
        let (connected, muted, voice_off, phase) = {
            let st = self.ivars().state.borrow();
            (st.connected, st.muted, st.voice_off, st.phase)
        };
        let add = |title: &str, tag: isize, checked: bool, enabled: bool| {
            // SAFETY: a plain title and the menuAction: selector this view defines
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(title),
                    (tag != 0).then_some(sel!(menuAction:)),
                    &NSString::from_str(""),
                )
            };
            item.setTag(tag);
            item.setEnabled(enabled);
            if checked {
                item.setState(NSControlStateValueOn);
            }
            // SAFETY: the view outlives the menu, which is dropped when it closes
            unsafe { item.setTarget(Some(self.as_ref())) };
            menu.addItem(&item);
        };
        let sep = || menu.addItem(&NSMenuItem::separatorItem(mtm));
        {
            let mut st = self.ivars().state.borrow_mut();
            st.menu_focus.clear();
            st.menu_inputs.clear();
            st.menu_outputs.clear();
        }
        if !connected {
            add("parlard is not running", 0, false, false);
        } else {
            if let Some(Response::State(st)) = link::request(&Request::State) {
                let sessions: Vec<_> = st.sessions.iter().filter(|s| s.session.is_some()).collect();
                if !sessions.is_empty() {
                    add("Talk to", 0, false, false);
                    for s in sessions {
                        let folder = std::path::Path::new(&s.cwd)
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .filter(|f| !f.is_empty())
                            .unwrap_or_else(|| "session".into());
                        let tag = TAG_FOCUS + self.ivars().state.borrow().menu_focus.len() as isize;
                        self.ivars().state.borrow_mut().menu_focus.push(s.session.clone().unwrap_or_default());
                        add(&format!("    {folder} ({})", harness_name(s.harness)), tag, s.focused, true);
                    }
                    sep();
                }
            }
            if let Some(Response::Devices { inputs, outputs }) = link::request(&Request::Devices) {
                add("Input", 0, false, false);
                for d in inputs {
                    let tag = TAG_INPUT + self.ivars().state.borrow().menu_inputs.len() as isize;
                    self.ivars().state.borrow_mut().menu_inputs.push(d.id);
                    add(&format!("    {}", d.name), tag, d.current, true);
                }
                add("Output", 0, false, false);
                for d in outputs {
                    let tag = TAG_OUTPUT + self.ivars().state.borrow().menu_outputs.len() as isize;
                    self.ivars().state.borrow_mut().menu_outputs.push(d.id);
                    add(&format!("    {}", d.name), tag, d.current, true);
                }
                sep();
            }
            add("Mute mic", TAG_MUTE, muted, true);
            add("Voice off (text only)", TAG_VOICE_OFF, voice_off, true);
            sep();
            let stopped = phase == Phase::Stopped;
            add(if stopped { "Start conversation" } else { "Stop conversation" }, TAG_TOGGLE, false, true);
        }
        sep();
        add("Close indicator", TAG_QUIT, false, true);
        NSMenu::popUpContextMenu_withEvent_forView(&menu, event, self);
    }

    fn command(&self, tag: isize) {
        let (muted, voice_off) = {
            let st = self.ivars().state.borrow();
            (st.muted, st.voice_off)
        };
        match tag {
            TAG_TOGGLE => self.toggle(),
            TAG_MUTE => send(link::set(|s| s.mic_muted = Some(!muted))),
            TAG_VOICE_OFF => send(link::set(|s| s.voice_off = Some(!voice_off))),
            TAG_QUIT => {
                let mtm = MainThreadMarker::from(self);
                NSApplication::sharedApplication(mtm).terminate(None);
            }
            t if t >= TAG_OUTPUT => {
                let id = self.ivars().state.borrow().menu_outputs.get((t - TAG_OUTPUT) as usize).cloned();
                if let Some(id) = id {
                    send(link::set(|s| s.output = Some(id)));
                }
            }
            t if t >= TAG_INPUT => {
                let id = self.ivars().state.borrow().menu_inputs.get((t - TAG_INPUT) as usize).cloned();
                if let Some(id) = id {
                    send(link::set(|s| s.input = Some(id)));
                }
            }
            t if t >= TAG_FOCUS => {
                let id = self.ivars().state.borrow().menu_focus.get((t - TAG_FOCUS) as usize).cloned();
                if let Some(id) = id {
                    send(link::set(|s| s.focus = Some(id)));
                }
            }
            _ => {}
        }
    }
}

impl State {
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
        }
    }
}

/// Wrap a premultiplied BGRA buffer in an NSImage. Core Graphics reads 32-bit little-endian
/// pixels with the alpha first as BGRA in memory, which is what the renderer writes.
fn image_from_bgra(buf: &[u8], w: usize) -> Option<Retained<NSImage>> {
    let data = buf.to_vec().into_boxed_slice();
    let len = data.len();
    let raw = Box::into_raw(data) as *mut u8;
    unsafe extern "C-unwind" fn release(_info: *mut core::ffi::c_void, data: NonNull<core::ffi::c_void>, size: usize) {
        // SAFETY: the pointer and length are the ones handed over below
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(data.as_ptr() as *mut u8, size)) });
    }
    // SAFETY: the provider owns the buffer and frees it through `release`
    let provider = unsafe { CGDataProvider::with_data(std::ptr::null_mut(), raw.cast(), len, Some(release)) }?;
    let space: CFRetained<CGColorSpace> = CGColorSpace::new_device_rgb()?;
    let info = CGBitmapInfo(CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0);
    // SAFETY: the dimensions match the buffer: w by w, 4 bytes per pixel
    let cg = unsafe {
        CGImage::new(
            w,
            w,
            8,
            32,
            w * 4,
            Some(&space),
            info,
            Some(&provider),
            std::ptr::null(),
            true,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;
    Some(NSImage::initWithCGImage_size(NSImage::alloc(), &cg, CGSize::new(SIZE, SIZE)))
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

/// A notice to the person: a line in parlard's log for now, so nothing modal appears over
/// their work. The menu shows the same state.
fn notify(text: &str) {
    eprintln!("parlar: {text}");
}

fn position_path() -> std::path::PathBuf {
    parlar::dirs::config().join("indicator.json")
}

fn save_position(p: CGPoint) {
    let path = position_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(path, format!("[{},{}]", p.x as i64, p.y as i64));
}

/// The saved spot kept within the whole desktop, else the top right of the main screen.
fn initial_frame(mtm: MainThreadMarker) -> CGRect {
    let screens = NSScreen::screens(mtm);
    let main = NSScreen::mainScreen(mtm).map(|s| s.frame());
    let saved = std::fs::read_to_string(position_path()).ok().and_then(|s| serde_json::from_str::<(f64, f64)>(&s).ok());
    let origin = match (saved, main) {
        (Some((x, y)), _) => {
            let inside = screens.iter().any(|s| {
                let f = s.frame();
                x + SIZE / 2.0 >= f.origin.x
                    && x + SIZE / 2.0 < f.origin.x + f.size.width
                    && y + SIZE / 2.0 >= f.origin.y
                    && y + SIZE / 2.0 < f.origin.y + f.size.height
            });
            if inside { CGPoint::new(x, y) } else { top_right(main) }
        }
        (None, m) => top_right(m),
    };
    CGRect::new(origin, CGSize::new(SIZE, SIZE))
}

fn top_right(main: Option<CGRect>) -> CGPoint {
    let f = main.unwrap_or(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(1440.0, 900.0)));
    // AppKit's y grows upward: the top is origin.y + height
    CGPoint::new(f.origin.x + f.size.width - SIZE - 40.0, f.origin.y + f.size.height - SIZE - 60.0)
}

pub fn run() -> Result<()> {
    let mtm = MainThreadMarker::new().context("the indicator must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    // no Dock icon, no menu bar of its own
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let frame = initial_frame(mtm);
    // SAFETY: NSWindow's designated initializer, with a borderless mask
    let win = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    win.setLevel(NSFloatingWindowLevel);
    win.setOpaque(false);
    win.setBackgroundColor(Some(&NSColor::clearColor()));
    win.setHasShadow(false);
    win.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    let scale = win.backingScaleFactor();
    let (tx, rx) = mpsc::channel();
    link::subscribe(tx);
    let state = State {
        swarm: Swarm::new(),
        rx,
        connected: false,
        lost_at: 0.0,
        phase: Phase::Connecting,
        muted: false,
        voice_off: false,
        user: 0.0,
        agent: 0.0,
        start: Instant::now(),
        last: 0.0,
        px: (SIZE * scale) as usize,
        menu_focus: Vec::new(),
        menu_inputs: Vec::new(),
        menu_outputs: Vec::new(),
    };
    let view = SwarmView::new(mtm, CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(SIZE, SIZE)), state);
    win.setContentView(Some(&view));
    win.orderFrontRegardless();
    // SAFETY: the view lives as long as the window, which lives for the run loop
    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            FRAME_S,
            &view,
            sel!(tick:),
            None,
            true,
        )
    };
    app.run();
    Ok(())
}

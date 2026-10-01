//! parlard: owns sessions, focus, the utterance queues and the speech queue.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::format;
use crate::proto::*;
use crate::voice::Voice;

struct Waiter {
    id: u64,
    tx: oneshot::Sender<WaitResult>,
    holds_turn: bool,
}

enum WaitResult {
    Items(Vec<Utterance>),
    Superseded,
}

struct Session {
    key: u64,
    session: Option<String>,
    harness: Harness,
    cwd: String,
    /// Parent pid of this session's MCP server, which is the harness process itself.
    mcp_parent: Option<u32>,
    /// MCP connections attached to this session (a reconnect overlaps the old one briefly).
    mcp_conns: u32,
    /// The harness process as seen from this session's hooks.
    harness_pid: Option<u32>,
    pending: Vec<Utterance>,
    waiter: Option<Waiter>,
    in_turn: bool,
    /// Voice mode reminder not yet delivered.
    remind: bool,
    /// The agent spoke through `say` since the user last spoke to it.
    said: bool,
    /// When the Stop hook last continued the turn with pending speech.
    blocked_at: Option<std::time::Instant>,
    /// Conversation lines not yet printed in this session's terminal.
    transcript: Vec<String>,
}

impl Session {
    fn new(key: u64, session: Option<String>, harness: Harness, cwd: String) -> Session {
        Session {
            key,
            session,
            harness,
            cwd,
            mcp_parent: None,
            mcp_conns: 0,
            harness_pid: None,
            pending: Vec::new(),
            waiter: None,
            in_turn: false,
            remind: false,
            said: false,
            blocked_at: None,
            transcript: Vec::new(),
        }
    }

    /// The harness pid by either route.
    fn pid(&self) -> Option<u32> {
        self.harness_pid.or(self.mcp_parent)
    }
}

/// Most transcript lines kept for a session that never prints them.
const TRANSCRIPT_CAP: usize = 40;

pub struct State {
    sessions: Vec<Session>,
    focus: Option<u64>,
    next_key: u64,
    next_utt: UtteranceId,
    next_waiter: u64,
    active: bool,
    mic_muted: bool,
    voice_off: bool,
    speaking: bool,
    listening: bool,
    /// Raised to cut the agent off mid-line (barge-in); the speaker polls it.
    barge: Arc<AtomicBool>,
    /// Mirrors "mic on" for the capture thread: active and not muted.
    mic_gate: Arc<AtomicBool>,
    /// Mirrors "voice on" for the speaker, which keeps its model loaded while it is up: active
    /// and not silenced.
    voice_gate: Arc<AtomicBool>,
    /// Words of the agent's line that the user talked over, for the next utterance.
    cut: Option<String>,
    /// The last utterance delivered and when, so a quick follow-up is marked as its continuation.
    last_heard: Option<(UtteranceId, u64, std::time::Instant)>,
    /// Focus saved from before a restart, waiting for that session's hooks to show up again.
    restore_focus: Option<String>,
    ui: broadcast::Sender<Ui>,
}

impl State {
    pub fn new(ui: broadcast::Sender<Ui>) -> Self {
        State {
            sessions: Vec::new(),
            focus: None,
            next_key: 1,
            next_utt: 1,
            next_waiter: 1,
            // the mic stays closed until a person starts the conversation
            active: false,
            mic_muted: false,
            voice_off: false,
            speaking: false,
            listening: false,
            barge: Arc::new(AtomicBool::new(false)),
            mic_gate: Arc::new(AtomicBool::new(false)),
            voice_gate: Arc::new(AtomicBool::new(false)),
            cut: None,
            last_heard: None,
            restore_focus: None,
            ui,
        }
    }

    /// Bring back the conversation state saved before a restart.
    pub fn restore_saved(&mut self) {
        let Some(saved) = Saved::load() else { return };
        self.active = saved.active;
        self.mic_muted = saved.mic_muted;
        self.voice_off = saved.voice_off;
        self.restore_focus = saved.focus;
        self.sync_gate();
    }

    fn save(&self) {
        let focus = self
            .focus
            .and_then(|k| self.sessions.iter().find(|s| s.key == k))
            .and_then(|s| s.session.clone())
            .or_else(|| self.restore_focus.clone());
        Saved { active: self.active, mic_muted: self.mic_muted, voice_off: self.voice_off, focus }.store();
    }

    /// A session showed up with the id that had focus before the restart.
    fn reclaim_focus(&mut self, i: usize) {
        if self.focus.is_none() && self.restore_focus.is_some() && self.sessions[i].session == self.restore_focus {
            self.focus = Some(self.sessions[i].key);
            self.restore_focus = None;
        }
    }

    fn find(&self, o: &Origin) -> Option<usize> {
        if let Some(sid) = &o.session
            && let Some(i) = self.sessions.iter().position(|s| s.session.as_deref() == Some(sid)) {
                return Some(i);
            }
        // hooks always carry a session id, so an origin without one is an MCP server (older
        // servers do not send the mcp flag or the harness pid)
        let mcp = o.mcp || o.session.is_none();
        let h = o.harness_pid.or_else(|| if mcp { o.pids.first().copied() } else { None })?;
        if mcp {
            // the MCP server matches its harness, whatever session id the harness reports
            return self.sessions.iter().position(|s| s.pid() == Some(h));
        }
        // a hook from a harness known so far only through its MCP server, or one that cleared
        // its conversation and starts a new session id
        self.sessions.iter().position(|s| s.session.is_none() && s.pid() == Some(h))
    }

    /// Find the session, attaching it on the fly when a hook from a session that started before
    /// parlard shows up. MCP-only origins are never created here.
    fn find_or_attach(&mut self, o: &Origin) -> Option<usize> {
        if let Some(i) = self.find(o) {
            return Some(i);
        }
        o.session.as_ref()?;
        let key = self.next_key;
        self.next_key += 1;
        let mut s = Session::new(key, o.session.clone(), Harness::Other, String::new());
        s.harness_pid = o.harness_pid;
        self.sessions.push(s);
        let i = self.sessions.len() - 1;
        self.reclaim_focus(i);
        Some(i)
    }

    fn focused(&self, i: usize) -> bool {
        self.focus == Some(self.sessions[i].key)
    }

    pub fn phase(&self) -> Phase {
        if !self.active {
            return Phase::Stopped;
        }
        if self.listening && self.speaking {
            return Phase::Interrupting;
        }
        if self.listening {
            return Phase::Listening;
        }
        if self.speaking {
            return Phase::Speaking;
        }
        let working = self
            .focus
            .and_then(|k| self.sessions.iter().find(|s| s.key == k))
            .is_some_and(|s| s.in_turn);
        if working { Phase::Working } else { Phase::Ready }
    }

    fn emit_phase(&self) {
        let _ = self.ui.send(Ui::Phase {
            phase: self.phase(),
            mic_muted: self.mic_muted,
            voice_off: self.voice_off,
        });
    }

    /// Take the pending utterances for delivery, with the voice mode reminder on the first one
    /// when the conversation was started after this session last heard from parlar.
    fn take(&mut self, i: usize) -> Vec<Utterance> {
        let s = &mut self.sessions[i];
        let mut items = std::mem::take(&mut s.pending);
        remind(s, &mut items);
        if !items.is_empty() {
            s.said = false;
        }
        items
    }

    fn note(&mut self, i: usize, line: String) {
        let t = &mut self.sessions[i].transcript;
        t.push(line);
        let over = t.len().saturating_sub(TRANSCRIPT_CAP);
        t.drain(..over);
    }

    /// Put utterances that did not reach their reader back in front of the queue.
    fn restore(&mut self, o: &Origin, items: Vec<Utterance>) {
        if items.is_empty() {
            return;
        }
        if let Some(i) = self.find(o) {
            let s = &mut self.sessions[i];
            let rest = std::mem::take(&mut s.pending);
            s.pending = items;
            s.pending.extend(rest);
            s.in_turn = false;
        }
    }

    fn supersede(&mut self, i: usize) {
        if let Some(w) = self.sessions[i].waiter.take() {
            let _ = w.tx.send(WaitResult::Superseded);
        }
    }

    /// A waiter that holds a turn open may only wait while the conversation is on and its session
    /// has focus.
    fn release_held(&mut self) {
        for i in 0..self.sessions.len() {
            let talking = self.active && self.focused(i);
            if !talking && self.sessions[i].waiter.as_ref().is_some_and(|w| w.holds_turn) {
                self.supersede(i);
            }
        }
    }

    fn report(&self) -> StateReport {
        StateReport {
            phase: self.phase(),
            active: self.active,
            mic_muted: self.mic_muted,
            voice_off: self.voice_off,
            sessions: self
                .sessions
                .iter()
                .enumerate()
                .map(|(i, s)| SessionInfo {
                    session: s.session.clone(),
                    harness: s.harness,
                    cwd: s.cwd.clone(),
                    focused: self.focused(i),
                    pending: s.pending.len(),
                    waiting: s.waiter.is_some(),
                })
                .collect(),
        }
    }

    /// Route a finished utterance to the focused session.
    pub fn deliver(&mut self, text: String, heard: Option<String>) -> (UtteranceId, Option<String>) {
        let id = self.next_utt;
        self.next_utt += 1;
        let heard = heard.filter(|h| h.trim() != text.trim());
        let interrupted_after = self.cut.take();
        let Some(i) = self.focus.and_then(|k| self.sessions.iter().position(|s| s.key == k)) else {
            eprintln!("heard u{id} with no session in focus, dropped: {text}");
            return (id, None);
        };
        let key = self.sessions[i].key;
        let recent = self
            .last_heard
            .filter(|(_, k, at)| *k == key && at.elapsed() < CONTINUATION)
            .map(|(prev, _, _)| prev);
        self.note(i, format!("you: {text}"));
        // a follow-up to an utterance nobody has read yet joins it instead of trailing behind
        if let Some(prev) = recent
            && let Some(last) = self.sessions[i].pending.last_mut().filter(|u| u.id == prev) {
                last.text = format!("{} {}", last.text.trim_end(), text.trim());
                if let Some(h) = heard {
                    last.heard = Some(format!("{} {}", last.heard.clone().unwrap_or_default(), h).trim().to_string());
                }
                self.last_heard = Some((prev, key, std::time::Instant::now()));
                eprintln!("heard u{prev} (continued): {text}");
                return (prev, self.sessions[i].session.clone());
            }
        self.last_heard = Some((id, key, std::time::Instant::now()));
        let u = Utterance { id, text, heard, revises: recent, interrupted_after };
        eprintln!(
            "heard u{id} -> {}: {}",
            if self.sessions[i].cwd.is_empty() { "session" } else { self.sessions[i].cwd.as_str() },
            u.text
        );
        let _ = self.ui.send(Ui::Caption { who: "user".into(), text: u.text.clone() });
        let s = &mut self.sessions[i];
        s.pending.push(u);
        if let Some(w) = s.waiter.take() {
            let mut items = std::mem::take(&mut s.pending);
            remind(s, &mut items);
            s.said = false;
            s.in_turn = true;
            // the waiting hook may be gone (killed, timed out): keep what it can no longer read
            if let Err(WaitResult::Items(back)) = w.tx.send(WaitResult::Items(items)) {
                s.pending = back;
                s.in_turn = false;
            }
        }
        let to = s.session.clone().or_else(|| Some(format!("mcp:{}", s.mcp_parent.unwrap_or(0))));
        self.emit_phase();
        (id, to)
    }
}

fn remind(s: &mut Session, items: &mut [Utterance]) {
    if s.remind
        && let Some(first) = items.first_mut() {
            s.remind = false;
            first.text = format!("{}\n{}", format::ACTIVATED, first.text);
        }
}

/// A follow-up this soon after a delivered utterance is marked as its continuation.
const CONTINUATION: Duration = Duration::from_secs(5);

pub type Shared = Arc<Mutex<State>>;

/// Conversation state that survives a daemon restart, in `$XDG_STATE_HOME/parlar/state.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    active: bool,
    mic_muted: bool,
    voice_off: bool,
    focus: Option<String>,
}

impl Saved {
    fn path() -> std::path::PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
        base.join("parlar/state.json")
    }

    fn load() -> Option<Saved> {
        serde_json::from_str(&std::fs::read_to_string(Self::path()).ok()?).ok()
    }

    fn store(&self) {
        // tests and test daemons on their own socket must not overwrite the real state
        if std::env::var_os("PARLAR_SOCKET").is_some() || cfg!(test) {
            return;
        }
        let p = Self::path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(s) = serde_json::to_string(self) {
            let _ = std::fs::write(p, s);
        }
    }
}

/// Device control, implemented by the process that owns the audio streams.
pub trait Audio: Send + Sync {
    fn devices(&self) -> (Vec<Device>, Vec<Device>);
    fn set_input(&self, id: &str) -> anyhow::Result<()>;
    fn set_output(&self, id: &str) -> anyhow::Result<()>;
}

pub struct Daemon {
    pub state: Shared,
    pub ui: broadcast::Sender<Ui>,
    pub voice: Arc<dyn Voice>,
    pub audio: Option<Arc<dyn Audio>>,
}

impl Daemon {
    pub fn new(voice: Arc<dyn Voice>) -> Self {
        let (ui, _) = broadcast::channel(256);
        Daemon { state: Arc::new(Mutex::new(State::new(ui.clone()))), ui, voice, audio: None }
    }

    pub async fn serve(self: Arc<Self>, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            // only lock down a directory that is ours, never a shared one like /tmp
            if dir.file_name().is_some_and(|n| n == "parlar") {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        if path.exists() {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                anyhow::bail!("parlard is already running on {}", path.display());
            }
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
        // removed when serving ends, and only by the daemon that bound it: a second parlard that
        // finds this one running must leave its socket alone
        let _unlink = Unlink(path.to_path_buf());
        eprintln!("parlard listening on {}", path.display());
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                state.lock().await.prune();
            }
        });
        loop {
            let (stream, _) = listener.accept().await?;
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.conn(stream).await {
                    eprintln!("connection: {e:#}");
                }
            });
        }
    }

    async fn conn(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let mut mcp_key = None;
        let r = self.lines(stream, &mut mcp_key).await;
        if let Some(key) = mcp_key {
            self.state.lock().await.mcp_gone(key);
        }
        r
    }

    /// Serve requests on one connection. An MCP server keeps its connection for its whole life,
    /// so the session it attached is recorded in `mcp_key` and released when the line ends.
    async fn lines(self: &Arc<Self>, stream: UnixStream, mcp_key: &mut Option<u64>) -> Result<()> {
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let req: Request = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    write(&mut wr, &Response::Error { message: format!("bad request: {e}") }).await?;
                    continue;
                }
            };
            if matches!(req, Request::Subscribe) {
                return self.subscribe(wr).await;
            }
            let mcp_origin = match &req {
                Request::Attach { origin, mcp: true, .. } => Some(origin.clone()),
                _ => None,
            };
            if let Request::Wait { origin, .. } = &req {
                let origin = origin.clone();
                // a waiting hook sends nothing more; if its end closes, it died, and the wait must
                // not keep a reader that will never read
                let resp = tokio::select! {
                    r = self.handle(req) => r,
                    _ = lines.next_line() => return Ok(()),
                };
                if let Err(e) = write(&mut wr, &resp).await {
                    if let Response::Utterances { items, .. } = resp {
                        self.state.lock().await.restore(&origin, items);
                    }
                    return Err(e);
                }
                continue;
            }
            let resp = self.handle(req).await;
            if let Some(o) = mcp_origin {
                let st = self.state.lock().await;
                let key = st.find(&o).map(|i| st.sessions[i].key);
                if mcp_key.is_none() {
                    *mcp_key = key;
                }
            }
            write(&mut wr, &resp).await?;
        }
        Ok(())
    }

    async fn subscribe(&self, mut wr: tokio::net::unix::OwnedWriteHalf) -> Result<()> {
        let mut rx = self.ui.subscribe();
        {
            let st = self.state.lock().await;
            let first = Ui::Phase { phase: st.phase(), mic_muted: st.mic_muted, voice_off: st.voice_off };
            write(&mut wr, &first).await?;
        }
        loop {
            match rx.recv().await {
                Ok(ev) => write(&mut wr, &ev).await?,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }

    pub async fn handle(&self, req: Request) -> Response {
        match req {
            Request::Attach { origin, harness, cwd, mcp } => {
                let mut st = self.state.lock().await;
                let self_active = st.active;
                let _ = self_active;
                let i = match st.find(&origin) {
                    Some(i) => i,
                    None => {
                        let key = st.next_key;
                        st.next_key += 1;
                        st.sessions.push(Session::new(key, None, harness, cwd.clone()));
                        st.sessions.len() - 1
                    }
                };
                let s = &mut st.sessions[i];
                if mcp {
                    s.mcp_parent = origin.harness_pid.or_else(|| origin.pids.first().copied());
                    s.mcp_conns += 1;
                } else {
                    if origin.harness_pid.is_some() {
                        s.harness_pid = origin.harness_pid;
                    }
                    if origin.session.is_some() {
                        s.session = origin.session.clone();
                    }
                }
                if !cwd.is_empty() {
                    s.cwd = cwd;
                }
                if harness != Harness::Other {
                    s.harness = harness;
                }
                st.reclaim_focus(i);
                // focus moves only when the person moves it (/parlar:talk, the indicator menu):
                // agent sessions get prompts all the time, so following prompts sends speech to
                // whichever agent happened to be busy
                st.emit_phase();
                Response::Attached { focused: st.focused(i), active: st.active }
            }
            Request::Detach { origin, rebind } => {
                let mut st = self.state.lock().await;
                if let Some(i) = st.find(&origin) {
                    st.supersede(i);
                    if rebind {
                        // /clear: the same harness comes back with a new session id
                        st.sessions[i].session = None;
                    } else {
                        let key = st.sessions.remove(i).key;
                        if st.focus == Some(key) {
                            st.focus = None;
                        }
                    }
                    st.emit_phase();
                }
                Response::Ok
            }
            Request::Claim { origin, at_stop } => {
                let mut st = self.state.lock().await;
                let Some(i) = st.find_or_attach(&origin) else { return empty() };
                let items = st.take(i);
                if at_stop && !items.is_empty() {
                    st.sessions[i].blocked_at = Some(std::time::Instant::now());
                    st.sessions[i].in_turn = true;
                }
                Response::Utterances { items, superseded: false }
            }
            Request::ClaimStop { origin } => {
                let mut st = self.state.lock().await;
                let Some(i) = st.find_or_attach(&origin) else { return empty() };
                let s = &mut st.sessions[i];
                if !s.pending.iter().any(|u| format::is_stop(&u.text)) {
                    return empty();
                }
                // a stop takes everything said so far with it, so the reason reads in order
                Response::Utterances { items: st.take(i), superseded: false }
            }
            Request::Wait { origin, timeout_ms, holds_turn } => self.wait(origin, timeout_ms, holds_turn).await,
            Request::Say { origin, text, kind } => self.say(origin, text, kind).await,
            Request::Event { origin, event, tool } => {
                let mut st = self.state.lock().await;
                let Some(i) = st.find_or_attach(&origin) else { return Response::Ok };
                match event {
                    TurnEvent::TurnStart => {
                        // a typed prompt: the idle waiter must not wake a busy session
                        st.supersede(i);
                        st.release_held();
                        st.sessions[i].in_turn = true;
                        st.sessions[i].said = true;
                    }
                    TurnEvent::ToolStart => st.sessions[i].in_turn = true,
                    TurnEvent::ToolEnd | TurnEvent::ToolError => {
                        st.sessions[i].in_turn = true;
                        if st.focused(i) {
                            let _ = st.ui.send(Ui::Tool { ok: event == TurnEvent::ToolEnd, name: tool });
                        }
                    }
                }
                st.emit_phase();
                Response::Ok
            }
            Request::TurnEnd { origin, last_message } => {
                let (opening, voice_off) = {
                    let mut st = self.state.lock().await;
                    let Some(i) = st.find_or_attach(&origin) else { return Response::Ok };
                    let quiet = !st.sessions[i].said && st.active && st.focused(i);
                    st.sessions[i].said = true;
                    let opening = quiet.then_some(()).and(last_message).map(|m| format::opening(&m));
                    (opening.filter(|m| !m.is_empty()), st.voice_off)
                };
                if let Some(text) = opening {
                    eprintln!("said nothing this turn, speaking its opening: {text}");
                    let _ = self.ui.send(Ui::Caption { who: "agent".into(), text: text.clone() });
                    if !voice_off {
                        self.voice.speak(text, SayKind::Answer, self.state.clone());
                    }
                }
                Response::Ok
            }
            Request::Transcript { origin } => {
                let mut st = self.state.lock().await;
                let lines = match st.find(&origin) {
                    Some(i) => std::mem::take(&mut st.sessions[i].transcript),
                    None => vec![],
                };
                Response::Transcript { lines }
            }
            Request::Talk { origin, harness, cwd } => {
                let mut st = self.state.lock().await;
                let i = match st.find(&origin) {
                    Some(i) => i,
                    None => {
                        let Some(sid) = origin.session.clone() else {
                            return Response::Error { message: "no session id to talk to".into() };
                        };
                        let key = st.next_key;
                        st.next_key += 1;
                        let mut s = Session::new(key, Some(sid), harness, cwd.clone());
                        s.harness_pid = origin.harness_pid;
                        st.sessions.push(s);
                        st.sessions.len() - 1
                    }
                };
                if !cwd.is_empty() && st.sessions[i].cwd.is_empty() {
                    st.sessions[i].cwd = cwd;
                }
                st.focus = Some(st.sessions[i].key);
                st.sessions[i].remind = true;
                st.active = true;
                st.mic_muted = false;
                st.sync_gate();
                st.release_held();
                st.emit_phase();
                st.save();
                Response::Attached { focused: true, active: true }
            }
            Request::Hear { text, heard } => {
                let mut st = self.state.lock().await;
                if !st.active || st.mic_muted {
                    return Response::Error { message: "voice mode is off or the mic is muted".into() };
                }
                let (id, delivered_to) = st.deliver(text, heard);
                Response::Heard { id, delivered_to }
            }
            Request::Devices => match &self.audio {
                Some(a) => {
                    let (inputs, outputs) = a.devices();
                    Response::Devices { inputs, outputs }
                }
                None => Response::Devices { inputs: vec![], outputs: vec![] },
            },
            Request::Set { active, mic_muted, voice_off, focus, input, output } => {
                if input.is_some() || output.is_some() {
                    let Some(a) = &self.audio else {
                        return Response::Error { message: "this parlard has no audio devices".into() };
                    };
                    let r = match (&input, &output) {
                        (Some(i), _) => a.set_input(i),
                        _ => Ok(()),
                    }
                    .and_then(|_| match &output {
                        Some(o) => a.set_output(o),
                        None => Ok(()),
                    });
                    if let Err(e) = r {
                        return Response::Error { message: format!("{e:#}") };
                    }
                }
                let mut st = self.state.lock().await;
                if let Some(a) = active {
                    if a && !st.active
                        && let Some(k) = st.focus {
                            st.sessions.iter_mut().filter(|s| s.key == k).for_each(|s| s.remind = true);
                        }
                    // waiters stay armed while stopped, so starting again can wake an idle session
                    st.active = a;
                }
                if let Some(m) = mic_muted {
                    st.mic_muted = m;
                }
                if let Some(v) = voice_off {
                    st.voice_off = v;
                }
                st.sync_gate();
                if let Some(f) = focus {
                    match st.sessions.iter().position(|s| s.session.as_deref() == Some(f.as_str())) {
                        Some(i) => {
                            st.sessions[i].remind = true;
                            st.focus = Some(st.sessions[i].key);
                        }
                        None => return Response::Error { message: format!("no session {f}") },
                    }
                }
                st.release_held();
                st.emit_phase();
                st.save();
                Response::Ok
            }
            Request::State => Response::State(self.state.lock().await.report()),
            Request::Subscribe => Response::Error { message: "subscribe must be the first request".into() },
        }
    }

    async fn wait(&self, origin: Origin, timeout_ms: u64, holds_turn: bool) -> Response {
        let (id, rx) = {
            let mut st = self.state.lock().await;
            let Some(i) = st.find_or_attach(&origin) else { return empty() };
            let talking = st.active && st.focused(i);
            if holds_turn && !talking {
                // holding a turn open is only right while someone is talking to this session
                return Response::Utterances { items: vec![], superseded: true };
            }
            let continued = st.sessions[i].blocked_at.is_some_and(|t| t.elapsed() < Duration::from_secs(3));
            if !holds_turn && continued {
                // the Stop hook just continued this turn; this waiter belongs to a turn that did
                // not end
                return Response::Utterances { items: vec![], superseded: true };
            }
            st.sessions[i].in_turn = false;
            st.emit_phase();
            if !st.sessions[i].pending.is_empty() {
                st.sessions[i].in_turn = true;
                return Response::Utterances { items: st.take(i), superseded: false };
            }
            st.supersede(i);
            let id = st.next_waiter;
            st.next_waiter += 1;
            let (tx, rx) = oneshot::channel();
            st.sessions[i].waiter = Some(Waiter { id, tx, holds_turn });
            (id, rx)
        };
        match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(WaitResult::Items(items))) => Response::Utterances { items, superseded: false },
            Ok(Ok(WaitResult::Superseded)) | Ok(Err(_)) => {
                Response::Utterances { items: vec![], superseded: true }
            }
            Err(_) => {
                let mut st = self.state.lock().await;
                for s in &mut st.sessions {
                    if s.waiter.as_ref().is_some_and(|w| w.id == id) {
                        s.waiter = None;
                    }
                }
                empty()
            }
        }
    }

    async fn say(&self, origin: Origin, text: String, kind: SayKind) -> Response {
        let (focused, items, voice_off) = {
            let mut st = self.state.lock().await;
            let Some(i) = st.find(&origin) else {
                return Response::Error { message: "session is not attached to parlard".into() };
            };
            let focused = st.focused(i) && st.active;
            let items = if focused { st.take(i) } else { vec![] };
            st.sessions[i].said = true;
            st.note(i, format!("parlar: {}", format::speakable(&text)));
            (focused, items, st.voice_off)
        };
        let spoken_text = format::speakable(&text);
        let _ = self.ui.send(Ui::Caption { who: "agent".into(), text: spoken_text.clone() });
        let spoken = focused && !voice_off && !spoken_text.is_empty();
        if spoken {
            self.voice.speak(spoken_text, kind, self.state.clone());
        }
        Response::Said { spoken, items }
    }
}

fn empty() -> Response {
    Response::Utterances { items: vec![], superseded: false }
}

async fn write<T: serde::Serialize>(wr: &mut tokio::net::unix::OwnedWriteHalf, v: &T) -> Result<()> {
    let mut line = serde_json::to_vec(v)?;
    line.push(b'\n');
    wr.write_all(&line).await?;
    Ok(())
}

impl State {
    /// Drop sessions whose harness process is gone without saying goodbye (a crash, a kill).
    fn prune(&mut self) {
        let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
        let gone: Vec<usize> = (0..self.sessions.len())
            .rev()
            .filter(|&i| self.sessions[i].pid().is_some_and(|p| !alive(p)))
            .collect();
        for i in gone {
            self.supersede(i);
            let key = self.sessions.remove(i).key;
            if self.focus == Some(key) {
                self.focus = None;
            }
        }
    }

    /// The MCP server of a session went away. A session known only through its MCP server is
    /// dropped; one with hooks keeps living until its harness exits.
    fn mcp_gone(&mut self, key: u64) {
        let Some(i) = self.sessions.iter().position(|s| s.key == key) else { return };
        let s = &mut self.sessions[i];
        s.mcp_conns = s.mcp_conns.saturating_sub(1);
        // a reconnecting MCP server overlaps its old connection; a hook session keeps the harness
        // pid so prune() can tell when the harness itself exits
        if s.session.is_none() && s.mcp_conns == 0 {
            self.supersede(i);
            self.sessions.remove(i);
            if self.focus == Some(key) {
                self.focus = None;
            }
            self.emit_phase();
        }
    }

    pub fn set_speaking(&mut self, on: bool) {
        self.speaking = on;
        self.emit_phase();
    }
    pub fn set_listening(&mut self, on: bool) {
        if self.listening != on {
            self.listening = on;
            self.emit_phase();
        }
    }
    /// Working directory of the session with voice focus.
    pub fn focused_cwd(&self) -> Option<String> {
        let k = self.focus?;
        self.sessions.iter().find(|s| s.key == k).map(|s| s.cwd.clone()).filter(|c| !c.is_empty())
    }

    pub fn speaking(&self) -> bool {
        self.speaking
    }
    pub fn barge(&self) -> Arc<AtomicBool> {
        self.barge.clone()
    }
    pub fn mic_gate(&self) -> Arc<AtomicBool> {
        self.mic_gate.clone()
    }
    /// Mirror "voice on" into `gate` from now on.
    pub fn share_voice_gate(&mut self, gate: Arc<AtomicBool>) {
        self.voice_gate = gate;
        self.sync_gate();
    }
    fn sync_gate(&self) {
        self.mic_gate.store(self.active && !self.mic_muted, Ordering::SeqCst);
        self.voice_gate.store(self.active && !self.voice_off, Ordering::SeqCst);
    }
    pub fn set_cut(&mut self, cut: String) {
        // nothing was heard yet when the user cut in: there is nothing to report
        self.cut = Some(cut).filter(|c| !c.trim().is_empty());
    }
    pub fn deliver_spoken(&mut self, text: String, heard: Option<String>) -> Option<UtteranceId> {
        if !self.active || self.mic_muted {
            return None;
        }
        Some(self.deliver(text, heard).0)
    }
    pub fn ui(&self) -> broadcast::Sender<Ui> {
        self.ui.clone()
    }
}

struct Unlink(std::path::PathBuf);

impl Drop for Unlink {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::{Engine, Queue};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, r: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>, req: &Request) -> Response {
        let mut v = serde_json::to_vec(req).unwrap();
        v.push(b'\n');
        w.write_all(&v).await.unwrap();
        serde_json::from_str(&r.next_line().await.unwrap().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn mcp_only_session_is_released_with_its_connection() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        let (a, b) = UnixStream::pair().unwrap();
        let served = tokio::spawn(d.clone().conn(b));
        let (rd, mut wr) = a.into_split();
        let mut lines = BufReader::new(rd).lines();
        let o = Origin { session: None, pids: vec![4242], harness_pid: Some(4242), mcp: true };
        let r = send(&mut wr, &mut lines, &Request::Attach { origin: o, harness: Harness::Claude, cwd: String::new(), mcp: true }).await;
        assert_eq!(r, Response::Attached { focused: false, active: false }, "attaching never takes focus");
        assert_eq!(d.state.lock().await.sessions.len(), 1);
        drop(wr);
        drop(lines);
        served.await.unwrap().unwrap();
        let st = d.state.lock().await;
        assert!(st.sessions.is_empty());
        assert_eq!(st.focus, None);
    }

    #[tokio::test]
    async fn turn_holding_waiter_is_released_when_the_conversation_stops() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        let on = |a| Request::Set { active: Some(a), mic_muted: None, voice_off: None, focus: None, input: None, output: None };
        d.handle(on(true)).await;
        let o = Origin { session: Some("cx".into()), pids: vec![1], ..Default::default() };
        d.handle(Request::Attach { origin: o.clone(), harness: Harness::Codex, cwd: String::new(), mcp: false }).await;
        d.handle(Request::Set { active: None, mic_muted: None, voice_off: None, focus: Some("cx".into()), input: None, output: None }).await;
        let d2 = d.clone();
        let w = tokio::spawn(async move { d2.handle(Request::Wait { origin: o, timeout_ms: 60_000, holds_turn: true }).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        d.handle(on(false)).await;
        let r = tokio::time::timeout(Duration::from_secs(2), w).await.expect("released").unwrap();
        assert_eq!(r, Response::Utterances { items: vec![], superseded: true });
    }

    #[tokio::test]
    async fn prompts_in_other_sessions_never_move_focus() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        let set = |focus: Option<&str>, active| Request::Set { active, mic_muted: None, voice_off: None, focus: focus.map(str::to_string), input: None, output: None };
        d.handle(set(None, Some(true))).await;
        let me = Origin { session: Some("me".into()), ..Default::default() };
        let agent = Origin { session: Some("agent".into()), ..Default::default() };
        for o in [&me, &agent] {
            d.handle(Request::Attach { origin: o.clone(), harness: Harness::Claude, cwd: String::new(), mcp: false }).await;
        }
        d.handle(set(Some("me"), None)).await;
        d.handle(Request::Event { origin: agent.clone(), event: TurnEvent::TurnStart, tool: None }).await;
        d.handle(Request::Hear { text: "hello".into(), heard: None }).await;
        d.handle(Request::Hear { text: "and more".into(), heard: None }).await;
        match d.handle(Request::Claim { origin: me, at_stop: false }).await {
            Response::Utterances { items, .. } => {
                assert_eq!(items.len(), 1, "a quick follow-up joins an unread utterance");
                assert!(items[0].text.ends_with("hello and more"), "{}", items[0].text);
            }
            r => panic!("{r:?}"),
        }
        match d.handle(Request::Claim { origin: agent, at_stop: false }).await {
            Response::Utterances { items, .. } => assert!(items.is_empty()),
            r => panic!("{r:?}"),
        }
    }

    #[tokio::test]
    async fn hook_session_survives_its_mcp_and_gets_utterances() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        d.handle(Request::Set { active: Some(true), mic_muted: None, voice_off: None, focus: None, input: None, output: None }).await;
        let hook = Origin { session: Some("s1".into()), pids: vec![10, 4242, 7], harness_pid: Some(4242), mcp: false };
        d.handle(Request::Attach { origin: hook.clone(), harness: Harness::Claude, cwd: "/w".into(), mcp: false }).await;
        let (a, b) = UnixStream::pair().unwrap();
        let served = tokio::spawn(d.clone().conn(b));
        let (rd, mut wr) = a.into_split();
        let mut lines = BufReader::new(rd).lines();
        let mcp = Origin { session: None, pids: vec![4242], harness_pid: Some(4242), mcp: true };
        send(&mut wr, &mut lines, &Request::Attach { origin: mcp, harness: Harness::Claude, cwd: String::new(), mcp: true }).await;
        assert_eq!(d.state.lock().await.sessions.len(), 1, "MCP merged into the hook session");
        drop(wr);
        drop(lines);
        served.await.unwrap().unwrap();
        assert_eq!(d.state.lock().await.sessions.len(), 1);
        d.handle(Request::Set { active: None, mic_muted: None, voice_off: None, focus: Some("s1".into()), input: None, output: None }).await;
        d.handle(Request::Hear { text: "hello".into(), heard: None }).await;
        match d.handle(Request::Claim { origin: hook, at_stop: false }).await {
            Response::Utterances { items, .. } => {
                assert!(items[0].text.ends_with("hello"));
                assert!(items[0].text.starts_with(format::ACTIVATED), "focus change carries the reminder");
            }
            r => panic!("{r:?}"),
        }
    }

    fn set(active: Option<bool>, focus: Option<&str>) -> Request {
        Request::Set { active, mic_muted: None, voice_off: None, focus: focus.map(str::to_string), input: None, output: None }
    }

    async fn talking_to(d: &Daemon, sid: &str, harness_pid: u32) -> Origin {
        let o = Origin { session: Some(sid.into()), pids: vec![1, harness_pid], harness_pid: Some(harness_pid), mcp: false };
        d.handle(Request::Attach { origin: o.clone(), harness: Harness::Claude, cwd: String::new(), mcp: false }).await;
        d.handle(set(Some(true), Some(sid))).await;
        o
    }

    #[tokio::test]
    async fn a_dead_waiter_does_not_swallow_speech() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        let o = talking_to(&d, "s", 500).await;
        let (a, b) = UnixStream::pair().unwrap();
        let served = tokio::spawn(d.clone().conn(b));
        let (_rd, mut wr) = a.into_split();
        let mut v = serde_json::to_vec(&Request::Wait { origin: o.clone(), timeout_ms: 60_000, holds_turn: false }).unwrap();
        v.push(b'\n');
        wr.write_all(&v).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // the hook process dies while waiting
        drop(wr);
        drop(_rd);
        let _ = served.await;
        d.handle(Request::Hear { text: "still here".into(), heard: None }).await;
        match d.handle(Request::Claim { origin: o, at_stop: false }).await {
            Response::Utterances { items, .. } => assert!(items[0].text.ends_with("still here")),
            r => panic!("{r:?}"),
        }
    }

    #[tokio::test]
    async fn a_nested_harness_never_matches_the_outer_session() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        // the outer session is known only through its MCP server (pid 700)
        let outer_mcp = Origin { session: None, pids: vec![700, 1], harness_pid: Some(700), mcp: true };
        d.handle(Request::Attach { origin: outer_mcp, harness: Harness::Claude, cwd: String::new(), mcp: true }).await;
        // a nested `claude -p` (pid 900) runs from the outer session's shell
        let inner = Origin { session: Some("inner".into()), pids: vec![901, 900, 702, 700, 1], harness_pid: Some(900), mcp: false };
        d.handle(Request::Attach { origin: inner, harness: Harness::Claude, cwd: String::new(), mcp: false }).await;
        let st = d.state.lock().await;
        assert_eq!(st.sessions.len(), 2, "the inner run gets its own session");
        assert!(st.sessions.iter().any(|s| s.session.is_none() && s.mcp_parent == Some(700)));
    }

    #[tokio::test]
    async fn clear_keeps_voice_focus() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        let old = talking_to(&d, "before-clear", 600).await;
        d.handle(Request::Detach { origin: old, rebind: true }).await;
        let new = Origin { session: Some("after-clear".into()), pids: vec![1, 600], harness_pid: Some(600), mcp: false };
        d.handle(Request::Attach { origin: new.clone(), harness: Harness::Claude, cwd: String::new(), mcp: false }).await;
        d.handle(Request::Hear { text: "hi".into(), heard: None }).await;
        match d.handle(Request::Claim { origin: new, at_stop: false }).await {
            Response::Utterances { items, .. } => assert_eq!(items.len(), 1),
            r => panic!("{r:?}"),
        }
    }

    #[tokio::test]
    async fn a_turn_holding_wait_refuses_when_not_talked_to() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        let o = talking_to(&d, "cx", 800).await;
        d.handle(set(Some(false), None)).await;
        let r = d.handle(Request::Wait { origin: o, timeout_ms: 60_000, holds_turn: true }).await;
        assert_eq!(r, Response::Utterances { items: vec![], superseded: true });
    }
}

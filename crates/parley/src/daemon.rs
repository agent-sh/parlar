//! parleyd: owns sessions, focus, the utterance queues and the speech queue.

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
    /// Ancestor pids reported by this session's hooks.
    hook_pids: Vec<u32>,
    pending: Vec<Utterance>,
    waiter: Option<Waiter>,
    in_turn: bool,
    /// Voice mode reminder not yet delivered.
    remind: bool,
}

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
    /// Words of the agent's line that the user talked over, for the next utterance.
    cut: Option<String>,
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
            active: true,
            mic_muted: false,
            voice_off: false,
            speaking: false,
            listening: false,
            barge: Arc::new(AtomicBool::new(false)),
            mic_gate: Arc::new(AtomicBool::new(true)),
            cut: None,
            ui,
        }
    }

    fn find(&self, o: &Origin) -> Option<usize> {
        if let Some(sid) = &o.session {
            if let Some(i) = self.sessions.iter().position(|s| s.session.as_deref() == Some(sid)) {
                return Some(i);
            }
            // a hook from a session that so far only has its MCP server attached
            return self.sessions.iter().position(|s| {
                s.session.is_none() && s.mcp_parent.is_some_and(|p| o.pids.contains(&p))
            });
        }
        // the MCP server: its first pid is its parent, the harness
        let parent = *o.pids.first()?;
        self.sessions
            .iter()
            .position(|s| s.mcp_parent == Some(parent) || s.hook_pids.contains(&parent))
    }

    /// Find the session, attaching it on the fly when a hook from a session that started before
    /// parleyd shows up. MCP-only origins are never created here.
    fn find_or_attach(&mut self, o: &Origin) -> Option<usize> {
        if let Some(i) = self.find(o) {
            return Some(i);
        }
        o.session.as_ref()?;
        let key = self.next_key;
        self.next_key += 1;
        self.sessions.push(Session {
            key,
            session: o.session.clone(),
            harness: Harness::Other,
            cwd: String::new(),
            mcp_parent: None,
            hook_pids: o.pids.clone(),
            pending: Vec::new(),
            waiter: None,
            in_turn: false,
            remind: true,
        });
        if self.focus.is_none() {
            self.focus = Some(key);
        }
        Some(self.sessions.len() - 1)
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

    fn take(&mut self, i: usize) -> Vec<Utterance> {
        std::mem::take(&mut self.sessions[i].pending)
    }

    fn supersede(&mut self, i: usize) {
        if let Some(w) = self.sessions[i].waiter.take() {
            let _ = w.tx.send(WaitResult::Superseded);
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
        let u = Utterance { id, text, heard, revises: None, interrupted_after };
        let Some(i) = self.focus.and_then(|k| self.sessions.iter().position(|s| s.key == k)) else {
            return (id, None);
        };
        let _ = self.ui.send(Ui::Caption { who: "user".into(), text: u.text.clone() });
        let s = &mut self.sessions[i];
        s.pending.push(u);
        if let Some(w) = s.waiter.take() {
            let items = std::mem::take(&mut s.pending);
            s.in_turn = true;
            let _ = w.tx.send(WaitResult::Items(items));
        }
        let to = s.session.clone().or_else(|| Some(format!("mcp:{}", s.mcp_parent.unwrap_or(0))));
        self.emit_phase();
        (id, to)
    }
}

pub type Shared = Arc<Mutex<State>>;

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
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if path.exists() {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                anyhow::bail!("parleyd is already running on {}", path.display());
            }
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
        eprintln!("parleyd listening on {}", path.display());
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
            let resp = self.handle(req).await;
            if let Some(o) = mcp_origin {
                let st = self.state.lock().await;
                *mcp_key = st.find(&o).map(|i| st.sessions[i].key);
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
                let i = match st.find(&origin) {
                    Some(i) => i,
                    None => {
                        let key = st.next_key;
                        st.next_key += 1;
                        st.sessions.push(Session {
                            key,
                            session: None,
                            harness,
                            cwd: cwd.clone(),
                            mcp_parent: None,
                            hook_pids: Vec::new(),
                            pending: Vec::new(),
                            waiter: None,
                            in_turn: false,
                            remind: false,
                        });
                        st.sessions.len() - 1
                    }
                };
                let s = &mut st.sessions[i];
                if mcp {
                    s.mcp_parent = origin.pids.first().copied();
                } else {
                    s.hook_pids = origin.pids.clone();
                    if origin.session.is_some() {
                        s.session = origin.session.clone();
                    }
                }
                if !cwd.is_empty() {
                    s.cwd = cwd;
                }
                s.harness = harness;
                // a session started by a person takes focus; an MCP reconnect does not steal it
                if !mcp || st.focus.is_none() {
                    st.focus = Some(st.sessions[i].key);
                }
                st.emit_phase();
                Response::Attached { focused: st.focused(i), active: st.active }
            }
            Request::Detach { origin } => {
                let mut st = self.state.lock().await;
                if let Some(i) = st.find(&origin) {
                    st.supersede(i);
                    let key = st.sessions.remove(i).key;
                    if st.focus == Some(key) {
                        st.focus = st.sessions.last().map(|s| s.key);
                    }
                    st.emit_phase();
                }
                Response::Ok
            }
            Request::Claim { origin } => {
                let mut st = self.state.lock().await;
                let Some(i) = st.find_or_attach(&origin) else { return empty() };
                let mut items = st.take(i);
                if st.sessions[i].remind && !items.is_empty() {
                    st.sessions[i].remind = false;
                    items[0].text = format!("{}\n{}", format::ACTIVATED, items[0].text);
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
                Response::Utterances { items: std::mem::take(&mut s.pending), superseded: false }
            }
            Request::Wait { origin, timeout_ms } => self.wait(origin, timeout_ms).await,
            Request::Say { origin, text, kind } => self.say(origin, text, kind).await,
            Request::Event { origin, event, tool } => {
                let mut st = self.state.lock().await;
                let Some(i) = st.find_or_attach(&origin) else { return Response::Ok };
                match event {
                    TurnEvent::TurnStart => {
                        // a typed prompt: the idle waiter must not wake a busy session
                        st.supersede(i);
                        st.sessions[i].in_turn = true;
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
                        return Response::Error { message: "this parleyd has no audio devices".into() };
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
                    if a && !st.active {
                        for s in &mut st.sessions {
                            s.remind = true;
                        }
                    }
                    if !a {
                        for i in 0..st.sessions.len() {
                            st.supersede(i);
                        }
                    }
                    st.active = a;
                }
                if let Some(m) = mic_muted {
                    st.mic_muted = m;
                }
                st.sync_gate();
                if let Some(v) = voice_off {
                    st.voice_off = v;
                }
                if let Some(f) = focus {
                    match st.sessions.iter().find(|s| s.session.as_deref() == Some(f.as_str())) {
                        Some(s) => st.focus = Some(s.key),
                        None => return Response::Error { message: format!("no session {f}") },
                    }
                }
                st.emit_phase();
                Response::Ok
            }
            Request::State => Response::State(self.state.lock().await.report()),
            Request::Subscribe => Response::Error { message: "subscribe must be the first request".into() },
        }
    }

    async fn wait(&self, origin: Origin, timeout_ms: u64) -> Response {
        let (id, rx) = {
            let mut st = self.state.lock().await;
            if !st.active {
                return empty();
            }
            let Some(i) = st.find_or_attach(&origin) else { return empty() };
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
            st.sessions[i].waiter = Some(Waiter { id, tx });
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
                return Response::Error { message: "session is not attached to parleyd".into() };
            };
            let focused = st.focused(i) && st.active;
            let items = if focused { st.take(i) } else { vec![] };
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
    /// The MCP server of a session went away. A session known only through its MCP server is
    /// dropped; one with hooks keeps living without the MCP link.
    fn mcp_gone(&mut self, key: u64) {
        let Some(i) = self.sessions.iter().position(|s| s.key == key) else { return };
        self.sessions[i].mcp_parent = None;
        if self.sessions[i].session.is_none() {
            self.supersede(i);
            self.sessions.remove(i);
            if self.focus == Some(key) {
                self.focus = self.sessions.last().map(|s| s.key);
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
    fn sync_gate(&self) {
        self.mic_gate.store(self.active && !self.mic_muted, Ordering::SeqCst);
    }
    pub fn set_cut(&mut self, cut: String) {
        self.cut = Some(cut);
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
        let o = Origin { session: None, pids: vec![4242] };
        let r = send(&mut wr, &mut lines, &Request::Attach { origin: o, harness: Harness::Claude, cwd: String::new(), mcp: true }).await;
        assert_eq!(r, Response::Attached { focused: true, active: true });
        assert_eq!(d.state.lock().await.sessions.len(), 1);
        drop(wr);
        drop(lines);
        served.await.unwrap().unwrap();
        let st = d.state.lock().await;
        assert!(st.sessions.is_empty());
        assert_eq!(st.focus, None);
    }

    #[tokio::test]
    async fn hook_session_survives_its_mcp_and_gets_utterances() {
        let d = Arc::new(Daemon::new(Arc::new(Queue::new(Engine::Silent))));
        let hook = Origin { session: Some("s1".into()), pids: vec![10, 4242, 7] };
        d.handle(Request::Attach { origin: hook.clone(), harness: Harness::Claude, cwd: "/w".into(), mcp: false }).await;
        let (a, b) = UnixStream::pair().unwrap();
        let served = tokio::spawn(d.clone().conn(b));
        let (rd, mut wr) = a.into_split();
        let mut lines = BufReader::new(rd).lines();
        let mcp = Origin { session: None, pids: vec![4242] };
        send(&mut wr, &mut lines, &Request::Attach { origin: mcp, harness: Harness::Claude, cwd: String::new(), mcp: true }).await;
        assert_eq!(d.state.lock().await.sessions.len(), 1, "MCP merged into the hook session");
        drop(wr);
        drop(lines);
        served.await.unwrap().unwrap();
        assert_eq!(d.state.lock().await.sessions.len(), 1);
        d.handle(Request::Hear { text: "hello".into(), heard: None }).await;
        match d.handle(Request::Claim { origin: hook }).await {
            Response::Utterances { items, .. } => assert_eq!(items[0].text, "hello"),
            r => panic!("{r:?}"),
        }
    }
}

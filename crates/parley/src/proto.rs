//! Wire protocol between parleyd and its clients: JSON lines over a unix socket.

use serde::{Deserialize, Serialize};

pub type UtteranceId = u64;

/// One finished user turn, as delivered to a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Utterance {
    pub id: UtteranceId,
    /// Cleaned text: fillers removed, self-repairs resolved where the rules are sure.
    pub text: String,
    /// Raw recognizer output, present only when cleanup changed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heard: Option<String>,
    /// Set when this utterance continues or corrects one that was already delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revises: Option<UtteranceId>,
    /// Words of the agent's speech that were cut off by this utterance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_after: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Claude,
    Codex,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SayKind {
    Answer,
    #[default]
    Status,
    Next,
    Question,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnEvent {
    /// The user submitted a typed prompt, or the agent otherwise started a turn.
    TurnStart,
    ToolStart,
    ToolEnd,
    ToolError,
}

/// Where a request comes from. The harness session id is known to hooks, the harness process
/// id is known to both hooks (as an ancestor) and the MCP server (as its parent).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Origin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Ancestor process ids, nearest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pids: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// A harness session started (SessionStart hook) or an MCP server connected.
    Attach {
        origin: Origin,
        harness: Harness,
        #[serde(default)]
        cwd: String,
        /// True when the caller is the MCP server; its parent pid is the harness.
        #[serde(default)]
        mcp: bool,
    },
    Detach { origin: Origin },
    /// Take every pending utterance for this session.
    Claim { origin: Origin },
    /// Take pending utterances whose text reads as a stop request, leave the rest.
    ClaimStop { origin: Origin },
    /// Block until an utterance for this session is ready, or until the timeout.
    Wait { origin: Origin, timeout_ms: u64 },
    Say {
        origin: Origin,
        text: String,
        #[serde(default)]
        kind: SayKind,
    },
    Event {
        origin: Origin,
        event: TurnEvent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
    },
    /// The agent's turn ended. When it said nothing through `say` during the turn, the opening
    /// of its final message is spoken instead, so weaker models still answer out loud.
    TurnEnd {
        origin: Origin,
        #[serde(default)]
        last_message: Option<String>,
    },
    /// Inject an utterance as if it had been spoken. Stand-in for the mic during development.
    Hear {
        text: String,
        #[serde(default)]
        heard: Option<String>,
    },
    Set {
        #[serde(default)]
        active: Option<bool>,
        #[serde(default)]
        mic_muted: Option<bool>,
        #[serde(default)]
        voice_off: Option<bool>,
        #[serde(default)]
        focus: Option<String>,
        /// Audio device id from `Devices`.
        #[serde(default)]
        input: Option<String>,
        #[serde(default)]
        output: Option<String>,
    },
    State,
    Devices,
    /// Stream `Ui` events until the connection closes.
    Subscribe,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Stopped,
    Connecting,
    Ready,
    Listening,
    Working,
    Speaking,
    Interrupting,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionInfo {
    pub session: Option<String>,
    pub harness: Harness,
    pub cwd: String,
    pub focused: bool,
    pub pending: usize,
    pub waiting: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateReport {
    pub phase: Phase,
    pub active: bool,
    pub mic_muted: bool,
    pub voice_off: bool,
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Utterances {
        items: Vec<Utterance>,
        /// True when a newer waiter for the same session replaced this one.
        #[serde(default)]
        superseded: bool,
    },
    Attached {
        focused: bool,
        active: bool,
    },
    Said {
        spoken: bool,
        /// Utterances that were pending, delivered with the say result.
        items: Vec<Utterance>,
    },
    Heard {
        id: UtteranceId,
        delivered_to: Option<String>,
    },
    State(StateReport),
    Devices {
        inputs: Vec<Device>,
        outputs: Vec<Device>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub current: bool,
}

/// Events for indicators. Levels are 0..1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ui", rename_all = "snake_case")]
pub enum Ui {
    Phase {
        phase: Phase,
        mic_muted: bool,
        voice_off: bool,
    },
    Levels {
        user: f32,
        agent: f32,
    },
    Tool {
        ok: bool,
        name: Option<String>,
    },
    Caption {
        who: String,
        text: String,
    },
}

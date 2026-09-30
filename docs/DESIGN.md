# parley design

Status: draft, 2026-10-01. Working name: parley.

Voice conversation mode for coding-agent harnesses (Claude Code, Codex, then Gemini CLI and
opencode). You talk to a running session and it talks back, while the session stays fully usable
and viewable in its own terminal. All message passing goes through the harness plugin (MCP server
plus hooks). Nothing is typed into the terminal and nothing depends on tmux or a specific shell.

## 1. Purpose and non-goals

Purpose:
- Speak to a live agent session: new requests when it is idle, steering while it works.
- Hear back only the conversational layer: answers, status, the next step it is about to take,
  questions. Tool calls, results, diffs and logs stay in the session.
- Feel like a conversation: it tolerates "hmm", thinking pauses and self-corrections, and you can
  talk over it.
- Run on a CPU-only laptop with natural-sounding output. A GPU is optional.

Non-goals:
- Not a dictation tool and not a general voice assistant. The brain is always the harness agent.
- No speech-to-speech model (Moshi and similar bring their own LLM).
- No terminal injection (PTY, tmux send-keys, xdotool).
- No cloud speech services by default.

## 2. Components

```
 harness session (any terminal)            parleyd (one per user)                indicator
 +----------------------------+     unix   +-----------------------------+  unix  +-----------+
 | plugin: parley mcp (stdio) |<---------->| audio in: mic -> AEC -> VAD |<------>| GNOME ext |
 |         parley hook <evt>  |   socket   |   -> STT -> endpointing     | socket | (swarm)   |
 +----------------------------+            | utterance queue per session |        +-----------+
                                           | say queue -> TTS -> speaker |        | statusline|
                                           +-----------------------------+        +-----------+
```

- `parleyd`: Rust daemon. Owns audio devices, models, the conversation state machine, the utterance
  queue and the speech queue. One per user, shared by every attached session. Exactly one session
  has voice focus at a time.
- `parley`: one Rust binary with subcommands.
  - `parley mcp`: stdio MCP server the harness launches. Exposes `say` and the conversation-mode
    instructions.
  - `parley hook <event>`: hook handler. Reads the hook JSON on stdin, talks to the daemon, prints
    the hook JSON reply. Must return in under 20 ms when there is nothing to deliver, and exit 0
    instantly when the daemon is not running.
  - `parley status`: one-line state for a harness status line.
  - `parley ctl`: start, stop, mute, focus, devices.
- Indicator: GNOME Shell extension (the only way to float above everything on GNOME Wayland).
  Tauri orb for macOS, Windows, KDE and wlroots later. Visual: the swarm (section 8).
- Socket: `$XDG_RUNTIME_DIR/parley/parley.sock`, mode 0600, JSON lines.

## 3. Transport: plugin = MCP server + hooks

Verified on this box against Claude Code 2.1.286 (Bedrock) and the openai/codex source (hooks and
plugins are stable in 0.159).

### 3.1 Input: user utterance to the agent

| Agent state | Mechanism | Latency |
|---|---|---|
| Idle at the prompt | `Stop` hook with `asyncRewake: true`: a background `parley hook wait` blocks on the daemon and exits 2 with the utterance on stderr, which wakes the model | as soon as the endpoint fires |
| Working (tool calls) | `PostToolUse` hook on `*` returns `hookSpecificOutput.additionalContext` with pending utterances | next tool boundary |
| Working, user says stop | `PreToolUse` hook on `*` returns `permissionDecision: deny` plus the utterance as the reason | next tool call is refused |
| Turn about to end | synchronous `Stop` hook claims anything pending and returns `decision: block` with it | instant |
| Mid-generation, no tool calls | nothing reaches the model until it stops | end of that reply |

asyncRewake was verified live on 2026-10-01: an idle Bedrock session woke 15 s after the turn
ended, answered the injected utterance, and the hook re-armed on the next Stop. The prompt stayed
usable while the waiter was pending.

Codex: the same hook events exist (`PostToolUse` additionalContext is recorded mid-turn,
`PreToolUse` deny, `Stop` block with reason, no timeout clamp on Stop). Codex has no asyncRewake,
so idle wake there uses either a blocking synchronous Stop hook (turn never ends while conversation
mode is on) or the app-server `thread/queue/add` when the TUI is attached to the shared daemon.
Open item O1.

Claude Code channels (`notifications/claude/channel`) are refused on Bedrock, Vertex and Foundry
in 2.1.286. They are an optional extra for first-party logins only.

### 3.2 Output: the agent to the user

- MCP tool `say(text, kind)`, `kind` one of `answer`, `status`, `next`, `question`. Returns at once.
  Its result also carries any pending utterances, so a `say` doubles as a check-in.
- The server `instructions` carry the conversation rules. Codex prioritizes the first 512
  characters and Claude Code truncates at 2048, so the core rule comes first:
  "Voice mode is on. Talk to the user only through say: short spoken sentences, no code, paths,
  or markdown. Say what you are about to do before long actions, and answer questions directly."
- A text filter runs before TTS as a safety net: strips markdown, code spans, URLs, long paths
  (a path becomes its last component), and collapses lists into sentences.
- Optional fallback (off by default): when a turn ends without any `say`, speak the first sentence
  of the final assistant message from the Stop hook input.
- Voice off mode: `say` text goes to the indicator captions only.

### 3.3 What the model sees

Delivered utterances use one plain format, the same for every path:

```
[voice u17] open the router file
(heard: "open the router config, no wait, the router file". Speech recognition, may contain
errors. When the user corrects themselves, the last version wins.)
```

The `heard` line is included only when cleanup changed the text. A follow-up that revises an
already delivered utterance is tagged `[voice u18 revises u17]`.

## 4. Speech pipeline (streaming)

Every stage runs concurrently and passes frames on as it produces them.

```
mic 48k -> resample 16k -> AEC (far end = TTS output) -> VAD (32 ms frames)
   -> STT streaming (partials) -> endpointing -> utterance queue
say -> text filter -> sentence split -> TTS per sentence -> playback (and AEC far end)
```

| Stage | Choice | Notes |
|---|---|---|
| AEC | sonora (pure Rust AEC3) | lets you talk over the agent. Half-duplex fallback when AEC is off |
| VAD | Silero VAD v6 | under 1 ms per frame on CPU |
| STT | Moonshine v2 Small (123M, MIT) | owner choice. streaming, English. Swappable engine trait |
| End of turn | Smart Turn v3.2 (8 MB, BSD-2) | audio based, trained on fillers |
| TTS | Kokoro-82M (Apache-2.0) | owner choice. sentence streaming |

Runtime: ONNX Runtime through the `ort` crate for every model. Per-model integration details are
pending the runtime survey (open item O2).

## 5. Endpointing: thinking pause vs. done

The utterance stays mutable until a hook claims it. Hooks claim late (next tool boundary), so a
wrong "done" call mid-work costs nothing: a continuation simply merges. Endpointing precision only
matters when the agent is idle and waiting.

Signals, fused into one hold/release decision each frame:
1. Silence length from VAD.
2. Smart Turn probability on the last up to 8 s of audio, run when silence passes 200 ms.
3. Lexical tail of the partial transcript: ends on "and", "so", "but", "um", "uh", "like",
   "let me think", "wait" means hold. A complete clause or a question releases sooner.
4. Dialogue context: if the agent's last `say` was a `question`, short answers release fast.
5. Per-user pause profile: learned distribution of within-turn pauses.

Policy (initial numbers, to be tuned):
- Release when Smart Turn says complete and silence is at least 300 ms, and the lexical tail does
  not say hold.
- Hold up to 2.5 s when the tail says hold or Smart Turn says incomplete.
- Hard release at 3 s of silence.
- Grace window: if speech resumes within 1.5 s of a release and the utterance was not claimed yet,
  merge. If it was claimed, the new utterance is tagged `revises`.

Tuning: record about 20 minutes of the owner's own speech with labels, then sweep thresholds and
report false cutoff rate vs. added latency.

## 6. Corrections

- Self-repair inside an utterance: a light rule pass removes fillers and marks repairs
  ("no wait", "I mean", "actually", "scratch that"). The model gets both the cleaned and heard
  text and resolves the repair itself.
- Recognition errors: a per-session vocabulary built from the repo (directory names, file stems,
  identifiers from the session) drives a post-STT correction pass. Explicit corrections by voice
  ("I said valkey, not wally") are learned into a per-user heard-to-meant map.
- The agent fixes the rest from context.

## 7. Barge-in and speech queue

- `say` lines queue. A `status` line older than a newer `status` is dropped unspoken.
- While TTS plays, VAD speech of 250 ms or more after AEC stops playback. The unspoken remainder
  and the interruption are reported with the next delivered utterance
  ("[voice u19, interrupted you after: I'll remove the copy]").
- Mute mic: capture stops, the indicator shows muted, speech out continues.
- Voice off: TTS stops, captions only.

## 8. Indicator: the swarm

Owner choice from `design/indicator-lab.html`. Forty fireflies. Blue is the user, amber is the
agent, red is a failed tool call.

| State | Motion |
|---|---|
| Stopped | settled in a line at the bottom, nearly invisible |
| Connecting | fireflies arrive one by one |
| Ready | loose slow drift |
| Listening | crowd and dance to your voice level (blue) |
| Working | tight fast orbit, one flare per tool call, red flare on error |
| Speaking | line up into the agent's voice envelope (amber) |
| You interrupt | half cluster (blue), half wave (amber) |
| Muted | settled line, dim, red mark |

Interaction: click stops or starts the conversation, hover shows a mute button, right-click opens
input device, output device, mute, voice off, stop. Keyboard: Enter, M, Shift+F10.

GNOME: extension draws with `St.DrawingArea` (Cairo) in `Main.layoutManager.addTopChrome`,
subscribes to the daemon socket for state plus levels at 30 Hz. Terminal: `parley status` for the
Claude Code statusLine (`refreshInterval: 1`).

## 9. Session focus

- Every attached session registers with its harness, cwd and session id (SessionStart hook).
- Focus goes to the most recently started session, changeable from the indicator menu or
  `parley ctl focus`. Only the focused session gets utterances and may speak. Other sessions'
  `say` calls queue as text in the indicator.

## 10. Open items

- O1: Codex idle wake. Blocking Stop hook vs. app-server `thread/queue/add`. Verify whether the
  TUI launched through the `codex` wrapper (bedrock profile) attaches to the shared daemon.
- O2: model runtime contracts for Moonshine v2 streaming, Kokoro G2P (espeak-ng is GPL, may need a
  process boundary), Smart Turn features, Silero state.
- O3: asyncRewake timeout ceiling. The waiter must outlive long idle periods, or re-arm.
- O4: Esc while a synchronous Stop hook blocks (Codex path).
- O5: Hebrew. Moonshine v2 has no Hebrew; a second STT engine would be needed.

## 11. Decision log

- D1 (2026-10-01): all message passing through the harness plugin, no terminal injection. Owner.
- D2: cascaded pipeline, no speech-to-speech model. The coding agent must be the brain.
- D3: STT Moonshine v2 Small, TTS Kokoro-82M. Owner.
- D4: indicator visual is the swarm. Owner.
- D5: idle push on Claude Code through a Stop hook with asyncRewake. Verified live on Bedrock.
- D6: GNOME Shell extension for the GNOME indicator, because Mutter does not honor keep-above for
  Wayland clients and has no layer-shell.
- D7: Rust for the daemon and the hook binary. Hooks run on every tool call, so startup time
  matters.

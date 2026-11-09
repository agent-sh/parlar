# parlar design

Status: working end to end on Claude Code (Bedrock), 2026-10-01. Codex plugin written, not yet
run live. Working name: parlar.

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
 harness session (any terminal)            parlard (one per user)                indicator
 +----------------------------+     unix   +-----------------------------+  unix  +-----------+
 | plugin: parlar mcp (stdio) |<---------->| audio in: mic -> AEC -> VAD |<------>| GNOME ext |
 |         parlar hook <evt>  |   socket   |   -> STT -> endpointing     | socket | (swarm)   |
 +----------------------------+            | utterance queue per session |        +-----------+
                                           | say queue -> TTS -> speaker |        | statusline|
                                           +-----------------------------+        +-----------+
```

- `parlard`: Rust daemon. Owns audio devices, models, the conversation state machine, the utterance
  queue and the speech queue. One per user, shared by every attached session. Exactly one session
  has voice focus at a time.
- `parlar`: one Rust binary with subcommands.
  - `parlar mcp`: stdio MCP server the harness launches. Exposes `say` and the conversation-mode
    instructions.
  - `parlar hook <event>`: hook handler. Reads the hook JSON on stdin, talks to the daemon, prints
    the hook JSON reply. Must return in under 20 ms when there is nothing to deliver, and exit 0
    instantly when the daemon is not running.
  - `parlar status`: one-line state for a harness status line.
  - `parlar ctl`: start, stop, mute, focus, devices.
- Indicator: GNOME Shell extension (the only way to float above everything on GNOME Wayland).
  Tauri orb for macOS, Windows, KDE and wlroots later. Visual: the swarm (section 8).
- Socket: `$XDG_RUNTIME_DIR/parlar/parlar.sock`, mode 0600, JSON lines.

## 3. Transport: plugin = MCP server + hooks

Verified on this box against Claude Code 2.1.286 (Bedrock) and the openai/codex source (hooks and
plugins are stable in 0.159).

### 3.1 Input: user utterance to the agent

| Agent state | Mechanism | Latency |
|---|---|---|
| Idle at the prompt | `Stop` hook with `asyncRewake: true`: a background `parlar hook wait` blocks on the daemon and exits 2 with the utterance on stderr, which wakes the model | as soon as the endpoint fires |
| Working (tool calls) | `PostToolUse` hook on `*` returns `hookSpecificOutput.additionalContext` with pending utterances | next tool boundary |
| Working, user says stop | `PreToolUse` hook on `*` returns `permissionDecision: deny` plus the utterance as the reason | next tool call is refused |
| Turn about to end | synchronous `Stop` hook claims anything pending and returns `decision: block` with it | instant |
| Mid-generation, no tool calls | nothing reaches the model until it stops | end of that reply |

asyncRewake was verified live on 2026-10-01: an idle Bedrock session woke 15 s after the turn
ended, answered the injected utterance, and the hook re-armed on the next Stop. The prompt stayed
usable while the waiter was pending. The same waiter also runs on SessionStart, so a fresh
session wakes on its first utterance without any typing (verified with recorded speech).

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

Runtime: libmoonshine (MIT) runs both the recognizer and Kokoro, with its own MIT G2P, so there
is no espeak-ng and no GPL in the process. Smart Turn runs through `ort` 2.0.0-rc.10 loaded
dynamically against the ONNX Runtime 1.23 that libmoonshine ships, so one runtime is loaded.
VAD is Moonshine's own line segmentation for now; Silero was not needed.

Measured on this rig (Core Ultra 9 275HX), CPU only:
- Kokoro: first audio 330 to 470 ms, synthesis at 0.36 to 0.53 of real time on 4 cores.
- Moonshine Small streaming: 0.6 of real time with partials (2 or 4 cores alike), 0.31 without;
  final text 300 ms after the end with partials, 0.7 to 1.25 s without. Partials are the default.
- Smart Turn: 37 to 50 ms per decision on 1 thread; features match Hugging Face to 2e-5.
- parlard while the user is silent: 0.7% of one core; about 400 MB resident with all models.

## 5. Endpointing: thinking pause vs. done

The utterance stays mutable until a hook claims it. Hooks claim late (next tool boundary), so a
wrong "done" call mid-work costs nothing: a continuation simply merges. Endpointing precision only
matters when the agent is idle and waiting.

Implemented: 1, 2 and 3 below (word rules win over the audio model, because speech can trail off
on a falling pitch; Kokoro's "and" scored 0.96 complete). 4 and 5 are not built yet.

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
subscribes to the daemon socket for state plus levels at 30 Hz. Terminal: `parlar status` for the
Claude Code statusLine (`refreshInterval: 1`).

Claude Code strip: `plugin/claude/hooks/strip.tsx`, a function-hooks module (early access API) that
draws the band above the prompt. It follows `parlar ctl watch`, so it gets the same events as the
GNOME indicator, and redraws at 8 Hz at most. Focus changes send no event, so it polls `parlar ctl
state` every 2 s while a conversation is on. It draws nothing while the conversation is off or
parlard is not running, and reconnects within 5 s when parlard comes back. The command hooks still
carry the conversation; the strip only shows it and runs `parlar ctl` for its buttons.

Transcript rows (`plugin/claude/hooks/rows.tsx`): a say call draws as `parlar ▸ <text>` on its tool
row, an utterance as `you ▸ <text>` on the row that delivered it: the voice wake row (its uuid,
noted at `session.append`), a say result ("the user said meanwhile"), or the tool call whose
PostToolUse context carried it (the last tool result appended before that context row). The
module sets `PARLAR_ROWS=1` for the hooks it starts; `parlar hook` then asks the daemon for the
transcript with `rows: true`, which drops the heard and spoken lines and returns the rest. ctrl+o
shows the engine's own rows. After a resume, say lines still draw from their rows' own output;
wake rows draw as the engine draws them and PostToolUse-delivered utterances are not shown.

Voice pane (`/parlar`, or `p` on the strip): the strip's captions while this session has focus
(200 lines), the sessions from the focus poll with "talk there" (`parlar ctl focus`), and the
devices from `parlar ctl devices`, one button each, deduplicated by name and capped at 6 per kind
(plain ALSA lists one card many times). Buttons rather than Select, which not every surface has.

Toasts: "parlard stopped" when the watch ends during a conversation and `parlar ctl state` no
longer answers (a module reload also ends the watch, and must stay quiet), "parlard is back" when
the reconnect finds it with the conversation on, and "Voice moved to <folder>" when the focus poll
sees this session lose focus.

## 9. Session focus

- Every attached session registers with its harness, cwd and session id (SessionStart hook, and
  again on each typed prompt for sessions that started before parlard).
- Focus follows typing: the session the user last typed into has voice focus. A new session takes
  focus only when nobody has it. The indicator menu lists sessions under "Talk to", and
  `parlar ctl focus` sets it directly. Only the focused session gets utterances and is spoken.
- Sessions whose harness process is gone are pruned every 30 s.
- If a turn that started from speech ends without any `say`, the opening of the final reply is
  spoken instead (Haiku 4.5 often skips `say`; Sonnet 5.5 used it in every test).

## 10. Open items

- O1: Codex idle wake. Blocking Stop hook vs. app-server `thread/queue/add`. Verify whether the
  TUI launched through the `codex` wrapper (bedrock profile) attaches to the shared daemon.
- O2: resolved, see section 4.
- O3: asyncRewake timeout ceiling. The waiter must outlive long idle periods, or re-arm.
- O4: Esc while a synchronous Stop hook blocks (Codex path).
- O5: Hebrew. Moonshine v2 has no Hebrew; a second STT engine would be needed.
- O6: echo cancellation, measured on a laptop (built-in speakers and mic, 2026-10-02). At the
  mic's +30 dB hardware gain the agent's voice drives the converter past full scale in about half
  the 100 ms frames, and a clipped echo is beyond AEC3: its leftover bursts to the level of real
  speech and the recognizer turns it into words. Two fixes. The far end now waits for the speaker
  latency the stream reports (42 ms there): fed no earlier than its echo, AEC3 cancels nothing.
  And while the mic clips on the agent's voice, the rest of that line and its 1.5 s tail are not
  heard (a turn that began over the echo is dropped), with one notice that talking over it needs
  a lower speaker volume or mic gain. The user's own speech clips there too, so nothing better is
  possible at that gain. Without clipping (mic at +12 dB), AEC3 left 0.003 rms and heard nothing
  false. `parlard echo <far> <mic> <out>` replays a recording through the canceller for tuning.
- O7: Codex fresh-session wake: Codex has no asyncRewake, so a new Codex session hears voice only
  after its first turn. Its blocking Stop hook waits only while the conversation is on and that
  session has focus, and is released the moment either changes (verified live on 0.159). While
  it waits, typed input queues as a steer; Esc ends the wait.
- O8: recognition of fillers and repairs depends on the recognizer: "no wait, list" came out as
  "No waitlist" in one test, so the repair cue was lost before cleanup.

## 11. Decision log

- D1 (2026-10-01): all message passing through the harness plugin, no terminal injection. Owner.
- D2: cascaded pipeline, no speech-to-speech model. The coding agent must be the brain.
- D3: STT Moonshine v2 Small, TTS Kokoro-82M. Owner.
- D4: indicator visual is the swarm. Owner.
- D5: idle push on Claude Code through a Stop hook with asyncRewake. Verified live on Bedrock.
- D6: GNOME Shell extension for the GNOME indicator, because Mutter does not honor keep-above for
  Wayland clients and has no layer-shell.
- D7: Rust for the daemon and the hook binary. Hooks run on every tool call, so startup time
  matters. Hook latency measured under 10 ms with and without a daemon.
- D8: libmoonshine for both recognition and voice (one dependency, MIT G2P, keyterm biasing).
- D9: portable build: pinned libmoonshine per target, loaded at run time from the XDG data dir
  (so `cargo install` works), a per-user install script. Owner: "it needs to work for Linux users, not solely on this machine".
- D10: an MCP-only session is released when its connection closes, so a dead MCP server never
  keeps voice focus.
- D11 (2026-10-01): Moonshine v2 Small failed on the owner's real speech. Recognition is now
  Phonon-2 (Fermion Research, a 2-bit derivative of Parakeet TDT 0.6B v3), exported to ONNX by us
  and published as tiyuvta/Phonon-2-ONNX; the int8 encoder is the default (closest on the owner's
  product names, 0.9 to 1.1 GB peak in our runtime). Owner.
- D12: no streaming recognizer. Silero VAD marks pauses and the recognizer runs on the turn so far
  at each pause, so nothing transcribes while nobody talks. The streaming one burned about five
  cores on room noise.
- D13: models and the voice load only during a conversation and unload two minutes after it
  stops (idle parlard is about 20 MB); the owner's
  bar is that a normal machine must not feel parlar.
- D14: one synthesis call per sentence with pauses capped at 250 ms; streamed chunks left holes of
  up to 0.7 s that the owner heard as the voice breaking up.
- D15: voice focus moves only by choice (/parlar:talk, the indicator menu); following prompts sent
  speech to busy agent sessions.
- D16: the plugin prints the spoken exchange as a hook systemMessage, at no model token cost.
- D17 (2026-10-01): no harness lets a hook into a running tool call, so speech during a long call
  waits for it to end. parlard says so itself, once per call, instead of forcing tools into the
  background: the owner ruled out changing how the model runs ("wait on it is cheaper than
  fetching repeatedly").
- D18 (2026-10-01): Claude Code fires no hook on Escape (verified: no PostToolUse, no Stop), so an
  interrupted session had no waiter and could not be woken by voice. A typed prompt now parks the
  background waiter instead of dropping it, and parlard watches the focused session's transcript
  for "[Request interrupted by user" to end the turn and let the parked waiter wake it. Codex has
  an Interrupt hook for the same.


# parlar

Talk to a running coding agent and hear it talk back, while the session keeps running in its own
terminal. parlar is a plugin for Claude Code (and Codex): you speak, the agent hears you whether it
is idle or in the middle of work, and it answers out loud in short spoken sentences while tool
calls, diffs and logs stay on screen. The conversation is also printed in the session, so you can
read back what was said.

Everything runs locally on the CPU. No speech leaves your machine.

- Listening: a voice detector (Silero VAD) marks speech and pauses; noise suppression and automatic
  gain make a laptop mic usable. Nothing transcribes while nobody talks.
- Speech recognition: [Phonon-2](https://huggingface.co/tiyuvta/Phonon-2-ONNX) in ONNX, run at each
  pause on the turn so far.
- End of turn: waits through thinking pauses ("so...", "and...") using word rules plus the Smart
  Turn audio model.
- Voice: Kokoro-82M, streamed by sentence. Talk over it and it stops.
- Indicator (GNOME): a small swarm of fireflies floating above your windows. Blue is you, amber is
  the agent, a red spark is a failed tool call.

Linux only for now (x86_64 and aarch64). The floating indicator needs GNOME; everything else works
on any desktop.

## Install

### From Claude Code

```
/plugin marketplace add avifenesh/parlar
/plugin install parlar@parlar
/parlar:setup
```

`/parlar:setup` checks your machine and installs what is missing, asking before each step that
changes your system.

### From source

Needs cargo ([rustup.rs](https://rustup.rs)), git, curl and the PipeWire headers
(Debian/Ubuntu `libpipewire-0.3-dev`, Fedora `pipewire-devel`, Arch `pipewire`).

```
git clone https://github.com/avifenesh/parlar && cd parlar
scripts/install.sh
claude plugin marketplace add ~/.local/share/parlar/marketplace/claude
claude plugin install parlar@parlar
parlar setup claude
```

`scripts/install.sh` builds, installs into `~/.local` (set `PREFIX` to change it), downloads the
speech models (about 800 MB, into `$XDG_DATA_HOME/parlar/models`), installs the GNOME indicator and
starts the `parlard` user service. `--no-service` skips the service. `parlar setup claude` lets the
agent speak without a permission prompt on every line.

On GNOME the indicator appears after your next login (GNOME on Wayland loads new extensions at
login).

## Use

| | |
|---|---|
| `/parlar:talk` | start talking to this session (it gets voice focus) |
| `/parlar:stop` | stop; the mic closes |
| `/parlar:mute`, `/parlar:mute off` | mute or unmute the mic, the conversation stays on |
| click the swarm | stop or start |
| right-click the swarm | pick the session to talk to, input and output devices, mute, voice off |

Only one session hears you at a time: the one you started with `/parlar:talk` or picked under
"Talk to". Other sessions, including busy agents, never get your speech.

While the agent works you can steer it ("also update the tests"), and "stop" or "wait" blocks its
next tool call. When it is idle, speaking wakes it.

From the shell:

```
parlar ctl state                  # what parlar is doing and which session has focus
parlar ctl devices                # inputs and outputs
parlar ctl input <id>             # switch the mic (remembered across restarts)
parlar ctl output <id>            # switch the speaker
parlar ctl voice-off              # captions only, no audio out
parlar status                     # one line, for a status line
```

For the Claude Code status line, set `statusLine.command` to `parlar status` with
`refreshInterval: 1`.

### Headphones

Bluetooth headsets have two modes. High quality playback (A2DP) turns the headset mic off, so
parlar then needs another mic, such as the laptop's. Hands-free mode has a working mic and lower
quality audio; parlar works in it both ways. `parlar ctl input pipewire:input_default` makes the
mic follow whatever the system default is.

## Codex

The Codex plugin gives the same push behavior through Codex hooks:

```
codex plugin marketplace add ~/.local/share/parlar/marketplace/codex
codex plugin add parlar@parlar
```

Then trust the parlar hooks once from Codex's hooks review prompt. Codex has no background wake,
so a brand new Codex session hears you after its first turn; after that, speaking continues the
conversation. Start and stop with `parlar ctl talk --session <id> --harness codex` or the
indicator menu.

## How it works

parlar never types into your terminal. The plugin is an MCP server plus hooks:

- the agent speaks through the MCP `say` tool;
- while it works, a hook after each tool call hands it what you said;
- a spoken "stop" denies its next tool call;
- when it is idle, a background Stop hook wakes it the moment you finish a sentence.

`parlard` is a user service that owns the mic, the speaker, the models and which session has
focus. The hooks and the MCP server talk to it over a unix socket in `$XDG_RUNTIME_DIR/parlar`.
While the conversation is stopped the mic is closed. Design notes and decisions are in
`docs/DESIGN.md`.

## Troubleshooting

- `journalctl --user -u parlard -f` shows what parlar hears (`hearing:`), where each utterance went
  (`heard u3 -> /path/to/session`) and what it says.
- Nothing is heard: check `parlar ctl state` (is it on, is a session focused), then the mic with
  `parlar ctl devices`. A Bluetooth headset in A2DP mode has no mic.
- Memory: while a conversation is on, parlard holds about 1.4 GB, mostly the recognizer; two minutes
  after you stop it unloads the models.
- Your words go to the wrong session: run `/parlar:talk` in the session you want.
- `parlard fetch` downloads missing models again; `parlard speak "hello"` writes a test line to
  `parlar-speak.wav`; `parlard final file.wav` runs a recording through the recognizer.

## Uninstall

```
systemctl --user disable --now parlard
claude plugin uninstall parlar@parlar
rm -rf ~/.local/bin/parlar ~/.local/bin/parlard ~/.local/lib/parlar ~/.local/share/parlar \
       ~/.config/parlar ~/.config/systemd/user/parlard.service \
       ~/.local/share/gnome-shell/extensions/parlar@avifenesh
```

## Develop

```
cargo build --release && cargo test --release
target/release/parlard --silent --input-wav clip.wav     # the pipeline on a recording
PARLAR_BIN=target/release/parlar claude --plugin-dir plugin/claude
```

Conventions are in `AGENTS.md`.

## Licenses

parlar is MIT or Apache-2.0, at your option. It downloads and uses: Phonon-2 ONNX (CC-BY-4.0;
Parakeet TDT 0.6B v3 by NVIDIA, Phonon-2 by Fermion Research, ONNX conversion by Tiyuvta), libmoonshine
(MIT) for Kokoro-82M (Apache-2.0), Smart Turn v3.2 (BSD-2-Clause) and Silero VAD (MIT), and links
sonora (BSD-3-Clause) and ONNX Runtime (MIT).

# Changelog

## [Unreleased]

### Added
- macOS (Apple silicon), first cut: builds with libmoonshine linked in and ONNX Runtime from
  Microsoft's release, process lookups through sysctl, a launchd user agent from
  `parlard service`, release builds and npm. Tested on CI only; the mic, the speaker and the
  plugin flow on a real Mac are not verified, and there is no floating indicator yet.

## [0.1.6] - 2026-10-01

### Added
- Windows floating indicator, `parlar-overlay`: the swarm above every window, with click, drag
  and a right-click menu for sessions, devices, mute and voice off. `parlard service` starts it at
  login, and the Windows release and npm package ship it.

### Fixed
- On Windows, a parlard started by `parlard service` from an SSH session or a terminal no longer
  dies with that session.

## [0.1.5] - 2026-10-01

### Added
- Windows x86_64: release builds and the npm package (with ONNX Runtime and the Visual C++
  runtime beside the binaries), a named pipe open only to its user, a login start through the
  per-user Run key (`parlard service`), and `.cmd` plugin launchers. Tested on Windows 11 with
  Claude Code: the say tool, hooks through Git Bash, voice and recognition. No floating
  indicator on Windows yet.

### Fixed
- The npm bins are Node launchers, so they work in any shell; the plugin launchers run the
  native binary inside the npm package, so hooks never start Node.
- The MCP server finds its harness past shells, so it shares one session with the hooks when the
  harness starts it through a shell.

## [0.1.4] - 2026-10-01

### Fixed
- `claude -p` and Agent SDK runs no longer hang while parlard is running. They wait for their
  background hooks before exiting, and parlar's voice waiters waited for speech for up to 23
  hours. The waiters now exit at once in headless runs (`CLAUDE_CODE_ENTRYPOINT=sdk-*`).

## [0.1.3] - 2026-10-01

### Added
- `config.toml` picks the language, the recognizer (Phonon-2, Parakeet TDT 0.6B v3 for 25
  European languages, or any onnx-asr TDT model directory), the encoder and the voice (any
  libmoonshine voice, or an external command). `parlard fetch` downloads what the settings need;
  `parlard config` prints them. Tested round trips in Spanish, French and German.
- Codex skills `$parlar:talk`, `$parlar:stop` and `$parlar:mute`. Codex plugins cannot ship slash
  commands, and the talk skill focuses its own session through `$CODEX_THREAD_ID`.
- The busy answer in Codex names short plain commands ("running cargo test").

### Changed
- Each pause transcribes only the speech since the previous pause, so the cost per pause stays
  flat however long you talk (about 200 ms against 300 to 640 ms over a 13 s turn), and turns
  longer than a minute keep their beginning.

### Fixed
- After Escape, speech wakes the session again. Claude Code runs no hook on an interrupt, so the
  session used to stay deaf until something was typed; parlard now reads the interrupt from the
  session transcript, and the background waiter stays parked during a turn instead of being
  dropped. Codex reports it through its Interrupt hook. The busy answer no longer outlives an
  interrupted step.
- Stopping or muting from the indicator (or `/parlar:stop`, `/parlar:mute`) now closes the mic
  device, not just the audio parlar reads from it. The system mic indicator goes out, and a
  Bluetooth headset can leave hands-free mode. Speech that was being heard is dropped instead of
  being transcribed from the queue.

## [0.1.2] - 2026-10-01

### Added
- Speech that arrives during a long tool call gets an answer from parlar itself, once per call:
  it names the step (from the agent's own description) and says your words go through when the
  step ends. A spoken "stop" is told to press Escape to stop the step now.
- npm package `@agent-sh/parlar`: installs the release binaries for your machine, sha256 checked.
- A staged demo GIF at the top of the README.

### Fixed
- After a parlard restart, a session gets its folder and harness back from the next hook, not only
  from the next typed prompt, so voice-only sessions keep the repo vocabulary.
- Codex: a parlard restart no longer ends a turn held open for voice; the Stop waiter reconnects
  like the Claude Code one does.
- Codex: the MCP server no longer reports the plugin's own folder as the session's folder.
- Codex: the SessionEnd hook timeout is 3 s, the most Codex allows.
- `parlar ctl hear` gets the same busy answer as speech from the mic.

## [0.1.1] - 2026-10-01

### Changed
- The repository moved to agent-sh/parlar.
- `parlard service` installs and starts the systemd user service for whichever parlard runs it,
  so `cargo install` users get the service too. `scripts/install.sh` uses it.
- Release builds for x86_64 and aarch64 are attached to each GitHub release.

## [0.1.0] - 2026-10-01

First release: voice conversation mode for Claude Code and Codex, with an MCP `say` tool, harness
hooks that push speech into idle and busy sessions, Phonon-2 recognition, Kokoro voice, and the
GNOME swarm indicator.

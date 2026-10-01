# Changelog

## [Unreleased]

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

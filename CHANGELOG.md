# Changelog

## [0.1.2] - 2026-10-01

### Fixed
- Codex: a parlard restart no longer ends a turn held open for voice; the Stop waiter reconnects
  like the Claude Code one does.
- Codex: the MCP server no longer reports the plugin's own folder as the session's folder.
- Codex: the SessionEnd hook timeout is 3 s, the most Codex allows.
- `parlar ctl hear` gets the same busy answer as speech from the mic.

## [Unreleased]

### Added
- Speech that arrives during a long tool call gets an answer from parlar itself, once per call:
  it names the step (from the agent's own description) and says your words go through when the
  step ends. A spoken "stop" is told to press Escape to stop the step now.
- npm package `@agent-sh/parlar`: installs the release binaries for your machine, sha256 checked.

### Fixed
- After a parlard restart, a session gets its folder and harness back from the next hook, not only
  from the next typed prompt, so voice-only sessions keep the repo vocabulary.

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

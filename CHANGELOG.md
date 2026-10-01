# Changelog

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

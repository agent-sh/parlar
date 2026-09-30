# parley

Voice conversation mode for coding-agent harnesses. Design and decisions: `docs/DESIGN.md`.
Indicator mockups: `design/indicator-lab.html` (the swarm is the chosen visual).

## Layout
- `crates/parley`: one binary. `parley daemon` (parleyd), `parley mcp`, `parley hook <event>`,
  `parley status`, `parley ctl`.
- `plugin/claude`: Claude Code plugin (MCP server plus hooks). `bin/parley` is a dev symlink to
  `target/release/parley`, not committed.

## Rules
- All message passing goes through the harness plugin. No terminal injection (PTY, tmux
  send-keys) in the product. tmux is allowed only as a test harness.
- Hooks run on every tool call: `parley hook` must stay free of an async runtime and return in
  milliseconds, and must exit 0 with no output when parleyd is not running.
- The first 512 characters of `format::INSTRUCTIONS` carry the core rule (Codex window); the
  whole text stays under 2048 (Claude Code truncation). A unit test enforces both.
- No em dashes in docs, comments or commit messages.

## Checks
- `cargo build --release && cargo test --release` (run under `nice -n 19`).
- End to end: start `parley daemon`, start `claude --plugin-dir plugin/claude` in a scratch dir,
  drive utterances with `parley ctl hear "..."`. Never `pkill -f` a pattern that appears in the
  same shell command line.

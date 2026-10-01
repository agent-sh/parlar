# parlar

Voice conversation mode for coding-agent harnesses. Design and decisions: `docs/DESIGN.md`.
Indicator mockups: `design/indicator-lab.html` (the swarm is the chosen visual).

## Layout
- `crates/parlar`: core library plus the light `parlar` binary (`mcp`, `hook <event>`, `status`,
  `ctl`). No audio and no ONNX Runtime here: hooks run on every tool call.
- `crates/parlard`: the daemon. Audio (cpal on PipeWire), Silero VAD, Phonon-2 (ONNX, TDT decoding
  in `stt.rs`) transcribing at pauses, endpointing with Smart Turn, Kokoro voice, repo vocabulary.
  Models load when a conversation starts and unload two minutes after it stops. Debug subcommands:
  `speak`, `final`, `turn`, `clean`, `devices`, `fetch`. The Phonon-2 export lives in its own repo
  (`~/projects/phonon2-onnx`, published as tiyuvta/Phonon-2-ONNX).
- `crates/parlar-moonshine`: bindings to libmoonshine. Its build script fetches the pinned
  release (sha256 checked) unless `PARLAR_MOONSHINE_DIR` is set, and copies the libraries next
  to the binaries.
- `plugin/claude`, `plugin/codex`: harness plugins. `bin/parlar` is a committed shell launcher
  that runs the installed binary and keeps hooks silent before install; the install script
  replaces it with the real binary in the local marketplace copy. Never symlink it to
  `target/release/parlar`: writing through the link overwrites the build (it is a hard link).
- `shell/gnome`: the GNOME Shell indicator (swarm). `scripts/install.sh`: per-user install.

## Rules
- All message passing goes through the harness plugin. No terminal injection (PTY, tmux
  send-keys) in the product. tmux is allowed only as a test harness.
- Hooks run on every tool call: `parlar hook` must stay free of an async runtime and return in
  milliseconds, and must exit 0 with no output when parlard is not running.
- The first 512 characters of `format::INSTRUCTIONS` carry the core rule (Codex window); the
  whole text stays under 2048 (Claude Code truncation). A unit test enforces both.
- No machine-specific paths. Binaries find their libraries through `$ORIGIN` rpaths, data goes
  under `$XDG_DATA_HOME`, the socket under `$XDG_RUNTIME_DIR`. It must work for any Linux user.
- No em dashes in docs, comments or commit messages.

## Checks
- `cargo build --release && cargo test --release` (run under `nice -n 19`).
- End to end: start `parlard --silent` (add `--input-wav` for speech), start
  `claude --plugin-dir plugin/claude` in a scratch dir (with parlar installed, or `PARLAR_BIN`
  pointing at `target/release/parlar`), drive utterances with
  `parlar ctl hear "..."`. Kill test processes by pid; never `pkill -f` a pattern that appears in
  the same shell command line.
- Indicator: test in a nested shell, `dbus-run-session -- env GSETTINGS_BACKEND=memory
  gnome-shell --devkit` (package mutter-dev-bin), then enable the extension on that bus only.

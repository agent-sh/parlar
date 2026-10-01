# parlar

## Project context
Voice conversation mode for coding-agent harnesses (Claude Code, Codex), in Rust, Linux only.
Part of agent-sh. Design and decisions: `docs/DESIGN.md`. Indicator mockups:
`design/indicator-lab.html` (the swarm is the chosen visual).

## Layout
- `crates/parlar`: core library plus the light `parlar` binary (`mcp`, `hook <event>`, `status`,
  `ctl`). No audio and no ONNX Runtime here: hooks run on every tool call.
- `crates/parlard`: the daemon. Audio (cpal on PipeWire), Silero VAD, Phonon-2 (ONNX, TDT decoding
  in `crates/parlard/src/stt.rs`) transcribing at pauses, endpointing with Smart Turn, Kokoro voice, repo vocabulary.
  Models load when a conversation starts and unload two minutes after it stops. Debug subcommands:
  `speak`, `final`, `turn`, `clean`, `devices`, `fetch`. The recognizer is the Phonon-2 ONNX export
  published as tiyuvta/Phonon-2-ONNX on Hugging Face.
- `crates/parlar-moonshine`: bindings to libmoonshine for Kokoro, loaded at run time
  (libloading). `parlard fetch` downloads the pinned release (sha256 checked) into
  `$XDG_DATA_HOME/parlar/lib`; `models::lib_dir` also looks next to the binary and in
  `lib/parlar` under the install prefix (packages), or takes `PARLAR_LIB_DIR`. Nothing links it, so `cargo install` works.
- `plugin/claude`, `plugin/codex`: harness plugins. `bin/parlar` is a committed shell launcher
  that runs the installed binary and keeps hooks silent before install; the install script
  replaces it with the real binary in the local marketplace copy.
- `shell/gnome`: the GNOME Shell indicator (swarm). `scripts/install.sh`: per-user install.

## Rules
- Put a real copy of the binary in a plugin's `bin/` (`install -m 755`). A symlink to
  `target/release/parlar` lets a write through the link overwrite the build (it is a hard link).
- All message passing goes through the harness plugin. No terminal injection (PTY, tmux
  send-keys) in the product. tmux is allowed only as a test harness.
- Hooks run on every tool call: `parlar hook` must stay free of an async runtime and return in
  milliseconds, and must exit 0 with no output when parlard is not running.
- The first 512 characters of `format::INSTRUCTIONS` carry the core rule (Codex window); the
  whole text stays under 2048 (Claude Code truncation). A unit test enforces both.
- No machine-specific paths. libmoonshine and ONNX Runtime are found at run time (see above), data
  goes under `$XDG_DATA_HOME`, the socket under `$XDG_RUNTIME_DIR`. It must work for any Linux user.
- No em dashes in docs, comments or commit messages.

## Checks
- `cargo build --release && cargo test --release` (run under `nice -n 19`).
- End to end: start `parlard --silent` (add `--input-wav` for speech), start
  `claude --plugin-dir plugin/claude` in a scratch dir (with parlar installed, or `PARLAR_BIN`
  pointing at `target/release/parlar`), drive utterances with
  `parlar ctl hear "..."`. Kill test processes by pid; never `pkill -f` a pattern that appears in
  the same shell command line.
- Isolate every end-to-end test from the owner's live parlard: a scratch `XDG_RUNTIME_DIR` and
  `XDG_STATE_HOME` for the daemon and the harness, and for Codex also its shell
  (`codex -c 'shell_environment_policy.set.PARLAR_SOCKET="<scratch socket>"'`), because Codex
  does not pass `XDG_RUNTIME_DIR` to commands it runs and they reach `/run/user/<uid>`. Check the
  live daemon's state is unchanged afterwards.
- Indicator: test in a nested shell, `dbus-run-session -- env GSETTINGS_BACKEND=memory
  gnome-shell --devkit` (package mutter-dev-bin), then enable the extension on that bus only.

# parley

Talk to a running coding-agent session and hear it talk back. The session keeps running in its
own terminal; parley passes messages through the harness plugin (an MCP server plus hooks), so it
works with any terminal and shell. Everything runs locally on the CPU.

- Speech to text: Moonshine v2 Small, streaming, biased toward the focused repo's file names.
- End of turn: word rules (a trailing "and", "so", "um" holds) fused with Smart Turn v3.2.
- Voice: Kokoro-82M, streamed by sentence, cut off when you talk over it.
- Indicator: the swarm, a GNOME Shell extension floating above all windows.

Linux only for now (x86_64 and aarch64).

## Install

Needs cargo, a C toolchain, curl and the PipeWire headers
(Debian/Ubuntu `libpipewire-0.3-dev`, Fedora `pipewire-devel`, Arch `pipewire`).

```
scripts/install.sh              # binaries in ~/.local, models, plugins, GNOME extension, user service
scripts/install.sh --no-service # same, without the systemd user service
```

The script prints the two commands that register the plugin with Claude Code and Codex.
`PREFIX`, `XDG_DATA_HOME` and `XDG_CONFIG_HOME` are honored.

## Use

- `parley ctl state`, `parley ctl on|off|mute|unmute|voice-on|voice-off`
- `parley ctl devices`, `parley ctl input <id>`, `parley ctl output <id>`
- `parley ctl hear "text"` delivers text as if it had been spoken (testing without a mic)
- `parley status` prints one line for a harness status line

## Develop

```
cargo build --release && cargo test --release
target/release/parleyd --silent --input-wav clip.wav   # run the pipeline on a recording
claude --plugin-dir plugin/claude                      # plugin from the source tree
```

`plugin/claude/bin/parley` must point at a built `parley` (a symlink to
`../../../target/release/parley` works). Design and decisions: `docs/DESIGN.md`.

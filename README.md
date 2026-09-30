# parley

Talk to a running coding-agent session and hear it talk back. The session keeps running in its
own terminal; parley passes messages through the harness plugin (MCP server plus hooks), so it
works with any terminal and shell.

Status: transport slice. Voice in and out are stand-ins (`parley ctl hear`, a command voice).

```
cargo build --release
target/release/parley daemon                      # or: --voice-cmd espeak-ng
claude --plugin-dir plugin/claude                 # in another terminal
target/release/parley ctl hear "what does the router do"
```

Status line: set Claude Code `statusLine` to `parley status` with `refreshInterval: 1`.

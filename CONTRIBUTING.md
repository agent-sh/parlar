# Contributing to parlar

## Development setup

```bash
git clone https://github.com/agent-sh/parlar.git
cd parlar
cargo build --release
target/release/parlard fetch   # libmoonshine and the models, about 800 MB, once
cargo test --release
```

Build needs the PipeWire and ALSA headers and clang (Debian/Ubuntu:
`libpipewire-0.3-dev libasound2-dev clang`).

## Before opening a PR

Run what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --document-private-items
cargo publish --workspace --dry-run --locked
agnix .
```

For voice changes, also test end to end. Start `parlard --silent` on a scratch socket
(`PARLAR_SOCKET=/tmp/p.sock`), add `--input-wav` to feed recorded speech, run
`claude --plugin-dir plugin/claude` in a scratch directory, and send utterances with
`parlar ctl hear "..."`. `AGENTS.md` lists the rules the code keeps: hooks stay fast and silent
without parlard, and message passing stays inside the plugin.

## PR guidelines

- Keep changes focused. Say what desktop, mic and harness you tested on.
- Include `parlar ctl state` and the `parlard` log (`journalctl --user -u parlard`) for audio or
  routing issues.
- Update `README.md` when a user-facing command changes, and `docs/DESIGN.md` when a design
  decision changes.
- A release bumps the version in `Cargo.toml`, `package.json` and both plugin manifests; CI checks
  they match. Pushing the `v<version>` tag builds the release binaries and publishes the crates
  and the npm package.
- Use conventional commit prefixes when practical (`fix:`, `feat:`, `docs:`, `chore:`).

## Security

Do not open public issues for vulnerabilities. See [SECURITY.md](SECURITY.md).

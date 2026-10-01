#!/bin/sh
# Install parley for the current user: binaries, libraries, models, harness plugins, the GNOME
# indicator and a systemd user service. Nothing here needs root.
#
#   scripts/install.sh            build, install, fetch models, enable the service
#   PREFIX=/opt/parley scripts/install.sh
#   scripts/install.sh --no-service
#
# Build needs: cargo, a C toolchain, curl, and on Linux the PipeWire headers
# (Debian/Ubuntu: libpipewire-0.3-dev, Fedora: pipewire-devel, Arch: pipewire).
set -eu

SERVICE=1
for a in "$@"; do
    case "$a" in
        --no-service) SERVICE=0 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PREFIX=${PREFIX:-$HOME/.local}
DATA=${XDG_DATA_HOME:-$HOME/.local/share}
CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}
BIN=$PREFIX/bin
LIB=$PREFIX/lib/parley
MARKET=$DATA/parley/marketplace

cargo build --release --manifest-path "$ROOT/Cargo.toml"
T=$ROOT/target/release

install -d "$BIN" "$LIB"
install -m 755 "$T/parley" "$T/parleyd" "$BIN/"
# parleyd finds these through its $ORIGIN/../lib/parley rpath
install -m 644 "$T/libmoonshine.so" "$T/libonnxruntime.so.1" "$LIB/"

# plugins: a local marketplace per harness, with bin/parley pointing at the installed binary
plugin() {
    harness=$1 dst=$2
    rm -rf "$dst"
    mkdir -p "$dst"
    cp -R "$ROOT/plugin/$harness/." "$dst/"
    rm -rf "$dst/bin"
    mkdir "$dst/bin"
    ln -s "$BIN/parley" "$dst/bin/parley"
}
plugin claude "$MARKET/claude/parley"
mkdir -p "$MARKET/claude/.claude-plugin"
cat > "$MARKET/claude/.claude-plugin/marketplace.json" <<JSON
{
  "name": "parley",
  "owner": { "name": "parley" },
  "plugins": [{ "name": "parley", "source": "./parley", "description": "Voice conversation mode" }]
}
JSON
plugin codex "$MARKET/codex/parley"
mkdir -p "$MARKET/codex/.agents/plugins"
cat > "$MARKET/codex/.agents/plugins/marketplace.json" <<JSON
{
  "name": "parley",
  "plugins": [{ "name": "parley", "source": "./parley" }]
}
JSON

"$BIN/parleyd" fetch

if command -v gnome-shell >/dev/null 2>&1; then
    EXT=$DATA/gnome-shell/extensions/parley@avifenesh
    rm -rf "$EXT"
    mkdir -p "$EXT"
    cp -R "$ROOT/shell/gnome/parley@avifenesh/." "$EXT/"
fi

if [ "$SERVICE" = 1 ] && command -v systemctl >/dev/null 2>&1; then
    UNIT=$CONFIG/systemd/user/parleyd.service
    mkdir -p "$(dirname "$UNIT")"
    sed "s|@BINDIR@|$BIN|" "$ROOT/packaging/parleyd.service" > "$UNIT"
    systemctl --user daemon-reload
    systemctl --user enable --now parleyd.service
fi

cat <<MSG

parley is installed in $PREFIX.

Claude Code:
  claude plugin marketplace add "$MARKET/claude"
  claude plugin install parley@parley
  Status line (optional): set statusLine.command to "parley status" with refreshInterval 1.

Codex:
  codex plugin marketplace add "$MARKET/codex"
  codex plugin add parley@parley
  Then trust the parley hooks once from the hooks review prompt.

GNOME indicator (Wayland picks up new extensions after you log out and back in):
  gnome-extensions enable parley@avifenesh

Devices: parley ctl devices, then parley ctl input <id> or parley ctl output <id>.
MSG

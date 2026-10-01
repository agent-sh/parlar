#!/bin/sh
# Install parlar for the current user: binaries, libmoonshine, models, harness plugins, the GNOME
# indicator and a systemd user service. Nothing here needs root.
#
#   scripts/install.sh            build, install, fetch models, enable the service
#   PREFIX=/opt/parlar scripts/install.sh
#   scripts/install.sh --no-service
#
# Build needs: cargo, a C toolchain, clang, and the PipeWire and ALSA headers
# (Debian/Ubuntu: libpipewire-0.3-dev libasound2-dev, Fedora: pipewire-devel alsa-lib-devel,
# Arch: pipewire alsa-lib). Fetching needs curl and tar.
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
MARKET=$DATA/parlar/marketplace

cargo build --release --manifest-path "$ROOT/Cargo.toml"
T=$ROOT/target/release

install -d "$BIN"
install -m 755 "$T/parlar" "$T/parlard" "$BIN/"

# plugins: a local marketplace per harness. Harnesses copy a plugin into their own cache and drop
# symlinks on the way, so bin/parlar is a real copy.
plugin() {
    harness=$1 dst=$2
    rm -rf "$dst"
    mkdir -p "$dst"
    cp -R "$ROOT/plugin/$harness/." "$dst/"
    rm -rf "$dst/bin"
    mkdir "$dst/bin"
    install -m 755 "$T/parlar" "$dst/bin/parlar"
}
plugin claude "$MARKET/claude/parlar"
mkdir -p "$MARKET/claude/.claude-plugin"
cat > "$MARKET/claude/.claude-plugin/marketplace.json" <<JSON
{
  "name": "parlar",
  "owner": { "name": "parlar" },
  "plugins": [{ "name": "parlar", "source": "./parlar", "description": "Voice conversation mode" }]
}
JSON
plugin codex "$MARKET/codex/parlar"
mkdir -p "$MARKET/codex/.agents/plugins"
cat > "$MARKET/codex/.agents/plugins/marketplace.json" <<JSON
{
  "name": "parlar",
  "plugins": [{ "name": "parlar", "source": "./parlar" }]
}
JSON

# libmoonshine (pinned, sha256 checked) and the models, into $DATA/parlar
"$BIN/parlard" fetch

if command -v gnome-shell >/dev/null 2>&1; then
    EXT=$DATA/gnome-shell/extensions/parlar@avifenesh
    rm -rf "$EXT"
    mkdir -p "$EXT"
    cp -R "$ROOT/shell/gnome/parlar@avifenesh/." "$EXT/"
fi

if [ "$SERVICE" = 1 ] && command -v systemctl >/dev/null 2>&1; then
    UNIT=$CONFIG/systemd/user/parlard.service
    mkdir -p "$(dirname "$UNIT")"
    sed "s|@BINDIR@|$BIN|" "$ROOT/packaging/parlard.service" > "$UNIT"
    systemctl --user daemon-reload
    systemctl --user enable parlard.service
    # restart, so a reinstall runs the new binary
    systemctl --user restart parlard.service
fi

cat <<MSG

parlar is installed in $PREFIX.

Claude Code:
  claude plugin marketplace add "$MARKET/claude"
  claude plugin install parlar@parlar
  parlar setup claude        (lets say run without a permission prompt)
  Status line (optional): set statusLine.command to "parlar status" with refreshInterval 1.

Codex:
  codex plugin marketplace add "$MARKET/codex"
  codex plugin add parlar@parlar
  Then trust the parlar hooks once from the hooks review prompt.

GNOME indicator (Wayland picks up new extensions after you log out and back in):
  gnome-extensions enable parlar@avifenesh

Devices: parlar ctl devices, then parlar ctl input <id> or parlar ctl output <id>.
MSG

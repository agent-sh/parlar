---
description: Install or repair parley voice mode on this machine
allowed-tools: Bash(command -v:*), Bash(parley --version:*), Bash(systemctl --user is-active parleyd:*), Bash(pkg-config --modversion libpipewire-0.3:*)
---
!`for t in parley parleyd cargo git curl; do printf '%s: ' "$t"; command -v "$t" || echo missing; done; printf 'parleyd service: '; systemctl --user is-active parleyd 2>/dev/null || true; printf 'PipeWire headers: '; pkg-config --modversion libpipewire-0.3 2>/dev/null || echo missing`

Set parley up for the user from the report above. Ask before each step that installs packages or changes system settings, and run the steps for them once they agree.

1. If parley and parleyd are found and the service is active, it is ready: tell them to run /parley:talk.
2. If both are found but the service is not active, run `systemctl --user enable --now parleyd`.
3. Otherwise build it from source. It needs cargo (rustup.rs), git, curl and the PipeWire headers (Debian and Ubuntu: libpipewire-0.3-dev, Fedora: pipewire-devel, Arch: pipewire). Clone with `git clone https://github.com/avifenesh/parley ~/.local/share/parley/src` (or `git -C ~/.local/share/parley/src pull` if it exists), then run `~/.local/share/parley/src/scripts/install.sh`. It builds, installs into ~/.local, downloads the speech models (about 250 MB), installs the GNOME indicator, and starts the parleyd user service.
4. Run `parley setup claude` so the say tool speaks without a permission prompt each time.
5. Tell them the floating indicator appears on GNOME after their next login, and that /parley:talk starts a conversation.

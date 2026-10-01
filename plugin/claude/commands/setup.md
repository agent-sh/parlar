---
description: Install or repair parlar voice mode on this machine
allowed-tools: Bash(command -v:*), Bash(parlar --version:*), Bash(systemctl --user is-active parlard:*), Bash(pkg-config --modversion libpipewire-0.3:*)
---
!`for t in parlar parlard cargo git curl; do printf '%s: ' "$t"; command -v "$t" || echo missing; done; printf 'parlard service: '; systemctl --user is-active parlard 2>/dev/null || true; printf 'PipeWire headers: '; pkg-config --modversion libpipewire-0.3 2>/dev/null || echo missing`

Set parlar up for the user from the report above. Ask before each step that installs packages or changes system settings, and run the steps for them once they agree.

1. If parlar and parlard are found and the service is active, it is ready: tell them to run /parlar:talk.
2. If both are found but the service is not active, run `systemctl --user enable --now parlard`.
3. Otherwise build it from source. It needs cargo (rustup.rs), git, curl, clang and the PipeWire and ALSA headers (Debian and Ubuntu: libpipewire-0.3-dev libasound2-dev clang, Fedora: pipewire-devel alsa-lib-devel clang, Arch: pipewire alsa-lib clang). Clone with `git clone https://github.com/avifenesh/parlar ~/.local/share/parlar/src` (or `git -C ~/.local/share/parlar/src pull` if it exists), then run `~/.local/share/parlar/src/scripts/install.sh`. It builds, installs into ~/.local, downloads libmoonshine and the speech models (about 800 MB), installs the GNOME indicator, and starts the parlard user service.
4. Run `parlar setup claude` so the say tool speaks without a permission prompt each time.
5. Tell them the floating indicator appears on GNOME after their next login, and that /parlar:talk starts a conversation.

#!/usr/bin/env bash
#
# Install (or update) usbdiag + usbdiag-gui into the current user's ~/.local
# tree — no root, no Flatpak sandbox.
#
# Native rather than Flatpak on purpose. A sandbox cannot read
# /sys/kernel/debug, and a full /sys/bus/usb traversal needs --filesystem=host
# or --device=all, at which point the sandbox is decorative. For a hardware
# diagnostic the native install is the honest primary path (docs/01 §10).
#
# First run installs; every later run updates. It builds release binaries and
# copies them to ~/.local/bin; the desktop entry and icons are refreshed only
# when they actually change, so a normal code update just swaps the binaries.
#
# Usage:
#   ./scripts/install-local.sh
#
# Neither binary needs or wants privileges. `usbdiag probe` asks for root only
# when a probe is explicitly requested; nothing here installs a setuid bit, a
# polkit rule, or a service.

set -euo pipefail

APP_ID="com.iboalali.usbdiag"

# Resolve the repo root from this script's location, so it works from any cwd.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# Destinations (XDG user dirs).
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
APPS_DIR="$DATA_DIR/applications"
ICONS_DIR="$DATA_DIR/icons/hicolor"
METAINFO_DIR="$DATA_DIR/metainfo"

# cargo is usually on PATH; fall back to the rustup default location.
if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    else
        echo "error: cargo not found on PATH (install rustup: https://rustup.rs)" >&2
        exit 1
    fi
fi

# The GUI needs the GTK4 + libadwaita development packages to build. Say so
# before spending a minute compiling the workspace to find out.
for mod in gtk4 libadwaita-1; do
    if ! pkg-config --exists "$mod" 2>/dev/null; then
        echo "error: $mod development files not found." >&2
        echo "       Ubuntu/Debian: sudo apt install libgtk-4-dev libadwaita-1-dev" >&2
        exit 1
    fi
done

echo "==> Building release binaries"
cargo build --release --bin usbdiag --bin usbdiag-gui

echo "==> Installing binaries -> $BIN_DIR"
install -Dm755 target/release/usbdiag "$BIN_DIR/usbdiag"
install -Dm755 target/release/usbdiag-gui "$BIN_DIR/usbdiag-gui"

# install_if_changed <src> <dest> : copy only when missing or different, and
# report (via the global `assets_changed`) whether a desktop/icon asset moved so
# the caches are refreshed just once, only when needed.
assets_changed=0
install_if_changed() {
    local src="$1" dest="$2"
    if ! cmp -s "$src" "$dest" 2>/dev/null; then
        install -Dm644 "$src" "$dest"
        echo "    updated $dest"
        assets_changed=1
    fi
}

echo "==> Installing desktop entry, icons + AppStream metadata (only if changed)"
install_if_changed "data/$APP_ID.desktop" "$APPS_DIR/$APP_ID.desktop"
install_if_changed "data/$APP_ID.metainfo.xml" "$METAINFO_DIR/$APP_ID.metainfo.xml"
install_if_changed \
    "data/icons/hicolor/scalable/apps/$APP_ID.svg" \
    "$ICONS_DIR/scalable/apps/$APP_ID.svg"
install_if_changed \
    "data/icons/hicolor/symbolic/apps/$APP_ID-symbolic.svg" \
    "$ICONS_DIR/symbolic/apps/$APP_ID-symbolic.svg"

if [ "$assets_changed" -eq 1 ]; then
    echo "==> Refreshing icon + desktop caches"
    command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && gtk-update-icon-cache -f -t "$ICONS_DIR" >/dev/null 2>&1 || true
    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
else
    echo "==> Desktop entry + icons already current"
fi

echo
echo "Done. 'USB Diagnostics' is in your app grid; or run: usbdiag-gui"
echo "The command-line tool is 'usbdiag' (try: usbdiag diag)."
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not on your PATH — add it to run 'usbdiag' from a terminal." ;;
esac

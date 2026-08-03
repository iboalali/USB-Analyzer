#!/usr/bin/env bash
# Launch usbdiag-gui and dump a PNG of its window, for visual UI verification.
#
# Why this dance (GNOME/Wayland host):
#   * GNOME locks down the org.gnome.Shell.Screenshot D-Bus methods -> AccessDenied.
#   * A GTK4 window grabbed via Xwayland comes out BLACK because of the GL
#     renderer, so we force GSK_RENDERER=cairo (software render into a real X
#     drawable) + GDK_BACKEND=x11.
#   * `xwd -id` grabs the window pixmap by id regardless of screen position; the
#     WM may place the window partly off-screen, which breaks `ffmpeg -f x11grab`.
#
# Usage:
#   .claude/skills/screenshot-app/capture.sh [OUTPUT.png]
# Env:
#   USBDIAG_BIN  path to the binary  (default ./target/debug/usbdiag-gui)
#   SETTLE       seconds to wait after the window maps (default 2)
#   WIDTH/HEIGHT open the window at this size, e.g. WIDTH=460 for the collapsed
#                breakpoint (passed straight through to the binary; there is no
#                xdotool on this host to resize an existing window)
#   SCHEME       `light` | `dark` — forces the colour scheme for this run only
set -euo pipefail

TITLE="usbdiag"
BIN="${USBDIAG_BIN:-./target/debug/usbdiag-gui}"
OUT="${1:-/tmp/usbdiag-shot.png}"
SETTLE="${SETTLE:-2}"

if [[ ! -x "$BIN" ]]; then
  echo "binary not found at $BIN -- build first: cargo build --bin usbdiag-gui" >&2
  exit 1
fi

# Fresh instance under the software renderer + X11 backend.
pkill -x usbdiag-gui 2>/dev/null || true

# SIGTERM runs no destructors, so the app's `udevadm monitor` child survives the
# pkill above and is reparented to init. The app cleans up correctly on a
# *graceful* close (its shutdown hook ends the subprocess), but that is not what
# this script does — so repeated captures used to leave a trail of stray
# monitors, three of which were found idling hours later.
#
# Only orphans are reaped: a udevadm whose parent is still alive belongs to a
# running instance, or to somebody's terminal, and is none of our business.
for _p in $(pgrep -x udevadm 2>/dev/null); do
  _ppid=$(awk '{print $4}' "/proc/$_p/stat" 2>/dev/null || echo 1)
  if [[ ! -d "/proc/$_ppid" ]] || ! grep -qs usbdiag "/proc/$_ppid/comm" 2>/dev/null; then
    grep -qs -- '--subsystem-match=typec' "/proc/$_p/cmdline" && kill "$_p" 2>/dev/null || true
  fi
done

env_extra=()
case "${SCHEME:-}" in
  # libadwaita honours this for a single process, so light and dark can be shot
  # back to back without touching the desktop's own setting.
  dark)  env_extra+=(ADW_DEBUG_COLOR_SCHEME=prefer-dark) ;;
  light) env_extra+=(ADW_DEBUG_COLOR_SCHEME=prefer-light) ;;
esac

size_args=()
if [[ -n "${WIDTH:-}" ]]; then size_args+=(--width "$WIDTH"); fi
if [[ -n "${HEIGHT:-}" ]]; then size_args+=(--height "$HEIGHT"); fi

# `env`, not a bare assignment prefix: only literal `NAME=value` words are
# treated as assignments, so an expanded array there is run as a command.
env GSK_RENDERER=cairo GDK_BACKEND=x11 "${env_extra[@]}" "$BIN" "${size_args[@]}" \
  >/tmp/usbdiag-shot.log 2>&1 &

# Wait (up to ~18s) for the window to map. Poll with a small sleep — xwininfo is
# too fast to pace the loop on its own.
WID=""
for _ in $(seq 1 60); do
  WID=$(xwininfo -root -tree 2>/dev/null | awk -v t="\"$TITLE\":" '$0 ~ t {print $1; exit}')
  if [[ -n "$WID" ]]; then break; fi
  sleep 0.3
done
if [[ -z "$WID" ]]; then
  echo "window titled '$TITLE' never appeared (see /tmp/usbdiag-shot.log)" >&2
  exit 1
fi

# Let the first udev tick land so the live pill shows its real state.
sleep "$SETTLE"

XWD="$(mktemp --suffix=.xwd)"
trap 'rm -f "$XWD"' EXIT
xwd -id "$WID" -out "$XWD"
ffmpeg -loglevel error -y -i "$XWD" "$OUT"
echo "$OUT"

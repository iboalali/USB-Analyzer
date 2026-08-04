#!/usr/bin/env bash
#
# Install the pieces that let the GUI run a privileged probe: the `usbdiag`
# command in a root-owned location, and the polkit action that makes one
# authentication cover a session.
#
# This is NOT the way to install the application — scripts/install-local.sh does
# that, into ~/.local, with no root at all. Run this one *as well*, and only if
# you want probes runnable from the window.
#
# Why it has to be a system install. The GUI refuses to run a helper as root
# unless the binary and every directory above it are root-owned and unwritable
# by anyone else. ~/.local/bin/usbdiag fails that on purpose: escalating a file
# you can rewrite means root executing whatever anything running as you put
# there. /usr/local/bin satisfies it, and is also where the polkit action has to
# point.
#
# Usage:
#   sudo ./scripts/install-system.sh
#   sudo ./scripts/install-system.sh --uninstall
#
# What it writes, and nothing else:
#   /usr/local/bin/usbdiag                                  root:root 0755
#   /usr/share/polkit-1/actions/com.iboalali.usbdiag.policy  root:root 0644
#
# No setuid bit, no service, no daemon. The window stays unprivileged; a probe
# is a child process that ends with its answer.

set -euo pipefail

APP_ID="com.iboalali.usbdiag"
BIN_DEST="/usr/local/bin/usbdiag"
POLICY_DEST="/usr/share/polkit-1/actions/$APP_ID.policy"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

if [ "$(id -u)" -ne 0 ]; then
    echo "error: this one needs root — it writes to /usr/local/bin and /usr/share." >&2
    echo "       sudo $0 ${*:-}" >&2
    exit 1
fi

if [ "${1:-}" = "--uninstall" ]; then
    echo "==> Removing $BIN_DEST and $POLICY_DEST"
    rm -f "$BIN_DEST" "$POLICY_DEST"
    echo
    echo "Done. The GUI will now say escalation is unavailable, which is true."
    echo "A user install under ~/.local (install-local.sh) is untouched."
    exit 0
fi

if [ "$#" -gt 0 ]; then
    echo "error: unknown argument: $1 (only --uninstall is understood)" >&2
    exit 1
fi

# Built beforehand rather than here. `cargo` under sudo would compile as root,
# leaving a root-owned target/ directory that the user's next ordinary build
# cannot write — a mess this script has no business creating.
if [ ! -x target/release/usbdiag ]; then
    echo "error: target/release/usbdiag not found." >&2
    echo "       Build it first, as yourself:  cargo build --release --bin usbdiag" >&2
    exit 1
fi

echo "==> Installing $BIN_DEST"
install -o root -g root -m 0755 target/release/usbdiag "$BIN_DEST"

echo "==> Installing $POLICY_DEST"
install -o root -g root -m 0644 "data/$APP_ID.policy" "$POLICY_DEST"

# polkitd watches the actions directory and picks new files up by itself, so
# there is nothing to reload. Read it back rather than assume: if the action is
# not there, every probe would still prompt and the only symptom would be a
# password prompt that never stops appearing.
echo "==> Checking polkit registered the action"
if command -v pkaction >/dev/null 2>&1; then
    if pkaction --action-id "$APP_ID.probe" >/dev/null 2>&1; then
        pkaction --verbose --action-id "$APP_ID.probe" | sed 's/^/    /'
    else
        echo "warning: polkit does not list $APP_ID.probe yet." >&2
        echo "         Probes will still run — they will just ask every time." >&2
    fi
else
    echo "    pkaction not installed, so this cannot be checked here"
fi

echo
echo "Done. In the GUI's *Active probes* card, a probe whose only obstacle is"
echo "privilege now offers 'Run as root…', and the password is asked once for"
echo "a burst of probes from one window rather than once per probe."
echo
echo "To undo all of it:  sudo $0 --uninstall"

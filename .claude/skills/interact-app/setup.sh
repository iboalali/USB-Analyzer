#!/usr/bin/env bash
# One-time (idempotent) setup for the interact-app driver: a local venv with
# python-xlib. Pure Python, no root, no system packages.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV="$DIR/.venv"
if [[ ! -x "$VENV/bin/python" ]]; then
  python3 -m venv "$VENV"
fi
"$VENV/bin/pip" install -q python-xlib
echo "$VENV/bin/python"

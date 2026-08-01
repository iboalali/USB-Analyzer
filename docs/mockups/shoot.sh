#!/bin/sh
# Render the GUI mockups to PNG. Output goes to $1, default ./shots.
#
# Headless Chrome rather than a real browser window: the mockups exist to be
# looked at side by side and diffed after an edit, and that only works if the
# pixel size is fixed.
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
out=${1:-$here/shots}
mkdir -p "$out"

chrome=$(command -v google-chrome || command -v chromium || command -v chromium-browser)

shot() { # file  hash  name  width  height
    "$chrome" --headless=new --disable-gpu --no-sandbox --hide-scrollbars \
        --force-device-scale-factor=2 --window-size="$4,$5" \
        --screenshot="$out/$3.png" "file://$here/$1$2" >/dev/null 2>&1
}

shot standalone.html       '#light'  standalone-light        1240 880
shot standalone.html       '#dark'   standalone-dark         1240 880
shot standalone-fault.html '#light'  standalone-fault-light  1240 890
shot standalone-fault.html '#dark'   standalone-fault-dark   1240 890
shot standalone-fault.html '#scroll' standalone-fault-lower  1240 890
shot compact.html          '#light'  compact-light            920 740
shot compact.html          '#dark'   compact-dark             920 740

echo "wrote 7 png to $out"

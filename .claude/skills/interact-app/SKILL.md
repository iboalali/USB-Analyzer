---
name: interact-app
description: Drive the running usbdiag-gui window (click, type, key-press) via XTEST on the X11 backend, to select a port or device and see its detail pane. Pairs with screenshot-app to verify each step. No root required.
---

# Interact with the running app

Sends real input (clicks, keystrokes) to the `usbdiag-gui` window so a pane can
be reached as a user would — then screenshotted to confirm what it shows. Uses
the X server's XTEST extension via `python-xlib`; works because the app runs
under the X11 backend (Xwayland). No root, no system packages.

Most of what this app does is *look* at hardware, so the interactions that
matter are few: select a sidebar row, toggle *show hubs*, hit refresh. That is
also the whole reason it is needed — the detail pane for a port or a device
cannot be seen any other way, since the window opens on whatever is worst.

## Prerequisites

1. **App running under the X11 backend.** The `screenshot-app` skill's
   `capture.sh` already sets `GSK_RENDERER=cairo GDK_BACKEND=x11`, or manually:
   ```sh
   GSK_RENDERER=cairo GDK_BACKEND=x11 ./target/debug/usbdiag-gui &
   ```
   Native Wayland (plain `cargo run`) will NOT work — XTEST needs the X backend.
2. **One-time driver setup** (creates a local venv, ignored by git):
   ```sh
   .claude/skills/interact-app/setup.sh          # prints the venv python path
   ```

## Use it

```sh
PY=.claude/skills/interact-app/.venv/bin/python
$PY .claude/skills/interact-app/interact.py "usbdiag" <command> [args] ...
```

Commands (chain as many as you like, executed in order):

| Command      | Effect                                                        |
|--------------|---------------------------------------------------------------|
| `click X Y`  | left-click at **window-relative** (X, Y); top-left incl. title bar is (0,0) |
| `key NAME`   | tap an X keysym: `Return` `Escape` `Tab` `Left` `Right` …      |
| `type TEXT`  | type a string (printable ASCII; shifted chars handled)        |
| `move X Y`   | move the window's top-left to screen (X, Y)                    |
| `sleep SECS` | pause (float) so a capture / repaint settles                   |

Example — select the second sidebar row (a Type-C port), then look:
```sh
$PY .../interact.py "usbdiag" click 245 240 sleep 1
```

## The verify loop

Interactions are blind unless you look. After driving, screenshot the **same**
running window — do NOT re-run `screenshot-app/capture.sh`, which relaunches the
app and throws the selection away:
```sh
WID=$(xwininfo -root -tree | awk '/"usbdiag":/{print $1; exit}')
xwd -id "$WID" -out /tmp/s.xwd && ffmpeg -y -i /tmp/s.xwd /tmp/s.png   # then Read /tmp/s.png
```
Find click targets by reading a screenshot first and measuring the pixel
position (coordinates are the same in the PNG and window-relative space).

## Gotchas

- **The sidebar reflows under you.** Rows carry a two-line reason, so their
  heights differ and change when a device is plugged in or unplugged, or when
  *show hubs* is toggled. Re-screenshot and re-measure before the next click.
- **A capture can land between your click and your grab.** The window repaints
  on udev events and on a 2 s fallback tick, and a rebuild rebuilds both panes.
  Sleep ~1 s after clicking before grabbing.
- **Window-relative coords, absolute clicks.** The driver reads the window's
  on-screen origin and adds it, so pass coordinates as measured off a screenshot.
- **Never click outside the target window — it ends the session.** A click that
  lands off the window hands keyboard focus to whatever is under it, and XTEST
  cannot take it back: clicks keep working while every later `type`/`key`
  silently goes nowhere. Only a restart fixes it.
- **Off-screen window.** If the WM parked the window partly off-screen (negative
  origin), clicks at small coordinates map off-screen. Run `move 60 60` first.
- **No side effects to fear.** v1 is a viewer: there is nothing to click that
  changes the system. That is not true once the probe panel arrives — a
  re-enumeration probe cycles a real port.

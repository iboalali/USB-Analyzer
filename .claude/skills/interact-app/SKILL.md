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
| `scroll DIR X Y [N]` | wheel `up`/`down` N notches (default 3), pointer at (X, Y) |
| `move X Y`   | move the window's top-left to screen (X, Y)                    |
| `sleep SECS` | pause (float) so a capture / repaint settles                   |

**`scroll` is how you see the bottom of a pane.** The detail pane is a
`ScrolledWindow` and its content is routinely taller than a window that fits on
this screen — the host pane ends with the *Active probes* card, which sits
entirely below the fold at the default size, so a screenshot without scrolling
silently omits it. Wheel events are routed by pointer **position**, not by focus,
so give coordinates over the pane you mean: `scroll down 800 600 8` for the detail
side. The same coordinates over the sidebar scroll the sidebar instead.

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

## Dropdowns and other popovers

A `GtkDropDown`'s list is **its own X toplevel**, not part of the window you
grabbed — `xwd -id <main window>` will not show it, and clicking blind at
coordinates measured from an earlier attempt does not work, because GTK
repositions the popover so the *currently selected* row sits near the button.
Two attempts at the same row can therefore need two different coordinates.

Find it, grab it, measure it, then click:

```sh
$PY .../interact.py "usbdiag" click <button-x> <button-y> sleep 1.3
xwininfo -root -children | grep usbdiag        # the popover is the small one
#   0x14001ef "usbdiag-gui": ... 231x454+1061+289   <- origin +1061+289
xwd -id 0x14001ef -out /tmp/pop.xwd && ffmpeg -y -i /tmp/pop.xwd /tmp/pop.png
```

Read `/tmp/pop.png`, measure the row's y *inside the popover*, then convert:
window-relative = popover origin + row offset − the main window's origin
(`+40+20` after `move 40 20`). Do the open-click and the row-click in **one**
`interact.py` invocation — the driver activates the named window on startup,
and a second invocation would raise the toplevel and dismiss the popover.

`xwd -root` does not work here at all (`BadMatch` under Xwayland), so grabbing
the whole screen is not an option.

## Two things XTEST cannot do here, and what to use instead

- **The header bar does not take synthesized clicks.** Verified by elimination:
  clicks on the hamburger, *and* on GTK's own minimize and maximize buttons, do
  nothing — no popover, no change in `Map State`, no change in geometry — while a
  click on a sidebar row in the same invocation selects it. So it is not the
  coordinates and not the widget. Do not spend time bisecting `y` for anything in
  that row; drive it another way.
- **`key` has no modifiers**, so accelerators cannot be tested. It does
  `XK.string_to_keysym(name)`, and a GTK accelerator string like
  `<Primary>comma` is not a keysym — it resolves to NoSymbol, taps keycode 0, and
  silently does nothing. `key Down`, `key Return`, `key Escape` are fine.

**Reach a menu item through its action instead.** `GApplication` exports its
action group on the session bus, so any app-level action is activatable without
touching the UI — which is how the preferences dialog gets tested at all:

```sh
gdbus call --session --dest com.iboalali.usbdiag \
  --object-path /com/iboalali/usbdiag \
  --method org.gtk.Actions.List
gdbus call --session --dest com.iboalali.usbdiag \
  --object-path /com/iboalali/usbdiag \
  --method org.gtk.Actions.Activate preferences '@av []' '@a{sv} {}'
```

Only `app.`-scoped actions appear there; a `win.`-scoped one does not. That is
one reason this app puts *Preferences* on the application.

**Prefer the keyboard inside a dropdown.** Clicking a row by measured coordinates
works once and then does not: reopening the popover can place it differently, and
a stale offset lands on the wrong row or on nothing. With the list open,
`key Down` × n then `key Return` needs no measurement and no second grab. Picking
*Dark* from a three-item `AdwComboRow` is `click <row> sleep 1.2 key Down key Down
key Return` — and after a click-by-coordinate attempt of the same thing wrote no
file at all, this is the form that worked.

## One instance, and killing the right one

`usbdiag-gui` is a single-instance `GApplication`. A second copy **hands off to the
first and exits immediately** — so if the user already has the app open, your
launch silently does nothing and the window you then drive is *theirs*, on their
backend (native Wayland from the app grid, where XTEST cannot reach it at all).
Check before launching:

```sh
pgrep -a usbdiag-gui
```

To run your own alongside theirs, give it a private session bus — uniqueness is
registered there:

```sh
GSK_RENDERER=cairo GDK_BACKEND=x11 dbus-run-session -- ./target/debug/usbdiag-gui
```

It costs a nested portal/keyring startup (~8 s before the window maps, and pages
of `dbus-daemon` noise in the log), so prefer a plain launch when the field is
clear.

**Never `pkill -f` / `pgrep -f` a pattern that names the binary.** The pattern
matches the command line of the very shell running it, so `pkill -f
'target/debug/usbdiag-gui'` kills your own wrapper — the tool call comes back
`exit code 144` (SIGTERM) having done nothing you asked. Kill by explicit PID,
and note that `dbus-run-session` means **two** processes: killing the wrapper
leaves the app running. Two surviving instances is the worst case, because the
driver picks the *first* window named `usbdiag` while you read the other one's
log — every conclusion from that run is worthless. Verify:

```sh
xwininfo -root -tree | grep -c '"usbdiag":'      # must be 1
```

## Timing a popover is not evidence

A `GtkPopover` closes on focus-out. Anything else taking focus — the user typing
in another window, a notification — dismisses it, and under Xwayland
`_NET_ACTIVE_WINDOW` goes to `0x0` when focus lands on a native Wayland window.
So "the list closed after N seconds" cannot distinguish *your* bug from someone
touching the keyboard, and a poll loop of `xwininfo` shell-outs gives no ordering
you can trust either.

**Make the app say what it did.** A temporary `eprintln!` in the render path,
printing the decision rather than the symptom, settles in one run what a dozen
screenshots cannot:

```
view rendered=6 revision=7 menu_open=true  -> DEFERRED
view rendered=6 revision=7 menu_open=false -> REBUILD
```

Remove it before committing.

**Do not try to screenshot the list.** `xwd` on a popover toplevel here comes back
holding whatever is *behind* it — the tooltip the pointer is provoking, and the
pane underneath — not the list. Two things make it useless: the driver leaves the
pointer resting on the button it clicked, so a tooltip toplevel of its own appears
beside the list, and the popover's pixmap is not readable under this
compositor/renderer combination. What *is* reliable is its **existence**, matched
by geometry (the kind list is ~233x454; a tooltip is a wide, short strip):

```sh
xwininfo -root -tree | grep -E '"usbdiag-gui": \("usbdiag-gui"' | grep -cE ' [0-9]{3}x4[0-9][0-9]'
```

**Open a popover in its own invocation.** Chaining `click <row> sleep 1.6 click
<dropdown>` fails silently: selecting a sidebar row rebuilds the pane, and the
second click can land before the control exists at that position. The row click
did work — so the symptom is a popover that never appears, which reads exactly
like the bug you may be trying to fix.

## Gotchas

- **The sidebar reflows under you.** Rows carry a two-line reason, so their
  heights differ and change when a device is plugged in or unplugged, or when
  *show hubs* is toggled. Re-screenshot and re-measure before the next click.
- **A capture can land between your click and your grab.** The window repaints
  on udev events and on a 2 s fallback tick, and a rebuild rebuilds both panes.
  Sleep ~1 s after clicking before grabbing — and ~2 s after a `scroll`, which
  moves more pixels than a click.
- **A partial grab looks like a bug in the app, and is not.** Symptom: the PNG is
  correct at the top left and solid **black** down the right and bottom edges,
  cutting through card content, often with the pane scrolled back to where it
  started. That is `xwd` reading the window's pixmap mid-relayout, not a broken
  widget and not an off-screen window — check the screen size before believing
  the off-screen theory (`xdpyinfo | grep dimensions`; this host is 4992x1728, so
  almost nothing is really off-screen). Sleep longer and grab again; if it
  persists, re-run `screenshot-app/capture.sh`, which forces `GSK_RENDERER=cairo`
  and waits for the first tick to settle.
- **Window-relative coords, absolute clicks.** The driver reads the window's
  on-screen origin and adds it, so pass coordinates as measured off a screenshot.
- **Never click outside the target window — it ends the session.** A click that
  lands off the window hands keyboard focus to whatever is under it, and XTEST
  cannot take it back: clicks keep working while every later `type`/`key`
  silently goes nowhere. Only a restart fixes it.
- **Do not `move` the window as a habit.** The driver adds the window's *current*
  on-screen origin to every coordinate, so moving buys nothing — and on a
  multi-head desktop it actively hurts: it drags the window off the display its
  owner had it on. Use `move` only when the origin is negative (see below).
- **Small coordinates are not "the top left of the screen".** Read the layout
  before choosing any absolute position:
  ```sh
  xrandr --listmonitors
  #  0: +*HDMI-1 3072/700x1728/390+1920+0   <- primary, starts at x=1920
  #  1: +eDP-1   1920/340x1200/220+0+323    <- laptop, starts at y=323
  ```
  On this host `move 40 20` is on the *secondary* display **and** above where it
  begins vertically, so the title bar lands in dead space and gets clipped. A
  safe spot here is `move 2100 150` — inside the primary's origin.
- **Off-screen window.** If the WM really parked it at a negative origin, clicks
  at small coordinates map off-screen; move it onto a monitor first, using the
  geometry above rather than a guess.
- **One control has a side effect.** The viewer touches no hardware, but the
  *What this is* dropdown writes a label to
  `$XDG_CONFIG_HOME/usbdiag/devices.json` and every later run reads it. Check
  `usbdiag labels` afterwards and clear anything a test left behind:
  `usbdiag labels <id> --forget`. Everything else is read-only — which will
  stop being true when the probe panel arrives, since a re-enumeration probe
  cycles a real port.

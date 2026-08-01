---
name: screenshot-app
description: Launch usbdiag-gui and capture a PNG of its window for visual UI verification on a GNOME/Wayland host. Use when asked to run the GUI, see it, or confirm a UI change actually renders.
---

# Screenshot the running app

`usbdiag-gui` is a native GTK4/libadwaita app. To *see* a UI change (not just
pass tests), launch it and screenshot the window. On this GNOME/Wayland host the
obvious screenshot paths fail, so use the committed recipe below.

## Quick path

```sh
cargo build --bin usbdiag-gui
.claude/skills/screenshot-app/capture.sh /tmp/usbdiag.png
```

Then **look at** `/tmp/usbdiag.png` (Read it). A black or blank frame means the
capture failed — see the gotchas.

`capture.sh` kills any running instance, relaunches under a capturable
configuration, waits for the window, lets the first udev tick land
(`SETTLE=2s`), and writes the PNG.

Env: `USBDIAG_BIN` (default `./target/debug/usbdiag-gui`), `SETTLE`,
`WIDTH`/`HEIGHT` (passed to the binary as `--width`/`--height`),
`SCHEME=light|dark`.

## The two things worth shooting

- **Both colour schemes.** Every colour in `style.css` is a libadwaita named
  colour precisely so the app follows the desktop; that only stays true if both
  are looked at. `SCHEME=dark` / `SCHEME=light` sets
  `ADW_DEBUG_COLOR_SCHEME` for the launched process alone.
- **Both widths.** Below ~500 sp an `AdwBreakpoint` collapses the split view,
  and the chain widget transposes from four columns to four rows on its own
  (that switch is made in `chain.rs` from the allocated width, not from the
  breakpoint). `WIDTH=460 HEIGHT=820` is the narrow shape.

```sh
SCHEME=dark  .claude/skills/screenshot-app/capture.sh /tmp/usbdiag-dark.png
WIDTH=460 HEIGHT=820 .claude/skills/screenshot-app/capture.sh /tmp/usbdiag-narrow.png
```

## Why the dance (do not "simplify" it away)

- **GNOME D-Bus screenshot is locked down.** `org.gnome.Shell.Screenshot`
  `Screenshot` / `ScreenshotWindow` return `AccessDenied` for non-shell callers
  on GNOME 45+. Do not rely on them.
- **GL renderer grabs as black.** A GTK4 window rendered via GL over Xwayland
  captures as a solid black rectangle. Forcing `GSK_RENDERER=cairo` renders into
  a normal X drawable that `xwd` can read. `GDK_BACKEND=x11` makes the window an
  Xwayland client in the first place.
- **`xwd -id`, not `x11grab`.** The window manager often places the window partly
  off-screen (negative origin), which makes `ffmpeg -f x11grab -i :0+X,Y` fail
  with `Permission denied`. `xwd -id <window-id>` grabs the window's own pixmap
  regardless of position; convert the `.xwd` to PNG with `ffmpeg -i in.xwd out.png`.
- **Kill by exact name.** Use `pkill -x usbdiag-gui`. Never `pkill -f
  target/debug/usbdiag-gui` — the pattern also matches the wrapping shell's own
  command line and kills it (silent, confusing failure).

## Notes

- The app reads real hardware, so what it shows depends on what is plugged in.
  A capture with the charger attached and one without are different pictures,
  and the port pane is the one that changes.
- `usbdiag-gui` needs no privileges; do not run it under `sudo` to make a probe
  work. v1 has no probes.
- `cairo` + `x11` is a *capture-only* configuration. For normal interactive use
  just run `cargo run --bin usbdiag-gui` (Wayland-native, GL).
- Tools required: `xwininfo`, `xwd`, `ffmpeg`. There is no `xdotool` on this
  host, which is why the window size is an argument to the binary rather than
  something the script does to an already-mapped window.
- Save anything worth keeping under `captures/claude/<date>_<session>/` — that
  directory is gitignored and survives a reboot, `/tmp` does not.

#!/usr/bin/env python3
"""Drive the Tempo Hours window over the X11/Xwayland backend via the XTEST
extension. No root required.

The app must be running under the X11 backend (GDK_BACKEND=x11) — e.g. launched
by the `screenshot-app` skill's capture.sh, which also uses GSK_RENDERER=cairo so
the same window can be screenshotted to verify what an interaction did.

Coordinates are WINDOW-RELATIVE (0,0 = top-left of the window, title bar
included); the driver adds the window's on-screen origin itself.

Usage:
    interact.py "<WM_NAME>" <command> [args] [<command> [args] ...]

Commands:
    click X Y        left-click at window-relative (X, Y)
    move  X Y        move the window's top-left to screen (X, Y) — use when the
                     window manager parked it partly off-screen
    key   NAME       tap a named key (X keysym: Return, Escape, Tab, Left,
                     Right, BackSpace, Delete, ...)
    type  TEXT       type a string (printable ASCII; handles shifted chars)
    sleep SECS       pause (float ok) — let async loads / animations settle

Example:
    interact.py "Tempo Hours" click 793 78 sleep 1 type "TM-5789" sleep 1 key Escape
"""
import sys
import time

from Xlib import X, XK, display
from Xlib.ext import xtest
from Xlib.protocol import event as Xevent

# Shifted symbol -> the keysym name of the physical (unshifted) key.
SHIFTED = {
    "~": "grave", "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6",
    "&": "7", "*": "8", "(": "9", ")": "0", "_": "minus", "+": "equal",
    "{": "bracketleft", "}": "bracketright", "|": "backslash", ":": "semicolon",
    '"': "apostrophe", "<": "comma", ">": "period", "?": "slash",
}
# Unshifted punctuation -> keysym name.
NAMED = {
    "`": "grave", "-": "minus", "=": "equal", "[": "bracketleft",
    "]": "bracketright", "\\": "backslash", ";": "semicolon", "'": "apostrophe",
    ",": "comma", ".": "period", "/": "slash", " ": "space", "\t": "Tab",
    "\n": "Return",
}


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)

    wm_name = sys.argv[1]
    dpy = display.Display()
    root = dpy.screen().root
    shift_kc = dpy.keysym_to_keycode(XK.string_to_keysym("Shift_L"))

    win = _find(root, wm_name)
    if win is None:
        print("window not found:", wm_name)
        sys.exit(1)

    origin = root.translate_coords(win, 0, 0)
    ox, oy = origin.x, origin.y
    geo = win.get_geometry()
    print(f"window '{wm_name}': origin=({ox},{oy}) size={geo.width}x{geo.height}")

    # Raise + activate so clicks land on this window and keys are delivered to it.
    win.configure(stack_mode=X.Above)
    _activate(dpy, root, win)
    dpy.sync()
    time.sleep(0.3)

    def tap(keycode, shift=False):
        if not keycode:
            return
        if shift:
            xtest.fake_input(dpy, X.KeyPress, shift_kc)
        xtest.fake_input(dpy, X.KeyPress, keycode)
        xtest.fake_input(dpy, X.KeyRelease, keycode)
        if shift:
            xtest.fake_input(dpy, X.KeyRelease, shift_kc)
        dpy.sync()
        time.sleep(0.02)

    args = sys.argv[2:]
    i = 0
    while i < len(args):
        cmd = args[i]
        if cmd == "click":
            rx, ry = int(args[i + 1]), int(args[i + 2])
            i += 3
            ax, ay = ox + rx, oy + ry
            xtest.fake_input(dpy, X.MotionNotify, x=ax, y=ay, root=root)
            dpy.sync(); time.sleep(0.12)
            xtest.fake_input(dpy, X.ButtonPress, 1)
            dpy.sync(); time.sleep(0.05)
            xtest.fake_input(dpy, X.ButtonRelease, 1)
            dpy.sync(); time.sleep(0.3)
            print(f"click ({rx},{ry}) -> screen ({ax},{ay})")
        elif cmd == "move":
            x, y = int(args[i + 1]), int(args[i + 2])
            i += 3
            _move(dpy, root, win, x, y)
            origin = root.translate_coords(win, 0, 0)
            ox, oy = origin.x, origin.y
            print(f"move -> ({x},{y}); origin now ({ox},{oy})")
        elif cmd == "key":
            name = args[i + 1]
            i += 2
            kc = dpy.keysym_to_keycode(XK.string_to_keysym(name))
            tap(kc)
            print(f"key {name}")
        elif cmd == "type":
            text = args[i + 1]
            i += 2
            for ch in text:
                kc, shift = _char_key(dpy, ch)
                tap(kc, shift)
            print(f"type {text!r}")
        elif cmd == "sleep":
            secs = float(args[i + 1])
            i += 2
            time.sleep(secs)
            print(f"sleep {secs}")
        else:
            print("unknown command:", cmd)
            sys.exit(2)

    dpy.sync()
    print("done")


def _find(win, name):
    try:
        if win.get_wm_name() == name:
            return win
    except Exception:
        pass
    try:
        children = win.query_tree().children
    except Exception:
        children = []
    for c in children:
        r = _find(c, name)
        if r:
            return r
    return None


def _activate(dpy, root, win):
    atom = dpy.intern_atom("_NET_ACTIVE_WINDOW")
    ce = Xevent.ClientMessage(
        window=win, client_type=atom, data=(32, [1, X.CurrentTime, 0, 0, 0])
    )
    root.send_event(ce, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)


def _move(dpy, root, win, x, y):
    # EWMH _NET_MOVERESIZE_WINDOW: flags = x/y present (bits 8,9) + app source
    # (bit 12); gravity 0.
    atom = dpy.intern_atom("_NET_MOVERESIZE_WINDOW")
    flags = (1 << 8) | (1 << 9) | (1 << 12)
    ce = Xevent.ClientMessage(
        window=win, client_type=atom, data=(32, [flags, x, y, 0, 0])
    )
    root.send_event(ce, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
    dpy.sync()
    time.sleep(0.2)


def _char_key(dpy, ch):
    """(keycode, shift?) for a printable character."""
    if ch.isalpha():
        return dpy.keysym_to_keycode(XK.string_to_keysym(ch.lower())), ch.isupper()
    if ch in SHIFTED:
        return dpy.keysym_to_keycode(XK.string_to_keysym(SHIFTED[ch])), True
    if ch in NAMED:
        return dpy.keysym_to_keycode(XK.string_to_keysym(NAMED[ch])), False
    if ch.isdigit():
        return dpy.keysym_to_keycode(XK.string_to_keysym(ch)), False
    return dpy.keysym_to_keycode(XK.string_to_keysym(ch)), False


if __name__ == "__main__":
    main()

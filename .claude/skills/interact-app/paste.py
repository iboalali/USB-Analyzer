#!/usr/bin/env python
"""Paste text into the running app from the real X clipboard.

Companion to interact.py, which types character by character. That is not the same
event as a paste, and the app tells them apart on purpose: the work-item field only
turns a pasted Jira URL into its key when the text arrives *whole* (typing one
passes through prefixes that are already valid keys). So a paste has to be a real
paste — Ctrl+V against a clipboard someone owns.

This host has no xclip/xsel/wl-copy and interact.py sends no modifier combos, so
both halves live here: the main thread owns the CLIPBOARD selection and answers
requests for it, a worker thread activates the window and sends Ctrl+V.

    paste.py "<text>" "<window name>" [X Y]

X Y (window-relative, as measured off a screenshot) clicks first, to put the caret
where the paste should land. Omit them when the target already has focus.

Same prerequisites as interact.py: the app running under the X11 backend
(GSK_RENDERER=cairo GDK_BACKEND=x11) and the venv from setup.sh:

    .claude/skills/interact-app/.venv/bin/python .claude/skills/interact-app/paste.py \
        "https://acme.atlassian.net/browse/TM-1234" "Log time" 400 164
"""

import sys
import threading
import time

from Xlib import X, XK, Xatom, display
from Xlib.ext import xtest
from Xlib.protocol import event

USAGE = 'usage: paste.py "<text>" "<window name>" [X Y]'

if len(sys.argv) not in (3, 5):
    sys.exit(USAGE)
TEXT, WIN_NAME = sys.argv[1], sys.argv[2]
CLICK = (int(sys.argv[3]), int(sys.argv[4])) if len(sys.argv) == 5 else None

# How long to keep answering clipboard requests after the paste. GTK reads the
# selection asynchronously, so the owner has to outlive the keystroke.
SERVE_SECONDS = 8

d = display.Display()
CLIPBOARD = d.intern_atom("CLIPBOARD")
UTF8 = d.intern_atom("UTF8_STRING")
TARGETS = d.intern_atom("TARGETS")

owner = d.screen().root.create_window(0, 0, 1, 1, 0, d.screen().root_depth)
owner.set_selection_owner(CLIPBOARD, X.CurrentTime)
d.sync()
print("clipboard owned:", TEXT)


def find(win, name):
    for child in win.query_tree().children:
        try:
            if child.get_wm_name() == name:
                return child
        except Exception:
            pass
        found = find(child, name)
        if found:
            return found
    return None


def send_paste():
    time.sleep(0.6)
    dpy = display.Display()
    root = dpy.screen().root
    win = find(root, WIN_NAME)
    if win is None:
        print("window not found:", WIN_NAME)
        return
    # _NET_ACTIVE_WINDOW, the same way interact.py activates: set_input_focus alone
    # leaves the keystroke going nowhere.
    root.send_event(
        event.ClientMessage(
            window=win,
            client_type=dpy.intern_atom("_NET_ACTIVE_WINDOW"),
            data=(32, [1, X.CurrentTime, 0, 0, 0]),
        ),
        event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask,
    )
    dpy.sync()
    time.sleep(0.4)

    if CLICK:
        # Window-relative in, absolute out. Get this right: a click that lands
        # *outside* the window hands keyboard focus to whatever is under it, and
        # XTEST cannot take it back on Xwayland — every later keystroke silently
        # goes nowhere until the app is restarted.
        at = root.translate_coords(win, CLICK[0], CLICK[1])
        xtest.fake_input(dpy, X.MotionNotify, x=at.x, y=at.y)
        xtest.fake_input(dpy, X.ButtonPress, 1)
        xtest.fake_input(dpy, X.ButtonRelease, 1)
        dpy.sync()
        print(f"clicked ({CLICK[0]},{CLICK[1]}) -> screen ({at.x},{at.y})")
        time.sleep(0.5)

    ctrl = dpy.keysym_to_keycode(XK.string_to_keysym("Control_L"))
    v = dpy.keysym_to_keycode(XK.string_to_keysym("v"))
    for kind, code in (
        (X.KeyPress, ctrl),
        (X.KeyPress, v),
        (X.KeyRelease, v),
        (X.KeyRelease, ctrl),
    ):
        xtest.fake_input(dpy, kind, code)
    dpy.sync()
    print("ctrl+v sent")


threading.Thread(target=send_paste, daemon=True).start()

# Serve the selection. GTK asks for TARGETS first, then the content — and it also
# asks once when ownership changes, before the paste, so more than one request per
# run is normal.
deadline = time.time() + SERVE_SECONDS
while time.time() < deadline:
    if d.pending_events() == 0:
        time.sleep(0.05)
        continue
    e = d.next_event()
    if e.type != X.SelectionRequest:
        continue
    prop = e.property
    if e.target == TARGETS:
        e.requestor.change_property(prop, Xatom.ATOM, 32, [TARGETS, UTF8, Xatom.STRING])
    elif e.target in (UTF8, Xatom.STRING):
        e.requestor.change_property(prop, e.target, 8, TEXT.encode())
    else:
        prop = 0  # refuse anything else
    e.requestor.send_event(
        event.SelectionNotify(
            time=e.time,
            requestor=e.requestor,
            selection=e.selection,
            target=e.target,
            property=prop,
        ),
        event_mask=0,
    )
    d.sync()
    print("served target", e.target)

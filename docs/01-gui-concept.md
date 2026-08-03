# GUI concept

A native GTK4 front end for `usb-probe`, in the shape of
[TempoUI-for-Linux](https://github.com/iboalali/TempoUI-for-Linux) — the same
workspace split, the same toolkit versions, the same Relm4 idioms.

This document is the design; the reasoning is here rather than in commit messages
because most of it is about what the UI must *not* do.

---

## 1. Scope of v1 — **built**

**Viewer only.** Device tree, detail pane, findings, and the bottleneck chain,
updating live from udev. No probes, no privilege, no consent dialogs, no
`pkexec`. The window opens and is useful with zero privileges.

Probes and the substitution workflow are designed for (§9) and deliberately not
built yet: the layout and the chain widget are the parts worth proving first, and
they carry no privilege risk while being proven.

Everything below is now describing shipped code rather than a plan. Where
building it changed a decision, the section says so.

---

## 2. Shape

| | TempoUI | usb-analyzer |
|---|---|---|
| logic, no GTK, all tests | `crates/core` | **`crates/usb-probe`** — 268 tests |
| thin GTK layer, no tests | `crates/ui` | **`crates/gui`** → binary `usbdiag-gui` |
| CLI | — | `crates/usbdiag` — unchanged |

Versions in lockstep, matching TempoUI exactly: `gtk4` 0.11 (`v4_14`),
`libadwaita` 0.9 (`v1_5`), `relm4` 0.11 (`libadwaita` feature). The development
machine has GTK 4.14.5 and libadwaita 1.5.0, so the Ubuntu 24.04 baseline holds
with no newer APIs.

`usb-probe` gains **no** dependency from this. Its 13-crate tree is a stated
design property that decided the shape of two probes; the GUI crate carries all
the weight.

Application ID `com.iboalali.usbdiag`, binary `usbdiag-gui`, CLI stays `usbdiag`.

---

## 3. Reading is in-process; probing is not

These are different problems and get different answers.

**Reading: link the library.** `usb_probe::capture()` on a worker thread. Typed
data, no subprocess per refresh, no JSON round trip, no parsing. Reading needs no
privilege, so the GUI process stays unprivileged — which is the whole point.

**Probing: `pkexec usbdiag probe … --json`.** Only when §9 arrives. One polkit
prompt showing the exact command — a better consent surface than anything drawn
by hand — and the safety gate stays inside the privileged process where a front
end cannot skip it. This is what the JSON API exists for.

Running the whole GUI as root to reach the probes would be the obvious shortcut
and is not acceptable for a window that sits open on a desktop.

---

## 4. Three ways this differs from TempoUI

**The core is synchronous and must stay dependency-thin.** `tempo-core` is async
`reqwest` + `tokio`. A capture here is cheap sysfs reads plus one `journalctl`
spawn, and that spawn is the expensive part — it is why `watch` caches the log at
all. So captures go off the GTK thread with `relm4::spawn_blocking`, and
`usb-probe` stays sync and free of `tokio`.

Reuse the log cache policy already worked out in the CLI's watch loop: hand the
previous `KernelLog` back to `capture_with_log` unless a real event arrived or
10 s have passed. Re-reading it every frame would spawn a process every frame.

**The event source is a blocking child process, not a socket.**
`monitor::Monitor` spawns `udevadm monitor --udev` and blocks on reads. That
becomes a Relm4 **Worker** owning the thread and forwarding events. The debounce
already exists — `wait_for_change(fallback, quiet 250 ms, max 1500 ms)` — so the
Worker wraps it. Do not reimplement debouncing in the UI; a device in a reset
loop must not be able to hold the display still, and that logic is already
correct.

`Snapshot::fingerprint()` exists for exactly this and should gate rebuilds: it
excludes time, I/O counters and sub-0.5 W battery drift, so the view stays still
while nothing is happening.

**No network, no keyring, no OAuth.** Whole categories of TempoUI's complexity
are absent. What replaces them is privilege, and v1 has none of it.

---

## 5. Presentation: adaptive from the start

WhatCable runs as a menu-bar popover *and* as an ordinary windowed app
(Settings, or `--desktop`). The same content therefore has to work at popover
width and at window width, and that is a constraint worth adopting: a small
window kept open while cables are plugged and unplugged is exactly how this tool
gets used.

So the layout is adaptive from the first commit — `AdwBreakpoint` collapsing
`AdwNavigationSplitView` to a single pane below ~500 px. Retrofitting that later
means rewriting every container.

GNOME has no menu-bar equivalent worth targeting, so there is no tray mode. The
compact presentation is just a narrow window.

Both shapes are drawn in [`mockups/`](mockups/) — HTML, real data from this
machine except where a badge says otherwise. Five things the ASCII sketch below
could not settle, and the drawings did:

- **Host findings need a subject row.** `Subject::Host` had nowhere to live in a
  sidebar organised by port and device. It becomes *System · This machine*, at
  the top, and on this laptop it is the only red row.
- **Sidebar dots carry their sentence.** §8 forbids a coloured dot without text.
  A tree of bare dots breaks that rule while looking perfectly normal, so every
  row gets a one-line reason under the name.
- **The chain transposes below the breakpoint** rather than shrinking — four
  labelled rows instead of four columns. Same widget, same data, rotated.
- **The bars stay linear.** 480 Mbps beside 10 Gbps is a 5 % sliver that reads
  as a rendering bug. It is not one; a log scale would flatter a USB 2 fallback.
- **A tray popover is a second information architecture**, not a narrow window:
  verdict first, no tree, no detail pane. That is a second view model to build
  and keep true. Ship the narrow window; take the popover only where a tray host
  exists and someone asks for it.


**Findings lead, the chain supports.** Reversed after
[`02-prior-art.md`](02-prior-art.md): the chain is what three other projects
already draw, and the findings are what none of them has. The detail pane runs
verdict → ruled out → findings → evidence → what cannot be answered, and the
chain appears under a heading that says it supports a statement above rather
than being the statement. Task #24 landed the data, so the mockups show real
verdict output.

**Ports lead, devices follow.** WhatCable is organised around the cable as the
subject, which matches the question people actually ask; this tool has been
organised around devices and findings. `Subject::Cable(port)` already exists in
the model, so the reordering costs nothing.

**Hubs collapse by default**, behind a `via N hubs` label, with a control to
reveal them. Showing every hub buries the device the user came for.

---

## 6. Files

Flat, like `crates/ui/src`:

```
main.rs      AppModel — report, selection, live flag, capture scheduling
monitor.rs   Worker wrapping monitor::Monitor
sidebar.rs   ports + device tree, severity dot per row, hub collapsing
detail.rs    the selected subject: verdict, ruled out, findings, chain, silence
chain.rs     draws a usb_probe::chain::Chain — cairo DrawingArea
findings.rs  finding rows, and the enum → CSS vocabulary
probes.rs    the probe catalogue on the host pane; displays blockers, runs nothing
style.css
```

No tests in this crate. Real logic belongs in `usb-probe`, same rule as
`tempo-core` — which is why **deriving** the chain went there
(`usb-probe/src/chain.rs`, 20 tests) and only **drawing** it stayed here. See §7.

---

## 7. The chain widget

A cairo `DrawingArea`, the same call `timeline.rs` makes. Boxes and labels cannot
do the proportional bars, the arrows and the "▲ the limit" marker without
fighting the layout engine, and the codebase already establishes cairo as the
answer for that.

Two chains, one widget:

- **power** — charger offer → cable current rating → contract → sink capability
- **data** — device capability → cable data rating → port capability → link

**Which stage is marked comes from the findings, never from logic in the
widget.** `Subject::Cable(port)` and codes like `CABLE_CURRENT_LIMIT`,
`PD_CONTRACT_BELOW_OFFER`, `CABLE_DATA_LIMIT` already name the culprit. A stage
is highlighted because a finding points at it. Any diagnosis living in
`chain.rs` would be a second rule engine, and it would drift from the first.

That rule is a `code → (chain, stage)` table, and a table deserves tests — so it
lives in `usb_probe::chain` with the stage derivation, and the GUI file is only a
draw function. Two properties are asserted rather than remembered: a power code
cannot mark a data stage even though both chains have a stage called *cable*, and
an Info finding never aims the marker at anything (Info means worth knowing, and
the "▲ the limit" marker is an accusation).

**A stage with no number is the normal case.** On UCSI the cable stage is
unknowable; `bcdUSB` names a specification rather than a rate, so a device's own
claim usually has no figure either. Both are `None`, and `None` is drawn as a
dashed outline — never as an empty bar, which reads as zero and is a far stronger
claim than "not reported". The card says how many stages are dashed and why.

The data chain's one genuinely measured ceiling is **path allows**: the narrowest
negotiated hop between the device and its root hub. A USB 3 drive behind a USB 2
hub is exactly that stage, and it names which hop.

---

## 8. The honesty rules, as pixels

This is the constraint to hold hardest, because a GUI makes it trivial to break.

- **Confidence on every finding**, never collapsed into a colour.
- **Severity colours the row, but a coloured dot never appears without its
  sentence.** A red mark with no text is an accusation with no evidence.
- **"might be defective" never becomes "is".**
- **`Heuristic` is a different kind of card, not a lower severity.** WhatCable's
  hedged orange "this looks unusual" trust signals are the right model: a visual
  class for suspicion, kept apart from the scale of severity.
- **Silence is not an answer.** Where a rule declines to fire for a good reason —
  unknown medium on a High-Speed link, where a cheap flash drive and a bad cable
  are indistinguishable — the detail pane says so. See task #24: the exonerating
  cases need to exist as data before they can be rendered.
- **Live faults surface as they happen.** An `AdwBanner` when an over-current or
  a drop arrives while the window is open. The tool is watching; it should react,
  not merely refresh.
- **A device's kind shows where it came from.** Three sources, three
  treatments: *asserted* by the class code, *guessed* from a product string, or
  *set by you*. The override is editable in place, in a *What this is* card that
  also states what the device itself claimed and which label is being applied —
  a stored override the user cannot see is a belief they cannot correct, and it
  will outlive their memory of setting it. Built in #26/#27; the guessed source
  exists in the model and nothing produces one yet.

---

## 9. Out of v1, designed for

- ~~**Probe panel.**~~ **Built** — `crates/gui/src/probes.rs`, on the host pane,
  and the prediction held: `Snapshot::capabilities` was already in the model and
  already read by `detail.rs`, so the panel asks `Capabilities::blocker` and
  displays the answer, knowing nothing about capabilities itself. It went on the
  host pane rather than into a new view because what the tool is permitted to do
  is a property of the machine, and the reason is dimmed inline rather than put
  in a tooltip — a tooltip on a row that cannot be clicked is a reason nobody
  will find. It runs nothing and says so; that waits on the item below.
- **`pkexec` escalation**, with `--dry-run --json` populating the confirmation
  dialog from `side_effects`, and structured refusals rendered from `code` +
  `message` rather than parsed from prose.
- **The substitution workflow.** The tool's own advice is repeatedly "swap the
  cable and measure again", and it is the one thing the CLI structurally cannot
  help with. Baseline → prompt → wait for the udev event → capture → diff →
  verdict. This is the strongest reason for the GUI to exist, and it is
  deliberately last, because it depends on everything above.

---

## 10. Packaging

Follow TempoUI's pattern — `data/com.iboalali.usbdiag.{desktop,metainfo.xml}`,
`scripts/install-local.sh`, CI on 24.04 and 26.04 — with one deviation.

**Flatpak is probably the wrong primary target.** A sandbox cannot read
`/sys/kernel/debug`, and full `/sys/bus/usb` traversal needs `--filesystem=host`
or `--device=all`, at which point the sandbox is decorative. For a hardware
diagnostic the native install script should be the primary path; Flatpak stays
optional and, if it ships, must say plainly which probes it cannot reach.

Copy the `screenshot-app` and `interact-app` skills from TempoUI. Its `CLAUDE.md`
makes them the way a UI change gets verified, and the chain widget in particular
needs looking at rather than assuming.

---

## 11. Open questions

- ~~Whether `usbdiag-gui` should offer a `--desktop`-style compact mode
  explicitly, or simply let the window be resized into the narrow breakpoint.~~
  Settled by the mockups: no mode. The narrow window is the same widget tree
  under a breakpoint, and it costs nothing. A tray popover would be a genuinely
  different view — that is the thing to say no to, not the narrow width.
- ~~Whether to bundle `usb.ids` for vendor names where a device exposes no
  strings.~~ Settled: read the system copy, don't bundle anything. See
  [`02-prior-art.md`](02-prior-art.md) and task #25. Most devices do carry
  usable strings — but a cable e-marker never does, so this is the difference
  between naming a cable and printing its VID in hex.
- ~~Whether a `clear` verdict with an empty `because` should look different from
  a cited one.~~ Settled by building it: it does. An uncited clear gets a hollow
  dot and a plain fact in the sidebar rather than its headline, because saying
  "Nothing wrong found" about a subject no rule examined is a claim the data does
  not support. §8's honesty rules cut in the reassuring direction too.

---

## 12. What building it changed

Three things, each found by running the app rather than by reading the code.

**An empty port advertised 4.5 W.** `TypecPort::typec_advertised_ceiling_mw()`
reports what the CC resistors offer, which exists whether or not anything is
plugged in — so every unused socket read *"4.5 W in"*. Power figures on a port row
are now suppressed unless something is attached.

**The transposed chain drew four rows into the height of two.**
`GtkDrawingArea::resize` is emitted from inside `size_allocate`, and changing a
size request there queues a resize *during* allocation, which GTK drops on the
floor. The widget kept the height of the layout it was no longer using. Deferring
the change to an idle callback fixes it, and the comment in `chain.rs` says why so
nobody simplifies it back.

**Sidebar reasons need two lines.** §5 said one, from the mockups. But the
sentence is a finding's title quoted verbatim, and those are written for a list
that names the subject separately — so they open with the device's own name and
run past a single line. Truncating a quote is worse than spending a second line on
it, and uneven row heights turn out to read fine: a row with more to say is
bigger.

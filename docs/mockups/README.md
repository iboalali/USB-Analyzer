# GUI mockups

HTML standing in for GTK. These are drawings, not a prototype — no widget here
maps to a `gtk::Widget`, and nothing in this directory is compiled or shipped.
They exist because [`../01-gui-concept.md`](../01-gui-concept.md) makes claims
about layout that prose cannot settle, and because it is cheaper to be wrong in
CSS than in Relm4.

| file | what it shows |
|---|---|
| `standalone.html` | the wide window, **real data** captured from this machine |
| `standalone-fault.html` | the wide window with something actually wrong — **synthetic** |
| `compact.html` | narrow window and tray popover, side by side, real data |
| `mockup.css` | libadwaita 1.5 palette, Ubuntu 24.04 font stack |
| `shoot.sh` | renders all seven PNGs through headless Chrome |

Open any of them in a browser. Append `#dark` for the dark palette, `#scroll`
to see the lower half of the fault page, `#dark-scroll` for both.

## Real versus synthetic

`standalone.html` and `compact.html` show what `usbdiag` prints on the
development laptop right now: the same two Type-C ports, the same six devices,
the same five findings, the same 100 W contract. Numbers were copied from
`usbdiag --json`, not invented, so the layout is being tested against the
awkward real thing — a 30-character device name, a bus with nothing on it, a
finding whose subject is the host rather than any device.

`standalone-fault.html` is **synthetic** and says so in a badge above the
window. Nothing on this machine is faulty, and the one widget that most needs
review — the chain with a stage marked as the limit — cannot be drawn from a
healthy capture. Its finding codes (`CABLE_CURRENT_LIMIT`,
`PD_CONTRACT_BELOW_OFFER`, `SS_HALF_IDLE`) are real ones the rule engine can
emit; the hardware is not.

## What drawing them settled

- **Host-level findings need a subject row.** `Subject::Host` exists in the
  model and had nowhere to live in a sidebar organised by port and device. It
  became a *System · This machine* row at the top, and it is the row that is red
  on this laptop.
- **Every dot carries its sentence in the sidebar.** The honesty rule said a
  coloured dot never appears without text; a tree of bare dots would have
  broken it while looking fine. Each row gets a one-line reason under the name.
- **The chain rotates rather than shrinks.** Four stages side by side stop
  being legible around 500 px, so below the breakpoint the chain becomes four
  labelled rows. Same data, same widget, transposed.
- **Keep the bars linear.** 480 Mbps next to 10 Gbps is a 5 % sliver, and it
  looks broken. It is not broken — that is what a USB 2 fallback costs, and a
  log scale would flatter it.
- **The tray popover is a second information architecture, not a narrow
  window.** It is verdict-first with no tree and no detail pane, which means a
  second view model to build and maintain. Worth knowing before agreeing to it.

## Regenerating

```sh
docs/mockups/shoot.sh                 # → docs/mockups/shots/
```

Screenshots are kept out of git (`/captures` is ignored); the copies from the
session that produced them are in `captures/claude/2026-08-01_bd64000a/`.

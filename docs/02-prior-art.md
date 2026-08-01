# Prior art

Three tools already answer "what is this USB-C cable" on Linux. All three were
read at the source level on 2026-08-01, not skimmed from their READMEs.

| | [usbeehive](https://github.com/abrauchli/usbeehive) | [nedrichards/whatcable-linux](https://github.com/nedrichards/whatcable-linux) | [Zetaphor/whatcable-linux](https://github.com/Zetaphor/whatcable-linux) |
|---|---|---|---|
| stack | Rust, CLI + D-Bus daemon | Python, GTK4/libadwaita + CLI | C++/Qt, CLI + Plasma applet |
| size | ~10.8 kLOC | ~2.6 kLOC | ~2.0 kLOC |
| licence | MIT | GPL | MIT |
| stars | 151 | 53 | 269 |

All three descend from [darrylmorley/whatcable](https://github.com/darrylmorley/whatcable)
on macOS (8 k stars). `usbeehive` is on crates.io at 0.11.0.

## What they do that this project also does

- **Type-C ports and Power Delivery.** `/sys/class/typec`,
  `/sys/class/usb_power_delivery`, PDO decode, active contract. All three.
- **Cable e-marker decode** where the platform exposes one. All three.
- **A charging bottleneck chain.** `usbeehive` has the closest thing to our
  rule engine: one enum, `Bottleneck { NoCharger, ChargerLimit, CableLimit,
  CableNoEMarker, DeviceLimit, SinkLimit, Fine }`. Zetaphor's
  `ChargingDiagnostic.cpp` is 56 lines and three outcomes.
- **Live updates over udev.** `usbeehive` and Zetaphor.
- **JSON output.** `usbeehive` and nedrichards.
- **A GTK4/libadwaita GUI.** nedrichards, already shipped.

Two of their decisions are worth adopting outright:

- **`usbeehive` bundles a USB-IF vendor database.** That settles open question
  §11.2 in the GUI concept — bundling is normal in this category, not a first
  data file to agonise over.
- **`usbeehive::cable::CableTrust`** flags a probably-counterfeit e-marker from
  three signals: a zero vendor ID in the ID Header VDO, a VID absent from the
  USB-IF database, and reserved bits set in the Cable VDO. Its doc comment says
  none is conclusive and that "UI consumers should render these with a hedged
  tone". That is §8 of the GUI concept, arrived at independently, and we have no
  equivalent signal.

## What none of them does

Verified by grep across all three trees, then by reading the hits:

| | present anywhere? |
|---|---|
| kernel log (`journalctl` / `dmesg` / `/dev/kmsg`) | **no** — the one match is a daemon telling users how to read *its own* logs |
| usbmon / URB error accounting | **no** — nedrichards' "debugfs" is `/sys/kernel/debug/usb/devices`, the device list, not URB capture |
| runtime PM (`power/control`, `active_duration`, `urbnum`) | **no**, in any of the three |
| USB 2.0 companion-port correlation | **no** — `usbeehive`'s "companion" means the paired PD port, a different concept |
| Billboard alt-mode failure | **no** — all three have class `0x11` in a name table and stop there |
| DRM connector cross-check | **no** |
| SCSI error counters | **no** |
| battery drain while on mains | **no** |
| any active probing | **no** |
| a privilege or consent model | **no** |
| confidence as a modelled field | **no** — `usbeehive` uses the word in comments only |

So the shape of the overlap is not "they do a bit of what we do". It is: **the
cable-identity half of this project is comprehensively solved, and the
diagnostic half is untouched.** Everything that distinguishes `usb-probe` — the
kernel-log correlation and its stale-event attribution, the runtime-PM
discrimination that stops the tool accusing a fingerprint reader of 35 bad
connections, the companion-port fallback detector, the three privileged probes
and the gate in front of them — has no counterpart in any of them.

## Consequences

**The GUI concept needs re-pointing.** Its headline is the power chain, and the
power chain is the one thing three other projects already draw. A viewer whose
first screen is a chain competes head-on with a shipped GTK4 app and loses on
familiarity. The findings list is the differentiated surface, so it should lead
— which makes task #24 (verdicts and exonerating findings) the gate, not a
nice-to-have. `usbeehive`'s `Bottleneck::Fine` is #24 for charging alone; ours
should be per-subject across every rule.

**Three things to take**, now tracked as tasks #25 and #26.

Not a bundled vendor database — the *system* one. `/usr/share/hwdata/usb.ids` and
`/usr/share/misc/usb.ids` are both present on this machine at 25 627 lines,
shipped by packages any machine with `lsusb` already has. Reading at runtime
cannot go stale, adds no data file, raises no licence question, and is better
data: it names Goodix, where `usbeehive::vendor::lookup(0x27c6)` returns hex.

Then the `CableTrust` heuristics (MIT), which need that database for one of
their three signals. And the device taxonomy — but split, so that
product-string guesses may only ever suppress a finding, never raise one.

**One option worth naming rather than assuming away.** `usbeehive` is MIT, in
Rust, with a D-Bus interface and a stable JSON surface. Contributing the
kernel-log and runtime-PM layers there, instead of maintaining a parallel
reader for the parts that already exist twice, is a real alternative to shipping
another cable viewer. It is not obviously the right call — the probe gate and
the confidence model are structural and would not transplant cleanly — but it
should be a decision, not an oversight.

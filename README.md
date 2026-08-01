# usb-analyzer

USB-C cable and Power Delivery diagnostics for Linux, in Rust.

Answers the two questions that actually come up:

- **"Why is this device slow?"** — did the link negotiate the speed both ends are
  capable of, and if not, what is the bottleneck?
- **"Why is this charging so slowly?"** — what did the charger offer, what did the
  contract actually take, and is the cable the reason for the gap?

```
$ usbdiag
usbdiag · LENOVO 21QRCTO1WW · kernel 6.17.0-1030-oem · typec via typec,typec_ucsi,ucsi_acpi
  kernel log: journalctl (27 usb events)

USB-C PORTS
  port0  left panel (center/left)
    roles          data=host  power=sink  vconn=yes
    usb / rev      usb2 [usb3]    PD 3.0  Type-C 1.0
    attached       Source, PD 3.0, vid 04b4:0012
    contract       20 V at up to 5 A (100 W)  drawing 45 W  [usb_power_delivery]
    cable          passive, type-c, 5 A, max 20 V, USB 3.2 / USB4 Gen 2 (10 Gbps), <10 ns (~1 m)
    port supports  ff01.1 DisplayPort Alt Mode (VESA)

FINDINGS
[MEDIUM] port0 Negotiated only 60 W from a supply offering 100 W
         The source advertised more power than the contract actually took. ...
         → Test with a known 5 A e-marked cable; if the contract jumps, the cable was the limit.
         PD_CONTRACT_BELOW_OFFER · measured
```

## Layout

| Crate | Role |
|---|---|
| `crates/usb-probe` | Library. Reads sysfs, decodes VDOs, runs the rule engine. No I/O beyond reads; no printing. |
| `crates/usbdiag` | CLI. Argument parsing and rendering only. |
| `crates/gui` | GTK4 + libadwaita viewer, binary `usbdiag-gui`. Widgets only; no rules. |

The split is deliberate: `usb-probe` returns a `Report` of plain serializable
data with no borrows and no sysfs handles, so a front end consumes it directly.
The GUI proved that literally — it links the library, holds no logic of its own,
and `usb-probe` gained no dependency from its existence. Its 13-crate tree is a
stated design property that decided the shape of two probes; `gtk4`,
`libadwaita` and `relm4` are dependencies of `crates/gui` alone.

```rust
let report = usb_probe::report(usb_probe::Options::default());
for f in &report.findings {
    println!("[{}] {} — {}", f.severity.label(), f.subject.display(), f.title);
}
```

Every decision lives in the library too, not in the CLI. `probe::plan` returns either a
`Plan` or a `Refusal`; `Refusal::is_recoverable()` says whether to offer a confirmation or
an error; and the mounted-filesystem and input-device refusals cannot be skipped by a
front end that forgets to check them, because the check is inside the gate rather than in
whatever called it.

### The JSON surface is an API

An out-of-process front end gets the same abilities, and the shapes are meant to be
depended on:

```console
$ usbdiag json                                     # snapshot + findings + exonerations + verdicts
$ usbdiag probe --json                             # {capabilities, probes[]}
$ usbdiag probe NAME --dry-run --json              # the Plan, incl. side_effects
$ usbdiag probe NAME --yes --force --json          # run it
```

A refusal is something to branch on rather than to read. It goes to **stdout** with exit
code 2, carrying a stable `code`, a `recoverable` flag, and the details in fields:

```json
{
  "code": "in_use",
  "recoverable": false,
  "message": "refusing to run a disruptive probe on 4-1: sda1 is mounted at /media/backup…",
  "target": "4-1",
  "holds": [{"via": "sda1", "kind": {"kind": "mounted", "where": "/media/backup"}}]
}
```

That report is written by hand rather than derived from the `Refusal` enum. A derived
shape would follow the enum, and the enum exists to make the *decision* clear — its
variants get renamed as the rules sharpen. A wire format's job is to not move when they
do.

`--dry-run` is the step before a confirmation dialog: it decides, emits the plan including
the `side_effects` list, and runs nothing. With `--json` the interactive prompt is skipped
entirely, since it would otherwise hang a front end on a read of a stdin nobody is typing
into.

## The window

```sh
cargo run --bin usbdiag-gui          # or ./scripts/install-local.sh
```

A read-only viewer for the same report, updating live from udev. The sidebar is
the machine, its Type-C ports and the device tree with hubs collapsed behind
*via N hubs*; the detail pane for whatever is selected runs **verdict → ruled out
→ findings → evidence → what cannot be answered**, in that order and for a
reason: the bottleneck chain is the one thing several other tools already draw,
and the findings are what none of them has, so the chain appears last, under a
heading saying it supports a statement above rather than being one.

It needs no privileges and has none of the probes — those stay in `usbdiag
probe`, behind the consent gate, in a process that can be given root without a
window sitting open as root all day.

The honesty rules are the same as the CLI's, drawn rather than printed:
confidence on every finding and never collapsed into a colour; a coloured dot
never without its sentence; a heuristic gets a visually different card, not a
lower severity; a chain stage the platform cannot report is a dashed outline
rather than an empty bar; and a `clear` verdict with nothing to cite reads as a
plain fact and a hollow dot, because "nothing wrong found" about a subject no
rule examined is a stronger claim than the data supports.

The window is adaptive from the first commit — below ~500 sp the split view
collapses to one pane and the chain transposes from four columns into four rows.
`--width` / `--height` open it at a given size, which is how the narrow shape
gets looked at.

## Usage

```
usbdiag [OPTIONS] [COMMAND]

COMMANDS
    all        Ports, topology and findings (default)
    ports      Type-C ports: roles, PD contract, cable e-marker, alt modes
    devices    USB topology, plus storage speed, why, and live throughput
    diag       Findings only
    watch      Re-render on change, driven by uevents
    json       Full snapshot and findings as JSON
    probe      List the active probes; 'probe NAME' runs one

OPTIONS
    -j --json   -v --verbose   --raw-log   --color/--no-color   --interval MS
    --sample MS    measure live storage throughput over this window

PROBE OPTIONS
    --target NAME   scope to one device: a sysfs name (6-1.2) or a disk (sdb)
    --duration MS   how long a sampling probe runs
    --cycles N      how many times 'reenumerate' cycles the port
    -y --yes        consent to a disruptive probe
    --force         accept the interruption without being asked
    --dry-run       decide and print the decision, then stop
```

Exit status is `0` when nothing actionable was found and `1` when there is a
medium-or-worse finding, so it works as a CI or scripting gate.

`watch` is the most useful mode while debugging: run it, plug the cable in, and
watch the PD contract and cable identity appear. It is event-driven — a plug
shows up as soon as udev reports it, not at the next poll — and it repaints only
when the state it is showing actually differs, so the screen stays still while
nothing is happening.

Events come from `udevadm monitor --udev`, which needs no privileges. Where
`udevadm` is missing the tool falls back to polling at `--interval` (default
2000 ms), which is also the fallback refresh when events *are* available, so a
missed event can never wedge the display. The kernel log is re-read on its own
slower cadence, and immediately on any event, because reading it is a process
spawn and everything else is a handful of sysfs reads.

## Active probing

Everything above reads state the kernel has already decided. That is enough for most
questions and it is why the default needs no privileges — but it cannot answer the one
that matters most about a marginal cable, which usually negotiates full speed and only
fails under load. Seeing that means generating the load.

So the tool is split in two, and the split is enforced in code rather than by convention.

| Class | Runs | Needs |
|---|---|---|
| **passive** | always, by default | nothing |
| **privileged, read-only** | when named | root, but changes nothing on the bus |
| **disruptive** | only when asked twice | root, and takes the device off the bus |

```console
$ usbdiag probe                       # what exists, what is ready here, and why not
$ usbdiag probe urb-errors            # named, so it runs — it only reads
$ usbdiag probe reenumerate --target 6-1.2 --yes --force
```

| Probe | Class | What it does |
|---|---|---|
| `snapshot` | passive | The ordinary scan. |
| `storage-sample` | passive | Two reads of `/sys/block/*/stat` over a window. |
| `urb-errors` | privileged read | Counts URB completion errors per device from usbmon. |
| `throughput` | privileged read | Reads a USB disk flat out with direct I/O. |
| `reenumerate` | **disruptive** | Cycles the hub port and records what comes back, ~20 times. |

**Consent is proportional to consequence.** A read-only probe runs when it is named —
naming it *is* the request, and there is nothing to undo afterwards. Requiring a
confirmation flag there would only teach the reflex of typing it, which is exactly what
makes the flag worthless on the probe where it matters. A disruptive probe asks twice, for
two different things: `--yes` to consent at all, then `--force` or a typed target name to
accept the interruption.

**Two refusals that consent cannot lift.** A disruptive probe is refused outright against
a disk holding a mounted filesystem or an active swap area, and against a subtree
containing an input device. The first is about data. The second is not — nothing is
destroyed by cycling a keyboard's port, but it removes the means of stopping whatever
happens next, which is not something anyone can agree to in advance. Everything else that
drops and comes back — unmounted disks, network interfaces that may be carrying the
session — is a warning printed in the confirmation instead.

The mount check resolves the whole stack rather than comparing names, because the
dangerous case looks nothing like the disk it endangers: a LUKS volume on a USB stick
appears in `/proc/self/mounts` as `/dev/mapper/backup`, which shares no substring with
`sdb`. Following `slaves/` links down to the physical disks is the only way to connect the
two. It is read at the moment of the decision, never from the snapshot, since a filesystem
can be mounted in between.

**No ioctl, anywhere.** The obvious way to load a link is usbfs and raw SCSI over
bulk-only transport; the obvious way to reset a device is `USBDEVFS_RESET`. Both need
`ioctl`, which needs libc, and there is no portable pure-Rust substitute — inline assembly
would work on x86_64 and break on aarch64, which is where a USB diagnostic tool earns its
keep. So load is generated by reading the block device, and a port is cycled by writing to
its `disable` attribute. The cost is that neither can touch a device that is not storage.

`O_DIRECT` carries the throughput measurement, and it has a trap worth naming: its value
differs between architectures, and Linux discards `open` flags it does not recognise
**without an error**. A wrong constant does not fail — it silently reads the page cache
and reports gigabytes per second over a USB cable. The value is therefore cfg-gated to
architectures where it is known, anything else is unsupported at any privilege, and it is
then proved at runtime by a deliberately misaligned read, which direct I/O rejects and
buffered I/O serves. No proof, no number.

**What restoring a port cannot promise.** The port is put back by a guard that runs on
every return, every error path and on a panic. It does *not* run if the process is killed,
because handling `SIGINT` needs a signal handler and therefore libc. The window is
150 ms, and the command to re-enable a stuck port is printed *before* anything happens,
since afterwards is precisely when it cannot be.

### What active probing still cannot reach

Root does not unlock the cable. **You can only probe what is addressable, and a passive
cable has no address** — its e-marker answers solely to the port controller, over SOP',
and the PD state machine lives in that controller's firmware where no userspace path
reaches it. `CONFIG_UCSI_DEBUGFS` would allow raw UCSI commands such as
`GET_CABLE_PROPERTY`; it is not set on the kernel this was built against
(6.17.0-1030-oem), so cable interrogation is closed here without a custom kernel build.

Cable probing is therefore always indirect: push traffic or power through it and observe
where it fails. And the list of things no privilege level reaches is unchanged — CC-line
voltages, eye diagrams, jitter, insertion loss.

### What measurement buys, and what it does not

Measured error counts **lift a heuristic finding to inferred, and stop there**. Nothing
reaches `measured` on the strength of them. The counts are measured; blaming the cable for
them is still a deduction, and a cable is only convicted by substitution. The suggestion on
every such finding says so, because it is the actual next step: swap the cable, measure
again, and see whether the number moves.

usbmon's error classes are kept apart for the same reason. Conflating them would produce
exactly the confident false accusation this tool exists to avoid:

- **transport** — `EPROTO`, `EILSEQ`, `EOVERFLOW`, `ETIMEDOUT`. Implicates the wire.
- **protocol** — `EPIPE`. On endpoint 0 this is a device declining a request, which is
  routine; on a data endpoint it is a halt.
- **cancelled** — `ENOENT`, `ECONNRESET`, `ESHUTDOWN`. A webcam stopping its stream
  cancels URBs in bulk. Counting those would condemn every healthy camera on the machine.

The throughput rule is careful in the same way, and for a reason specific to USB.
Comparing achieved throughput against the *link* rate would condemn nearly every healthy
drive — 110 MB/s over a 5 Gbps link that allows 450 is an ordinary flash drive. The media
baseline that would fix that is mostly unavailable, because USB bridges do not implement
the SCSI VPD page that reports whether a disk spins, and the kernel then defaults the flag
to "rotating" for everything. So the yardstick is the slowest thing the medium could
plausibly be: a known platter's ceiling; or, when the medium is unknown on a SuperSpeed
link, only a collapse below what USB 2.0 itself would have delivered. Unknown medium on a
High-Speed link is **not judged at all**, because a cheap flash drive and a bad cable are
indistinguishable at those rates. The measurement is always shown; only the accusation is
withheld.

## Where the data comes from

The default run is read-only and needs no privileges. The three sources below the line are
reached only by `usbdiag probe`.

| Source | What it gives |
|---|---|
| `/sys/bus/usb/devices/*` | Descriptors, the **negotiated** link speed, lane count, requested bus power, driver bindings |
| `/sys/bus/usb/devices/*/*:*/​*-port*` | Per-port state, `connect_type`, and `over_current_count` — a hardware fault counter |
| `/sys/class/typec/*` | Port roles, PD/Type-C revisions, alt modes, the attached partner, and the **cable e-marker** |
| `/sys/bus/thunderbolt/*` | USB4/Thunderbolt routers, domain security, and **active-cable retimers** — a second, independent source of cable identity |
| `/sys/class/usb_power_delivery/*` | PDO lists: what each side *advertises* it can supply or accept |
| `/sys/class/power_supply/*` | The negotiated contract (`voltage_now` x `current_now`), plus batteries and mains |
| `/sys/block/*/stat` | Live storage throughput — two reads and the time between them, no privileges |
| `/dev/kmsg`, else `journalctl -k`, else `dmesg` | Resets, enumeration failures, link-training failures, over-current |
| `udevadm monitor --udev` | Change notification for `watch`. Not data — just a reason to look again |
| `/sys/class/drm/card*-*` | Display outputs: what is plugged in, whether it is being driven, and the monitor's EDID — the independent check on a DisplayPort Alt Mode claim |
| `/proc/self/mounts`, `/proc/swaps` | What is in use, resolved through `slaves/` links down to the physical disk. Not data — the check that stands between a disruptive probe and someone's filesystem |
| **`/sys/kernel/debug/usb/usbmon/0u`** | *root.* Every URB and its completion status — the one source that observes behaviour rather than negotiated state |
| **`/dev/sdX`** | *root.* Read with `O_DIRECT` to put the link under load and measure what it actually carries |
| **`.../usbN-portM/disable`** | *root, disruptive.* Switches a hub port off and on, to see whether the link trains the same way twice |

Capabilities and the live contract are kept strictly apart, because a charging
complaint is almost always the gap between them.

On systems with `kernel.dmesg_restrict=1` (most distributions), `/dev/kmsg`
requires root. The tool falls back to `journalctl -k`, which usually works
unprivileged. If no source is readable, the reset-history rules are skipped and
that is **reported as a finding** rather than silently dropped.

## What it can and cannot know

Findings carry a confidence level, because the honest answer differs by field:

- **`measured`** — read straight off a descriptor, an e-marker, or a hardware
  counter. A cable's 3 A rating and a port's over-current count are facts.
- **`inferred`** — deduced from two measured facts disagreeing. "This device says
  USB 3.2, the port says USB 3.2, the link came up at 480 Mbps" means something in
  between is the limit, and the cable is the usual suspect.
- **`heuristic`** — a symptom pattern that could have another cause, such as a
  reset storm.

Measured URB errors can lift a heuristic finding to inferred. Nothing lifts a cable
finding to measured, at any privilege level, because attributing measured errors to the
cable is still a deduction — see [what measurement buys](#what-measurement-buys-and-what-it-does-not).

**Not possible in software, at all:** eye diagrams, jitter, insertion loss,
CC-line voltages, the true rating of a cable with no e-marker, and sniffing
traffic between two other devices. Those need hardware — a Total Phase Beagle, a
Cynthion/LUNA, or a USB-C PD analyzer. The tool says so in its own output rather
than implying otherwise.

A few limits worth knowing:

- **Cable identity depends on firmware.** `port0-cable` appears only when the
  cable has an e-marker chip *and* the port controller reports SOP' data upward.
  A plain 60 W or USB 2.0 cable has no e-marker to read; that is normal, and is
  reported as `CABLE_NOT_EMARKED` at info level.
- **`power_supply` field names are misleading.** Verified against real UCSI
  hardware: `voltage_now` x `current_now` is the *contract* (20 V x 5 A on a
  100 W charger), while `voltage_min`/`voltage_max`/`current_max` are a
  capability **range**, not limits — one real reading had `voltage_max` = 13.2 V
  while `voltage_now` = 20 V. Treating the `*_max` pair as the contract made a
  full 100 W contract look like 47 W. Nothing in this node measures the current
  actually being drawn, and the tool does not pretend otherwise.
- **A power-limited PPS APDO is not worth its arithmetic.** A 100 W charger
  advertises PPS as 3.3-21 V at 5 A, which multiplies out to 105 W it cannot
  deliver. APDOs flagged `pps_power_limited` are excluded from capability
  comparisons so the fixed PDOs decide the maximum.
- **A socket is not a device.** The kernel log spans the whole boot while the
  device tree is a snapshot, so an event naming `5-1` describes a *location*,
  not whatever occupies it now. Events are dated against each device's attach
  time (`/proc/uptime` minus `power/connected_duration`, compared to monotonic
  log timestamps) and discarded when they predate the current occupant. Without
  this, a phone plugged into a socket a faulty hub had used was reported as
  having a defective cable, on evidence from 20 minutes before it arrived.
- **Power direction changes what the readings mean.** While the machine is
  *sourcing* (charging a watch, powering an accessory), the `ucsi-source-psy`
  node describes power coming *in*, so it reports `online=0` and `current_now=0`
  even while 5 V flows out. The tool reports the advertised limit instead and
  says outgoing draw is not measurable there, rather than claiming nothing is
  happening. The sink-side power rules are gated on direction for the same
  reason.
- **A local port's alt-mode `active` flag means nothing.** `/sys/class/typec/portN/portN.M/active`
  reads `yes` for every mode the *port* supports, whatever is attached — on this
  ThinkPad both ports permanently claim Lenovo, Thunderbolt and DisplayPort modes
  are all active while one holds a charger that reports zero alternate modes.
  Only the **partner's** copy of the flag describes a mode that was entered, so
  the port's list is rendered as "port supports" and no rule reads its `active`.
- **DRM says whether a picture came out; it will not say what mode.**
  `/sys/class/drm/card*-*` gives `connected`, `enabled`, `dpms`, the offered mode
  list and the EDID — enough to check a DisplayPort Alt Mode claim against
  reality. The mode actually being scanned out lives in the atomic KMS state,
  which needs debugfs or a DRM master connection, so the tool never claims it.
  Note too that a sleeping monitor still reads `connected`: attached and being
  driven are reported separately.
- **Type-C ports are not correlated to USB devices unless firmware says so.**
  Only the `connector` symlink is trusted. Matching by `physical_location` is
  tempting but ambiguous in practice — on this ThinkPad four USB receptacles
  share one location descriptor — and a wrong correlation would pin a finding on
  the wrong cable.

### The descriptor trap

A USB 3 device carries **separate descriptor sets** for SuperSpeed and High-Speed
operation. When it falls back to USB 2.0 it presents the USB 2.0 set, reports
`bcdUSB 2.10`, and stops advertising USB 3 — precisely when you want to know.
Verified on one drive, one cable, two sockets:

```
USB-A socket:              version 3.00   speed 5000   bMaxPower 144mA
via a USB 2.0-only adapter: version 2.10   speed  480   bMaxPower 100mA
```

So "claims USB 3 but linked at 480" is structurally unable to catch a real
fallback, and a single snapshot of the fallback state is indistinguishable from a
genuine USB 2.0 device.

The port topology has no such blind spot. One physical receptacle appears as two
logical ports sharing an ACPI `_PLD` location token — a USB 2.0 half and a
SuperSpeed half on different buses. A device on the slow half while the fast half
sits empty means SuperSpeed never trained, whatever the device says about itself.
That is `SS_HALF_IDLE`, and it is restricted to mass storage on purpose: a USB 2.0
keyboard produces identical topology and is entirely normal.

### Defective cable, or just the wrong one?

When the SuperSpeed half of a socket errors while its USB 2.0 half works, the log
answers a question that matters more than "is something wrong":

```
retries → trained once → failed    the SuperSpeed pairs are wired but the contact
                                   is intermittent → the cable is likely DEFECTIVE

retries → never trained            the pairs are not in the path at all
                                   → WRONG cable, nothing is broken
```

A cable lacking SuperSpeed wiring can never reach a trained state even once, so
"trained then failed" is specific to a physical fault. Both were observed on the
same machine: a hub with a loose built-in cable (defective — later confirmed by
its owner) and a USB 2.0-only adapter (wrong cable, working exactly as designed).

The asymmetry is why the symptom looks the way it does. USB 2.0 uses one
differential pair at 480 Mbps with generous margins; SuperSpeed adds two pairs at
multi-gigabit rates where signal integrity is unforgiving. A degrading connector
kills SuperSpeed long before USB 2.0 notices — so "half the device works" is
itself the diagnosis.

`usbdiag` reports this as **might** be defective, never *is*. A dirty contact, a
failing hub controller, or a marginal port produce identical evidence.

## Rules

| Code | Confidence | Catches |
|---|---|---|
| `SS_HALF_FAILED` | inferred | A device on the USB 2.0 half while the SuperSpeed half of the same socket is **erroring**. Distinguishes a defective cable from a merely wrong one (see below). |
| `SS_HALF_IDLE` | heuristic | Storage on the USB 2.0 half of a receptacle whose SuperSpeed half is idle and quiet — the only detector that survives a USB 3 fallback (see below). |
| `DEVICE_FAILED_TO_ENUMERATE` | measured | A device that appears in the kernel log but never in sysfs, located to its physical socket, with the errno decoded. |
| `BILLBOARD_ALT_MODE_FAILED` | measured | A USB-C device presenting a Billboard interface — its own declaration that an Alternate Mode could not be entered. |
| `LINK_BELOW_DEVICE_CAPABILITY` | inferred | A device still reporting USB 3.x while linked at 480 Mbps. Distinguishes a USB 2.0 hub upstream from a suspect cable, and internal devices from cabled ones. Cannot see a true fallback. |
| `LINK_SLOW_DESPITE_CAPABLE_CABLE` | inferred | Slow link where the e-marker already rules the cable out. |
| `LINK_SINGLE_LANE` | measured | USB 3.2 device running one lane instead of two. |
| `CABLE_DATA_LIMIT` | measured | E-marker says USB 2.0 only, so SuperSpeed is impossible. |
| `CABLE_CURRENT_LIMIT` | measured | 3 A cable capping the link at 60 W. |
| `CABLE_VOLTAGE_EXCEEDED` | measured | Contract voltage above the cable's declared rating. |
| `CABLE_NOT_EMARKED` | heuristic | No cable identity available and the rating could matter; capability unknown. |
| `CABLE_EMARKER_NOT_REPORTED` | inferred | Controller reports no e-marker, but a >3 A contract proves the cable is 5 A rated. |
| `PD_NO_CONTRACT` | inferred | PD-capable device attached but no contract in effect. |
| `SINK_UNDERPOWERED_NO_PD` | measured | This machine is drawing 5 V with no PD contract while advertising far higher sink capability — the "why is my laptop barely charging" case. |
| `BATTERY_DRAINING_ON_AC` | measured | Mains present but the pack is not gaining — the supply is not keeping up with the load. |
| `PARTNER_NO_PD` | measured | Attached device speaks no PD at all, so the link is capped at 5 V. Info-level: normal for a watch charger or accessory being powered. |
| `PD_CONTRACT_BELOW_OFFER` | measured | Took much less power than the source offered. |
| `PD_SOURCE_BELOW_SINK_CAPABILITY` | measured | Charger smaller than the port can accept. |
| `DP_ALTMODE_NOT_ACTIVE` | measured | DisplayPort advertised but not engaged. |
| `DP_ALT_MODE_NO_OUTPUT` | inferred | The partner *entered* DisplayPort Alt Mode and the graphics driver still sees nothing on any DisplayPort output — a cable carrying power and USB data but no high-speed pairs. Untested against hardware. |
| `PORT_OVER_CURRENT_COUNT` | measured | The port's current limiter actually fired. |
| `DEVICE_RESET_STORM` | heuristic / measured | Repeated resets. Becomes *measured* when runtime-PM accounting shows autosuspend is the cause. |
| `ACTIVE_CABLE_PRESENT` | measured | Retimers enumerated — cable identity read from the cable's own silicon, independent of PD SOP'. |
| `USB4_LINK_BELOW_CAPABILITY` | inferred | A generation-4 router running single-lane instead of 40 Gbps. |
| `KERNEL_BLAMED_CABLE` | measured | The kernel logged its own bad-cable warning. |
| `ENUMERATION_FAILURE` | measured | Descriptor reads failed or no address accepted. |
| `LINK_TRAINING_FAILURE` | measured | A port could not bring its link up cleanly. |
| `HOST_CONTROLLER_FAILURE` | measured | xHCI controller declared dead. |
| `BUS_OVER_CURRENT`, `BUS_POWER_INSUFFICIENT`, `BUS_BANDWIDTH_INSUFFICIENT` | measured | Power and bandwidth faults from the ring buffer. |
| `KERNEL_LOG_UNAVAILABLE` | measured | Reset history could not be read, so those rules were skipped. |

Findings that only exist once a probe has been run:

| Code | Confidence | Catches |
|---|---|---|
| `LINK_ERROR_RATE` | measured | Transport errors counted from usbmon over a window. High above 1% of completions. Protocol errors and driver cancellations are excluded, not merely weighted. |
| `THROUGHPUT_FAR_BELOW_LINK` | measured | A measured sequential read far below what both the link and the slowest plausible medium allow. Silent where a slow drive would explain it. |
| `STORAGE_READ_FAILED` | measured | A read that began and then died. The drive answered and then stopped answering, which no amount of negotiated state would have revealed. |
| `LINK_INTERMITTENT` | measured | The port was cycled and did not behave the same way twice. High when the device failed to re-appear at all, medium when it merely trained slower. |
| `LINK_STABLE_UNDER_CYCLING` | measured | Info. Every cycle trained identically — which does not clear a cable, but does rule out intermittency, and a deliberate test should say something when it passes. |

### What a device is

`usb_probe::kind` classifies each device from its class codes — hub, storage,
camera, smart-card reader, and so on — because what a device *is* changes what
a reading means: 12 Mbps is correct for a keyboard and a fault for an external
SSD.

The taxonomy is a rule input, so it carries a rule of its own. `Kind::asserted()`
returns a kind only when the device's own descriptors said so, and it is the
only accessor a rule may use as grounds for a **new** finding; the plain kind,
which will later include guesses and user corrections, is for display and for
staying quiet. A wrong guess that suppresses costs a missed detection; a wrong
guess that accuses costs a false accusation against hardware someone then
replaces. Only one of those is recoverable.

Every kind carries where it came from — declared by the device, guessed from its
name, or set by you — and both front ends show it, because a belief the user
cannot see is one they cannot correct.

### Telling it what something is

```sh
usbdiag label 4-1 --medium rotating     # the fact a USB bridge will not report
usbdiag label 3-5.2 --kind smartcard    # correct a class code that shrugged
usbdiag labels                          # what is stored
usbdiag labels 0781:5583 --forget       # and how to take it back
usbdiag --no-overrides                  # the machine as read, ignoring all of it
```

The same control lives in the GUI, in the *What this is* card on any device.

A label is a fact you supply that the bus cannot. The case that justifies the
feature is the medium: `queue/rotational` is meaningless over USB because
bridges omit SCSI VPD page B1h, so on a High-Speed link the throughput rule
cannot tell a slow flash drive from a bad cable and correctly says nothing.
Tell it the disk spins and it has a yardstick.

Unlike a guess the tool made, a label may **sharpen** a finding as well as
quieten one — you are holding the object. It is still not a measurement, so any
finding resting on one is capped at `inferred` and names the label it used.

Scope is the model (`VID:PID`), so correcting one drive corrects every drive of
that model; `--this-one` narrows it to a single unit where the serial is worth
keying on. Placeholders are refused — two of the six serials on this machine are
all zeros, and keying on those would relabel every zero-serial device ever
plugged in.

Nothing generalises: a correction is a stored fact about one identity, replayed
on sight, never mined for patterns. Storage is
`$XDG_CONFIG_HOME/usbdiag/devices.json`, plain JSON, safe to edit by hand, and
written **only** by an explicit command.

### Exonerations

These do not appear in `findings`. They live in a separate `exonerations` list, so that
saying *"this is fine"* can never inflate a fault count, trip the exit code, or be mistaken
for an accusation. All are Info.

| Code | Says |
|---|---|
| `CHARGING_AT_FULL_OFFER` | The contract reached the charger's best profile. Nothing on this port is holding power back; only a bigger charger would help. |
| `CABLE_NOT_LIMITING` | Nothing was lost across the cable. Reachable **without an e-marker**, which matters: on UCSI the cable's identity is never exposed, so arithmetic on the contract is the only way to clear one. |
| `LINK_AT_DEVICE_MAXIMUM` | Linked below the port's rate, but at the device's own ceiling. Restricted to `bcdUSB 3.0x`, the one version that names exactly one rate. |
| `ALT_MODE_NOT_REQUESTED` | DisplayPort is offered and nothing asked for it — normal for a charger, and alarming in a raw capability dump. |
| `MEDIUM_EXPLAINS_THROUGHPUT` | A measured read below the link rate, on media the kernel reports as rotating. The platters are the bottleneck, not the cable. Needs `probe throughput`. |

### Verdicts

One sentence per subject — host, each port, each cable, each non-root-hub device — in a
`verdicts` list, with an `outcome` of `fault`, `minor` or `clear` and the codes it rests on.

**A headline is always a finding's title quoted verbatim, or the fixed string
`"Nothing wrong found"`.** There is no code path that composes a sentence of its own, so a
verdict cannot state anything the findings do not. An empty `because` on a `clear` verdict
is meaningful: the subject was examined and nothing fired either way, which is a weaker
clean bill of health than one with an exoneration to cite.

Info-only subjects are `clear`, not `minor`. Info means *worth knowing, not worth acting
on*, so grading one as minor would manufacture a problem out of a note.

## Build

```sh
cargo build --release -p usbdiag     # ./target/release/usbdiag
cargo test --workspace               # 296 tests
```

The CLI and the library have **no non-Rust dependencies**: `serde` and
`serde_json` only, sysfs read with `std::fs`, no `libusb`.

The GUI is the exception and is kept separate for that reason. It needs the GTK4
and libadwaita development packages, and building the whole workspace needs them
too:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev   # Ubuntu 24.04+
cargo build --release                            # both binaries
./scripts/install-local.sh                       # into ~/.local, no root
```

Flatpak is deliberately not the primary target: a sandbox cannot read
`/sys/kernel/debug`, and a full `/sys/bus/usb` traversal needs
`--filesystem=host` or `--device=all`, at which point the sandbox is decorative.

The VDO bit-field decoders and the rule engine are unit-tested against
hand-constructed values — a 5 A/10 Gbps cable, a 3 A/USB 2.0 cable, PD 2.0 vs
PD 3.0 product-type renumbering — because the interesting diagnostic cases cannot
be produced on demand from real hardware.

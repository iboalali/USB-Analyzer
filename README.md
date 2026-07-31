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
    alt modes      ff01.1 DisplayPort Alt Mode (VESA) active

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

The split is deliberate: `usb-probe` returns a `Report` of plain serializable
data with no borrows and no sysfs handles, so a native UI (egui, iced, GTK) can
consume it directly and `crates/usbdiag/src/render.rs` is the only file a GUI
replaces. `Report` also round-trips through JSON, so an out-of-process front end
works too.

```rust
let report = usb_probe::report(usb_probe::Options::default());
for f in &report.findings {
    println!("[{}] {} — {}", f.severity.label(), f.subject.display(), f.title);
}
```

## Usage

```
usbdiag [OPTIONS] [COMMAND]

COMMANDS
    all        Ports, topology and findings (default)
    ports      Type-C ports: roles, PD contract, cable e-marker, alt modes
    devices    USB topology with the link speed each device negotiated
    diag       Findings only
    watch      Re-render on change — plug a cable in and watch it negotiate
    json       Full snapshot and findings as JSON

OPTIONS
    -j --json   -v --verbose   --raw-log   --color/--no-color   --interval MS
```

Exit status is `0` when nothing actionable was found and `1` when there is a
medium-or-worse finding, so it works as a CI or scripting gate.

`watch` is the most useful mode while debugging: run it, plug the cable in, and
watch the PD contract and cable identity appear.

## Where the data comes from

Everything is read-only. Nothing requires root.

| Source | What it gives |
|---|---|
| `/sys/bus/usb/devices/*` | Descriptors, the **negotiated** link speed, lane count, requested bus power, driver bindings |
| `/sys/bus/usb/devices/*/*:*/​*-port*` | Per-port state, `connect_type`, and `over_current_count` — a hardware fault counter |
| `/sys/class/typec/*` | Port roles, PD/Type-C revisions, alt modes, the attached partner, and the **cable e-marker** |
| `/sys/class/usb_power_delivery/*` | PDO lists: what each side *advertises* it can supply or accept |
| `/sys/class/power_supply/*` | The negotiated contract: `voltage_now` x `current_now` |
| `/dev/kmsg`, else `journalctl -k`, else `dmesg` | Resets, enumeration failures, link-training failures, over-current |

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

**Not possible in software, at all:** eye diagrams, jitter, insertion loss,
CC-line voltages, the true rating of a cable with no e-marker, and sniffing
traffic between two other devices. Those need hardware — a Total Phase Beagle, a
Cynthion/LUNA, or a USB-C PD analyzer. The tool says so in its own output rather
than implying otherwise.

Two limits worth knowing:

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
- **Power direction changes what the readings mean.** While the machine is
  *sourcing* (charging a watch, powering an accessory), the `ucsi-source-psy`
  node describes power coming *in*, so it reports `online=0` and `current_now=0`
  even while 5 V flows out. The tool reports the advertised limit instead and
  says outgoing draw is not measurable there, rather than claiming nothing is
  happening. The sink-side power rules are gated on direction for the same
  reason.
- **Type-C ports are not correlated to USB devices unless firmware says so.**
  Only the `connector` symlink is trusted. Matching by `physical_location` is
  tempting but ambiguous in practice — on this ThinkPad four USB receptacles
  share one location descriptor — and a wrong correlation would pin a finding on
  the wrong cable.

## Rules

| Code | Confidence | Catches |
|---|---|---|
| `LINK_BELOW_DEVICE_CAPABILITY` | inferred | SuperSpeed device stuck on a USB 2.0 link. Distinguishes a USB 2.0 hub upstream from a suspect cable, and internal devices from cabled ones. |
| `LINK_SLOW_DESPITE_CAPABLE_CABLE` | inferred | Slow link where the e-marker already rules the cable out. |
| `LINK_SINGLE_LANE` | measured | USB 3.2 device running one lane instead of two. |
| `CABLE_DATA_LIMIT` | measured | E-marker says USB 2.0 only, so SuperSpeed is impossible. |
| `CABLE_CURRENT_LIMIT` | measured | 3 A cable capping the link at 60 W. |
| `CABLE_VOLTAGE_EXCEEDED` | measured | Contract voltage above the cable's declared rating. |
| `CABLE_NOT_EMARKED` | heuristic | No cable identity available and the rating could matter; capability unknown. |
| `CABLE_EMARKER_NOT_REPORTED` | inferred | Controller reports no e-marker, but a >3 A contract proves the cable is 5 A rated. |
| `PD_NO_CONTRACT` | inferred | PD-capable device attached but no contract in effect. |
| `PARTNER_NO_PD` | measured | Attached device speaks no PD at all, so the link is capped at 5 V. Explains a slow-charging watch charger or accessory. |
| `PD_CONTRACT_BELOW_OFFER` | measured | Took much less power than the source offered. |
| `PD_SOURCE_BELOW_SINK_CAPABILITY` | measured | Charger smaller than the port can accept. |
| `DP_ALTMODE_NOT_ACTIVE` | measured | DisplayPort advertised but not engaged. |
| `PORT_OVER_CURRENT_COUNT` | measured | The port's current limiter actually fired. |
| `DEVICE_RESET_STORM` | heuristic | Repeated resets — marginal connection. Downgraded for internal devices. |
| `KERNEL_BLAMED_CABLE` | measured | The kernel logged its own bad-cable warning. |
| `ENUMERATION_FAILURE` | measured | Descriptor reads failed or no address accepted. |
| `LINK_TRAINING_FAILURE` | measured | A port could not bring its link up cleanly. |
| `HOST_CONTROLLER_FAILURE` | measured | xHCI controller declared dead. |
| `BUS_OVER_CURRENT`, `BUS_POWER_INSUFFICIENT`, `BUS_BANDWIDTH_INSUFFICIENT` | measured | Power and bandwidth faults from the ring buffer. |
| `KERNEL_LOG_UNAVAILABLE` | measured | Reset history could not be read, so those rules were skipped. |

## Build

```sh
cargo build --release        # ./target/release/usbdiag
cargo test                   # 78 tests
```

No non-Rust dependencies. Dependencies are `serde` and `serde_json` only; sysfs
is read with `std::fs`, so `libusb` is not needed.

The VDO bit-field decoders and the rule engine are unit-tested against
hand-constructed values — a 5 A/10 Gbps cable, a 3 A/USB 2.0 cable, PD 2.0 vs
PD 3.0 product-type renumbering — because the interesting diagnostic cases cannot
be produced on demand from real hardware.

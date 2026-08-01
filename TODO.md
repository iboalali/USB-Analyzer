# TODO

Open work, with the reasoning behind each item. Completed work is in the git log.

The design constraint that governs all of it: **the default must stay unprivileged,
read-only and non-invasive.** Anything that needs root, and especially anything that
takes a device off the bus, is opt-in and says so before acting.

---

## Blocked on hardware

### Validate `LINK_BELOW_DEVICE_CAPABILITY`, or delete it

Two of three cases are settled:

- **Healthy SuperSpeed — passed.** MEDION HDDrive-n-GO (`174c:55aa`, ST1000LM024 1 TB)
  on USB-A: `version 3.00`, 5000 Mbps, 1 lane, `bMaxPower 144mA`. No findings, which is
  correct — 5 Gbps is USB 3.0's ceiling, not a downshift. Reproduced on both USB-A
  receptacles.
- **Fallback via a USB 2.0-only adapter — resolved by `SS_HALF_IDLE`.** The same drive
  through a Google Quick Switch A-to-C adapter reports `version 2.10`, 480 Mbps. The
  descriptor check *cannot* fire, because the device stops claiming USB 3 the moment it
  falls back. `SS_HALF_IDLE` catches it from port topology instead, and was verified
  against this live state.

**Remaining: the same drive behind a USB 2.0 hub.** The open question is whether a USB 3
device linked at 480 Mbps through a SuperSpeed-capable path, bandwidth-limited by an
upstream USB 2.0 hub, still reports `version 3.x`.

- Still `3.x` → the rule has a real niche. Keep it, and validate the `upstream_is_hs`
  branch that blames the hub rather than the cable.
- Drops to `2.10` → the descriptor check is dead in every reachable scenario and
  `LINK_BELOW_DEVICE_CAPABILITY` should be **deleted**, leaving `SS_HALF_IDLE` as the
  sole detector.

A USB 2.0 hub occupies only the 2.0 half of its receptacle, so `SS_HALF_IDLE` may fire
too — check the two rules do not produce a contradictory pair. Fold the observed values
into fixtures either way.

---

## The root-probe chain

Everything the tool did before this chain reads *negotiated state*. A marginal cable
often negotiates full speed and only fails under load, which is exactly the case passive
reading cannot see. These six items are how it learns to push. The first two have
landed, and are kept here with their reasoning because the remaining four build on it.

### 1. A privilege/capability model — **done**

`caps::detect()` resolves the usbmon text stream and usbfs write access, and reports
which of five registered probes each unlocks. Rendered under `-v`.

Detection is by attempt where an attempt is cheap and side-effect free, because "am I
root, therefore I may" is wrong often enough to matter. Where an attempt cannot answer,
the kernel config can, and the four ways of being unavailable are kept apart because the
fix differs completely: **not loaded** (`modprobe`), **denied** (privilege),
**unsupported** (different kernel), **undetermined** (`/sys/kernel/debug` is 0700 and
sometimes the honest answer is that we cannot see).

### 2. usbmon URB error accounting — **done**

`usbmon::sample()` watches the text stream for a window and accounts completions per
device address. `LINK_ERROR_RATE` (Measured) fires above 3 transport errors and a 0.1%
rate; High above 1%.

Three classes, kept apart, because conflating them would produce exactly the confident
false accusation this tool exists to avoid:

- **transport** — `EPROTO`, `EILSEQ`, `EOVERFLOW`, `ETIMEDOUT`. Implicates the wire.
- **protocol** — `EPIPE`. On endpoint 0 this is a device declining a request, which is
  routine; on a data endpoint it is a halt.
- **cancelled** — `ENOENT`, `ECONNRESET`, `ESHUTDOWN`. A webcam stopping its stream
  cancels URBs in bulk. Counting those would condemn every healthy camera on the machine.

Corroboration deliberately stops short of what the original task asked for. Measured
errors annotate the findings they speak to and lift Heuristic to Inferred, but nothing
reaches Measured: the counts are measured, and blaming the cable for them is still a
deduction. A cable is only convicted by substitution.

**Validated.** 638 real lines from `/sys/kernel/debug/usb/usbmon/0u` on this machine
parse with nothing unrecognised: 319 completions, 319 resubmissions correctly skipped.
Half of all real traffic is the `-115` resubmission that immediately follows each
completion, so mistaking it for an outcome would have reported a permanently failing
bus. Six of those lines are now a verbatim fixture in `usbmon::tests`.

The isochronous type letter is `Z`, settled from the shipped module rather than from
documentation — `strings usbmon.ko` gives the letter table `CZBI`, indexed by endpoint
type, and the format `%lx %u %c %c%c:%d:%03u:%u`. `S` is now rejected: it is the
submission event letter, and accepting it as a transfer type would let malformed lines
through. The same strings show the older `<bus>t` files use a shorter address word with
no endpoint field, which is why only the `u` form is read.

Still unseen by the parser: a non-zero completion status. The classification is keyed on
errno values, which are stable, but no real error has been observed. That needs failing
hardware, not another capture.

### 3. An opt-in `usbdiag probe` subcommand — **done**

The gate. `usbdiag probe` lists the catalogue and runs nothing; `usbdiag probe NAME`
runs one. The decision lives in `probe::plan`, not in the CLI, so a GUI cannot grow its
own subtly different version of "is this disk mounted".

`all` / `ports` / `devices` / `diag` / `watch` are untouched and still passive.
Exit codes unchanged: 0 clean, 1 medium-or-worse, 2 bad usage **or a probe refused**.

**Consent is proportional to consequence — a deliberate deviation from the spec above.**
The original said every probe refuses until a confirmation flag is passed. That was
dropped while building it: a flag required even for a probe that only reads becomes a
reflex, typed without thought, and therefore worth nothing on the one probe where it
matters. So passive probes run unasked, a **read-only** probe runs when named — naming it
*is* the request, and there is nothing to undo — and only a **disruptive** probe asks. It
asks twice, for two different things: `--yes` to consent at all, and either `--force` or
a typed confirmation of the target name to accept the interruption.

**One refusal consent cannot lift.** A disruptive probe against a disk holding a mounted
filesystem or an active swap area is refused however many times the user says yes.
`block::holders()` resolves the whole stack rather than comparing names, because the
dangerous case looks nothing like the disk: a LUKS volume on a USB stick appears in
`/proc/self/mounts` as `/dev/mapper/backup`, which shares no substring with `sdb`.
Following `slaves/` down to the physical disks is the only way to connect the two.
`/proc/swaps` counts too. Read fresh at the moment of the decision, never from the
snapshot, since a filesystem can be mounted between capture and probe.

Ordering of refusals is deliberate: interface missing, then unimplemented, then target
unresolvable, then in use, and only then consent. Being told the disk is mounted is more
use than being told to confirm something that would then be refused anyway.

`--target` takes a sysfs name (`6-1.2`) or a disk (`sdb`, `/dev/sdb`), because both are
what a user has in hand — the device tree prints one and `df` prints the other. For
`urb-errors` it narrows the *display* only, and says so: usbmon watches every bus at once
and has no per-device mode, so claiming otherwise would be a lie about the measurement.

The unimplemented probes are listed and then refused **by name**. A gap that is visible
gets closed; one that is merely absent does not.

The disruptive gate is tested through a stand-in `ProbeInfo`, since both real disruptive
probes refuse as unimplemented long before reaching it. Without that the code standing
between a probe and someone's data would have shipped having never once run.

**Found by running it, not by testing it:** `Snapshot::buses` holds root hubs with their
children *nested*, and the first version of `subtree`/`resolve_target` iterated `buses`
directly. Every synthetic snapshot in the suite happens to be flat, so it passed
everywhere and then resolved nothing at all for `4-1` on a real machine. The fixtures
involved are now nested.

### 4. Throughput probe — root, read-only — **done**

Measures what a link *achieves* against what it negotiated: a sequential read of
`/dev/sdX` with direct I/O, for a bounded window, compared against what the link and the
medium allow.

**Not usbfs, and not disruptive.** The original plan was `USBDEVFS_*` ioctls and a SCSI
READ over bulk-only transport. That needs `ioctl`, which needs libc — a dependency this
crate has already refused once, in writing, in `caps.rs`, as the reason the binary usbmon
API is out of reach. There is no portable pure-Rust `ioctl`: inline assembly would work
on x86_64 and break on aarch64, which is exactly where a USB diagnostic tool earns its
keep. A block read is bottlenecked by the same chain — cable, bridge, media — costs
nothing in dependencies, and because it detaches no driver it can run against a
**mounted** drive, which is the state a drive is in when somebody says it is slow. What
it gives up: it cannot load a device that is not storage, and the block layer's overhead
is inside the number. Neither matters for the question being asked.

**`O_DIRECT` is the whole measurement, and it has two traps.** Without it the page cache
answers and the probe reports several GB/s over a 480 Mbps link. Its value is *not the
same on every architecture* — `0o40000` on x86 and ARM, `0o400000` on PowerPC — and Linux
discards `open` flags it does not recognise **without an error**, so a wrong constant does
not fail, it silently measures the cache. So the value is cfg-gated to architectures where
it is known, anything else is reported `Unsupported` at any privilege, and then it is
proved at runtime: a deliberately misaligned one-byte read is rejected with `EINVAL` under
direct I/O and succeeds without it. If that read succeeds, no number is reported at all.
Verified positively on this machine, not just asserted in the negative.

**The judging rule is where this could have gone badly wrong.** Comparing achieved
throughput against the link rate would condemn nearly every healthy drive: 110 MB/s over a
5 Gbps link that allows 450 is an ordinary flash drive. And the media baseline the original
plan called for is mostly unavailable — `medium()` returns `Unknown` for almost everything
on USB, because bridges do not implement VPD page B1h. Following the original spec
literally ("media unknown → no finding") would have produced a **fourth unreachable rule**.
So the comparison is against the slowest thing the medium could plausibly be:

- medium known to spin → the platter's ceiling, flagged below half of it
- medium unknown on a SuperSpeed link → flagged only below ~40 MB/s, what plain USB 2.0
  would have delivered. No storage device negotiates 5 Gbps and then reads slower than a
  High-Speed link would have allowed, whatever it is made of
- medium unknown on a High-Speed link → **no finding, deliberately.** A genuinely slow
  flash drive and a bad cable are indistinguishable at those rates

The measurement is always shown. Only the accusation is withheld — a rate on screen with
"no conclusion drawn" is useful; a measurement suppressed because it could not be
interpreted is not.

**Contention is measured, not assumed.** `/sys/block/<dev>/stat` is read either side of
the window and our own bytes subtracted, so traffic that was not ours is visible. A
contended sample is displayed with that caveat and never judged: it describes load, not
the link.

Findings: `THROUGHPUT_FAR_BELOW_LINK` (Measured, Medium) and `STORAGE_READ_FAILED`
(Measured, High) — the second for a read that begins and then dies, which is a hardware
symptom, as opposed to one that never starts, which is usually a permission and is not
reported as a fault.

The usbfs variant is still the only way to load a non-storage device. If that is ever
wanted it should be a separate disruptive probe rather than a change to this one.

### 5. Re-enumeration cycling — root, disruptive

The only test that finds intermittency. A cable that trains SuperSpeed 16 times out of
20 is failing in a way no single passive read can reveal.

Write `1` then `0` to the hub port's `disable`
(`/sys/bus/usb/devices/usbX/X-0:1.0/usbX-portN/disable`, verified writable), or issue
`USBDEVFS_RESET`. Wait for re-enumeration, re-read speed and lane count, repeat ~10
times, report the distribution.

New finding: `LINK_INTERMITTENT` (Measured) when the negotiated speed varies across
cycles, with the distribution as evidence. A device that sometimes trains and sometimes
does not is almost never a firmware problem.

The most invasive of the three: it drops the device off the bus every cycle. Must refuse
on a mounted filesystem or an input device the user may be typing on. Restore port state
on every exit path including signals.

### 6. Document the root-probe design in the README

Keeping the existing honesty about what software can and cannot know:

- the Passive/Active split, and why the default stays unprivileged
- which probes need root, and which are disruptive versus read-only
- the confidence story, as built rather than as originally sketched: usbmon error counts
  are measured, and they lift a Heuristic finding to Inferred — but no cable finding
  reaches Measured, because attributing measured errors to the cable is still a
  deduction, and a cable is only convicted by substitution
- **what active probing still cannot reach, and why.** No userspace path exists to send
  PD/SOP' messages: the PD state machine lives in the port controller firmware.
  `CONFIG_UCSI_DEBUGFS` is not set on this kernel (6.17.0-1030-oem), and it is the one
  interface that would let root issue raw UCSI commands such as `GET_CABLE_PROPERTY`, so
  cable interrogation is closed here without a custom kernel build
- the underlying reason: **you can only probe what is addressable, and a passive cable
  has no address.** Its e-marker answers solely to the port controller over SOP'. Cable
  probing is therefore always indirect — push traffic or power through it and observe
  where it fails
- out of reach at any privilege level: CC-line voltages, eye diagrams, jitter,
  insertion loss

---

## Unprivileged work worth doing

### Read SCSI error counters — a passive error signal for storage

`/sys/block/<dev>/device/` exposes counters that are world-readable, unlike usbmon: 
`iorequest_cnt`, `iodone_cnt`, `ioerr_cnt`, `iotmo_cnt`, `device_busy`, `device_blocked`.
For USB storage this reaches part of what `LINK_ERROR_RATE` reaches, with no privilege at
all — which matters, because usbmon needs root and most people running this will not have
it.

**`ioerr_cnt` is not a bus-error counter**, and treating it as one would repeat exactly
the mistake usbmon's classification exists to prevent. It counts any command that did not
return GOOD status, including the kernel probing for optional features and being told no.
From this machine, both drives freshly plugged in and working perfectly:

```
sda  SanDisk Ultra USB 3.0   iorequest 0x243  ioerr 0x2  iotmo 0x0
sdb  TOSHIBA TransMemory     iorequest 0x20d  ioerr 0x2  iotmo 0x0
```

Two healthy flash drives, both at exactly `ioerr_cnt=2` — routine CHECK CONDITION replies
from discovery. A rule firing on `ioerr_cnt > 0` would condemn every storage device on
every machine.

- the values are **hex** (`0x243`), not decimal
- `iotmo_cnt` is the closest thing to a genuine transport failure and deserves its own
  weight — a timeout means the device stopped answering
- the signal is the **delta over a window**, not the absolute count, since the baseline is
  a fixed handful from discovery. Sampling fits the existing shape of `block::sample()`
- only SCSI-attached devices have these; NVMe has no such directory, so absence is normal
- give it its own code (`STORAGE_IO_ERRORS`) rather than folding it into
  `LINK_ERROR_RATE`, so the two sources are never conflated in evidence

---

## Shipped but unverified against hardware

Both are implemented, tested synthetically, and have never seen the situation they exist
for. Listed here so the gap is not mistaken for coverage.

- **`DP_ALT_MODE_NO_OUTPUT`** — worse than unverified: probably **unreachable** on this
  platform. A Dell DA20 is driving a 1440p monitor through DisplayPort Alt Mode, so the
  mode is unambiguously active, and yet `/sys/class/typec/port1-partner/` has no
  `number_of_alternate_modes` attribute and no alt-mode directories at all. UCSI never
  enumerates the partner's modes here even while one is entered, so the rule's
  precondition — a partner alt mode with SVID `ff01` and `active = yes` — cannot be
  satisfied. Same shape as the `LINK_BELOW_DEVICE_CAPABILITY` problem above. Keep only if
  a tcpm-based system populates partner alt modes properly; otherwise delete, since
  `BILLBOARD_ALT_MODE_FAILED` plus the DRM cross-check already covers "adapter attached,
  no picture" and fired correctly on this hardware.
- **`probe throughput` against a real disk** — the O_DIRECT mechanism is proved on this
  machine (the misaligned-read check passes positively on ext4) and every refusal path is
  exercised unprivileged, but the measurement itself needs root to open `/dev/sdX` and has
  not been run. `sudo ./target/debug/usbdiag probe throughput --duration 3000` settles it.
  The number to sanity-check is the SanDisk on `4-1`: a 5 Gbps link, so anything in the
  100–200 MB/s range is a healthy flash drive and must produce **no** finding.
- **`probe urb-errors` end to end** — every refusal path was exercised against live
  hardware, but the run itself has not been: it needs `sudo modprobe usbmon` and root,
  and the machine has rebooted since the module was last loaded. The parser is validated
  against 638 real lines and the gate is validated unprivileged; what is untested is only
  the join between them.
- **The udev wake path in `watch`** — parsing, threading and debouncing are covered by a
  canned stream, and `udevadm monitor` is confirmed to spawn with the right filters, but
  no real uevent has travelled the whole chain. Plugging anything in while
  `usbdiag watch` runs settles it.

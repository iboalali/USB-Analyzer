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

### 3. An opt-in `usbdiag probe` subcommand

The gate. Nothing active runs without it.

- refuses without an explicit flag, and refuses outright when not root, naming what it
  would need
- `all` / `ports` / `devices` / `diag` behaviour is unchanged and never invasive
- `--target <sysfs-name>` scopes a probe to one device instead of sweeping the bus
- each probe declares read-only or disruptive, and prints that before acting
- disruptive probes need a second confirmation, and **must refuse on a device holding a
  mounted filesystem**
- exit-code semantics unchanged: 0 clean / 1 medium-or-worse / 2 usage

### 4. Throughput probe via usbfs — root, disruptive

Measure what a link *achieves* against what it negotiated.

Open `/dev/bus/usb/BBB/DDD` and use `USBDEVFS_*` ioctls — prefer raw ioctls over libusb
to keep the crate free of non-Rust dependencies. For mass storage, a SCSI READ over
bulk-only transport is the cleanest read-only load.

**Must read `/sys/block/<dev>/queue/rotational` before judging anything.** A real device
here proves why: the MEDION drive above is a 5400 rpm 2.5" disk (`rotational=1`) behind
an ASMedia bridge on a 5 Gbps link. It sustains ~100–120 MB/s; the link allows ~400–500.
A naive probe would report "110 MB/s on a 5 Gbps link" and condemn a healthy drive. So
the baseline comes from the media, not the link:

- `rotational=1` → compare against ~150 MB/s, flag only when far below
- `rotational=0` → compare against the link rate, allowing for cheap flash
- media type unknown → emit no finding at all, and say so

Disruptive: `USBDEVFS_DISCONNECT_CLAIM` takes the device from its driver for the
duration. Restore it on every exit path including errors and signals — use a guard type
so the restore cannot be skipped.

New finding: `THROUGHPUT_BELOW_LINK_RATE` (Measured), only when achieved throughput is
far below what **both** the negotiated rate and the media type allow.

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

## Shipped but unverified against hardware

Both are implemented, tested synthetically, and have never seen the situation they exist
for. Listed here so the gap is not mistaken for coverage.

- **`DP_ALT_MODE_NO_OUTPUT`** — needs any device that negotiates DisplayPort Alt Mode: a
  dock, or a USB-C→HDMI adapter. Only its guards are currently proven, including the
  false positive this machine would otherwise produce from a local port's meaningless
  `active` flag.
- **The udev wake path in `watch`** — parsing, threading and debouncing are covered by a
  canned stream, and `udevadm monitor` is confirmed to spawn with the right filters, but
  no real uevent has travelled the whole chain. Plugging anything in while
  `usbdiag watch` runs settles it.

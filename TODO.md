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

### 5. Re-enumeration cycling — root, disruptive — **done**

The only test that finds intermittency. A cable that trains SuperSpeed 16 times out of 20
is failing in a way no single passive read can reveal, because each individual reading
looks fine. Twenty attempts and a distribution is the whole method.

Writes `1` then `0` to the hub port's `disable`, waits for re-enumeration, records speed
and lane count, repeats. `USBDEVFS_RESET` was the alternative and is refused for the same
reason as everywhere else: it needs `ioctl`, which needs libc.

**The port is found through the device's own `port` symlink**, not by deriving a path
from its name. Two things fall out of that for free: hub interface numbering never has to
be guessed at, and a root hub has no `port` symlink at all — so cycling one, which would
drop every device on that bus at once, fails at the first step rather than needing a
special case. The gate refuses root hubs explicitly as well, so the user gets an
explanation instead of a silent nothing.

`--cycles` is separate from `--duration` and defaults to 20. A cycle takes as long as the
hardware takes, so the useful control is how many attempts, not for how long. The minimum
is 2, enforced in the parser: one attempt cannot disagree with itself, and a run of one
must never be reportable as intermittent.

**A second refusal that consent cannot lift.** Alongside the mounted-filesystem check, a
subtree containing an input device is refused outright. This is not about data — nothing
is destroyed — it is that disabling the port a keyboard is on removes the means of
stopping whatever happens next, which is not a thing a user can meaningfully agree to in
advance. Everything else that will drop and come back — disks that are present but
unmounted, network interfaces that are up and may be carrying the session — is a
**warning, not a refusal**, printed as part of the confirmation. That is what the second
confirmation is for: the first yes is to the idea, the second is to the consequences.

Findings: `LINK_INTERMITTENT` (Measured; High when the device failed to re-appear at all,
Medium when it merely trained slower) and `LINK_STABLE_UNDER_CYCLING` (Info). The clean
result is reported deliberately — twenty identical trainings do not prove a cable is good,
but they rule out intermittency, and a user who ran a deliberate test should learn
something from it passing.

**What cannot be guaranteed, stated plainly.** The port is restored by a guard whose
`Drop` runs on every return, every error path and on a panic — all covered by tests. It
does **not** run if the process is killed, because handling `SIGINT` needs a signal
handler and therefore libc. Mitigations: the disabled window is 150 ms, and the exact
command to re-enable a stuck port is printed *before* anything happens, since afterwards
is precisely when it cannot be printed.

**Refusal ordering changed while building this.** Privilege used to be checked before
anything about the request. That meant an unprivileged user asking to cycle a mounted
disk was told "you need root", went and found sudo, and only then learned the disk was
mounted and it was never going to work. Request errors are deterministic and fixable
without escalating, so they now come first; privilege is checked once the request is
known to be sound.

Every refusal path is verified against this machine, including the mounted-filesystem one
firing for real on the SanDisk auto-mounted at `/media/iboalali/UNRAID`.

### 6. Document the root-probe design in the README — **done**

Covered in a new **Active probing** section: the three-class split and why the default
stays unprivileged, which probes need root and which of those are disruptive, the two
refusals consent cannot lift and why an input device is one of them, the no-ioctl
constraint and what it costs, the `O_DIRECT` trap, and the limit on restoring a port
through a signal.

Two sub-sections carry the parts that are easy to overstate. *What active probing still
cannot reach* — root does not unlock the cable, because **you can only probe what is
addressable and a passive cable has no address**; its e-marker answers solely to the port
controller over SOP', `CONFIG_UCSI_DEBUGFS` is unset on this kernel, and CC voltages, eye
diagrams, jitter and insertion loss are out of reach at any privilege. *What measurement
buys, and what it does not* — measured errors lift heuristic to inferred and stop there,
the three usbmon error classes and why conflating them would be the exact false accusation
the tool exists to avoid, and why the throughput rule is judged against the slowest
plausible medium rather than the link.

The JSON surface is documented as an API rather than an output format, since that is now
what it is. The rules table gained the five findings that only exist once a probe has run,
and the data-source table gained usbmon, `/dev/sdX` and the port `disable` attribute,
marked as the three that need root.

---

## The JSON surface is an API, not a print format — **done**

Two kinds of front end were considered, and they were in different states. One that
**links the library** was always fine: every decision lives in `usb-probe`, so
`probe::plan` hands back either a `Plan` or a `Refusal`, `Refusal::is_recoverable()` says
whether to show a confirmation or an error, and the mounted-disk and keyboard refusals
cannot be skipped by a caller who forgets to check, because the check is inside the gate.
`render.rs` really is the only file to replace.

One that **shells out and parses `--json`** could render state but could not drive a
probe. Three gaps, now closed:

- **Refusals were prose on stderr.** For the disruptive probes the refusals *are* half the
  interaction, so a front end was left parsing English to discover that a disk was
  mounted. There is now a `RefusalReport` on stdout with a stable `code`
  (`in_use`, `critical_device`, `whole_bus`, `needs_consent`, …), a `recoverable` flag, and
  the details in fields: which disk, mounted where, which of the two confirmations is
  missing. Exit code is still 2.
- **The catalogue was not exposed.** `probe --json` emitted only `Capabilities`, so a
  front end had to hardcode the probe list. It now emits `{capabilities, probes}`, where
  each probe carries its class, requirement, summary, `ready`, and the `blocker` when it is
  not. Registry and capability are joined *there* rather than left for the caller, because
  combining them is the step a front end would get subtly wrong — and getting it wrong
  means offering a button that cannot work.
- **`Plan` was unreachable.** `--dry-run` decides, prints the decision, and stops. With
  `--json` it emits the plan, including `side_effects` — the list a confirmation dialog
  needs. That is the whole flow: dry-run to find out what would happen, show the dialog,
  then run with `--yes --force`.

`RefusalReport` is written out by hand rather than derived from `Refusal`. A derived shape
would follow the enum, and the enum exists to make the *decision* clear — its variants get
renamed as the rules sharpen. The wire format's job is to not move when they do. Tests
assert the exact slugs and field names for the same reason.

Also: a machine is never prompted. With `--json` the interactive confirmation is skipped
entirely, since prompting would hang a front end on a read of a stdin nobody is typing
into. It gets the refusal, decides, and asks again with the answer.

`Duration` is serialised as `window_ms`, because seconds-and-nanoseconds is a poor thing to
put in front of another program.

## A GTK4 front end

Concept and decisions: [`docs/01-gui-concept.md`](docs/01-gui-concept.md).
**Read [`docs/02-prior-art.md`](docs/02-prior-art.md) first** — three tools already ship
the cable-identity half of this, one of them a GTK4/libadwaita app. The chain should not
be the headline; the findings should, which promotes #24 from nice-to-have to gate. Shaped after
[TempoUI-for-Linux](https://github.com/iboalali/TempoUI-for-Linux) — same workspace split,
same gtk4 / libadwaita / relm4 versions, same Ubuntu 24.04 baseline. v1 is a live viewer
with no probes and no privilege.

Both presentations are drawn in [`docs/mockups/`](docs/mockups/) — HTML, libadwaita
palette, real data from this machine except one deliberately synthetic fault page.
Drawing them settled five things prose had left open (host findings need their own
subject row; sidebar dots carry their sentence; the chain transposes rather than shrinks
below the breakpoint; the bars stay linear; a tray popover is a second view model, not a
narrow window) and closed the `--desktop`-mode question: no mode.

The one piece of library work it depends on is below.

### Say "not a cable problem" out loud

The tool expresses "nothing is wrong here" as **silence**, and silence is not an answer —
a clean report leaves the user unsure whether it looked. WhatCable on macOS handles this
better with explicit exonerating verdicts (*"Device runs at 10 Gbps, this is the fastest
it supports, not a cable problem"*), and it is the same discipline this tool already
applies internally, just never surfaced.

- **A verdict per subject** — one plain sentence, before any detail. It belongs in
  `usb-probe`, not the GUI: a headline derived from findings inside a renderer is a second
  rule engine, and it will drift from the first.
- **Exonerating findings** for the cases deliberately passed over: linked at the device's
  own maximum; the medium explains the rate rather than the link; no alt mode was
  requested, so none failing is correct; a 3 A cable is not the limit when the charger
  only offers 60 W.

Two risks. A verdict must summarise the findings and never make a new claim. And
exonerations must not become noise — Info level, collapsed by default in the CLI, shown in
the GUI only for the selected subject.

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

### Name vendors from `usb.ids`, and flag e-markers that look counterfeit

From [`docs/02-prior-art.md`](docs/02-prior-art.md). Two features, one item, because the
second needs the first.

**Vendor names.** Device names come from the sysfs `manufacturer` and `product` strings,
which is fine for devices and useless for cables: an e-marker carries a vendor ID in its
ID Header VDO and never a string. On hardware that exposes a cable at all — not this
laptop, see below — we would print `0x2109` where a name exists.

`usbeehive` solves this by bundling a USB-IF table. **Read the system database instead.**
`/usr/share/hwdata/usb.ids` and `/usr/share/misc/usb.ids` are both present here, 25 627
lines, shipped by `hwdata` and `usb.ids` respectively — packages already on any machine
with `lsusb`. Reading at runtime beats bundling on every axis that matters: it cannot go
stale, it adds no data file, and it raises no licence question. It is also simply better
data — the system table names Goodix (`27c6  Shenzhen Goodix Technology Co.,Ltd.`) where
`usbeehive::vendor::lookup(0x27c6)` falls back to hex. Absence must stay non-fatal: hex is
the correct answer when the file is not installed, and the parse is two fields of a
tab-indented text file, not a dependency.

**Counterfeit signals**, after `usbeehive::cable::CableTrust` (MIT). Three, none
conclusive: a zero vendor ID in the ID Header VDO, a VID the database does not know, and
reserved bits set in the Cable VDO.

Three traps, in order of how badly each would misfire:

- **This must be Heuristic, and Info or Low.** "Your cable may be counterfeit" is the
  most damaging sentence this tool could emit wrongly, and all three signals are weak.
  A cheap-but-honest cable from an unregistered vendor trips two of them.
- **The reserved-bits check must inherit our PD-revision uncertainty.** `vdo.rs` already
  refuses to guess between the PD 2.0 and 3.0 product-type encodings, reporting *"Passive
  or Active Cable (ambiguous without PD revision)"*. The Cable VDO layout differs between
  passive and active, so where the revision is unknown the check cannot run — it would
  read reserved bits at the wrong offsets and fire on healthy PD 2.0 cables.
- **The unknown-VID signal is only as good as `usb.ids`.** Suppress it entirely when the
  file is missing, or every cable becomes suspicious on a minimal system.

Untestable here. UCSI exposes no cable node — `/sys/class/typec/` has ports and partners
and no `port0-cable` — so every line of this is dead code on this machine and gets
fixtures, not a live check. Same category as #12 and #23.

### Classify devices, but only let a guess suppress a finding

`usbeehive` carries a 28-kind taxonomy — Camera, Gamepad, Keyboard, SecurityKey,
SmartcardReader, Phone, VideoCapture, Storage, Hub — built from class codes plus
product-string heuristics. We classify by USB class code alone.

This is not decoration. What a device *is* changes what an observation *means*: 480 Mbps
is correct for a keyboard and a fault for an external SSD; 900 mA is a phone charging and
a mouse malfunctioning. The taxonomy is a rule input, which is exactly why it is
dangerous.

**The constraint that makes it safe: a classification may suppress or soften a finding,
never create one.** The asymmetry is not stylistic. A wrong guess that suppresses costs a
missed detection — bad. A wrong guess that accuses costs a false accusation against
hardware the user then replaces — the failure this whole project is built to avoid. Only
one of those is recoverable, so guesses may only ever push toward silence.

That splits the taxonomy in two, and the split should be in the type, not in a comment:

- **Class-code derived** — from `bDeviceClass` / `bInterfaceClass`. Not a guess; the
  device asserts it. May feed any rule.
- **String-heuristic derived** — "this product string contains *webcam*". A guess, and it
  must be marked as one so a rule cannot silently treat it as fact.

Start with the class-code half only. It covers hubs, HID, storage, audio, video and
Billboard, which is most of what the rules actually need, and it carries no risk. The
string heuristics are worth having later, behind the suppress-only rule, and are worth
nothing at all if that rule is not enforced by the types.

**On this machine the class code already answers 7 of 9 devices** — three hubs, a
smartcard reader, two mass-storage devices, a Billboard adapter, and a wireless
controller. Only the Goodix fingerprint reader is opaque, at `bDeviceClass ef` /
`bInterfaceClass ff`: Miscellaneous over Vendor Specific, which is the class-code
equivalent of a shrug. So the free half is most of the value, and the risky half is for
one device.

### Let the user correct a device's type, and remember it

Show the kind in the UI, and let it be overridden. Requested for the display; worth
building for what the override unlocks.

**This is not cosmetic.** Both storage devices here report class `08`, which says
*storage* and nothing about **what kind**. `block::medium()` returns `Unknown` for
nearly all USB because bridges omit VPD page B1h — which is exactly why
`THROUGHPUT_FAR_BELOW_LINK` has to reason about "the slowest plausible medium" and
`ROTATING_SHORTFALL` instead of a real threshold. A user who says *"that one is a
spinning disk"* supplies a fact no amount of reading can recover, and the rule gets a
yardstick. That is the case that justifies the feature.

It also means **an override must be allowed to sharpen a finding, not merely suppress
one** — the opposite of the rule above for string heuristics, and correctly so. A
product-string guess may only suppress because the tool invented it. A user assertion is
better evidence than anything on the wire: they are holding the object.

**Scope: the model, with the unit as an escape hatch.** Default to `VID:PID`, so
correcting one SanDisk Ultra corrects every SanDisk Ultra. Offer "just this one" via
`VID:PID:serial` where a serial exists and is trustworthy.

**Trustworthy is doing real work in that sentence.** Two of the six serials on this
machine are placeholders — MediaTek reports `000000000`, the Dell DA20 reports
`00000000000000000`. Key on those naively and correcting one adapter relabels every
zero-serial device ever plugged in. Degenerate serials — all zeros, all one repeated
character, shorter than a few characters — must be rejected as identifiers and fall back
to model scope. The three genuine serials here (`4C530001010412118490`,
`54B80A3FA797C091604B95`, `UID9802CAEE_XXXX_MOC_B`) show what a real one looks like.

**Remember, never generalise.** A correction is a stored fact about one identity,
applied on every future sighting. The tool does not mine corrections for patterns and
start guessing — a rule inferred from user data would produce findings with no traceable
cause, which is the one thing this project cannot afford. Every stored override is
listable and deletable from the CLI, because a belief the user cannot inspect is a belief
they cannot correct.

**Provenance goes on the device kind, not in `Confidence`.** The confidence enum is about
certainty and should stay three-valued; where a fact came from is a second axis, and
conflating them would make a stale override invisible behind an `inferred` badge. So the
device kind carries `source: class | heuristic | user`, the UI shows it per device where
the user can act on it, and any finding that leans on a declaration cites it in evidence
("medium: rotating — set by you") and is capped at `Inferred`. Measured means read off
the hardware, and a declaration is not that however true it is.

**This is the project's first persistent state**, which is worth being reluctant about.
`$XDG_CONFIG_HOME/usbdiag/devices.json`, JSON because `serde_json` is already a
dependency and the tree stays at 13 crates. Absent file means no overrides and no error.
Hand-editable, listable (`usbdiag labels`), clearable. **Only an explicit command writes
it** — no read path ever persists anything.

It also changes what `capture()` *is*: currently a pure function of the machine, now a
function of the machine and a config file. Two runs on identical hardware can differ. So
`--no-overrides` must exist to get the unmodified view, and the JSON must carry the
source field so a consumer can tell which is which. Without both, "why does it say that"
becomes unanswerable, and bug reports become useless.

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
- **`probe reenumerate` actually cycling a port** — every refusal is verified on this
  machine and the guard logic is covered by tests, including a panic mid-run, but no port
  has been switched off in anger. Needs root and a target that is neither mounted nor an
  input device: the Dell DA20 on `5-1.2` is the obvious candidate, since it carries no
  storage. `sudo ./target/debug/usbdiag probe reenumerate --target 5-1.2 --yes --force`.
  The expected result on healthy hardware is `LINK_STABLE_UNDER_CYCLING`, 20 of 20 at
  12M — and the device must still be present afterwards.
- **`probe urb-errors` end to end** — every refusal path was exercised against live
  hardware, but the run itself has not been: it needs `sudo modprobe usbmon` and root,
  and the machine has rebooted since the module was last loaded. The parser is validated
  against 638 real lines and the gate is validated unprivileged; what is untested is only
  the join between them.
- **The udev wake path in `watch`** — parsing, threading and debouncing are covered by a
  canned stream, and `udevadm monitor` is confirmed to spawn with the right filters, but
  no real uevent has travelled the whole chain. Plugging anything in while
  `usbdiag watch` runs settles it.

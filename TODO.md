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
be the headline; the findings should, which promoted #24 from nice-to-have to gate. Shaped
after [TempoUI-for-Linux](https://github.com/iboalali/TempoUI-for-Linux) — same workspace
split, same gtk4 / libadwaita / relm4 versions, same Ubuntu 24.04 baseline.

Both presentations are drawn in [`docs/mockups/`](docs/mockups/) — HTML, libadwaita
palette, real data from this machine except one deliberately synthetic fault page.
Drawing them settled five things prose had left open (host findings need their own
subject row; sidebar dots carry their sentence; the chain transposes rather than shrinks
below the breakpoint; the bars stay linear; a tray popover is a second view model, not a
narrow window) and closed the `--desktop`-mode question: no mode.

### v1 viewer — **done**

`crates/gui`, binary `usbdiag-gui`. Live viewer, no probes, no privilege. Sidebar (system
row, Type-C ports, device tree with hubs collapsed), detail pane ordered verdict → ruled
out → findings → evidence → what cannot be answered, cairo chain widget, udev updates.
`gtk4` 0.11 / `libadwaita` 0.9 / `relm4` 0.11, exactly TempoUI's versions.

**The chain derivation went into `usb-probe`, not the GUI.** `crates/usb-probe/src/chain.rs`
turns a port or a device into four stages with magnitudes; `crates/gui/src/chain.rs` only
draws them. The concept doc already forbade diagnosis in the widget, and the rule it gave —
a stage is marked because a *finding* points at it — is a `code → stage` table that deserves
tests. It now has 20, including one asserting a power code cannot mark a data stage and one
asserting an Info finding never aims the marker at anything.

**A stage with no number is the normal case, and is drawn as a dashed outline.** On UCSI the
cable stage is unknowable, and `bcdUSB` names a specification rather than a rate, so a
device's own claim usually has no figure either — two of four stages on the data chain here.
An empty bar would read as zero, which is a much stronger and quite false statement; the
card says how many stages are dashed and why.

**A verdict that cites nothing does not get to speak in the sidebar.** Rows fall back to a
plain fact and a hollow dot. This settles the question left open under #24: `clear` with an
empty `because` is drawn differently from a cited `clear`, because saying "Nothing wrong
found" in green about a subject no rule examined is a claim the data does not support.

Three things found only by running it:

- **An empty port advertised 4.5 W.** `typec_advertised_ceiling_mw()` reports what the CC
  resistors offer, which exists whether or not anything is plugged in — so every unused
  socket read "4.5 W in". The meta is now suppressed unless something is attached.
- **The transposed chain drew four rows into the height of two.** `GtkDrawingArea::resize`
  is emitted from inside `size_allocate`, and changing a size request there queues a resize
  during allocation, which GTK drops. Deferred to an idle callback.
- **Sidebar reasons need two lines, not one.** They are finding titles quoted verbatim, and
  those are written for a list that names the subject separately — so they open with the
  device's own name and run long. Truncating the quote is worse than spending a line.

Verified by screenshot in both colour schemes and at both widths (`.claude/skills/`
`screenshot-app` and `interact-app`, ported from TempoUI). Live updates were confirmed by
accident, which is the best way: a dock was plugged in mid-session and the sidebar grew two
buses and a storage row without being asked.

### Probe panel — **done, and it runs nothing**

`crates/gui/src/probes.rs`, on the host pane. §9 predicted this needed no new plumbing and
that held exactly: `Snapshot::capabilities` was already in the model and already read by
`detail.rs`, so the panel asks `Capabilities::blocker` and displays the answer. It adds no
detection and no second opinion about what the machine allows.

**It lives on the host pane rather than in a new view.** What the tool is *permitted* to do
is a property of the computer, and the sidebar already has a row for that — so this costs no
navigation and no second view model, which is the thing §5 warned against. It comes last,
straight after *what cannot be answered here*, because the two read as one thought: here is
what could not be determined, and here is what would determine it.

**Nothing is run from it, and it says so in its first line.** Until `pkexec` escalation
lands the app has no honest way to run a probe, and a row that looks clickable and does
nothing is worse than a row that admits it. What it adds today is the answer to *"what could
this tool do here, and why can it not right now"* — which the GUI could not previously give
at all, since `cannot_answer` recommends `usbdiag probe throughput` without ever saying
whether it could run on this machine.

**Readiness is a chip, never a dot.** A dot in this app means severity. A machine that will
not let a stranger read raw disks is not faulty, so putting readiness on the fault scale
would quietly grade the computer. Same reason the class chips borrow the *confidence*
palette: a disruptive probe is a stronger kind of action, not a worse one.

Live, this machine reports **2 of 5 can run here** — and reading it surfaced something that
matters for the next item. See below.

### Tell privilege apart from absence — **done**

Found by reading the probe panel on this laptop, and it turned out to be worse than the flat
chip that prompted it. "No USB disk attached" was reported as `Availability::Unsupported`,
whose documented meaning is *"this kernel was not built with it. Fix: a different kernel."* So
the tool told the user their kernel was deficient when the honest instruction was to plug a
drive in — and `Absent` is the one state in that enum a **uevent** can clear rather than a
reboot.

`Availability::Absent` is the fifth variant, and `Availability::remedy() -> Remedy` makes the
axis explicit: privilege, a kernel module, a different kernel, something to run it on, or
nothing. `Remedy::root_may_help()` is the escalation gate, true for exactly one variant —
including **not** for `Unclear`, because "we could not tell" is not grounds for asking someone
for their password.

`Capabilities::interface_for()` came out of it too. Without it the GUI would have had to map
`Requirement` to a field itself, which is a second opinion about what a probe needs and would
silently miss a variant added later. `blocker()` now uses it as well, so the mapping exists
once.

**Both front ends were saying the misleading thing, not just the GUI.** The CLI's status column
was `needs {requirement}`, which rendered as *"needs read access to raw disks"* on a machine
with no disks — indistinguishable from a permission problem. Both now name the remedy: *needs a
kernel module*, *needs something to run it on*, *needs privilege*. Three probes, three
different instructions, where before all three read as "you are not allowed".

### Ask for root per probe, never for the app — **decided, slice 1 of 5 done**

The app is never restarted and never holds privilege: one `pkexec usbdiag probe … --json` per
run, as §9 planned. Two decisions taken: **ship a polkit action** (so one prompt covers a
session) and **sticky results** (a probe reading is not erased by the next live capture).

Three facts from this machine shaped the rest:

- **There is no auth caching by default.** `pkaction --verbose org.freedesktop.policykit.exec`
  reports `implicit active: auth_admin`, *not* `auth_admin_keep` — so plain `pkexec` prompts on
  every single probe. One prompt per session needs our own action with `auth_admin_keep`, and
  that file lives in `/usr/share/polkit-1/actions/`, which needs a root install.
- **Escalating the user-local install is a footgun.** `install-local.sh` writes
  `~/.local/bin/usbdiag`, user-owned and user-writable; running that as root means root executes
  a binary anything running as you can rewrite first. So escalation must require a root-owned
  system path — which is also where the policy file has to go. Both facts point the same way:
  **escalation is a feature of the system install**, and the panel should say so as plainly as
  it says *needs something to run it on*.
- **`urb-errors` is not a privilege problem here.** Its remedy is `LoadModule`, so a password
  would authenticate and the probe would still fail. Loading a kernel module is a different act
  from reading one, and deserves its own consent rather than being smuggled into a probe run.
  `root_may_help()` already returns false for it.

**Slice 1 — `probe::Preview` — done.** The blocker was that `--dry-run` could not say what a
probe would do: `plan` refuses on missing privilege, so unprivileged it answered *"needs root"*
and nothing else. A dialog cannot show consequences with that, and authenticating first to find
out is the wrong order.

`Preview` is a second type rather than a `Plan` with `blocked_by` bolted on, because `Plan`'s
guarantee is load-bearing — *"holding one means every check has already passed"* — and `run`
takes a `Plan`. Describing a probe therefore cannot become a way of running one, structurally
rather than by discipline. `approve` produces the `Preview`, `Preview::approve` converts it, and
`plan` is now just those two composed, so the two views cannot drift.

It still refuses what no password fixes: a misspelled target, a mounted disk, a keyboard in the
way. And the existing 20 probe tests were re-pointed through the conversion, which is the proof
the refactor changed no behaviour.

**Slice 2 — sticky results — done.** `model::Measured`, `Snapshot::carry_forward`, and
`report_carrying`. `Options` is `Copy`, so the carried measurements deliberately do **not** live
there — a `Vec` would have cost that and rippled through every call site — and the fold happens
between capture and analysis, which is the only seam where it can work.

**Evidence is carried, never conclusions.** The numbers are folded in *before* the rules run, so
every finding is re-derived against the machine as it is now. Carrying findings instead would
freeze a judgement about a world that may have changed, and nothing downstream could tell.

**Re-enumeration is the invalidator, not a clock.** A wall-clock TTL would be an invented
number, and a reading of a drive nobody has touched for an hour is still true. What actually
breaks a measurement is the device becoming a *different connection* — and
`connected_duration_ms` says exactly that: connected for less time than the measurement is old
means it went away and came back. Unknown duration is kept rather than dropped, because absent
data is not evidence of a reconnect.

**Address reuse is the trap, and this project has been bitten by it twice** (#18, #19: stale
kernel events attributed to a device that arrived later). URB stats are keyed by
`(bus, device_address)` and the kernel reissues addresses, so every entry is re-identified
against `busnum`/`devnum` plus the same connection test, rather than assumed.

Two smaller rules, each with a test: a measurement taken *this* run always beats a carried one,
since two rates for one disk would leave every consumer choosing; and a cycling run is never
carried at all — it is a history rather than a state, and unlike a read rate it cannot be
re-derived, because the port is no longer being cycled.

`Snapshot::carried` is provenance only. Whatever is listed there has already been folded into
the fields above, so a consumer that ignores it sees a consistent snapshot — it just cannot say
how fresh part of it is. **Untested against real hardware**, and unavoidably so: producing a
measurement needs root and a USB disk, which is #23's blocker too. Six unit tests carry it.

**Slice 3 — cooperative cancel — done.** `cancel::Cancel`, and `--stop-on-eof`.

A cancel button cannot work by signalling: an unprivileged parent may not kill a root child. So
the probe agrees to stop instead, and the transport is **stdin EOF** — the parent closes the pipe
to cancel, and a parent that dies produces the identical event, which covers the case nobody
remembers to handle. No signals, no `libc`, no pid to track.

**The check sits between cycles and never inside one.** A cycle writes `1` then `0` to the port;
stopping between those two writes would leave a device switched off, which is the one outcome
this probe must never produce. Cooperation is what buys that guarantee — force could not.

**A stopped run may convict and may never exonerate.** `ReenumerationRun::stopped` decides it.
Intermittency seen in three cycles is intermittency, whatever happened next. But
`LINK_STABLE_UNDER_CYCLING` earns its keep from the *number* of attempts, and reporting it from
an abandoned run would turn "I changed my mind" into "your cable is fine" — the strongest claim
in `diag.rs` on the weakest evidence. Same asymmetry as everywhere else here.

**Opt-in, because stdin is the keyboard.** On a terminal, watching it would swallow what the user
types and never see EOF until Ctrl-D, so an interactive run must not enable this. `--stop-on-eof`
is also rejected outside `probe`, following the rule the sibling flags already follow: a flag that
silently does nothing is worse than an error.

**Verified end to end, unprivileged.** `block::sample` was made interruptible along the way,
which matters beyond tidiness — it is the only cancellable probe that needs no root, so it is the
only way to test the transport at all on this machine. An 8 s sample takes 8.21 s normally and
0.22 s with stdin closed; closing the pipe 1.2 s into a 9 s window ends the child at 1.3 s. The
two probes that motivated the work, `reenumerate` and `throughput`, are still fixtures-and-units
only, for the same reason as #23.

**Slice 4 — the dialog and `pkexec` — done.** `escalate::Helper`, and a run button on the probe
panel.

**A helper the user can rewrite is refused, not warned about.** This was the decision taken before
building it, and the shape of the module follows from it: `Helper` cannot be constructed except by
`find`, which walks the binary *and every directory above it* and requires root ownership with no
write bit for anyone else — replacing a directory replaces everything under it, so a root-owned
binary inside a directory you can rename is a root-owned binary you can swap. A development build
is tried first and honestly refused rather than escalated because it happened to be nearest.
Escalation is therefore a property of the system install, which is the same conclusion the polkit
action reaches from the other direction.

**The answer is on stdout; the exit code only explains its absence.** `usbdiag` exits 1 when it
*found* something, so branching on the code first would turn every real finding into a failure.
A dismissed password prompt is told apart from a broken run for the same reason: changing your
mind is not an error, and it must not put a red message on the screen.

**Consent travels as the flags that carry it and in no other way**, so a front end cannot assert
what it was not given — no `--force`, and the child refuses. The child re-runs the whole gate as
root, so a disk mounted between the dialog and the password is caught by the process that is about
to act rather than by the one that asked. The GUI decides nothing about safety; it asks
`probe::preview`, draws the answer, and passes the request on.

**A button appears only where it can work.** Privilege must be the whole obstacle
(`Remedy::Privilege`), the probe must need no target, and a trusted helper must exist. That
excludes `urb-errors` on this machine (its remedy is `LoadModule`: a password would authenticate
and the probe would still fail), `throughput` with no disk attached, and `reenumerate` always —
cycling a port has to be aimed at one device, so it cannot be offered from a panel about the
machine. Aiming it belongs to the device pane, which is a later slice.

**Four states, each saying only what is true of itself.** The panel's opening line started as one
sentence for both halves of the failure, ending "— see below". On this machine it pointed at
nothing: no probe here is waiting only on privilege, so the install message was correctly
suppressed and the reference dangled. The first wording of the replacement then claimed *"nothing
below is waiting on a password"* — which `reenumerate`, sitting three rows down with `needs
privilege` on it, flatly contradicted. It now says what stands in the way, which is true in every
combination.

`ProbeInfo::takes_a_window`/`takes_cycles` moved the knob table onto the probe, where the
description, the child's command line and the capture options now share it. That found a real gap:
`throughput` takes a window and was missing from the old private list, so the one probe that reads
a disk flat out never said for how long — `--dry-run` now ends *"for 3.0s"*.

**Still unvalidated against hardware**, and this is the slice where that starts to bite: with no
`usbmon` loaded, no USB disk attached and no system install, this machine offers no button at all.
The three states were reached by reading rather than by clicking. What is needed to click one is in
#23.

Remaining slice: **(5)** the polkit action, so one prompt covers a session instead of every run.

### Still out of v1

The substitution workflow ([`docs/01-gui-concept.md`](docs/01-gui-concept.md) §9) — the
strongest reason for the GUI to exist, and deliberately last, since it depends on everything
above.

### A mouse was accusing the charger — **done**

Reported from the running GUI: port0 warned that the charger was not supplying enough, when
nothing was actually wrong. The complaint came with the right principle attached — *it should
only be a warning if the power in is lower than the power out* — and chasing it found two
stacked bugs, the first much worse than the symptom.

**`read_batteries_from` collected every `type = Battery` in `/sys/class/power_supply`, with no
`scope` filter.** A Logitech receiver publishes `hidpp_battery_10`, which the kernel marks
`scope = Device` and which reads `Discharging` for as long as the mouse is awake. So
`not_keeping_up()` was true whenever the mouse was in use, and that single boolean feeds **two**
rules: it raised `BATTERY_DRAINING_ON_AC`, and — this is what was actually seen — it promoted
`PD_SOURCE_BELOW_SINK_CAPABILITY` from Low to Medium. A mouse being moved turned a port into a
warning.

**And the principle was missing even for the right pack.** `BAT0` here reads `Not charging` at
95%, which is what a full battery does: charge current tapers to zero as it fills, and firmware
then lets it drift down a few percent before topping off again. `Battery::ESSENTIALLY_FULL_PCT`
is that band. Unknown capacity is deliberately **not** treated as full, because absent data must
not silence a pack that is measurably losing ground.

`Battery::scope` and `is_system()` now carry the distinction, `not_keeping_up()` documents all
three guards as the false accusations they each are, and the CLI stopped painting a mouse red as
*"discharging on mains"* — a peripheral's cell has nothing to do with the mains, so it now says
so.

Live, port0 went from Medium to **Low**, reading *"Not a fault — the supply is simply smaller
than the port's maximum"*, and `BATTERY_DRAINING_ON_AC` disappeared. The GUI now opens on the
host's genuine `HIGH` instead of on a port that was never faulty.

### The GUI orphaned a `udevadm monitor` on every run — **done**

Found by answering "what is running in the shell": three stray `udevadm monitor` processes,
reparented to systemd-user, idling for hours. One per earlier launch of the viewer.

**The first explanation was wrong, and testing it is what found the bug.** `capture.sh` kills
with `pkill -x`, so the obvious story was that SIGTERM skips destructors while a graceful close
would run `Monitor::drop` and reap the child. So the window was closed by clicking its ✕
instead — and the child survived that too.

`Monitor::drop` is correct and simply unreachable. `MonitorWorker::update` re-queues `In::Wait`
at the end of every pass, so the worker thread is permanently inside a blocking wait; at exit
that thread is killed where it stands, `MonitorWorker` is never dropped, and the kill never
happens. The cleanup was written in the one place guaranteed not to run.

`monitor::Stopper` is the fix: a cloneable handle over the child, held by the **GTK thread**,
which does get to run. The worker hands one over the instant the monitor starts, and
`Component::shutdown` ends it. `stop()` returns whether there was a live child, which is the
only way a test can tell "cleaned up" from "nothing to clean up".

**What this does not cover, verified rather than assumed:** `SIGKILL`, and `SIGTERM` with no
handler, still orphan the child — checked by `pkill`-ing the app afterwards and finding one
left. Nothing in-process can catch those, and the usual answer, `PR_SET_PDEATHSIG`, needs
`libc`, which `usb-probe` does not depend on and which decided the shape of two probes. So
`capture.sh` now reaps orphans itself before launching, and only orphans: a `udevadm` whose
parent is alive belongs to a running instance or to somebody's terminal.

`usbdiag watch` was never affected. Ctrl-C signals the whole process group, so the shell reaps
`udevadm` directly.

### A settings page, and the first setting is a theme override

There is nowhere in `usbdiag-gui` to change anything about the app itself. Add one — and the
setting wanted now is a **theme override**: follow the desktop (today's only behaviour), or
force light, or force dark.

**This cuts against a stated design property, deliberately.** Every colour in `style.css` is a
libadwaita *named* colour specifically so the app follows the system, and both schemes are
screenshotted for that reason. An override does not undo that — `StyleManager::set_color_scheme`
with `ForceLight`/`ForceDark`/`Default` moves the whole palette as one, which is exactly why the
named colours were worth the discipline. What it does mean is that "follows the desktop" stops
being an invariant and becomes a default.

Three things to get right:

- **The chain widget must repaint.** `crates/gui/src/chain.rs` reads
  `StyleManager::default().is_dark()` inside its draw function, so an override that does not
  trigger a redraw leaves a cairo widget in the old palette while every GTK widget around it has
  moved. `connect_dark_notify` is the hook; the existing rebuild path may already cover it, which
  needs checking rather than assuming.
- **Where the preference lives is an open question.** GSettings is the idiomatic answer and costs
  a schema that `install-local.sh` would have to compile and install, which turns a
  copy-two-binaries script into something with a real install step. The alternative is a small
  JSON beside the existing `$XDG_CONFIG_HOME/usbdiag/devices.json`. It must **not** go in
  `devices.json` itself: that file is the library's, it is read by the CLI, and a GUI display
  preference has no business in a capture.
- **There is no menu to put it in.** The header bar has a refresh button and window controls and
  nothing else, so this needs the conventional hamburger → *Settings* / *About*, which is also
  where an About dialog would finally have a home.

Worth noting for whoever builds it: `ADW_DEBUG_COLOR_SCHEME` is how `screenshot-app` already
forces a scheme per process. That is the same mechanism from the outside, so the skill keeps
working regardless, and can still shoot both schemes without touching the new setting.

### Say "not a cable problem" out loud — **done**

`verdict.rs`. Two halves that need each other: exonerations give a clean verdict something
to cite, and without them "nothing found" is an assertion rather than a summary.

**The headline invariant is structural, not a discipline.** A verdict headline is always a
finding's title quoted verbatim, or the fixed `Verdict::NOTHING_FOUND`. There is no branch
that composes a sentence, so "a verdict never makes a new claim" cannot be violated by
someone editing a rule later. A test asserts it against a populated snapshot.

**Exonerations are a separate list on `Report`, not a flag on `Finding`.** The failure mode
worth designing against is one being counted as a fault — swelling a clean report until it
looks dirty, or tripping the exit code. A separate `Vec` makes that impossible rather than
unlikely, and cost nothing: 38 `Finding` literals stayed untouched.

**Info-only subjects are `Clear`, not `Minor`.** Found while writing the first test. Info
means worth knowing and not worth acting on, so grading such a subject as `Minor` invents a
problem out of a note. `Minor` now requires Low or above.

Five exonerations. `CHARGING_AT_FULL_OFFER` and `CABLE_NOT_LIMITING` fire when the contract
equals the charger's best offer — the second is the sentence the tool exists to be able to
say, and it needs **no e-marker**, which is the point on a UCSI platform where cable
identity is never exposed. `LINK_AT_DEVICE_MAXIMUM`, `ALT_MODE_NOT_REQUESTED`, and
`MEDIUM_EXPLAINS_THROUGHPUT`, the last reachable only after `probe throughput` and untested
for the same reason as the rest of that path.

**`bcdUSB` names a specification, not a rate — and this was nearly got wrong twice.** The
guard was written for `2.00`, which 12 Mbps and 480 Mbps devices both claim. The first draft
then mapped `>= 3.1` to 10 Gbps, and hardware here contradicts it: `6-1`, a VIA hub,
declares `bcdUSB 3.10` and links at 5 Gbps into a 10 Gbps port. Only `3.0x` names exactly
one rate, so everything else returns `None` and stays silent. Missing an exoneration costs
nothing; a false "this is as fast as it gets" costs the user the actual fault.

Three noise decisions, each from reading real output rather than reasoning about it:
root hubs get no verdict (eight controllers nobody forms an opinion about); an empty port
gets no alt-mode exoneration (a tautology, one per unused port); and a sinking port needed
its own power statement, because the cable one lives on the cable subject and left port0
headlined by DisplayPort while it was charging at 100 W.

**Validated live.** With the charger attached, port0 read *"Charging at 100 W, the most this
charger offers"* and its cable *"The cable is not what limits charging here"*. The charger
was then unplugged mid-session and both correctly disappeared, `4-1`'s
`LINK_AT_DEVICE_MAXIMUM` surviving unchanged.

~~Still open: the CLI shows `clear` verdicts with an empty `because` only as a count.
Whether a GUI should draw those differently from a cited `clear` is a real question.~~
Settled by the viewer: a `clear` with nothing to cite gets a hollow dot and a plain fact
instead of its headline, because an uncited clean bill of health is a claim the data does
not support.

## Unprivileged work worth doing

### Read SCSI error counters — a passive error signal for storage — **done**

`ScsiCounters` / `ScsiDelta` on `BlockDevice`, read from `/sys/block/<dev>/device/`, plus
the `STORAGE_IO_ERRORS` rule. World-readable, so this reaches part of what
`LINK_ERROR_RATE` reaches with no privilege at all — which matters, because usbmon needs
root and most people running this will not have it.

**Only the delta is judged, and that is the whole design.** Two healthy flash drives on
this machine both read `ioerr_cnt = 2` straight out of discovery: `ioerr_cnt` counts any
command that did not return GOOD, including the kernel probing for an optional feature and
being told no. A rule on the absolute value condemns every storage device on every machine.
So the rule fires only on what moved during a sampling window, which means only when the
caller passed `--sample` — the same shape as the throughput rules. Two tests pin it: one
that the discovery baseline is never an accusation, one that an unsampled capture stays
silent even with 9000 errors on the clock.

**A timeout carries its own weight.** `iotmo_cnt` is a command that never came back, which
is a transport failure rather than a device declining something, so it goes straight to
High. Errors alone are graded by rate against real traffic — 0.1 % is ordinary, 2 % is not
— and errors with *no* requests in the window are not judged at all, because the counter
moved for traffic nobody watched.

**The counters are hex** (`0x243`), which is the kind of thing that fails silently: `0x243`
read as decimal fails outright, but `0x20` read as 20 is wrong by a factor of sixteen and
looks plausible. `read_hex_u64` and its own test.

Its own code rather than folding into `LINK_ERROR_RATE`, so the two sources are never
conflated: one is URB status off the bus, this is the SCSI layer's own accounting.

The absolute counters are *shown* in the storage view, with the caveat attached — a small
non-zero `ioerr` is what healthy looks like — and an idle sampling window is reported as
idle rather than as clean, since a clean bill of health nobody earned is the failure mode
this project keeps returning to.

**Not yet seen against a real drive.** The reader is exercised end to end against a
synthetic sysfs tree carrying the exact hex values captured from the SanDisk here, and the
NVMe-has-no-such-directory case is covered, but no USB storage has been attached since the
rule was written. `usbdiag devices --sample 2000` with a drive plugged in settles it; the
expected result on healthy hardware is a `while watching` line reading *clean* and **no**
finding.

### Name vendors from `usb.ids`, and flag e-markers that look counterfeit — **done**

`usbids.rs` and `trust.rs`, plus `CABLE_IDENTITY_UNUSUAL`.

**Vendor names, read not bundled.** `/usr/share/misc/usb.ids`, 728 KB, parsed once per
process behind a `OnceLock` and only when something asks. It cannot go stale, adds no data
file, raises no licence question, and is better data than a bundled table: it names
`27c6  Shenzhen Goodix Technology Co.,Ltd.` where `usbeehive` falls back to hex. Absence is
non-fatal — hex is the correct answer with no database installed.

Two parsing traps, both tested. Everything after `C 00` is the device-class list and reuses
the same indentation for entirely different numbers, so reading past it files audio class
codes as vendors. And two-tab lines are interface names under a product, not products.

**The naming rule is: sysfs first, database only where sysfs said nothing.** A database
entry must never override what the hardware reports. The visible win on this machine is the
hub, whose `manufacturer` is absent entirely — `USB2.0 Hub` becomes `Genesys Logic, Inc.
USB2.0 Hub`. That immediately produced its own bug: the webcam also reports no manufacturer
and a product of `Logitech StreamCam`, which became `Logitech, Inc. Logitech StreamCam`. A
product string that already carries the brand is left alone, matched on the vendor's first
word of three characters or more.

**The trust signals are the most dangerous thing here**, and the design is about keeping
them weak. Heuristic always, Info for one signal and Low for two or more, and the word
*counterfeit* appears nowhere — there is a test asserting the output contains none of
counterfeit/fake/clone/fraud/stolen and *does* contain the innocent explanation.

Both conditional signals refuse to run rather than guess:

- **Unknown vendor** is gated on `UsbIds::available()`. With no database every vendor is
  unknown and the signal would fire on every cable on the machine.
- **Reserved bits** are gated on the cable being known *passive*. `vdo.rs` deliberately
  reports "Passive or Active Cable (ambiguous without PD revision)" when it cannot tell, and
  that uncertainty is inherited: the two VDO layouts put different fields at the same
  offsets. Only bits reserved under *both* PD 2.0 and PD 3.x are examined — B20, B17, B8,
  B7. B4..B3 are deliberately skipped and `bits_not_checked()` says so, because PD 2.0 uses
  them for SuperSpeed directionality and a healthy PD 2.0 cable has them set.

**Writing this exposed a fixture bug.** `test_support`'s "healthy e-marked cable" built its
ID Header as `0b011 << 27` and left the low sixteen bits at zero — which is not spare space,
it is the vendor id. The fixture had been declaring vendor 0000 all along, and the new rule
correctly flagged it. It now carries a real registered id.

Still untestable against hardware, as predicted: UCSI exposes no cable node, so every line
of the trust half runs only against fixtures here. Same category as #12 and #23. The vendor
naming half *is* live and validated against the four real vendors in this laptop.

### Classify devices, but only let a guess suppress a finding — **done**

`kind.rs`. `DeviceKind` (15 kinds) plus `KindSource` (`class` | `heuristic` | `user`),
and `Kind::asserted()` as the single place the suppress-only rule is enforced: it returns
a kind **only** when the device's own descriptors said so, and it is the only accessor a
rule may use as grounds for a new finding. `Kind::kind` is the best available answer,
guesses included, and is for display and for staying quiet.

The asymmetry is not stylistic. A wrong guess that suppresses costs a missed detection. A
wrong guess that accuses costs a false accusation against hardware the user then goes and
replaces — the failure this whole project exists to avoid. Only one is recoverable.

**Computed, not stored.** `usb_version_num` is the precedent for a derived field living in
the struct, but it is a parse of one attribute and cannot drift. A classification reads
`device_class`, the interface list and `is_root_hub` together, and the test builders mutate
all three after construction — `root_hub_version` sets `device_class` on an already-built
device. A stored kind would have been silently stale there, so it is a method.

**Billboard deliberately stays a raw interface check.** It is the one class code left in the
rule engine. A Billboard is a symptom, not an identity: a dock that also exposes storage
classifies as storage, and asking "what is this device" would lose the failure report the
class exists to carry. Everything that genuinely asks what a device *is* now goes through
`kind`, including the GUI's hub-collapsing, which had grown its own copy of the hub class
code.

**Composite devices are ranked, not first-wins.** A webcam is video plus audio plus HID; a
headset is audio plus HID. Interface order must not decide the answer, so the kinds have a
total order and the most specific wins — with a test asserting the order is total and
another asserting enumeration order is irrelevant.

**A composite device, validated live.** A Logitech StreamCam plugged in later is the case
the ranking exists for: `bDeviceClass ef` defers, and the interface list is video, video,
audio, audio, vendor-specific, HID. It comes out **camera**, which is what the synthetic
test predicted and what a person would say.

**Against real hardware it names every attached device but one.** The hub, the Bluetooth
radio (`bDeviceClass ef` deferring to three `0xe0` interfaces) and the smart-card reader all
answer. The Goodix fingerprint reader is `ef` over `ff` — Miscellaneous over Vendor
Specific, the class-code equivalent of a shrug — and comes out `Unknown` rather than being
forced into a bucket. That is the one device the string heuristics would be for.

Rendering it turned up a small thing worth keeping: printing the kind next to the raw class
gave a hub "hub" three times over. `DeviceKind::from_class` lets a renderer tell "this class
code names the same thing" from "this class code declined to answer", so `miscellaneous`
survives on the Bluetooth radio — where it explains *why* the device class deferred — and
the duplicate `hub` does not.

**The string-heuristic half is not built**, and no `KindSource::Heuristic` is produced yet.
The enforcement point exists and is tested against a hand-made guess, so the half that
carries risk cannot land without going through it. What it needs first is a second device
worth guessing about; one fingerprint reader is not a corpus.

### Let the user correct a device's type, and remember it — **done**

`overrides.rs`, `usbdiag label` / `usbdiag labels`, `--no-overrides`, and a *What this is*
card in the GUI with the kind, where it came from, and a dropdown to change it.

**The rule is the inverse of the one for string guesses, and that was got wrong first.**
`KindSource::User` was written as *not* evidence, which would have made a correction able
only to silence a rule. The design here says the opposite and is right: the user is holding
the object, so a declaration may **sharpen** a finding. What it may not do is claim to have
been measured, so `Kind::cap` weakens any finding that leaned on one to `Inferred` and the
finding cites the label by id. Fixed with the tests inverted to match.

**The justifying case works end to end.** On a High-Speed link an unknown medium is not
judgeable at all — a cheap flash drive and a bad cable look identical — so the rule stays
silent. Declare the disk rotating and 8 MB/s against a ~120 MB/s platter becomes an
unambiguous finding, at `Inferred`, citing `declared by you for 1234:5678 (this model)`.
Two tests: one that the silence becomes an answer, one that the label cannot invent a fault
where the measurement is fine.

**Placeholder serials are refused, with real ones as the test corpus.** `usable_serial`
rejects empty, shorter than four characters, and one character repeated — which is exactly
what catches the MediaTek `000000000` and the Dell DA20 `00000000000000000` on this machine
while accepting the three genuine serials. Confirmed live: `usbdiag label 3-5.1 --this-one`
refuses and explains, rather than silently labelling every zero-serial device ever plugged
in.

**A bug worth remembering: `owner_of` picked the wrong device.** A disk at
`.../usb4/4-1/host1/block/sdb` sits inside both `usb4` and `4-1`, and the first attempt took
the *longest name* — which is `usb4`, the root hub, the wrong answer. It is the *deepest*
match that owns the disk. Now keyed on position in the path, with its own test.

**And one in the GUI: the guard that ate the first click.** `set_selected` is called before
`connect_selected_notify`, so building the row cannot fire the handler — but a defensive
"skip the initial notify" guard was added anyway, and with nothing to skip on construction it
swallowed the user's first real choice instead. Found by clicking the control and watching
the file not change. Replaced by the check that is actually needed: the pane rebuilds on
every capture, so re-selecting the stored value must not rewrite the file.

**Capture is no longer a pure function of the machine**, which is the cost of the feature.
`Options::overrides` defaults on, `--no-overrides` gets the unmodified view, and every
applied label is serialized onto the device so a JSON consumer can tell a reading from a
declaration. Without both, "why does it say that" is unanswerable.

`serde_json` became a real dependency of `usb-probe` (10 crates -> 14). The shipped binary is
unchanged at 13, since `usbdiag` already depended on it.

Still deliberately absent: a per-unit control in the GUI. `--this-one` exists in the CLI, but
choosing between "this model" and "this one" is a question most people should not have to
answer, and the model default is right far more often.

---

## Hand out a binary, or keep handing out nothing? — **discuss first**

**Nothing here is decided.** Written down while CI was being built, to be talked through
rather than acted on.

CI compiles both binaries and runs one of them, and then throws them away: `target/` dies
with the runner and the only uploaded artifact is the smoke run's `snapshot.json`. So the
pipeline currently answers *"does this compile, lint, test and run"* and says nothing about
distribution. `scripts/install-local.sh` is still the only thing that produces a release
build, and it runs on the user's own machine.

The two binaries are not the same problem.

- **`usbdiag` is a strong candidate for a static musl build.** It has no native
  dependencies at all — that is why `throughput.rs` hand-defines `O_DIRECT` per
  architecture instead of taking `libc`, and why the shipped tree is 13 crates. A single
  file that can be copied onto whatever machine is misbehaving and run with nothing
  installed is close to the ideal shape for this tool, since the machine in question is by
  definition the one having trouble. `x86_64-unknown-linux-musl` should need no code
  changes, but that is an expectation and not a result — build it before believing it.
- **`usbdiag-gui` cannot be handed out the same way.** It links GTK4 and libadwaita
  dynamically, so a build is only good for a matching distribution. That means per-distro
  artifacts, or Flatpak — and [`docs/01-gui-concept.md`](docs/01-gui-concept.md) §10 already
  argues Flatpak is the wrong primary target for something that reads `/sys` and
  `/sys/kernel/debug`, because the sandbox has to be opened wide enough to be decorative.

Open questions, all of them genuine:

- Artifacts on every run, only on tags, or both? Per-run artifacts expire and are mostly
  noise; tagged releases are the thing people can link to.
- Does a `.deb` earn its maintenance, given `install-local.sh` already covers the native
  path without root?
- **arm64 is the interesting gap.** `ubuntu-24.04-arm` runners exist, and the `aarch64`
  branch of the `O_DIRECT` constant ships today without ever having been compiled, let
  alone run. A cross build would be worth more than a downloadable x86 binary.
- Whether any of this should wait for the GUI's out-of-v1 work to land, since the
  substitution workflow is the reason a non-developer would want a binary in the first
  place.

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

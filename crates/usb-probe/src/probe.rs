//! The gate every active probe passes through.
//!
//! [`caps`](crate::caps) answers *may this process*. This module answers *may
//! this process, to this device, right now, having been asked*. Nothing that
//! touches the bus runs without going through [`plan`] first, and [`plan`]
//! touches nothing itself — it decides, and returns either a [`Plan`] or a
//! [`Refusal`] that says exactly why.
//!
//! Keeping the decision here rather than in the command-line front end is
//! deliberate: the safety rules are not a property of one user interface. A GUI
//! that grew its own version of "is this disk mounted" would eventually get it
//! wrong, and getting it wrong means pulling a mounted filesystem off the bus.
//!
//! # Consent is proportional to consequence
//!
//! The original sketch for this had every probe refuse until a confirmation
//! flag was passed. That was rejected while building it, because a flag that is
//! required even for a probe that only reads becomes a reflex — typed without
//! thought, and therefore worthless on the one probe where it matters. So:
//!
//! * **passive** — runs unasked, as it always has;
//! * **privileged, read-only** — runs when named. Naming it *is* the request,
//!   and there is nothing to undo afterwards;
//! * **disruptive** — needs consent, and separately needs the interruption
//!   acknowledged. Two deliberate acts, because it takes a device off the bus.
//!
//! # One refusal that consent cannot lift
//!
//! A disruptive probe against a disk holding a mounted filesystem or an active
//! swap area is refused outright, however many times the user says yes. Consent
//! covers what the user knows they are doing; nobody consents to losing a
//! filesystem they had forgotten was mounted.

use std::time::Duration;

use serde::Serialize;

use crate::block::{self, Hold};
use crate::caps::{Capabilities, ProbeClass, ProbeInfo};
use crate::model::Snapshot;
use crate::Options;

/// What the caller is asking for.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub name: &'a str,
    /// A USB sysfs name (`6-1.2`) or a block device name (`sdb`), which is
    /// resolved back to the USB device it hangs off.
    pub target: Option<&'a str>,
    /// How long the probe should run, where it takes a window at all.
    pub window: Duration,
    /// How many times to cycle the port, for the probe that cycles one.
    /// Separate from `window` because a cycle takes as long as the hardware
    /// takes; the useful control is how many attempts, not for how long.
    pub cycles: usize,
    /// The user asked for something out of band, knowingly.
    pub consented: bool,
    /// The user separately accepted that this one interrupts the device.
    pub accepts_disruption: bool,
}

impl<'a> Request<'a> {
    pub fn new(name: &'a str, window: Duration) -> Self {
        Self {
            name,
            target: None,
            window,
            cycles: DEFAULT_CYCLES,
            consented: false,
            accepts_disruption: false,
        }
    }
}

/// Enough attempts to see a one-in-ten fault, few enough to sit through.
pub const DEFAULT_CYCLES: usize = 20;

/// A device a probe has been pointed at, resolved against a real snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub sysfs_name: String,
    pub label: String,
    /// Disks reached through this device, if any.
    pub blocks: Vec<String>,
    /// A root hub has no upstream port of its own, and everything on the bus
    /// hangs off it.
    pub is_root_hub: bool,
}

/// An approved probe. Holding one means every check has already passed.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub probe: &'static ProbeInfo,
    pub target: Option<Target>,
    /// Serialised as `window_ms`: serde renders a `Duration` as seconds plus
    /// nanoseconds, which is a poor thing to put in front of another program.
    #[serde(rename = "window_ms", serialize_with = "millis")]
    pub window: Duration,
    pub cycles: usize,
    /// What else stops working while this runs. Not reasons to refuse — things
    /// the user should know before saying yes the second time, which is what
    /// the second confirmation is for.
    pub side_effects: Vec<String>,
}

/// The one sentence both [`Plan`] and [`Preview`] open with.
///
/// Shared rather than written twice: a confirmation dialog that described a
/// probe differently from the run that follows it would be the most expensive
/// possible place for the two to drift apart.
fn describe_probe(
    probe: &ProbeInfo,
    target: &Option<Target>,
    window: Duration,
    cycles: usize,
) -> String {
    let mut s = format!("{} — {}", probe.name, probe.class.label());
    if let Some(t) = target {
        s.push_str(&format!(", on {} ({})", t.sysfs_name, t.label));
    }
    if probe.takes_a_window() {
        s.push_str(&format!(", for {:.1}s", window.as_secs_f64()));
    }
    if probe.takes_cycles() {
        s.push_str(&format!(", {cycles} times"));
    }
    s.push_str(match probe.class {
        ProbeClass::Passive | ProbeClass::PrivilegedRead => ". Nothing on the bus changes.",
        ProbeClass::Disruptive => ". The device leaves the bus and comes back when this finishes.",
    });
    s
}

/// What a probe *would* do, and what stands between it and doing it.
///
/// [`Plan`] deliberately means "approved" — holding one is proof every check
/// passed — so this is a separate type rather than a `Plan` with the guarantee
/// weakened. A front end needs to describe consequences **before** asking for a
/// password or a confirmation, and [`plan`] cannot serve that: it refuses on
/// missing privilege, so an unprivileged caller learns "needs root" and never
/// learns what it was agreeing to.
///
/// That ordering is not a flaw in `plan` — the refusals it gives first are the
/// ones the user can fix without escalating, which is worth more than a sudo
/// round trip to discover a misspelled target. It is simply the wrong question
/// for a confirmation dialog, so this asks a different one.
///
/// **Not runnable.** [`run`] takes a `Plan`, and the only way to get one is
/// [`plan`], which refuses everything this records. Describing a probe can
/// therefore never become a way of running it.
#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub probe: &'static ProbeInfo,
    pub target: Option<Target>,
    #[serde(rename = "window_ms", serialize_with = "millis")]
    pub window: Duration,
    pub cycles: usize,
    pub side_effects: Vec<String>,
    /// Why this process cannot run it. `None` means privilege is not the
    /// obstacle — which is not the same as it being runnable, since consent may
    /// still be outstanding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
    /// Consents not yet given. Empty for the read-only probes, which are
    /// requested by name and leave nothing behind.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub awaiting: Vec<Consent>,
}

impl Preview {
    /// True when nothing is outstanding, so [`plan`] would hand back a `Plan`.
    pub fn runnable(&self) -> bool {
        self.blocked_by.is_none() && self.awaiting.is_empty()
    }

    /// Whether escalating to root would clear the way.
    ///
    /// The gate for an escalation prompt, and the reason [`crate::caps::Remedy`]
    /// exists: a probe blocked because no disk is attached must never be offered
    /// a password dialog, since no password attaches a disk.
    pub fn root_may_help(&self, caps: &Capabilities) -> bool {
        self.blocked_by.is_some() && caps.remedy(self.probe).root_may_help()
    }

    /// What will happen, with no account of what is standing in the way.
    ///
    /// The half a confirmation dialog wants: the obstacle is the *reason the
    /// dialog is open*, and repeating it inside the question reads as an argument
    /// against the button beside it. A terminal explaining why nothing ran wants
    /// [`Preview::describe`] instead.
    pub fn what_it_does(&self) -> String {
        let mut s = describe_probe(self.probe, &self.target, self.window, self.cycles);
        for effect in &self.side_effects {
            s.push_str(&format!("\n  · {effect}"));
        }
        s
    }

    /// The same sentence [`Plan::describe`] gives, plus what is outstanding.
    pub fn describe(&self) -> String {
        let mut s = self.what_it_does();
        if let Some(why) = &self.blocked_by {
            s.push_str(&format!("\n  ! cannot run as this process: {why}"));
        }
        for c in &self.awaiting {
            s.push_str(&format!("\n  ? awaiting consent: {}", c.slug()));
        }
        s
    }
}

impl Plan {
    /// What is about to happen, in the words the user should see before it
    /// does. Every probe says its class out loud, so "disruptive" is never a
    /// surprise discovered afterwards.
    pub fn describe(&self) -> String {
        let mut s = describe_probe(self.probe, &self.target, self.window, self.cycles);
        for effect in &self.side_effects {
            s.push_str(&format!("\n  · {effect}"));
        }
        s
    }


    /// Fold the plan into capture options.
    ///
    /// The probes that are only a matter of *reading more* need no separate
    /// execution path — they are the ordinary capture with one more source
    /// switched on.
    pub fn capture_options(&self, base: Options) -> Options {
        let ms = self.window.as_millis() as u64;
        match self.probe.name {
            "storage-sample" => Options {
                storage_sample_ms: ms,
                ..base
            },
            "urb-errors" => Options {
                urb_sample_ms: ms,
                ..base
            },
            _ => base,
        }
    }
}

fn millis<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(d.as_millis() as u64)
}

/// Carry out an approved plan and analyse the result.
///
/// Takes a [`Plan`] rather than a name, so there is no way to reach this
/// without having passed [`plan`] first — the type is the proof that consent,
/// capability and the mounted-filesystem check have all already happened.
pub fn run(plan: &Plan, base: Options) -> crate::model::Report {
    run_until(plan, base, &crate::cancel::Cancel::never())
}

/// The same run, stoppable part-way through.
///
/// A privileged probe launched by a front end is a root child of an unprivileged
/// parent, which cannot signal it — so cancelling has to be something the probe
/// agrees to. See [`crate::cancel`] for why that is the only shape available, and
/// for the stdin-EOF transport that also covers the parent dying.
pub fn run_until(
    plan: &Plan,
    base: Options,
    cancel: &crate::cancel::Cancel,
) -> crate::model::Report {
    let mut snapshot = crate::capture_until(plan.capture_options(base), cancel);

    if plan.probe.name == "throughput" {
        let only = plan.target.as_ref().map(|t| t.blocks.clone());
        for disk in crate::throughput::targets(&snapshot, only.as_deref()) {
            // Between disks as well as inside a read: a stop asked for during the
            // first of four drives should not have to wait out the other three.
            if cancel.stopped() {
                break;
            }
            match crate::throughput::measure(&disk, plan.window, cancel) {
                Ok(sample) => snapshot.throughput.push(sample),
                // A disk that cannot be opened is reported as a sample that
                // failed, not omitted — a silently missing row reads as "there
                // was nothing to measure".
                Err(e) => snapshot.throughput.push(crate::model::ThroughputSample {
                    device: disk,
                    error: Some(e.to_string()),
                    ..Default::default()
                }),
            }
        }
    }

    if plan.probe.name == "reenumerate" {
        // Both unwraps are structural: the gate refuses this probe without a
        // target, and refuses a target with no hub port behind it.
        if let Some(target) = &plan.target {
            match crate::reenumerate::port_for(&target.sysfs_name) {
                None => {
                    // The gate already refused root hubs, so reaching here
                    // means the device went away between capture and probe.
                    snapshot.reenumeration = Some(crate::model::ReenumerationRun {
                        device: target.sysfs_name.clone(),
                        requested_cycles: plan.cycles,
                        error: Some(format!(
                            "{} has no hub port to cycle any more — it looks to have been \
                             unplugged since the scan",
                            target.sysfs_name
                        )),
                        ..Default::default()
                    });
                }
                Some(port) => {
                    let run =
                        crate::reenumerate::cycle(&port, &target.sysfs_name, plan.cycles, cancel);
                    snapshot.reenumeration = Some(run);
                    // The device has been on and off the bus several times, so
                    // everything read before is stale. Read it again.
                    let fresh = crate::capture(base);
                    snapshot = Snapshot {
                        reenumeration: snapshot.reenumeration,
                        ..fresh
                    };
                }
            }
        }
    }

    crate::diag::report(snapshot)
}

/// One probe as a caller sees it: what it is, and whether it could run here.
///
/// The registry and the capability check are joined here rather than left for
/// the caller to combine, because combining them is exactly the step a front
/// end would get subtly wrong — and getting it wrong means offering a button
/// that cannot work.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeStatus {
    #[serde(flatten)]
    pub info: &'static ProbeInfo,
    /// Nothing stands in the way right now.
    pub ready: bool,
    /// What does, when something does.
    pub blocker: Option<String>,
}

/// Every probe, with this machine's answer for each.
pub fn catalogue(caps: &Capabilities) -> Vec<ProbeStatus> {
    crate::caps::PROBES
        .iter()
        .map(|info| {
            let blocker = if info.implemented {
                caps.blocker(info)
            } else {
                Some(format!("{} is not implemented yet", info.name))
            };
            ProbeStatus {
                info,
                ready: blocker.is_none(),
                blocker,
            }
        })
        .collect()
}

/// Why a probe will not run. Every variant names the thing that would fix it,
/// except the one that nothing fixes.
#[derive(Debug, Clone)]
pub enum Refusal {
    NoSuchProbe {
        name: String,
        known: Vec<&'static str>,
    },
    NotImplemented(&'static ProbeInfo),
    /// The interface it needs is missing, or this process may not use it.
    Unavailable {
        probe: &'static ProbeInfo,
        why: String,
    },
    NoSuchTarget {
        name: String,
    },
    /// A disruptive probe swept across the whole bus is never reasonable.
    TargetRequired(&'static ProbeInfo),
    /// The refusal consent cannot lift.
    InUse {
        target: String,
        holds: Vec<Hold>,
    },
    /// The other one it cannot lift: taking this device off the bus would
    /// remove the user's means of stopping what happens next.
    CriticalDevice {
        target: String,
        uses: Vec<String>,
    },
    /// A root hub: it has no upstream port to cycle, and everything on the bus
    /// hangs off it.
    WholeBus {
        target: String,
    },
    /// The only recoverable one: the user has not said yes yet.
    NeedsConsent {
        probe: &'static ProbeInfo,
        what: Consent,
    },
}

/// A refusal in a shape another program can act on.
///
/// Written out by hand rather than derived from [`Refusal`]. A derived
/// representation would follow the enum, and the enum exists to make the
/// *decision* clear — variants get added and renamed as the rules sharpen. This
/// is the wire format, and the point of a wire format is that it does not move
/// when the code behind it does.
///
/// Deliberately flat: a caller reading `code` and `recoverable` has everything
/// it needs to decide between showing an error and showing a confirmation, with
/// no nested matching.
#[derive(Debug, Clone, Serialize)]
pub struct RefusalReport {
    /// Stable slug. The one field a caller should branch on.
    pub code: &'static str,
    /// Whether consent would change the answer. `false` means every other
    /// field is an explanation, not a negotiation.
    pub recoverable: bool,
    /// The same sentence a human would be shown.
    pub message: String,
    pub probe: Option<&'static str>,
    pub target: Option<String>,
    /// Which act of consent is missing: `to_probe` or `to_disrupt`.
    pub consent: Option<&'static str>,
    /// Filesystems and swap keeping a disk busy, for `in_use`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub holds: Vec<Hold>,
    /// Input devices in the way, for `critical_device`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// Every probe there is, for `no_such_probe`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub known_probes: Vec<&'static str>,
}

impl Refusal {
    /// Stable identifier for this kind of refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NoSuchProbe { .. } => "no_such_probe",
            Refusal::NotImplemented(_) => "not_implemented",
            Refusal::Unavailable { .. } => "unavailable",
            Refusal::NoSuchTarget { .. } => "no_such_target",
            Refusal::TargetRequired(_) => "target_required",
            Refusal::InUse { .. } => "in_use",
            Refusal::CriticalDevice { .. } => "critical_device",
            Refusal::WholeBus { .. } => "whole_bus",
            Refusal::NeedsConsent { .. } => "needs_consent",
        }
    }

    pub fn report(&self) -> RefusalReport {
        let mut r = RefusalReport {
            code: self.code(),
            recoverable: self.is_recoverable(),
            message: self.message(),
            probe: None,
            target: None,
            consent: None,
            holds: Vec::new(),
            uses: Vec::new(),
            known_probes: Vec::new(),
        };
        match self {
            Refusal::NoSuchProbe { known, .. } => r.known_probes = known.clone(),
            Refusal::NotImplemented(p) | Refusal::TargetRequired(p) => r.probe = Some(p.name),
            Refusal::Unavailable { probe, .. } => r.probe = Some(probe.name),
            Refusal::NoSuchTarget { name } => r.target = Some(name.clone()),
            Refusal::InUse { target, holds } => {
                r.target = Some(target.clone());
                r.holds = holds.clone();
            }
            Refusal::CriticalDevice { target, uses } => {
                r.target = Some(target.clone());
                r.uses = uses.clone();
            }
            Refusal::WholeBus { target } => r.target = Some(target.clone()),
            Refusal::NeedsConsent { probe, what } => {
                r.probe = Some(probe.name);
                r.consent = Some(match what {
                    Consent::ToProbe => "to_probe",
                    Consent::ToDisrupt => "to_disrupt",
                });
            }
        }
        r
    }
}

/// Which of the two acts of consent is missing.
///
/// The slugs match the `consent` field of [`RefusalReport`], so a front end that
/// already branches on one can branch on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Consent {
    /// Agreement to run an out-of-band probe at all.
    ToProbe,
    /// Separate acceptance that this one interrupts the device.
    ToDisrupt,
}

impl Consent {
    pub fn slug(&self) -> &'static str {
        match self {
            Consent::ToProbe => "to_probe",
            Consent::ToDisrupt => "to_disrupt",
        }
    }
}

impl Refusal {
    /// Whether saying yes would change the answer. A front end should offer a
    /// confirmation only for these, and never for the rest.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Refusal::NeedsConsent { .. })
    }

    pub fn message(&self) -> String {
        match self {
            Refusal::NoSuchProbe { name, known } => {
                format!("no probe called '{name}' — known probes: {}", known.join(", "))
            }
            Refusal::NotImplemented(p) => format!(
                "'{}' is registered but not implemented yet, so it will not pretend to run",
                p.name
            ),
            Refusal::Unavailable { why, .. } => why.clone(),
            Refusal::NoSuchTarget { name } => format!(
                "no USB device or disk called '{name}' — use the sysfs name from the device \
                 tree, such as 6-1.2, or a block device name such as sdb"
            ),
            Refusal::TargetRequired(p) => format!(
                "'{}' is disruptive, so it must be pointed at one device with --target rather \
                 than swept across the bus",
                p.name
            ),
            Refusal::InUse { target, holds } => format!(
                "refusing to run a disruptive probe on {target}: {}. Unmount it first — this \
                 refusal cannot be overridden, because taking the device off the bus would \
                 take the filesystem with it",
                holds
                    .iter()
                    .map(|h| h.describe())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Refusal::CriticalDevice { target, uses } => format!(
                "refusing to cycle {target}: {}. Disabling that port takes the input device \
                 with it, so if anything then goes wrong there is no key left to press. This \
                 refusal cannot be overridden — move the device to another port, or name a \
                 different target",
                uses.join(", ")
            ),
            Refusal::WholeBus { target } => format!(
                "{target} is a root hub. It has no upstream port to cycle, and cycling the \
                 bus itself would take every device on it down at once — name one of the \
                 devices below it instead"
            ),
            Refusal::NeedsConsent { probe, what } => match what {
                Consent::ToProbe => format!(
                    "'{}' does more than read the system's own records, so it needs to be \
                     agreed to explicitly",
                    probe.name
                ),
                Consent::ToDisrupt => format!(
                    "'{}' takes the device off the bus and back on again. Anything reading or \
                     writing it at the time will see it disappear",
                    probe.name
                ),
            },
        }
    }
}

/// Decide whether a probe may run, and against what.
///
/// Reads `/proc/self/mounts` fresh rather than trusting the snapshot, because a
/// filesystem can be mounted between capture and probe, and the stale answer is
/// the dangerous one.
pub fn plan(caps: &Capabilities, snap: &Snapshot, req: &Request) -> Result<Plan, Refusal> {
    preview(caps, snap, req)?.approve()
}

/// Describe what the probe would do, without refusing for missing privilege or
/// missing consent.
///
/// The call a front end makes *before* prompting for anything. See [`Preview`]
/// for why this is a separate entry point rather than a flag on [`plan`], and
/// note what it still refuses: a misspelled target, a mounted disk, a keyboard
/// in the way. Those are facts about the request, and no password or dialog
/// changes them.
pub fn preview(caps: &Capabilities, snap: &Snapshot, req: &Request) -> Result<Preview, Refusal> {
    let Some(probe) = crate::caps::probe(req.name) else {
        return Err(Refusal::NoSuchProbe {
            name: req.name.to_string(),
            known: crate::caps::PROBES.iter().map(|p| p.name).collect(),
        });
    };

    // Read now, not at capture time. A filesystem can be mounted in between,
    // and the stale answer is the dangerous one.
    approve(probe, caps, snap, req, &block::holders())
}

impl Preview {
    /// Turn a clear preview into a [`Plan`], or say what is in the way.
    ///
    /// The single place the two views are reconciled, so they cannot disagree
    /// about what counts as approved. Privilege is reported before consent for
    /// the same reason it is checked last: being asked to confirm something that
    /// would then be refused anyway is worse than being told it cannot run.
    pub fn approve(self) -> Result<Plan, Refusal> {
        if let Some(why) = self.blocked_by {
            return Err(Refusal::Unavailable {
                probe: self.probe,
                why,
            });
        }
        if let Some(what) = self.awaiting.first().copied() {
            return Err(Refusal::NeedsConsent {
                probe: self.probe,
                what,
            });
        }
        Ok(Plan {
            probe: self.probe,
            target: self.target,
            window: self.window,
            cycles: self.cycles,
            side_effects: self.side_effects,
        })
    }
}

/// The decision proper, with the disk state passed in.
///
/// Separated from [`plan`] so it can be tested against a probe that does not
/// exist and disks that are not there. Both disruptive probes are unwritten, so
/// without this the code guarding them would ship having never once run.
fn approve(
    probe: &'static ProbeInfo,
    caps: &Capabilities,
    snap: &Snapshot,
    req: &Request,
    holders: &std::collections::BTreeMap<String, Vec<Hold>>,
) -> Result<Preview, Refusal> {
    // First of all the checks: telling someone to load a kernel module, or to
    // add --target, for a probe that then turns out not to be written yet
    // would send them down a dead end.
    if !probe.implemented {
        return Err(Refusal::NotImplemented(probe));
    }

    let disruptive = probe.class == ProbeClass::Disruptive;
    let target = match req.target {
        Some(name) => Some(resolve_target(snap, name).ok_or_else(|| Refusal::NoSuchTarget {
            name: name.to_string(),
        })?),
        None if disruptive => return Err(Refusal::TargetRequired(probe)),
        None => None,
    };

    // Both absolute checks come before consent, not after: being told the disk
    // is mounted is more use than being told to confirm something that would
    // then be refused anyway.
    let mut side_effects = Vec::new();
    if disruptive {
        if let Some(t) = &target {
            let holds: Vec<Hold> = t
                .blocks
                .iter()
                .filter_map(|b| holders.get(b))
                .flatten()
                .cloned()
                .collect();
            if !holds.is_empty() {
                return Err(Refusal::InUse {
                    target: t.sysfs_name.clone(),
                    holds,
                });
            }

            if t.is_root_hub {
                return Err(Refusal::WholeBus {
                    target: t.sysfs_name.clone(),
                });
            }

            // The other thing consent cannot cover. Taking a keyboard off the
            // bus removes the means of interrupting whatever happens next, so
            // agreeing to it is not something a user can meaningfully do.
            let inputs = crate::reenumerate::input_devices(snap, &t.sysfs_name);
            if !inputs.is_empty() {
                return Err(Refusal::CriticalDevice {
                    target: t.sysfs_name.clone(),
                    uses: inputs,
                });
            }

            side_effects = crate::reenumerate::side_effects(snap, &t.sysfs_name);
        }
    }

    // Privilege and consent are *recorded* rather than raised, and [`plan`]
    // turns them into refusals. Everything above this line is a fact about the
    // request that no privilege and no amount of agreeing would change, which is
    // why those stay hard errors even when only describing.
    //
    // Privilege comes after everything about the request itself, because the
    // request errors are the ones the user can fix without escalating. Answering
    // "you need root" first costs them a sudo round trip to discover that the
    // target was misspelled, or that the disk is mounted and it was never going
    // to work at any privilege.
    let blocked_by = caps.blocker(probe);

    // Only the disruptive class asks. A read-only probe was requested by name
    // and leaves nothing behind, so a confirmation there would be ceremony —
    // and the reflex it teaches is what makes the confirmation worth anything.
    let mut awaiting = Vec::new();
    if disruptive {
        if !req.consented {
            awaiting.push(Consent::ToProbe);
        }
        if !req.accepts_disruption {
            awaiting.push(Consent::ToDisrupt);
        }
    }

    Ok(Preview {
        probe,
        target,
        window: req.window,
        cycles: req.cycles.clamp(2, 100),
        side_effects,
        blocked_by,
        awaiting,
    })
}

/// A USB sysfs name, or a disk name resolved back to the USB device carrying it.
///
/// Both are accepted because both are what a user has in hand: the device tree
/// prints `6-1.2`, and `df` prints `sdb`. Refusing the second would mean asking
/// someone to translate a disk into a bus address before they can name it,
/// which is exactly the sort of step that gets skipped.
fn resolve_target(snap: &Snapshot, name: &str) -> Option<Target> {
    if let Some(dev) = snap.device(name) {
        let blocks = snap
            .storage_devices()
            .into_iter()
            .find(|(d, _)| d.sysfs_name == dev.sysfs_name)
            .map(|(_, b)| b.iter().map(|b| b.name.clone()).collect())
            .unwrap_or_default();
        return Some(Target {
            sysfs_name: dev.sysfs_name.clone(),
            label: dev.label(),
            blocks,
            is_root_hub: dev.is_root_hub,
        });
    }

    let disk = name.trim_start_matches("/dev/");
    snap.storage_devices()
        .into_iter()
        .find(|(_, blocks)| blocks.iter().any(|b| b.name == disk))
        .map(|(dev, blocks)| Target {
            sysfs_name: dev.sysfs_name.clone(),
            label: dev.label(),
            blocks: blocks.iter().map(|b| b.name.clone()).collect(),
            is_root_hub: dev.is_root_hub,
        })
}

#[cfg(test)]
mod tests {
    /// A preview must describe the probe *before* privilege or consent is
    /// settled, because a confirmation dialog that cannot say what will happen is
    /// worthless — and asking for a password first, to find out, is worse.
    ///
    /// This is the exact case that made the flow impossible: unprivileged,
    /// `--dry-run` answered "needs root" and nothing else.
    #[test]
    fn a_preview_describes_a_probe_it_cannot_run() {
        let caps = caps_with(
            Availability::Usable,
            Availability::Denied("hub port controls are root-only".into()),
        );
        let snap = snapshot_with_a_disk();
        // Deliberately *not* `cycle_request`, which consents up front: the point
        // is a preview taken before anything has been agreed to.
        let bare = Request {
            target: Some("6-1"),
            ..Request::new(CYCLE.name, Duration::from_secs(1))
        };
        let p = approve(&CYCLE, &caps, &snap, &bare, &free()).unwrap();

        // Everything the dialog needs is here, despite there being no privilege.
        assert_eq!(p.target.as_ref().unwrap().sysfs_name, "6-1");
        assert_eq!(p.cycles, DEFAULT_CYCLES);
        assert!(p.describe().contains("disruptive"));
        assert!(p.describe().contains("6-1"));

        // And so is everything standing in the way.
        assert!(p.blocked_by.as_deref().unwrap().contains("root-only"));
        assert_eq!(p.awaiting, vec![Consent::ToProbe, Consent::ToDisrupt]);
        assert!(!p.runnable());

        // Root is the missing thing here, so an escalation prompt is honest.
        assert!(p.root_may_help(&caps));
    }

    /// Describing must never become a way of running. The only route to a `Plan`
    /// is through the conversion, and it refuses everything the preview recorded.
    #[test]
    fn a_preview_cannot_be_turned_into_a_run() {
        let caps = caps_with(
            Availability::Usable,
            Availability::Denied("root-only".into()),
        );
        let snap = snapshot_with_a_disk();

        let blocked = approve(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap();
        assert!(matches!(
            blocked.approve().unwrap_err(),
            Refusal::Unavailable { .. }
        ));

        // Privileged but unconsented: the other half of the gate.
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let bare = Request {
            target: Some("6-1"),
            ..Request::new(CYCLE.name, Duration::from_secs(1))
        };
        let unconsented = approve(&CYCLE, &caps, &snap, &bare, &free()).unwrap();
        assert!(!unconsented.runnable());
        assert!(matches!(
            unconsented.approve().unwrap_err(),
            Refusal::NeedsConsent {
                what: Consent::ToProbe,
                ..
            }
        ));
    }

    /// No password attaches a disk. The escalation prompt must stay shut for a
    /// probe whose obstacle is an absent target, which is the distinction
    /// `Remedy` was added for.
    #[test]
    fn an_absent_target_never_invites_escalation() {
        let mut caps = caps_with(Availability::Usable, Availability::Usable);
        caps.block_read = Interface {
            availability: Availability::Absent("no raw disk nodes under /dev".into()),
            path: None,
        };
        let snap = snapshot_with_a_disk();
        let req = Request::new("throughput", Duration::from_secs(1));
        let p = preview(&caps, &snap, &req).unwrap();

        assert!(!p.runnable(), "it cannot run");
        assert!(
            !p.root_may_help(&caps),
            "and root would not change that, so do not ask for a password"
        );
    }

    /// A preview still refuses what no privilege and no consent could fix. These
    /// are facts about the request, and softening them for the sake of a nicer
    /// dialog would be the dangerous direction to soften in.
    #[test]
    fn a_preview_still_refuses_what_consent_cannot_lift() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();

        // `CYCLE` is a stand-in outside the real registry, so these go through
        // `approve` — the same decision path `preview` runs, minus the name
        // lookup that `no_such_probe` already covers elsewhere.
        let refuse = |target| {
            approve(&CYCLE, &caps, &snap, &cycle_request(Some(target)), &free()).unwrap_err()
        };

        // A keyboard in the way: explicitly non-overridable.
        let e = refuse("6-1.1");
        assert!(!e.is_recoverable(), "{}", e.message());

        // A misspelled target.
        assert_eq!(refuse("nope").code(), "no_such_target");

        // A whole bus.
        assert_eq!(refuse("usb6").code(), "whole_bus");
    }

    /// The decision exactly as [`plan`] reports it: describe, then approve.
    ///
    /// Every test below went through `approve` directly before `Preview`
    /// existed. Routing them through the conversion instead is the proof the
    /// refactor changed no behaviour — the same inputs must still produce the
    /// same refusals, in the same order.
    fn decide(
        probe: &'static ProbeInfo,
        caps: &Capabilities,
        snap: &Snapshot,
        req: &Request,
        holders: &std::collections::BTreeMap<String, Vec<Hold>>,
    ) -> Result<Plan, Refusal> {
        approve(probe, caps, snap, req, holders)?.approve()
    }

    use super::*;
    use crate::caps::{Availability, Interface};
    use crate::model::BlockDevice;

    fn caps_with(usbmon: Availability, usbfs: Availability) -> Capabilities {
        Capabilities {
            effective_uid: Some(0),
            usbmon: Interface {
                availability: usbmon,
                path: None,
            },
            usbfs: Interface {
                availability: usbfs.clone(),
                path: None,
            },
            block_read: Interface {
                availability: usbfs.clone(),
                path: None,
            },
            port_control: Interface {
                availability: usbfs,
                path: None,
            },
        }
    }

    /// Nested the way a real capture is, with the device inside its root hub's
    /// `children`. A flat fixture here once hid a bug that made every target
    /// below a root hub unresolvable on real hardware.
    fn snapshot_with_a_disk() -> Snapshot {
        let mut dev = crate::test_support::device("6-1", " 3.10", 5000.0, Some("usb6"));
        dev.product = Some("Ultra USB 3.0".into());
        dev.manufacturer = Some("SanDisk".into());
        let usb_path = dev.sysfs_path.clone();

        let mut bus = crate::test_support::root_hub("usb6", 5000.0);
        bus.children.push(dev);

        Snapshot {
            buses: vec![bus],
            block_devices: vec![BlockDevice {
                name: "sdb".into(),
                sysfs_path: usb_path.join("6-1:1.0/host2/block/sdb"),
                model: None,
                vendor: None,
                size_bytes: None,
                rotational: None,
                removable: None,
                stats: None,
                throughput: None,
                scsi: None,
                scsi_delta: None,
            }],
            ..crate::test_support::empty_snapshot()
        }
    }

    fn ok(caps: &Capabilities, snap: &Snapshot, req: Request) -> Plan {
        plan(caps, snap, &req).unwrap_or_else(|e| panic!("{}", e.message()))
    }

    #[test]
    fn a_passive_probe_needs_nothing_at_all() {
        let caps = Capabilities::default();
        let snap = Snapshot::default();
        let p = ok(&caps, &snap, Request::new("snapshot", Duration::ZERO));
        assert_eq!(p.probe.class, ProbeClass::Passive);
        assert!(p.describe().contains("Nothing on the bus changes"));
    }

    /// Naming a read-only probe is the request. Requiring a confirmation flag
    /// on top would train the reflex that makes the flag useless where it
    /// counts.
    #[test]
    fn a_read_only_privileged_probe_runs_when_named() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = Snapshot::default();
        let p = ok(&caps, &snap, Request::new("urb-errors", Duration::from_secs(3)));
        assert_eq!(p.probe.class, ProbeClass::PrivilegedRead);
        assert!(p.describe().contains("for 3.0s"), "{}", p.describe());

        let opts = p.capture_options(Options::default());
        assert_eq!(opts.urb_sample_ms, 3000);
        // And it must not quietly switch on anything else.
        assert_eq!(opts.storage_sample_ms, 0);
    }

    #[test]
    fn an_unavailable_interface_refuses_before_anything_is_attempted() {
        let caps = caps_with(
            Availability::NotLoaded("run modprobe usbmon".into()),
            Availability::Usable,
        );
        let e = plan(
            &caps,
            &Snapshot::default(),
            &Request::new("urb-errors", Duration::from_secs(1)),
        )
        .unwrap_err();
        assert!(e.message().contains("modprobe usbmon"), "{}", e.message());
        assert!(!e.is_recoverable(), "no amount of consent loads a module");
    }

    /// A registered probe with no code behind it must say so, not fail
    /// obscurely somewhere further in.
    /// Every registered probe is written now, so this is exercised through a
    /// stand-in — the check has to keep working for the next probe that is
    /// listed before it is built.
    #[test]
    fn an_unimplemented_probe_refuses_by_name() {
        const UNWRITTEN: ProbeInfo = ProbeInfo {
            name: "test-unwritten",
            class: ProbeClass::Disruptive,
            needs: crate::caps::Requirement::Usbfs,
            implemented: false,
            summary: "Registered but not built, used only by tests.",
        };
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let e = decide(
            &UNWRITTEN,
            &caps,
            &snap,
            &Request {
                target: Some("6-1"),
                consented: true,
                accepts_disruption: true,
                ..Request::new(UNWRITTEN.name, Duration::from_secs(1))
            },
            &free(),
        )
        .unwrap_err();
        assert!(e.message().contains("not implemented"), "{}", e.message());
        // Consent given in full, and it still does not run.
        assert!(!e.is_recoverable());
    }

    /// Nothing in the registry should be advertised and then refused. When this
    /// fires, either a probe was written and the flag not flipped, or one was
    /// registered ahead of being built — the second is fine, and the test is
    /// here so it is a deliberate choice rather than an oversight.
    #[test]
    fn every_registered_probe_is_implemented() {
        let unwritten: Vec<&str> = crate::caps::PROBES
            .iter()
            .filter(|p| !p.implemented)
            .map(|p| p.name)
            .collect();
        assert!(unwritten.is_empty(), "not implemented: {unwritten:?}");
    }

    #[test]
    fn an_unknown_probe_lists_the_real_ones() {
        let e = plan(
            &Capabilities::default(),
            &Snapshot::default(),
            &Request::new("wiggle-the-cable", Duration::ZERO),
        )
        .unwrap_err();
        assert!(e.message().contains("urb-errors"), "{}", e.message());
    }

    #[test]
    fn a_target_may_be_named_as_a_bus_address_or_as_a_disk() {
        let snap = snapshot_with_a_disk();
        let by_bus = resolve_target(&snap, "6-1").unwrap();
        assert_eq!(by_bus.blocks, vec!["sdb".to_string()]);
        // Both spellings of the disk reach the same USB device.
        assert_eq!(resolve_target(&snap, "sdb"), Some(by_bus.clone()));
        assert_eq!(resolve_target(&snap, "/dev/sdb"), Some(by_bus));
        assert!(resolve_target(&snap, "6-9").is_none());
    }

    // -----------------------------------------------------------------------
    // The disruptive gate
    //
    // Exercised through a stand-in, because the two real disruptive probes are
    // not written yet and refuse before reaching any of this. The alternative
    // was to ship the code that stands between a probe and someone's data
    // without ever having run it.
    // -----------------------------------------------------------------------

    const CYCLE: ProbeInfo = ProbeInfo {
        name: "test-cycle",
        class: ProbeClass::Disruptive,
        needs: crate::caps::Requirement::Usbfs,
        implemented: true,
        summary: "A stand-in for the disruptive probes, used only by tests.",
    };

    fn held(disk: &str, hold: Hold) -> std::collections::BTreeMap<String, Vec<Hold>> {
        std::collections::BTreeMap::from([(disk.to_string(), vec![hold])])
    }

    fn free() -> std::collections::BTreeMap<String, Vec<Hold>> {
        std::collections::BTreeMap::new()
    }

    fn cycle_request<'a>(target: Option<&'a str>) -> Request<'a> {
        Request {
            target,
            consented: true,
            accepts_disruption: true,
            ..Request::new(CYCLE.name, Duration::from_secs(1))
        }
    }

    /// A root hub is not a device you can cycle: it has no upstream port, and
    /// everything on the bus is behind it.
    #[test]
    fn a_root_hub_is_refused_because_it_is_the_whole_bus() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let e = decide(&CYCLE, &caps, &snap, &cycle_request(Some("usb6")), &free()).unwrap_err();
        assert!(matches!(e, Refusal::WholeBus { .. }), "{}", e.message());
        assert!(
            e.message().contains("every device on it"),
            "{}",
            e.message()
        );
        assert!(!e.is_recoverable());
    }

    #[test]
    fn a_disruptive_probe_must_be_pointed_at_one_device() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let e = decide(&CYCLE, &caps, &snap, &cycle_request(None), &free()).unwrap_err();
        assert!(e.message().contains("--target"), "{}", e.message());
        assert!(!e.is_recoverable());

        // Pointed at something, it is allowed — and says what it will do.
        let p = decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap();
        assert!(p.describe().contains("leaves the bus"), "{}", p.describe());
        assert_eq!(p.target.unwrap().sysfs_name, "6-1");
    }

    /// The refusal that consent cannot lift.
    #[test]
    fn a_mounted_disk_is_refused_however_much_consent_is_given() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let mounted = held(
            "sdb",
            Hold {
                via: "sdb1".into(),
                kind: crate::block::HoldKind::Mounted("/media/stick".into()),
            },
        );

        // Named as the disk, and named as the bus address: the same disk is
        // behind both, so both must refuse.
        for target in ["sdb", "6-1"] {
            let e = decide(&CYCLE, &caps, &snap, &cycle_request(Some(target)), &mounted)
                .unwrap_err();
            let m = e.message();
            assert!(m.contains("/media/stick"), "names where it is mounted: {m}");
            assert!(m.contains("cannot be overridden"), "{m}");
            assert!(
                !e.is_recoverable(),
                "asking again must never be offered as a way out"
            );
        }

        // Unmounted, the same request goes through.
        assert!(decide(&CYCLE, &caps, &snap, &cycle_request(Some("sdb")), &free()).is_ok());
    }

    /// Two acts, not one: agreeing to probe at all, and accepting that this
    /// particular probe interrupts the device.
    #[test]
    fn disruption_needs_a_second_and_separate_yes() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();

        let bare = Request {
            target: Some("6-1"),
            ..Request::new(CYCLE.name, Duration::from_secs(1))
        };
        let e = decide(&CYCLE, &caps, &snap, &bare, &free()).unwrap_err();
        assert!(matches!(
            e,
            Refusal::NeedsConsent {
                what: Consent::ToProbe,
                ..
            }
        ));
        assert!(e.is_recoverable(), "this one a yes does fix");

        let consented = Request {
            consented: true,
            ..bare
        };
        let e = decide(&CYCLE, &caps, &snap, &consented, &free()).unwrap_err();
        assert!(
            matches!(
                e,
                Refusal::NeedsConsent {
                    what: Consent::ToDisrupt,
                    ..
                }
            ),
            "the first yes must not carry the second: {}",
            e.message()
        );
        assert!(e.message().contains("off the bus"), "{}", e.message());
    }

    /// The second refusal consent cannot lift.
    ///
    /// Losing a keyboard is not like losing a filesystem — nothing is
    /// destroyed — but it removes the means of stopping whatever happens next,
    /// which is not something a user can meaningfully agree to in advance.
    #[test]
    fn a_keyboard_in_the_subtree_is_refused_whatever_consent_is_given() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let mut snap = snapshot_with_a_disk();

        let mut kbd = crate::test_support::device("6-1.1", " 2.00", 12.0, Some("6-1"));
        kbd.product = Some("Compact Keyboard".into());
        kbd.interfaces.push(crate::model::UsbInterface {
            sysfs_name: "6-1.1:1.0".into(),
            number: Some(0),
            class: Some(0x03),
            subclass: Some(1),
            protocol: Some(1),
            driver: Some("usbhid".into()),
            description: None,
        });
        snap.buses[0].children[0].children.push(kbd);

        let e = decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap_err();
        let m = e.message();
        assert!(m.contains("a keyboard"), "{m}");
        assert!(m.contains("no key left to press"), "{m}");
        assert!(m.contains("cannot be overridden"), "{m}");
        assert!(!e.is_recoverable());

        // A sibling with no keyboard under it is still fine.
        assert!(decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1.1")), &free()).is_err());
    }

    /// Things that will drop and come back are warnings, not refusals — they
    /// belong in the confirmation so the second yes is informed.
    #[test]
    fn a_disk_that_is_present_but_unmounted_is_a_warning_not_a_refusal() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();

        let plan = decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap();
        assert!(
            plan.side_effects.iter().any(|e| e.contains("sdb")),
            "{:?}",
            plan.side_effects
        );
        // And it reaches the text the user actually sees.
        assert!(plan.describe().contains("sdb"), "{}", plan.describe());
    }

    // -----------------------------------------------------------------------
    // The wire format
    //
    // These assert on exact field names and exact slugs. That is the point:
    // another program branches on them, and a rename that goes unnoticed here
    // is a silent break there.
    // -----------------------------------------------------------------------

    fn json(v: &impl serde::Serialize) -> serde_json::Value {
        serde_json::to_value(v).unwrap()
    }

    /// Every refusal must carry a stable code, and no two may share one.
    #[test]
    fn every_refusal_has_its_own_stable_code() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let mounted = held(
            "sdb",
            Hold {
                via: "sdb1".into(),
                kind: crate::block::HoldKind::Mounted("/media/stick".into()),
            },
        );
        let bare = Request {
            target: Some("6-1"),
            ..Request::new(CYCLE.name, Duration::from_secs(1))
        };

        let refusals = [
            plan(&caps, &snap, &Request::new("nope", Duration::ZERO)).unwrap_err(),
            decide(&CYCLE, &caps, &snap, &cycle_request(None), &free()).unwrap_err(),
            decide(&CYCLE, &caps, &snap, &cycle_request(Some("zz")), &free()).unwrap_err(),
            decide(&CYCLE, &caps, &snap, &cycle_request(Some("usb6")), &free()).unwrap_err(),
            decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &mounted).unwrap_err(),
            decide(&CYCLE, &caps, &snap, &bare, &free()).unwrap_err(),
            decide(
                &CYCLE,
                &caps,
                &snap,
                &Request {
                    consented: true,
                    ..bare.clone()
                },
                &free(),
            )
            .unwrap_err(),
        ];

        let codes: Vec<&str> = refusals.iter().map(|r| r.code()).collect();
        assert_eq!(
            codes,
            [
                "no_such_probe",
                "target_required",
                "no_such_target",
                "whole_bus",
                "in_use",
                "needs_consent",
                "needs_consent",
            ]
        );

        for r in &refusals {
            let v = json(&r.report());
            assert_eq!(v["code"], r.code());
            assert_eq!(v["recoverable"], r.is_recoverable());
            assert!(
                v["message"].as_str().is_some_and(|m| !m.is_empty()),
                "every refusal explains itself: {v}"
            );
        }
    }

    /// The structured parts a caller acts on, rather than displays.
    #[test]
    fn a_refusal_carries_its_details_in_fields_not_only_in_prose() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let mounted = held(
            "sdb",
            Hold {
                via: "sdb1".into(),
                kind: crate::block::HoldKind::Mounted("/media/stick".into()),
            },
        );

        let v = json(
            &decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &mounted)
                .unwrap_err()
                .report(),
        );
        assert_eq!(v["code"], "in_use");
        assert_eq!(v["target"], "6-1");
        assert_eq!(v["holds"][0]["via"], "sdb1");
        assert_eq!(v["holds"][0]["kind"]["kind"], "mounted");
        assert_eq!(v["holds"][0]["kind"]["where"], "/media/stick");

        // Which of the two confirmations is missing, without parsing English.
        let v = json(
            &decide(
                &CYCLE,
                &caps,
                &snap,
                &Request {
                    target: Some("6-1"),
                    consented: true,
                    ..Request::new(CYCLE.name, Duration::from_secs(1))
                },
                &free(),
            )
            .unwrap_err()
            .report(),
        );
        assert_eq!(v["code"], "needs_consent");
        assert_eq!(v["consent"], "to_disrupt");
        assert_eq!(v["recoverable"], true);

        // An unknown probe hands back the real list rather than making the
        // caller guess.
        let v = json(
            &plan(&caps, &snap, &Request::new("nope", Duration::ZERO))
                .unwrap_err()
                .report(),
        );
        assert!(v["known_probes"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("urb-errors")));
    }

    /// A duration is not seconds-and-nanoseconds to anyone outside this
    /// process, and the side effects a confirmation dialog needs must survive
    /// serialisation.
    #[test]
    fn a_plan_serialises_in_units_another_program_can_use() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let p = decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap();

        let v = json(&p);
        assert_eq!(v["window_ms"], 1000);
        assert!(v.get("window").is_none(), "the Duration shape must not leak");
        assert_eq!(v["probe"]["name"], "test-cycle");
        assert_eq!(v["probe"]["class"], "disruptive");
        assert_eq!(v["target"]["sysfs_name"], "6-1");
        assert_eq!(v["cycles"], 20);
        assert!(
            v["side_effects"][0].as_str().unwrap().contains("sdb"),
            "{v}"
        );
    }

    /// The catalogue joins the registry to this machine's verdict, so a front
    /// end never has to combine them itself and never offers a dead button.
    #[test]
    fn the_catalogue_says_what_is_ready_and_why_not() {
        let caps = Capabilities::default();
        let v = json(&catalogue(&caps));
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), crate::caps::PROBES.len());

        // Flattened, so a row is one object rather than a nesting.
        assert_eq!(rows[0]["name"], "snapshot");
        assert_eq!(rows[0]["class"], "passive");
        assert_eq!(rows[0]["ready"], true);
        assert_eq!(rows[0]["blocker"], serde_json::Value::Null);

        let urb = rows.iter().find(|r| r["name"] == "urb-errors").unwrap();
        assert_eq!(urb["ready"], false);
        assert!(urb["blocker"].as_str().unwrap().contains("usbmon"));
        assert!(urb["summary"].as_str().is_some_and(|s| !s.is_empty()));
    }

    /// Order matters. A mounted disk is reported as mounted even when the user
    /// has not consented yet — being told to confirm something that will then
    /// be refused anyway is the wrong answer.
    #[test]
    fn being_in_use_outranks_a_missing_confirmation() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let mounted = held(
            "sdb",
            Hold {
                via: "sdb".into(),
                kind: crate::block::HoldKind::Swap,
            },
        );
        let bare = Request {
            target: Some("6-1"),
            ..Request::new(CYCLE.name, Duration::from_secs(1))
        };
        let e = decide(&CYCLE, &caps, &snap, &bare, &mounted).unwrap_err();
        assert!(matches!(e, Refusal::InUse { .. }), "{}", e.message());
        assert!(e.message().contains("swap"), "{}", e.message());
    }

    /// A missing interface outranks consent — there is no point asking to
    /// confirm something this process could not do either way — but it does
    /// *not* outrank anything wrong with the request itself.
    ///
    /// The order matters for a real reason: answering "you need root" to a
    /// misspelled target, or to a disk that is mounted, sends the user off to
    /// find sudo only to be refused again for a reason that was knowable all
    /// along.
    #[test]
    fn privilege_is_reported_after_the_request_is_found_to_be_sound() {
        let caps = caps_with(
            Availability::Usable,
            Availability::Denied("usbfs nodes are root-only".into()),
        );
        let snap = snapshot_with_a_disk();

        // A sound request, no privilege: privilege is the answer.
        let e = decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap_err();
        assert!(e.message().contains("root-only"), "{}", e.message());

        // Missing target, no privilege: the target is the answer, since that
        // is the part the user can fix from here.
        let e = decide(&CYCLE, &caps, &snap, &cycle_request(None), &free()).unwrap_err();
        assert!(e.message().contains("--target"), "{}", e.message());

        // Mounted disk, no privilege: still the mount, because no amount of
        // privilege was ever going to change it.
        let mounted = held(
            "sdb",
            Hold {
                via: "sdb1".into(),
                kind: crate::block::HoldKind::Mounted("/media/stick".into()),
            },
        );
        let e = decide(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &mounted).unwrap_err();
        assert!(matches!(e, Refusal::InUse { .. }), "{}", e.message());
    }

    #[test]
    fn a_bad_target_is_refused_with_an_example() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let e = plan(
            &caps,
            &snapshot_with_a_disk(),
            &Request {
                target: Some("sdz"),
                consented: true,
                ..Request::new("urb-errors", Duration::from_secs(1))
            },
        )
        .unwrap_err();
        assert!(e.message().contains("6-1.2"), "shows the shape: {}", e.message());
    }
}

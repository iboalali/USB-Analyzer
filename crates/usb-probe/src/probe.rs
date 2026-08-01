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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone)]
pub struct Plan {
    pub probe: &'static ProbeInfo,
    pub target: Option<Target>,
    pub window: Duration,
    pub cycles: usize,
    /// What else stops working while this runs. Not reasons to refuse — things
    /// the user should know before saying yes the second time, which is what
    /// the second confirmation is for.
    pub side_effects: Vec<String>,
}

impl Plan {
    /// What is about to happen, in the words the user should see before it
    /// does. Every probe says its class out loud, so "disruptive" is never a
    /// surprise discovered afterwards.
    pub fn describe(&self) -> String {
        let mut s = format!("{} — {}", self.probe.name, self.probe.class.label());
        if let Some(t) = &self.target {
            s.push_str(&format!(", on {} ({})", t.sysfs_name, t.label));
        }
        if self.takes_a_window() {
            s.push_str(&format!(", for {:.1}s", self.window.as_secs_f64()));
        }
        if self.probe.name == "reenumerate" {
            s.push_str(&format!(", {} times", self.cycles));
        }
        s.push_str(match self.probe.class {
            ProbeClass::Passive | ProbeClass::PrivilegedRead => ". Nothing on the bus changes.",
            ProbeClass::Disruptive => {
                ". The device leaves the bus and comes back when this finishes."
            }
        });
        for effect in &self.side_effects {
            s.push_str(&format!("\n  · {effect}"));
        }
        s
    }

    fn takes_a_window(&self) -> bool {
        matches!(self.probe.name, "storage-sample" | "urb-errors")
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

/// Carry out an approved plan and analyse the result.
///
/// Takes a [`Plan`] rather than a name, so there is no way to reach this
/// without having passed [`plan`] first — the type is the proof that consent,
/// capability and the mounted-filesystem check have all already happened.
pub fn run(plan: &Plan, base: Options) -> crate::model::Report {
    let mut snapshot = crate::capture(plan.capture_options(base));

    if plan.probe.name == "throughput" {
        let only = plan.target.as_ref().map(|t| t.blocks.clone());
        for disk in crate::throughput::targets(&snapshot, only.as_deref()) {
            match crate::throughput::measure(&disk, plan.window) {
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
                    let run = crate::reenumerate::cycle(&port, &target.sysfs_name, plan.cycles);
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

/// Which of the two acts of consent is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// Agreement to run an out-of-band probe at all.
    ToProbe,
    /// Separate acceptance that this one interrupts the device.
    ToDisrupt,
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
) -> Result<Plan, Refusal> {
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

    // Privilege is checked after everything about the request itself, because
    // the request errors are the ones the user can fix without escalating.
    // Answering "you need root" first costs them a sudo round trip to discover
    // that the target was misspelled, or that the disk is mounted and it was
    // never going to work at any privilege.
    if let Some(why) = caps.blocker(probe) {
        return Err(Refusal::Unavailable { probe, why });
    }

    // Only the disruptive class asks. A read-only probe was requested by name
    // and leaves nothing behind, so a confirmation there would be ceremony —
    // and the reflex it teaches is what makes the confirmation on this line
    // worth anything.
    if disruptive && !req.consented {
        return Err(Refusal::NeedsConsent {
            probe,
            what: Consent::ToProbe,
        });
    }
    if disruptive && !req.accepts_disruption {
        return Err(Refusal::NeedsConsent {
            probe,
            what: Consent::ToDisrupt,
        });
    }

    Ok(Plan {
        probe,
        target,
        window: req.window,
        cycles: req.cycles.clamp(2, 100),
        side_effects,
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
        let e = approve(
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
        let e = approve(&CYCLE, &caps, &snap, &cycle_request(Some("usb6")), &free()).unwrap_err();
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
        let e = approve(&CYCLE, &caps, &snap, &cycle_request(None), &free()).unwrap_err();
        assert!(e.message().contains("--target"), "{}", e.message());
        assert!(!e.is_recoverable());

        // Pointed at something, it is allowed — and says what it will do.
        let p = approve(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap();
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
            let e = approve(&CYCLE, &caps, &snap, &cycle_request(Some(target)), &mounted)
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
        assert!(approve(&CYCLE, &caps, &snap, &cycle_request(Some("sdb")), &free()).is_ok());
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
        let e = approve(&CYCLE, &caps, &snap, &bare, &free()).unwrap_err();
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
        let e = approve(&CYCLE, &caps, &snap, &consented, &free()).unwrap_err();
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

        let e = approve(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap_err();
        let m = e.message();
        assert!(m.contains("a keyboard"), "{m}");
        assert!(m.contains("no key left to press"), "{m}");
        assert!(m.contains("cannot be overridden"), "{m}");
        assert!(!e.is_recoverable());

        // A sibling with no keyboard under it is still fine.
        assert!(approve(&CYCLE, &caps, &snap, &cycle_request(Some("6-1.1")), &free()).is_err());
    }

    /// Things that will drop and come back are warnings, not refusals — they
    /// belong in the confirmation so the second yes is informed.
    #[test]
    fn a_disk_that_is_present_but_unmounted_is_a_warning_not_a_refusal() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();

        let plan = approve(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap();
        assert!(
            plan.side_effects.iter().any(|e| e.contains("sdb")),
            "{:?}",
            plan.side_effects
        );
        // And it reaches the text the user actually sees.
        assert!(plan.describe().contains("sdb"), "{}", plan.describe());
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
        let e = approve(&CYCLE, &caps, &snap, &bare, &mounted).unwrap_err();
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
        let e = approve(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &free()).unwrap_err();
        assert!(e.message().contains("root-only"), "{}", e.message());

        // Missing target, no privilege: the target is the answer, since that
        // is the part the user can fix from here.
        let e = approve(&CYCLE, &caps, &snap, &cycle_request(None), &free()).unwrap_err();
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
        let e = approve(&CYCLE, &caps, &snap, &cycle_request(Some("6-1")), &mounted).unwrap_err();
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

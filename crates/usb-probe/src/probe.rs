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
            consented: false,
            accepts_disruption: false,
        }
    }
}

/// A device a probe has been pointed at, resolved against a real snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub sysfs_name: String,
    pub label: String,
    /// Disks reached through this device, if any.
    pub blocks: Vec<String>,
}

/// An approved probe. Holding one means every check has already passed.
#[derive(Debug, Clone)]
pub struct Plan {
    pub probe: &'static ProbeInfo,
    pub target: Option<Target>,
    pub window: Duration,
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
        s.push_str(match self.probe.class {
            ProbeClass::Passive | ProbeClass::PrivilegedRead => ". Nothing on the bus changes.",
            ProbeClass::Disruptive => {
                ". The device leaves the bus and comes back when this finishes."
            }
        });
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

    // Checked here rather than inside `approve`, and therefore first: telling
    // someone to add --target to a probe that then turns out not to exist yet
    // would send them down a dead end.
    if !probe.implemented {
        return Err(Refusal::NotImplemented(probe));
    }

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
    if let Some(why) = caps.blocker(probe) {
        return Err(Refusal::Unavailable { probe, why });
    }

    let disruptive = probe.class == ProbeClass::Disruptive;
    let target = match req.target {
        Some(name) => Some(resolve_target(snap, name).ok_or_else(|| Refusal::NoSuchTarget {
            name: name.to_string(),
        })?),
        None if disruptive => return Err(Refusal::TargetRequired(probe)),
        None => None,
    };

    // Before consent, not after: being told the disk is mounted is more use
    // than being told to confirm something that will then be refused anyway.
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
        }
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
    #[test]
    fn an_unimplemented_probe_refuses_by_name() {
        let caps = caps_with(Availability::Usable, Availability::Usable);
        let snap = snapshot_with_a_disk();
        let e = plan(
            &caps,
            &snap,
            &Request {
                target: Some("6-1"),
                consented: true,
                accepts_disruption: true,
                ..Request::new("reenumerate", Duration::from_secs(1))
            },
        )
        .unwrap_err();
        assert!(e.message().contains("not implemented"), "{}", e.message());
        // Consent given in full, and it still does not run.
        assert!(!e.is_recoverable());
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

    /// And a missing interface outranks everything: there is no point asking
    /// for consent to something this process could not do either way.
    #[test]
    fn an_unusable_interface_outranks_consent_and_targeting() {
        let caps = caps_with(
            Availability::Usable,
            Availability::Denied("usbfs nodes are root-only".into()),
        );
        let snap = snapshot_with_a_disk();
        let e = approve(&CYCLE, &caps, &snap, &cycle_request(None), &free()).unwrap_err();
        assert!(e.message().contains("root-only"), "{}", e.message());
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

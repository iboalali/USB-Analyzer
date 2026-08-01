//! Saying "nothing is wrong here" out loud.
//!
//! Every rule in [`crate::diag`] answers "is this broken". None of them answers
//! "is this fine", and the tool expressed that answer as **silence** — a clean
//! report and a report where nothing was examined look identical, so a user
//! reading either cannot tell whether the tool looked and found nothing or
//! simply did not look.
//!
//! Two things fix it, and they need each other:
//!
//! - **Exonerations** — Info-level statements that a specific thing is *not*
//!   the problem, emitted exactly where an accusing rule deliberately declined.
//! - **Verdicts** — one sentence per subject, so the answer arrives before the
//!   detail rather than having to be assembled from it.
//!
//! The exonerations are what give a clean verdict something to cite. Without
//! them "nothing found" is an assertion; with them it is a summary.

use crate::diag::{CLASS_MASS_STORAGE, SVID_DISPLAYPORT, watts};
use crate::model::{
    Confidence, Finding, Outcome, Severity, Snapshot, Subject, UsbDevice, Verdict,
};

/// Which exoneration makes the best headline for a subject that has several.
///
/// Ordered by how directly each answers the question people actually arrive
/// with. "The cable is not the limit" beats "an alt mode nobody asked for was
/// not entered", every time.
const HEADLINE_PRIORITY: &[&str] = &[
    "CHARGING_AT_FULL_OFFER",
    "CABLE_NOT_LIMITING",
    "LINK_AT_DEVICE_MAXIMUM",
    "MEDIUM_EXPLAINS_THROUGHPUT",
    "ALT_MODE_NOT_REQUESTED",
];

// ---------------------------------------------------------------------------
// Exonerations
// ---------------------------------------------------------------------------

/// Statements that a thing is *not* at fault.
///
/// `findings` is passed in so a rule can decline to exonerate a subject that
/// has already been accused. Saying "the cable is fine" beside "the cable is
/// the problem" would be worse than saying nothing at all.
pub fn exonerate(snap: &Snapshot, findings: &[Finding]) -> Vec<Finding> {
    let mut out = Vec::new();

    power_chain_clear(snap, findings, &mut out);
    link_at_device_maximum(snap, findings, &mut out);
    alt_mode_not_requested(snap, findings, &mut out);
    medium_explains_throughput(snap, findings, &mut out);

    out.sort_by(|a, b| {
        a.subject
            .display()
            .cmp(&b.subject.display())
            .then_with(|| a.code.cmp(&b.code))
    });
    out
}

fn accused(findings: &[Finding], subject: &Subject) -> bool {
    findings
        .iter()
        .any(|f| &f.subject == subject && f.severity >= Severity::Low)
}

/// The contract matched the best thing the charger offered, so nothing in
/// between reduced it — cable included.
///
/// This is the sentence the whole tool exists to be able to say. It needs no
/// e-marker, which matters: on UCSI platforms the cable's identity is never
/// exposed, so the *only* way to clear a cable is to show that nothing was
/// lost across it.
fn power_chain_clear(snap: &Snapshot, findings: &[Finding], out: &mut Vec<Finding>) {
    for port in &snap.ports {
        let Some(partner) = &port.partner else {
            continue;
        };
        if !port.is_sinking() {
            continue;
        }
        let subject = Subject::Cable(port.name.clone());
        if accused(findings, &subject) || accused(findings, &Subject::Port(port.name.clone())) {
            continue;
        }

        let (Some(offered), Some(got)) = (
            partner.pd.as_ref().and_then(|pd| pd.max_source_power_mw()),
            port.power_supply
                .as_ref()
                .and_then(|c| c.contract_power_mw()),
        ) else {
            continue;
        };
        if got < offered {
            continue;
        }

        if !accused(findings, &Subject::Port(port.name.clone())) {
            out.push(Finding {
                code: "CHARGING_AT_FULL_OFFER".into(),
                severity: Severity::Info,
                confidence: Confidence::Measured,
                subject: Subject::Port(port.name.clone()),
                title: format!("Charging at {}, the most this charger offers", watts(got)),
                detail: "The contract reached the charger's best profile, so nothing on this \
                         port is holding power back. A bigger charger would charge faster; \
                         nothing about this port, its cable, or this machine would."
                    .to_string(),
                evidence: vec![format!(
                    "contract {} == best source PDO {}",
                    watts(got),
                    watts(offered)
                )],
                suggestion: None,
            });
        }

        out.push(Finding {
            code: "CABLE_NOT_LIMITING".into(),
            severity: Severity::Info,
            confidence: Confidence::Measured,
            subject,
            title: "The cable is not what limits charging here".into(),
            detail: format!(
                "The charger's best offer is {}, and {} is what the contract settled on — so \
                 nothing between the two gave anything up. A cable rated too low would have \
                 forced a smaller contract, and this one did not. Swapping it cannot make \
                 charging faster.",
                watts(offered),
                watts(got)
            ),
            evidence: vec![format!(
                "best source PDO {} == contract {}",
                watts(offered),
                watts(got)
            )],
            suggestion: None,
        });
    }
}

/// Linked below the port's rate, but at the device's own ceiling.
///
/// Restricted to the case that actually invites suspicion: something plugged
/// into a faster port than it can use. Exonerating every device for running at
/// its own maximum would fire on all nine devices on the development machine
/// and mean nothing on any of them.
fn link_at_device_maximum(snap: &Snapshot, findings: &[Finding], out: &mut Vec<Finding>) {
    for dev in snap.devices() {
        let subject = Subject::Device(dev.sysfs_name.clone());
        if accused(findings, &subject) {
            continue;
        }
        let Some(ceiling) = declared_ceiling_mbps(dev) else {
            continue;
        };
        let (Some(speed), Some(parent)) = (
            dev.speed.as_ref(),
            dev.parent.as_deref().and_then(|p| snap.device(p)),
        ) else {
            continue;
        };
        let Some(upstream) = parent.speed.as_ref() else {
            continue;
        };

        // Only interesting when the port could have gone faster and the device
        // could not. Equal rates are unremarkable; a device below its own
        // ceiling is LINK_BELOW_DEVICE_CAPABILITY's business, not ours.
        if upstream.mbps <= speed.mbps || (speed.mbps - ceiling).abs() > f64::EPSILON {
            continue;
        }

        out.push(Finding {
            code: "LINK_AT_DEVICE_MAXIMUM".into(),
            severity: Severity::Info,
            confidence: Confidence::Measured,
            subject,
            title: format!(
                "{} is linked at {}, which is its own maximum",
                dev.label(),
                speed.short()
            ),
            detail: format!(
                "The port upstream can carry {}, so the link looks slow next to it. It is not: \
                 the device declares USB {} and {} is all that version allows. No cable or port \
                 change can raise this.",
                upstream.short(),
                dev.usb_version.as_deref().unwrap_or("?"),
                speed.short()
            ),
            evidence: vec![format!(
                "declares USB {}, negotiated {}, upstream {} at {}",
                dev.usb_version.as_deref().unwrap_or("?"),
                speed.short(),
                parent.sysfs_name,
                upstream.short()
            )],
            suggestion: None,
        });
    }
}

/// The signalling ceiling a device's declared USB version allows — which is
/// almost never knowable, so this almost always returns `None`.
///
/// **`bcdUSB` names the specification a device was written against, not the
/// rate its silicon can reach.** `2.00` is claimed by 12 Mbps devices and
/// 480 Mbps ones alike. `3.10` covers Gen 1 at 5 Gbps and Gen 2 at 10 Gbps, and
/// `3.20` adds 20 Gbps on top.
///
/// Only `3.0x` is unambiguous: the USB 3.0 specification defines exactly one
/// SuperSpeed rate.
///
/// This was nearly got wrong. Mapping `>= 3.1` to 10 Gbps looks reasonable and
/// is contradicted by hardware on the development machine: `6-1`, a VIA hub,
/// declares `bcdUSB 3.10` and links at 5 Gbps into a 10 Gbps port. Under the
/// wrong mapping it is not at its ceiling and stays unexonerated; under a
/// mapping that assumed 3.10 meant Gen 1, a genuine 10 Gbps device downshifted
/// to 5 would have been told it was running as fast as it could. Silence is the
/// right failure here, so the ambiguous cases return `None`.
fn declared_ceiling_mbps(dev: &UsbDevice) -> Option<f64> {
    match dev.usb_version_num? {
        v if v >= 3.1 => None,
        v if v >= 3.0 => Some(5_000.0),
        _ => None,
    }
}

/// A DisplayPort Alt Mode was advertised and not entered, with nothing asking
/// for it.
///
/// The inverse of `DP_ALT_MODE_NO_OUTPUT`, which fires when a mode *was*
/// entered and produced no picture. An advertised-but-idle alt mode on a port
/// with a charger attached is correct behaviour, and looks alarming in a raw
/// dump of the port's capabilities.
fn alt_mode_not_requested(snap: &Snapshot, findings: &[Finding], out: &mut Vec<Finding>) {
    for port in &snap.ports {
        let subject = Subject::Port(port.name.clone());
        if accused(findings, &subject) {
            continue;
        }
        if !port
            .alt_modes
            .iter()
            .any(|m| m.svid == Some(SVID_DISPLAYPORT))
        {
            continue;
        }
        // An empty port needs no defending. "Nothing is attached, so nothing
        // entered the mode" is a tautology, and one per unused port is noise.
        if port.partner.is_none() {
            continue;
        }
        // Entered is a different situation entirely, handled elsewhere.
        let entered = port.partner.as_ref().is_some_and(|p| {
            p.alt_modes
                .iter()
                .any(|m| m.svid == Some(SVID_DISPLAYPORT) && m.active == Some(true))
        });
        if entered {
            continue;
        }

        out.push(Finding {
            code: "ALT_MODE_NOT_REQUESTED".into(),
            severity: Severity::Info,
            confidence: Confidence::Measured,
            subject,
            title: "DisplayPort Alt Mode is offered here but nothing asked for it".into(),
            detail: "The port can carry DisplayPort. The attached device never requested it, \
                     which is normal for a charger or a plain USB device — an unentered mode is \
                     only a problem when something needed it."
                .to_string(),
            evidence: vec![format!(
                "port advertises SVID ff01; partner {} does not claim it active",
                port.partner.as_ref().map(|p| p.sysfs_name.as_str()).unwrap_or("?")
            )],
            suggestion: None,
        });
    }
}

/// Throughput below the link rate, explained by the storage medium.
///
/// Only reachable after `usbdiag probe throughput`, which needs root. Untested
/// against hardware for the same reason as the rest of that path — see task
/// #23.
fn medium_explains_throughput(snap: &Snapshot, findings: &[Finding], out: &mut Vec<Finding>) {
    for sample in &snap.throughput {
        // A contended read measures the wrong thing, and the model says so.
        // Clearing a device on a number that was never valid would be worse
        // than staying quiet.
        if sample.error.is_some() || sample.contended_bytes.unwrap_or(0) > 0 {
            continue;
        }
        let (Some(rate), Some(dev)) = (sample.bytes_per_second, owning_device(snap, &sample.device))
        else {
            continue;
        };
        let subject = Subject::Device(dev.sysfs_name.clone());
        if accused(findings, &subject) {
            continue;
        }
        let Some(link) = dev.speed.as_ref() else {
            continue;
        };
        // Rotating media is the only medium the kernel reports with enough
        // confidence to explain a shortfall. `Unknown` means the bridge did not
        // answer, and guessing there is what this rule exists to avoid.
        let rotating = snap
            .block_devices
            .iter()
            .find(|b| b.name == sample.device)
            .and_then(|b| b.rotational)
            == Some(true);
        if !rotating || rate >= link.practical_bps() {
            continue;
        }

        out.push(Finding {
            code: "MEDIUM_EXPLAINS_THROUGHPUT".into(),
            severity: Severity::Info,
            confidence: Confidence::Measured,
            subject,
            title: format!(
                "{} reads slower than the link, and the disk explains it",
                dev.label()
            ),
            detail: format!(
                "The measured rate is below what a {} link can carry, but the kernel reports \
                 this device as rotating media, and a spinning disk cannot saturate that link \
                 regardless of the cable. The bottleneck is the platters.",
                link.short()
            ),
            evidence: vec![format!(
                "{} rotational=1, measured {:.0} MB/s against a {} link",
                sample.device,
                rate / 1.0e6,
                link.short()
            )],
            suggestion: None,
        });
    }
}

fn owning_device<'a>(snap: &'a Snapshot, block_name: &str) -> Option<&'a UsbDevice> {
    let block = snap.block_devices.iter().find(|b| b.name == block_name)?;
    let path = block.sysfs_path.to_string_lossy().to_string();
    snap.devices()
        .into_iter()
        .filter(|d| d.has_interface_class(CLASS_MASS_STORAGE))
        .find(|d| path.contains(&format!("/{}/", d.sysfs_name)))
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// One sentence per subject.
///
/// The headline is always a finding's title or [`Verdict::NOTHING_FOUND`] —
/// there is deliberately no branch that composes a sentence of its own, so a
/// verdict cannot outrun its evidence.
pub fn verdicts(snap: &Snapshot, findings: &[Finding], exonerations: &[Finding]) -> Vec<Verdict> {
    let mut subjects: Vec<Subject> = vec![Subject::Host];
    for port in &snap.ports {
        subjects.push(Subject::Port(port.name.clone()));
        if port.partner.is_some() {
            subjects.push(Subject::Cable(port.name.clone()));
        }
    }
    // Root hubs are controllers, not things a user plugs in or forms an opinion
    // about. Eight of them on this machine would have been eight verdicts
    // nobody asked for.
    for dev in snap.devices().into_iter().filter(|d| !d.is_root_hub) {
        subjects.push(Subject::Device(dev.sysfs_name.clone()));
    }

    subjects
        .into_iter()
        .map(|s| verdict_for(&s, findings, exonerations))
        .collect()
}

fn verdict_for(subject: &Subject, findings: &[Finding], exonerations: &[Finding]) -> Verdict {
    let mine: Vec<&Finding> = findings.iter().filter(|f| &f.subject == subject).collect();
    let clears: Vec<&Finding> = exonerations
        .iter()
        .filter(|f| &f.subject == subject)
        .collect();

    // Worst first; ties broken by the order the engine already sorted into, so
    // the headline is stable across runs.
    let worst = mine.iter().max_by_key(|f| f.severity);

    // Info does not make a subject "minor". Info means worth knowing and not
    // worth acting on, so a subject whose only findings are Info is *clear* —
    // calling it Minor would invent a problem out of a note. Its codes still
    // land in `because`, and its title can still be the headline when there is
    // no exoneration with a better claim on the line.
    let (outcome, headline) = match worst {
        Some(f) if f.severity >= Severity::Medium => (Outcome::Fault, f.title.clone()),
        Some(f) if f.severity >= Severity::Low => (Outcome::Minor, f.title.clone()),
        _ => (
            Outcome::Clear,
            best_headline(&clears)
                .or_else(|| worst.map(|f| f.title.clone()))
                .unwrap_or_else(|| Verdict::NOTHING_FOUND.to_string()),
        ),
    };

    let mut because: Vec<String> = mine.iter().map(|f| f.code.clone()).collect();
    because.extend(clears.iter().map(|f| f.code.clone()));
    because.sort();
    because.dedup();

    Verdict {
        subject: subject.clone(),
        outcome,
        headline,
        because,
    }
}

fn best_headline(clears: &[&Finding]) -> Option<String> {
    HEADLINE_PRIORITY
        .iter()
        .find_map(|code| clears.iter().find(|f| f.code == *code))
        .or_else(|| clears.first())
        .map(|f| f.title.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag;
    use crate::test_support::*;

    fn codes(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|x| x.code.as_str()).collect()
    }

    /// A machine with several subjects in several states, so the properties
    /// below are exercised against more than one shape at a time.
    fn busy_snapshot() -> Snapshot {
        let mut snap = empty_snapshot();
        snap.ports.push(charging_port(100_000, None, 20_000, 5_000));
        snap.ports.push(idle_port());

        let mut usb4 = root_hub("usb4", 10_000.0);
        let mut ssd = device("4-1", "3.00", 5_000.0, Some("usb4"));
        ssd.product = Some("Ultra USB 3.0".into());
        usb4.children.push(ssd);
        snap.buses.push(usb4);

        let mut usb3 = root_hub("usb3", 480.0);
        usb3.children.push(device("3-4", "2.00", 12.0, Some("usb3")));
        snap.buses.push(usb3);
        snap
    }

    /// The headline must be traceable to a finding, always. This is the one
    /// property that keeps a verdict from becoming a second rule engine.
    #[test]
    fn every_headline_is_quoted_or_the_fallback() {
        let snap = busy_snapshot();
        let findings = diag::analyze(&snap);
        let clears = exonerate(&snap, &findings);

        for v in verdicts(&snap, &findings, &clears) {
            let quoted = findings
                .iter()
                .chain(clears.iter())
                .any(|f| f.title == v.headline);
            assert!(
                quoted || v.headline == Verdict::NOTHING_FOUND,
                "{:?} invented a headline: {:?}",
                v.subject,
                v.headline
            );
        }
    }

    /// An exoneration beside an accusation of the same subject would be the
    /// tool contradicting itself in one screen.
    #[test]
    fn a_subject_is_never_both_accused_and_cleared() {
        let snap = busy_snapshot();
        let findings = diag::analyze(&snap);
        for clear in exonerate(&snap, &findings) {
            assert!(
                !accused(&findings, &clear.subject),
                "{} exonerates an accused subject {:?}",
                clear.code,
                clear.subject
            );
        }
    }

    #[test]
    fn exonerations_are_always_info() {
        let snap = busy_snapshot();
        let findings = diag::analyze(&snap);
        for c in exonerate(&snap, &findings) {
            assert_eq!(c.severity, Severity::Info, "{} is not Info", c.code);
        }
    }

    /// A contract that matched the offer clears the cable — the sentence the
    /// tool exists to be able to say, and the only way to reach it on a
    /// platform that never exposes an e-marker.
    #[test]
    fn a_contract_matching_the_offer_clears_the_cable() {
        let mut snap = empty_snapshot();
        snap.ports.push(charging_port(
            /* offer_mw */ 100_000,
            /* cable_current_ma */ None,
            /* contract_v */ 20_000,
            /* contract_a */ 5_000,
        ));
        let findings = diag::analyze(&snap);
        let clears = exonerate(&snap, &findings);
        assert!(codes(&clears).contains(&"CABLE_NOT_LIMITING"));

        let v = verdicts(&snap, &findings, &clears);
        let cable = v
            .iter()
            .find(|v| matches!(v.subject, Subject::Cable(_)))
            .expect("a cable verdict");
        assert_eq!(cable.outcome, Outcome::Clear);
        // The exoneration wins the headline over the Info-level e-marker note,
        // which is also about this cable and says far less.
        assert!(cable.headline.contains("not what limits charging"));
        assert!(cable.because.contains(&"CABLE_NOT_LIMITING".to_string()));
        assert!(cable.because.contains(&"CABLE_EMARKER_NOT_REPORTED".to_string()));
    }

    /// The same port, with a cable that did cap the contract, must not be
    /// cleared.
    #[test]
    fn a_capped_contract_does_not_clear_the_cable() {
        let mut snap = empty_snapshot();
        snap.ports.push(charging_port(100_000, Some(3_000), 20_000, 3_000));
        let findings = diag::analyze(&snap);
        assert!(!codes(&exonerate(&snap, &findings)).contains(&"CABLE_NOT_LIMITING"));
    }

    /// A USB 3.0 device on a 10 Gbps port is at its own ceiling; the same
    /// device on a 5 Gbps port is unremarkable and should stay silent.
    #[test]
    fn a_usb3_device_on_a_faster_port_is_cleared() {
        let mut snap = empty_snapshot();
        let mut usb4 = root_hub("usb4", 10_000.0);
        usb4.children.push(device("4-1", "3.00", 5_000.0, Some("usb4")));
        snap.buses.push(usb4);

        let findings = diag::analyze(&snap);
        assert!(codes(&exonerate(&snap, &findings)).contains(&"LINK_AT_DEVICE_MAXIMUM"));

        // Same device, port no faster than it: nothing worth saying.
        let mut snap = empty_snapshot();
        let mut usb4 = root_hub("usb4", 5_000.0);
        usb4.children.push(device("4-1", "3.00", 5_000.0, Some("usb4")));
        snap.buses.push(usb4);
        let findings = diag::analyze(&snap);
        assert!(!codes(&exonerate(&snap, &findings)).contains(&"LINK_AT_DEVICE_MAXIMUM"));
    }

    /// `bcdUSB 3.10` is claimed by Gen 1 hubs that top out at 5 Gbps as well as
    /// by Gen 2 devices that reach 10 — `6-1` on the development machine is the
    /// former. Neither can be told from the other here, so neither is cleared.
    #[test]
    fn a_usb31_device_is_too_ambiguous_to_clear() {
        let mut snap = empty_snapshot();
        let mut usb6 = root_hub("usb6", 10_000.0);
        usb6.children
            .push(device("6-1", "3.10", 5_000.0, Some("usb6")));
        snap.buses.push(usb6);

        let findings = diag::analyze(&snap);
        assert!(!codes(&exonerate(&snap, &findings)).contains(&"LINK_AT_DEVICE_MAXIMUM"));
    }

    /// `bcdUSB 2.00` says which specification the descriptor was written
    /// against, not what the silicon can do, so a 12 Mbps device declaring it
    /// must never be described as running at its maximum.
    #[test]
    fn a_full_speed_device_is_never_called_maxed_out() {
        let mut snap = empty_snapshot();
        let mut usb3 = root_hub("usb3", 480.0);
        usb3.children.push(device("3-4", "2.00", 12.0, Some("usb3")));
        snap.buses.push(usb3);

        let findings = diag::analyze(&snap);
        assert!(!codes(&exonerate(&snap, &findings)).contains(&"LINK_AT_DEVICE_MAXIMUM"));
    }

    /// Every subject in the snapshot gets an answer, including the ones no rule
    /// touched — that absence is the whole complaint this task addresses.
    #[test]
    fn every_subject_gets_a_verdict() {
        let snap = busy_snapshot();
        let findings = diag::analyze(&snap);
        let v = verdicts(&snap, &findings, &exonerate(&snap, &findings));

        assert!(v.iter().any(|v| v.subject == Subject::Host));
        for dev in snap.devices() {
            let has = v
                .iter()
                .any(|v| v.subject == Subject::Device(dev.sysfs_name.clone()));
            if dev.is_root_hub {
                // A controller is not something anyone forms an opinion about,
                // and there are eight of them on the development machine.
                assert!(!has, "root hub {} got a verdict", dev.sysfs_name);
            } else {
                assert!(has, "no verdict for {}", dev.sysfs_name);
            }
        }
        for port in &snap.ports {
            assert!(
                v.iter()
                    .any(|v| v.subject == Subject::Port(port.name.clone()))
            );
        }
    }

    /// A `Clear` with nothing to cite is a weaker statement than one with
    /// evidence, and the difference has to survive into the data or a UI
    /// cannot draw it differently.
    #[test]
    fn an_uncited_clear_verdict_has_an_empty_because() {
        let snap = empty_snapshot();
        let v = verdicts(&snap, &[], &[]);
        let host = v.iter().find(|v| v.subject == Subject::Host).unwrap();
        assert_eq!(host.outcome, Outcome::Clear);
        assert_eq!(host.headline, Verdict::NOTHING_FOUND);
        assert!(host.because.is_empty());
    }
}

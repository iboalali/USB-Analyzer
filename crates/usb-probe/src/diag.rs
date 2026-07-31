//! Diagnostic rules.
//!
//! Each rule compares two or more independently-read facts and reports where
//! they disagree. That framing matters: Linux can *measure* what a link
//! negotiated and what a charger offered, but it cannot measure a cable. Every
//! conclusion about a cable is therefore an inference from a disagreement —
//! "this device says it speaks USB 3.2, the port says it speaks USB 3.2, and yet
//! the link came up at 480 Mbps, so something in between is the limit".
//!
//! Findings carry a [`Confidence`] so a UI can distinguish a register read from
//! a symptom match, and they always carry the readings they rest on.

use crate::model::*;
use crate::vdo;

/// Reset count at which repetition stops looking like coincidence.
const RESET_WARN: usize = 3;
const RESET_ALARM: usize = 8;

/// Fraction of an offered/desired power level below which the gap is worth
/// reporting, expressed as a percentage to keep the math in integers.
const POWER_GAP_PCT: u32 = 80;

/// Contract current above which the 5 A conclusion is unambiguous. Cables can
/// only declare 3 A or 5 A, so a contract well clear of 3 A rules out the former;
/// a contract *just* over 3 A (a 65 W charger's 3.25 A) is better explained as
/// possibly captive, since captive cables carry no e-marker at all.
const CLEARLY_5A_MA: u32 = 4500;

/// Run every rule against a snapshot, strongest finding first.
pub fn analyze(snap: &Snapshot) -> Vec<Finding> {
    let mut f = Vec::new();

    host_rules(snap, &mut f);
    for port in &snap.ports {
        port_rules(snap, port, &mut f);
    }
    for dev in snap.devices() {
        device_rules(snap, dev, &mut f);
    }
    hub_port_rules(snap, &mut f);

    // Strongest first, then by confidence, then stable by code for determinism.
    f.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| conf_rank(a.confidence).cmp(&conf_rank(b.confidence)))
            .then_with(|| a.code.cmp(&b.code))
            .then_with(|| a.subject.display().cmp(&b.subject.display()))
    });
    f
}

fn conf_rank(c: Confidence) -> u8 {
    match c {
        Confidence::Measured => 0,
        Confidence::Inferred => 1,
        Confidence::Heuristic => 2,
    }
}

/// Convenience wrapper: snapshot plus findings.
pub fn report(snap: Snapshot) -> Report {
    let findings = analyze(&snap);
    Report {
        snapshot: snap,
        findings,
    }
}

// ---------------------------------------------------------------------------
// Host-level rules
// ---------------------------------------------------------------------------

fn host_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    let log = &snap.kernel_log;

    if log.source == KernelLogSource::Unavailable {
        out.push(Finding {
            code: "KERNEL_LOG_UNAVAILABLE".into(),
            severity: Severity::Info,
            confidence: Confidence::Measured,
            subject: Subject::Host,
            title: "Kernel log not readable — reset and enumeration history is missing".into(),
            detail: log
                .note
                .clone()
                .unwrap_or_else(|| "No kernel log source could be read.".into()),
            evidence: vec!["kernel.dmesg_restrict blocks /dev/kmsg for non-root users".into()],
            suggestion: Some(
                "Re-run with sudo, or grant journal access, to enable the link-quality rules."
                    .into(),
            ),
        });
    }

    let hc: Vec<&KernelEvent> = log
        .events
        .iter()
        .filter(|e| e.kind == EventKind::HostControllerFailure)
        .collect();
    if !hc.is_empty() {
        out.push(Finding {
            code: "HOST_CONTROLLER_FAILURE".into(),
            severity: Severity::High,
            confidence: Confidence::Measured,
            subject: Subject::Host,
            title: format!(
                "xHCI host controller failed {} time(s) this boot",
                hc.len()
            ),
            detail: "The kernel declared a USB host controller unresponsive or dead. Every device \
                     on that controller drops out when this happens. This is a host-side or \
                     power-management fault, not a cable fault."
                .into(),
            evidence: hc.iter().rev().take(4).map(|e| e.text.clone()).collect(),
            suggestion: Some(
                "Check for a BIOS/firmware update and test whether it correlates with \
                 suspend/resume."
                    .into(),
            ),
        });
    }

    // Bus-wide faults from the ring buffer. Table-driven so a newly classified
    // event kind cannot be silently left without a rule.
    const BUS_FAULTS: [(EventKind, &str, Severity, &str, &str, &str); 3] = [
        (
            EventKind::OverCurrent,
            "BUS_OVER_CURRENT",
            Severity::High,
            "Over-current condition reported on a USB port",
            "The port shut down because the attached device or cable drew more current than the \
             port allows. A shorted cable, a damaged connector, or a device with a failed power \
             stage all produce this.",
            "Inspect the cable and connectors for damage before reusing them.",
        ),
        (
            EventKind::InsufficientPower,
            "BUS_POWER_INSUFFICIENT",
            Severity::Medium,
            "A device was denied its requested bus power",
            "The device asked for more current than the port could grant, so the kernel rejected \
             its configuration. Common on bus-powered hubs with several devices behind them.",
            "Use a self-powered hub, or connect the device directly to a port on the machine.",
        ),
        (
            EventKind::InsufficientBandwidth,
            "BUS_BANDWIDTH_INSUFFICIENT",
            Severity::Medium,
            "A device could not be configured for lack of bus bandwidth",
            "The host controller could not reserve the periodic bandwidth the device asked for. \
             Isochronous devices — webcams and audio interfaces — reserve bandwidth up front, so \
             two of them on one controller can exhaust it even when nothing is transferring.",
            "Move the device to a port on a different controller, shown as a different bus here.",
        ),
    ];

    for (kind, code, sev, title, detail, suggestion) in BUS_FAULTS {
        let hits: Vec<&KernelEvent> = log.events.iter().filter(|e| e.kind == kind).collect();
        if hits.is_empty() {
            continue;
        }
        out.push(Finding {
            code: code.into(),
            severity: sev,
            confidence: Confidence::Measured,
            subject: Subject::Host,
            title: title.into(),
            detail: detail.into(),
            evidence: hits.iter().rev().take(4).map(|e| e.text.clone()).collect(),
            suggestion: Some(suggestion.into()),
        });
    }
}

// ---------------------------------------------------------------------------
// Type-C port rules
// ---------------------------------------------------------------------------

fn port_rules(snap: &Snapshot, port: &TypecPort, out: &mut Vec<Finding>) {
    let Some(partner) = &port.partner else {
        return; // Nothing attached: nothing to diagnose.
    };

    let contract = port.power_supply.as_ref();
    let offered_mw = partner.pd.as_ref().and_then(|pd| pd.max_source_power_mw());
    let wanted_mw = port.local_pd.as_ref().and_then(|pd| pd.max_sink_power_mw());
    let contract_mw = contract.and_then(|c| c.contract_power_mw());

    // --- Attached device does not speak PD at all --------------------------
    if partner.supports_pd == Some(false) {
        let (title, detail) = if port.is_sourcing() {
            (
                "Attached device does not support Power Delivery — 5 V only".to_string(),
                "The device negotiated power the old way, through the Type-C CC resistor \
                 advertisement, so it is limited to 5 V at whatever current this port advertises. \
                 Normal for watch chargers, small accessories and most captive-cable devices; \
                 higher voltages would require the device to implement PD."
                    .to_string(),
            )
        } else {
            (
                "Attached charger does not support Power Delivery — 5 V only".to_string(),
                "This supply offers no PD contract, so the machine can only draw 5 V from it at \
                 the advertised Type-C current. Fast charging needs a PD source."
                    .to_string(),
            )
        };
        out.push(Finding {
            code: "PARTNER_NO_PD".into(),
            severity: Severity::Info,
            confidence: Confidence::Measured,
            subject: Subject::Port(port.name.clone()),
            title,
            detail,
            evidence: vec![
                "partner supports_usb_power_delivery = no".into(),
                format!(
                    "power_operation_mode = {} ({})",
                    port.power_operation_mode.as_deref().unwrap_or("?"),
                    if port.is_sourcing() {
                        "advertised to the device"
                    } else {
                        "advertised to this machine"
                    }
                ),
            ],
            suggestion: None,
        });
    }

    // --- PD contract present at all? ---------------------------------------
    if partner.speaks_pd() && !port.pd_contract_active() {
        out.push(Finding {
            code: "PD_NO_CONTRACT".into(),
            severity: Severity::Medium,
            confidence: Confidence::Inferred,
            subject: Subject::Port(port.name.clone()),
            title: "Attached device speaks Power Delivery, but no PD contract is in effect".into(),
            detail: "Without a contract the link falls back to Type-C current advertisement \
                     (5 V at 0.5-3 A). If this is a charger, it is charging at a small fraction \
                     of its rating."
                .into(),
            evidence: vec![
                format!(
                    "power_operation_mode = {}",
                    port.power_operation_mode.as_deref().unwrap_or("?")
                ),
                "partner supports_usb_power_delivery = yes".into(),
            ],
            suggestion: Some(
                "Re-seat the connector, then try a different cable — a cable with a damaged CC \
                 line prevents PD negotiation while still passing 5 V."
                    .into(),
            ),
        });
    }

    // --- Charger weaker than the machine wants -----------------------------
    // Only meaningful while drawing power: when this machine is the source, its
    // own sink capabilities are irrelevant to what the attached device offers.
    if let (Some(offered), Some(wanted)) = (offered_mw, wanted_mw) {
        if port.is_sinking() && offered * 100 < wanted * POWER_GAP_PCT {
            out.push(Finding {
                code: "PD_SOURCE_BELOW_SINK_CAPABILITY".into(),
                severity: Severity::Low,
                confidence: Confidence::Measured,
                subject: Subject::Port(port.name.clone()),
                title: format!(
                    "Charger offers {} but this machine can accept {}",
                    watts(offered),
                    watts(wanted)
                ),
                detail: "Not a fault — the supply is simply smaller than the port's maximum. \
                         Expect slower charging, and possible battery drain under heavy load."
                    .into(),
                evidence: vec![
                    format!("best source PDO: {}", best_pdo_desc(partner.pd.as_ref(), true)),
                    format!("best local sink PDO: {}", best_pdo_desc(port.local_pd.as_ref(), false)),
                ],
                suggestion: None,
            });
        }
    }

    // --- Negotiated less than offered --------------------------------------
    if let (Some(offered), Some(got)) = (offered_mw, contract_mw) {
        if port.is_sinking() && got * 100 < offered * POWER_GAP_PCT {
            out.push(Finding {
                code: "PD_CONTRACT_BELOW_OFFER".into(),
                severity: Severity::Medium,
                confidence: Confidence::Measured,
                subject: Subject::Port(port.name.clone()),
                title: format!(
                    "Negotiated only {} from a supply offering {}",
                    watts(got),
                    watts(offered)
                ),
                detail: "The source advertised more power than the contract actually took. The \
                         usual causes are a cable rated below the required current, a cable \
                         without an e-marker (which caps the link at 3 A), or a host policy limit."
                    .into(),
                evidence: vec![
                    format!("contract: {}", contract_desc(contract)),
                    format!("best source PDO: {}", best_pdo_desc(partner.pd.as_ref(), true)),
                    cable_summary(port),
                ],
                suggestion: Some(
                    "Test with a known 5 A e-marked cable; if the contract jumps, the cable was \
                     the limit."
                        .into(),
                ),
            });
        }
    }

    // --- Cable rules -------------------------------------------------------
    match &port.cable {
        Some(cable) => cable_rules(snap, port, cable, offered_mw, contract, out),
        // A contract above 3 A is only legal over a 5 A e-marked cable, so the
        // cable must have one even though the controller never reported it. That
        // is a firmware reporting gap, not a cable limitation — and saying
        // "unmarked cables are limited to 3 A" here would be flatly wrong.
        None if contract.is_some_and(|c| c.contract_requires_5a_cable()) => {
            let ma = contract.and_then(|c| c.contract_current_ma()).unwrap_or(0);
            let amps = milliamps(ma);
            // A source may advertise more than 3 A only after verifying the
            // cable over SOP', or when the cable is captive to the charger. We
            // cannot tell which from here, so do not assert "5 A e-marked"
            // outright — at 3.25 A a captive cable is entirely plausible.
            let (title, detail) = if ma >= CLEARLY_5A_MA {
                (
                    format!(
                        "Cable is 5 A rated — the {amps} contract leaves no alternative, though \
                         the controller does not report the e-marker"
                    ),
                    "The only VBUS current ratings a cable can declare are 3 A and 5 A, and this \
                     contract exceeds 3 A by a wide margin, so the cable is 5 A capable. This \
                     platform's port controller simply does not pass SOP' cable data up to the \
                     kernel, which is why no cable node exists in sysfs."
                        .to_string(),
                )
            } else {
                (
                    format!("Cable is carrying {amps}, above the 3 A unmarked-cable limit"),
                    "Power Delivery permits a source to advertise more than 3 A only after it has \
                     read a 5 A rating from the cable's e-marker, or when the cable is captive to \
                     the charger and needs no e-marker of its own. This controller does not report \
                     SOP' data, so which of the two applies cannot be determined — but either way \
                     the cable is adequate for the contract in effect."
                        .to_string(),
                )
            };
            out.push(Finding {
                code: "CABLE_EMARKER_NOT_REPORTED".into(),
                severity: Severity::Info,
                confidence: Confidence::Inferred,
                subject: Subject::Cable(port.name.clone()),
                title,
                detail,
                evidence: vec![
                    format!("contract current {amps} (> 3 A unmarked limit)"),
                    format!("{}-cable does not exist in sysfs", port.name),
                ],
                suggestion: Some(
                    "Nothing to do. Cable capability cannot be read on this platform, but this \
                     cable is carrying the active contract without trouble."
                        .into(),
                ),
            });
        }
        // Otherwise a missing e-marker only matters when something could want
        // more than an unmarked cable provides: more than 3 A, or SuperSpeed
        // data. A 5 V non-PD accessory makes the cable's rating irrelevant, and
        // reporting it there is noise that trains the user to ignore findings.
        None if cable_rating_could_matter(snap, port, partner) => {
            out.push(Finding {
                code: "CABLE_NOT_EMARKED".into(),
                severity: Severity::Info,
                confidence: Confidence::Heuristic,
                subject: Subject::Cable(port.name.clone()),
                title: "No cable identity available — cable capability cannot be read".into(),
                detail: "The kernel exposes no e-marker for this cable. Either the cable has no \
                         e-marker chip (normal for USB 2.0 and 60 W cables), or the port \
                         controller does not report SOP' data. An unmarked cable is limited to \
                         3 A and, in practice, often to USB 2.0 data rates."
                    .into(),
                evidence: vec![format!("{}-cable does not exist in sysfs", port.name)],
                suggestion: Some(
                    "Nothing to fix by itself. To rule the cable out, swap in a known-good \
                     5 A e-marked cable and compare."
                        .into(),
                ),
            });
        }
        None => {}
    }

    // --- Alt mode available but not engaged --------------------------------
    for am in &partner.alt_modes {
        if am.svid == Some(0xff01) && am.active == Some(false) {
            out.push(Finding {
                code: "DP_ALTMODE_NOT_ACTIVE".into(),
                severity: Severity::Low,
                confidence: Confidence::Measured,
                subject: Subject::Port(port.name.clone()),
                title: "Attached device advertises DisplayPort Alt Mode, but it is not active"
                    .into(),
                detail: "DisplayPort over USB-C needs the cable to carry the SuperSpeed pairs. A \
                         USB 2.0-only or charge-only cable leaves the mode advertised but unusable."
                    .into(),
                evidence: vec![format!(
                    "{}: svid=ff01 active=no vdo={}",
                    am.sysfs_name,
                    am.vdo.map(|v| v.hex.to_string()).unwrap_or_else(|| "?".into())
                )],
                suggestion: Some("Use a full-featured USB-C cable rated for video.".into()),
            });
        }
    }
}

fn cable_rules(
    snap: &Snapshot,
    port: &TypecPort,
    cable: &Cable,
    offered_mw: Option<u32>,
    contract: Option<&PortPowerSupply>,
    out: &mut Vec<Finding>,
) {
    let Some(id) = &cable.identity else {
        return;
    };
    let d = &id.decoded;

    // Cable current rating vs. what the supply wants to push.
    if let Some(rating) = d.cable_current_ma {
        let needs_5a = offered_mw.is_some_and(|mw| mw > 60_000)
            || contract
                .and_then(|c| c.current_max_ma)
                .is_some_and(|ma| ma > 3000);
        if rating <= 3000 && needs_5a {
            out.push(Finding {
                code: "CABLE_CURRENT_LIMIT".into(),
                severity: Severity::Medium,
                confidence: Confidence::Measured,
                subject: Subject::Cable(port.name.clone()),
                title: "Cable is rated 3 A, which caps this link at 60 W".into(),
                detail: "The cable's e-marker declares 3 A current handling. Anything above 60 W \
                         (20 V x 3 A) requires a 5 A e-marked cable, so the higher PDOs cannot be \
                         used no matter what the charger offers."
                    .into(),
                evidence: vec![
                    format!("cable e-marker: {} rating", milliamps(rating)),
                    offered_mw
                        .map(|mw| format!("source offers up to {}", watts(mw)))
                        .unwrap_or_else(|| "source capabilities unknown".into()),
                ],
                suggestion: Some("Use a 5 A (100 W or 240 W) e-marked cable.".into()),
            });
        }
    }

    // Cable voltage rating vs. the contract in effect.
    if let (Some(max_mv), Some(now_mv)) = (
        d.cable_max_voltage_mv,
        contract.and_then(|c| c.voltage_now_mv).filter(|v| *v > 0),
    ) {
        if now_mv > max_mv {
            out.push(Finding {
                code: "CABLE_VOLTAGE_EXCEEDED".into(),
                severity: Severity::High,
                confidence: Confidence::Measured,
                subject: Subject::Cable(port.name.clone()),
                title: format!(
                    "Contract voltage {} exceeds the cable's {} rating",
                    volts(now_mv),
                    volts(max_mv)
                ),
                detail: "The negotiated voltage is above what the cable declares it can carry. \
                         This should not happen and points at a mis-programmed e-marker or a \
                         counterfeit cable."
                    .into(),
                evidence: vec![
                    format!("cable max VBUS: {}", volts(max_mv)),
                    format!("voltage_now: {}", volts(now_mv)),
                ],
                suggestion: Some("Stop using this cable for high-voltage charging.".into()),
            });
        }
    }

    // Cable data rating vs. what the two ends could do.
    if let Some(vdo1) = id.product_type_vdo1 {
        let cable_mbps = vdo::cable_speed_mbps(vdo1.raw);
        if cable_mbps > 0.0 && cable_mbps <= 480.0 && port.supports_usb3() {
            out.push(Finding {
                code: "CABLE_DATA_LIMIT".into(),
                severity: Severity::High,
                confidence: Confidence::Measured,
                subject: Subject::Cable(port.name.clone()),
                title: "Cable is USB 2.0 only — SuperSpeed is impossible over it".into(),
                detail: "The cable's e-marker declares USB 2.0 as its highest speed, so the port's \
                         SuperSpeed capability cannot be used. This is normal for charge-focused \
                         cables and is the single most common cause of 'my USB 3 drive is slow'."
                    .into(),
                evidence: vec![
                    format!(
                        "cable highest speed: {}",
                        d.cable_max_speed.as_deref().unwrap_or("?")
                    ),
                    format!(
                        "port usb_capability: {}",
                        port.usb_capability
                            .as_ref()
                            .map(|c| c.raw.clone())
                            .unwrap_or_else(|| "?".into())
                    ),
                    format!("cable product_type_vdo1 = {}", vdo1.hex),
                ],
                suggestion: Some("Swap in a cable rated for USB 3.2 Gen 1 or better.".into()),
            });
        }

        // Cable is fast enough, yet a device behind this port linked slow.
        if cable_mbps > 480.0 {
            for dev in devices_on_port(snap, port) {
                if dev.claims_superspeed() && dev.linked_below_superspeed() {
                    out.push(Finding {
                        code: "LINK_SLOW_DESPITE_CAPABLE_CABLE".into(),
                        severity: Severity::Medium,
                        confidence: Confidence::Inferred,
                        subject: Subject::Device(dev.sysfs_name.clone()),
                        title: format!(
                            "{} linked at {} although the cable is rated {}",
                            dev.sysfs_name,
                            dev.speed.as_ref().map(|s| s.short()).unwrap_or_default(),
                            d.cable_max_speed.as_deref().unwrap_or("faster")
                        ),
                        detail: "With the cable ruled out by its own e-marker, the limit is the \
                                 device, the receptacle, or a marginal connection that failed \
                                 SuperSpeed training and fell back."
                            .into(),
                        evidence: vec![
                            format!("device version {}", dev.usb_version.as_deref().unwrap_or("?")),
                            format!("cable rating {}", d.cable_max_speed.as_deref().unwrap_or("?")),
                        ],
                        suggestion: Some("Re-seat the plug and check for connector debris.".into()),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Device rules
// ---------------------------------------------------------------------------

fn device_rules(snap: &Snapshot, dev: &UsbDevice, out: &mut Vec<Finding>) {
    // A root hub is not behind a cable; skip link-quality rules for it.
    if !dev.is_root_hub {
        link_speed_rules(snap, dev, out);
    }
    kernel_history_rules(snap, dev, out);
}

fn link_speed_rules(snap: &Snapshot, dev: &UsbDevice, out: &mut Vec<Finding>) {
    if !(dev.claims_superspeed() && dev.linked_below_superspeed()) {
        // Not a downshift. Check the narrower dual-lane case instead.
        if dev.usb_version_num.is_some_and(|v| v >= 3.2)
            && dev.tx_lanes == Some(1)
            && dev.speed.as_ref().is_some_and(|s| s.mbps <= 10_000.0)
        {
            out.push(Finding {
                code: "LINK_SINGLE_LANE".into(),
                severity: Severity::Info,
                confidence: Confidence::Measured,
                subject: Subject::Device(dev.sysfs_name.clone()),
                title: format!(
                    "{} runs single-lane at {}",
                    dev.label(),
                    dev.speed.as_ref().map(|s| s.short()).unwrap_or_default()
                ),
                detail: "The device claims USB 3.2, which allows a two-lane (20 Gbps) link, but \
                         only one lane is in use. Two-lane operation needs a Gen 2x2 cable, port, \
                         and device all together."
                    .into(),
                evidence: vec![format!(
                    "tx_lanes=1 rx_lanes={} version={}",
                    dev.rx_lanes.unwrap_or(1),
                    dev.usb_version.as_deref().unwrap_or("?")
                )],
                suggestion: None,
            });
        }
        return;
    }

    // Downshift confirmed. Decide whether the upstream chain explains it,
    // because blaming a cable when a USB 2.0 hub is upstream would be wrong.
    let parent = dev.parent.as_deref().and_then(|p| snap.device(p));
    let upstream_is_hs = parent.is_some_and(|p| p.linked_below_superspeed());

    let (severity, detail, suggestion) = if dev.is_internal() {
        (
            Severity::Low,
            "This is an internal device (`removable = fixed`), so there is no cable to swap. \
             Either it is wired to a USB 2.0-only internal header, or its SuperSpeed pairs are \
             not routed on this board."
                .to_string(),
            None,
        )
    } else if upstream_is_hs {
        let pname = parent.map(|p| p.label()).unwrap_or_default();
        (
            Severity::Medium,
            format!(
                "The upstream link is itself USB 2.0, so this device cannot exceed it. The limit \
                 is the chain through \"{pname}\", not necessarily this device's own cable."
            ),
            Some(
                "Connect the device directly to a SuperSpeed port, or replace the upstream hub \
                 or its cable."
                    .to_string(),
            ),
        )
    } else {
        (
            Severity::High,
            "The device advertises SuperSpeed and the upstream port provides it, yet the link \
             negotiated USB 2.0. The SuperSpeed differential pairs are not getting through — a \
             USB 2.0-only cable, a charge-only cable, a damaged cable, or a dirty connector."
                .to_string(),
            Some(
                "Swap the cable for one known to carry USB 3 data; that resolves the large \
                 majority of these."
                    .to_string(),
            ),
        )
    };

    let mut evidence = vec![
        format!(
            "device claims USB {}",
            dev.usb_version.as_deref().unwrap_or("?")
        ),
        format!(
            "negotiated {}",
            dev.speed
                .as_ref()
                .map(|s| s.label.clone())
                .unwrap_or_else(|| "?".into())
        ),
    ];
    if let Some(p) = parent {
        evidence.push(format!(
            "upstream {} at {}",
            p.sysfs_name,
            p.speed.as_ref().map(|s| s.short()).unwrap_or_default()
        ));
    }

    out.push(Finding {
        code: "LINK_BELOW_DEVICE_CAPABILITY".into(),
        severity,
        confidence: Confidence::Inferred,
        subject: Subject::Device(dev.sysfs_name.clone()),
        title: format!(
            "{} supports SuperSpeed but linked at {}",
            dev.label(),
            dev.speed.as_ref().map(|s| s.short()).unwrap_or_default()
        ),
        detail,
        evidence,
        suggestion,
    });
}

fn kernel_history_rules(snap: &Snapshot, dev: &UsbDevice, out: &mut Vec<Finding>) {
    let events = snap.kernel_log.for_device(&dev.sysfs_name);
    if events.is_empty() {
        return;
    }

    let resets = events
        .iter()
        .filter(|e| e.kind == EventKind::DeviceReset)
        .count();
    if resets >= RESET_WARN {
        // An internal device has no cable to blame, and its resets are usually
        // just runtime power management cycling it. Reporting those at High
        // would drown out the cases that matter.
        let (severity, detail, suggestion) = if dev.is_internal() {
            (
                Severity::Low,
                "This is an internal device (`removable = fixed`), so no cable or connector is \
                 involved. Repeated resets on internal devices are usually runtime power \
                 management suspending and resuming them, which is normal."
                    .to_string(),
                Some(
                    "Only worth pursuing if the device actually misbehaves; then look at its \
                     autosuspend settings rather than at cabling."
                        .to_string(),
                ),
            )
        } else {
            (
                if resets >= RESET_ALARM {
                    Severity::High
                } else {
                    Severity::Medium
                },
                "Repeated resets on a device that otherwise works are the classic signature of a \
                 marginal connection: the link drops, the kernel recovers it, and the cycle \
                 repeats. A worn cable, a loose plug, or aggressive autosuspend on a device that \
                 handles it badly all look like this."
                    .to_string(),
                Some(
                    "Try another cable and port. If the resets follow the device, suspect the \
                     device or its power management rather than the cable."
                        .to_string(),
                ),
            )
        };
        out.push(Finding {
            code: "DEVICE_RESET_STORM".into(),
            severity,
            confidence: Confidence::Heuristic,
            subject: Subject::Device(dev.sysfs_name.clone()),
            title: format!("{} was reset {resets} times this boot", dev.label()),
            detail,
            evidence: events
                .iter()
                .filter(|e| e.kind == EventKind::DeviceReset)
                .rev()
                .take(3)
                .map(|e| {
                    format!(
                        "{} {}",
                        e.timestamp.as_deref().unwrap_or(""),
                        e.text
                    )
                    .trim()
                    .to_string()
                })
                .collect(),
            suggestion,
        });
    }

    for (kind, code, title, detail) in [
        (
            EventKind::CableSuspect,
            "KERNEL_BLAMED_CABLE",
            "The kernel explicitly reported a bad cable",
            "The hub driver could not enable the port and logged its cable warning. This message \
             is emitted when a port fails to come up at all.",
        ),
        (
            EventKind::EnumerationFailure,
            "ENUMERATION_FAILURE",
            "Device failed to enumerate",
            "Descriptor reads failed or the device never accepted an address. Signal integrity, \
             insufficient power, and failing device hardware all produce this.",
        ),
        (
            EventKind::LinkTrainingFailure,
            "LINK_TRAINING_FAILURE",
            "A port failed to train its link",
            "The port could not bring the link up cleanly and either retried or gave up. On a \
             SuperSpeed port this usually ends in a silent fall back to USB 2.0.",
        ),
    ] {
        let hits: Vec<&&KernelEvent> = events.iter().filter(|e| e.kind == kind).collect();
        if hits.is_empty() {
            continue;
        }
        out.push(Finding {
            code: code.into(),
            severity: Severity::High,
            confidence: Confidence::Measured,
            subject: Subject::Device(dev.sysfs_name.clone()),
            title: format!("{}: {}", dev.sysfs_name, title),
            detail: detail.into(),
            evidence: hits.iter().rev().take(3).map(|e| e.text.clone()).collect(),
            suggestion: Some("Replace the cable first — it is the cheapest variable.".into()),
        });
    }
}

fn hub_port_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    for dev in snap.devices() {
        for p in &dev.ports {
            if p.over_current_count.is_some_and(|c| c > 0) {
                out.push(Finding {
                    code: "PORT_OVER_CURRENT_COUNT".into(),
                    severity: Severity::High,
                    confidence: Confidence::Measured,
                    subject: Subject::Device(dev.sysfs_name.clone()),
                    title: format!(
                        "{} tripped over-current protection {} time(s)",
                        p.name,
                        p.over_current_count.unwrap_or(0)
                    ),
                    detail: "This is a hardware counter, not an inference: the port's current \
                             limiter actually fired. A shorted or crushed cable, a bent pin, or \
                             debris bridging VBUS to ground will do it."
                        .into(),
                    evidence: vec![format!(
                        "{}/over_current_count = {}",
                        p.name,
                        p.over_current_count.unwrap_or(0)
                    )],
                    suggestion: Some(
                        "Inspect the cable and both connectors for damage before reusing them."
                            .into(),
                    ),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Correlation and formatting helpers
// ---------------------------------------------------------------------------

/// USB devices sitting behind a Type-C port.
///
/// Only the firmware-provided `connector` link is trusted here. Matching by
/// `physical_location` is tempting but ambiguous on real hardware (four USB
/// receptacles can share one location descriptor), and a wrong correlation
/// would attach a finding to the wrong cable.
/// Whether a cable's rating could plausibly limit anything on this port.
///
/// True when the partner speaks PD (so voltages above 5 V and currents above 3 A
/// are on the table), when the machine is drawing power (where 3 A vs 5 A decides
/// 60 W vs 100 W), or when a SuperSpeed-capable device sits behind the port.
fn cable_rating_could_matter(snap: &Snapshot, port: &TypecPort, partner: &Partner) -> bool {
    partner.speaks_pd()
        || port
            .power_supply
            .as_ref()
            .is_some_and(|ps| ps.is_drawing_power())
        || devices_on_port(snap, port)
            .iter()
            .any(|d| d.claims_superspeed())
}

pub fn devices_on_port<'a>(snap: &'a Snapshot, port: &TypecPort) -> Vec<&'a UsbDevice> {
    let mut out = Vec::new();
    for dev in snap.devices() {
        for p in &dev.ports {
            if p.connector.as_deref() != Some(port.name.as_str()) {
                continue;
            }
            if let Some(child) = p.child.as_deref().and_then(|c| snap.device(c)) {
                out.push(child);
            }
        }
    }
    out
}

fn best_pdo_desc(pd: Option<&PowerDelivery>, source: bool) -> String {
    let Some(pd) = pd else {
        return "unknown".into();
    };
    let list = if source {
        &pd.source_capabilities
    } else {
        &pd.sink_capabilities
    };
    list.iter()
        .max_by_key(|p| p.power_mw().unwrap_or(0))
        .map(|p| p.describe())
        .unwrap_or_else(|| "none advertised".into())
}

fn contract_desc(ps: Option<&PortPowerSupply>) -> String {
    let Some(ps) = ps else {
        return "unknown".into();
    };
    match (ps.voltage_now_mv, ps.current_max_ma) {
        (Some(v), Some(i)) if v > 0 => format!(
            "{} at up to {} ({})",
            volts(v),
            milliamps(i),
            ps.contract_power_mw().map(watts).unwrap_or_else(|| "?".into())
        ),
        _ => "no contract".into(),
    }
}

fn cable_summary(port: &TypecPort) -> String {
    match port.cable.as_ref().and_then(|c| c.identity.as_ref()) {
        Some(id) => format!(
            "cable: {} rating, {}",
            id.decoded
                .cable_current_ma
                .map(milliamps)
                .unwrap_or_else(|| "unstated".into()),
            id.decoded.cable_max_speed.as_deref().unwrap_or("unknown speed")
        ),
        None => "cable: no e-marker reported (3 A limit applies)".into(),
    }
}

pub fn watts(mw: u32) -> String {
    let w = mw as f64 / 1000.0;
    if w.fract().abs() < 0.05 {
        format!("{w:.0} W")
    } else {
        format!("{w:.1} W")
    }
}

pub fn volts(mv: u32) -> String {
    let v = mv as f64 / 1000.0;
    if v.fract().abs() < 0.005 {
        format!("{v:.0} V")
    } else {
        format!("{v:.1} V")
    }
}

pub fn milliamps(ma: u32) -> String {
    let a = ma as f64 / 1000.0;
    if a.fract().abs() < 0.005 {
        format!("{a:.0} A")
    } else {
        format!("{a:.2} A")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    fn codes(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|x| x.code.as_str()).collect()
    }

    #[test]
    fn flags_superspeed_device_on_a_usb2_link() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb2", 10000.0);
        hub.children.push(device("2-1", " 3.20", 480.0, Some("usb2")));
        snap.buses.push(hub);

        let f = analyze(&snap);
        let hit = f
            .iter()
            .find(|x| x.code == "LINK_BELOW_DEVICE_CAPABILITY")
            .expect("downshift must be reported");
        assert_eq!(hit.severity, Severity::High);
        assert_eq!(hit.confidence, Confidence::Inferred);
        assert!(hit.suggestion.as_deref().unwrap().contains("cable"));
    }

    #[test]
    fn blames_the_upstream_hub_rather_than_the_cable() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb2", 10000.0);
        let mut mid = device("2-1", " 2.10", 480.0, Some("usb2"));
        mid.children
            .push(device("2-1.1", " 3.20", 480.0, Some("2-1")));
        hub.children.push(mid);
        snap.buses.push(hub);

        let f = analyze(&snap);
        let hit = f
            .iter()
            .find(|x| {
                x.code == "LINK_BELOW_DEVICE_CAPABILITY"
                    && x.subject == Subject::Device("2-1.1".into())
            })
            .unwrap();
        // Downgraded, because a USB 2.0 hub upstream fully explains it.
        assert_eq!(hit.severity, Severity::Medium);
        assert!(hit.detail.contains("upstream link is itself USB 2.0"));
    }

    #[test]
    fn usb2_device_on_usb2_link_is_not_a_finding() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb1", 480.0);
        hub.children
            .push(device("1-1", " 2.10", 480.0, Some("usb1")));
        snap.buses.push(hub);
        assert!(!codes(&analyze(&snap)).contains(&"LINK_BELOW_DEVICE_CAPABILITY"));
    }

    #[test]
    fn root_hubs_are_exempt_from_link_rules() {
        let mut snap = empty_snapshot();
        // A USB 3.1 root hub reporting 480 would otherwise look like a downshift.
        snap.buses.push(root_hub_version("usb2", 480.0, " 3.10"));
        assert!(!codes(&analyze(&snap)).contains(&"LINK_BELOW_DEVICE_CAPABILITY"));
    }

    /// A soldered-down device has no cable, so its resets must not be reported
    /// as a cable problem. This is the false positive that would make the tool
    /// untrustworthy on any laptop with an internal fingerprint reader.
    #[test]
    fn internal_devices_are_not_blamed_on_cables() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb3", 480.0);
        let mut dev = device("3-4", " 2.00", 12.0, Some("usb3"));
        dev.removable = Some("fixed".into());
        hub.children.push(dev);
        snap.buses.push(hub);
        snap.kernel_log = reset_log("3-4", 21);

        let f = analyze(&snap);
        let hit = f.iter().find(|x| x.code == "DEVICE_RESET_STORM").unwrap();
        assert_eq!(hit.severity, Severity::Low, "21 resets on an internal device is routine");
        assert!(hit.detail.contains("internal device"));
        assert!(
            !hit.suggestion.as_deref().unwrap_or("").contains("another cable"),
            "must not advise swapping a nonexistent cable"
        );
    }

    #[test]
    fn internal_device_link_downshift_does_not_blame_a_cable() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb2", 10000.0);
        let mut dev = device("2-1", " 3.20", 480.0, Some("usb2"));
        dev.removable = Some("fixed".into());
        hub.children.push(dev);
        snap.buses.push(hub);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "LINK_BELOW_DEVICE_CAPABILITY")
            .unwrap();
        assert_eq!(hit.severity, Severity::Low);
        assert!(hit.detail.contains("no cable to swap"));
    }

    #[test]
    fn escalates_on_repeated_resets() {
        let mut snap = empty_snapshot();
        snap.buses.push({
            let mut h = root_hub("usb3", 480.0);
            // Explicitly removable, so the internal-device branch is not taken.
            let mut d = device("3-4", " 1.10", 12.0, Some("usb3"));
            d.removable = Some("removable".into());
            h.children.push(d);
            h
        });

        snap.kernel_log = reset_log("3-4", 4);
        let f = analyze(&snap);
        let hit = f.iter().find(|x| x.code == "DEVICE_RESET_STORM").unwrap();
        assert_eq!(hit.severity, Severity::Medium);

        snap.kernel_log = reset_log("3-4", 14);
        let f = analyze(&snap);
        let hit = f.iter().find(|x| x.code == "DEVICE_RESET_STORM").unwrap();
        assert_eq!(hit.severity, Severity::High);
        assert!(hit.title.contains("14 times"));

        // Below the threshold, resets are noise.
        snap.kernel_log = reset_log("3-4", 2);
        assert!(!codes(&analyze(&snap)).contains(&"DEVICE_RESET_STORM"));
    }

    #[test]
    fn reports_a_3a_cable_capping_a_100w_charger() {
        let mut snap = empty_snapshot();
        snap.ports.push(charging_port(
            /* offer_mw */ 100_000,
            /* cable_current_ma */ Some(3000),
            /* contract_v */ 20_000,
            /* contract_i */ 3000,
        ));
        let f = analyze(&snap);
        let hit = f.iter().find(|x| x.code == "CABLE_CURRENT_LIMIT").unwrap();
        assert_eq!(hit.confidence, Confidence::Measured);
        assert!(hit.title.contains("60 W"));
        assert!(matches!(hit.subject, Subject::Cable(_)));
    }

    #[test]
    fn a_5a_cable_on_a_100w_charger_is_clean() {
        let mut snap = empty_snapshot();
        snap.ports
            .push(charging_port(100_000, Some(5000), 20_000, 5000));
        let f = analyze(&snap);
        assert!(!codes(&f).contains(&"CABLE_CURRENT_LIMIT"));
        assert!(!codes(&f).contains(&"PD_CONTRACT_BELOW_OFFER"));
    }

    #[test]
    fn reports_contract_far_below_offer() {
        let mut snap = empty_snapshot();
        // 100 W offered, 15 W taken.
        snap.ports
            .push(charging_port(100_000, Some(3000), 5000, 3000));
        let f = analyze(&snap);
        let hit = f
            .iter()
            .find(|x| x.code == "PD_CONTRACT_BELOW_OFFER")
            .unwrap();
        assert!(hit.title.contains("100 W"));
        assert!(hit.evidence.iter().any(|e| e.contains("cable")));
    }

    #[test]
    fn missing_emarker_is_reported_as_info_only() {
        let mut snap = empty_snapshot();
        snap.ports.push(charging_port(60_000, None, 20_000, 3000));
        let f = analyze(&snap);
        let hit = f.iter().find(|x| x.code == "CABLE_NOT_EMARKED").unwrap();
        assert_eq!(hit.severity, Severity::Info);
        assert_eq!(hit.confidence, Confidence::Heuristic);
    }

    /// Supplying power to a non-PD device (a watch charger) is healthy. It must
    /// explain the 5 V ceiling and must not raise cable or power-gap findings:
    /// nothing on that link can want more than an unmarked cable provides.
    #[test]
    fn sourcing_to_a_non_pd_device_is_explained_not_alarmed() {
        let mut snap = empty_snapshot();
        snap.ports.push(sourcing_port_non_pd());
        let f = analyze(&snap);

        let hit = f.iter().find(|x| x.code == "PARTNER_NO_PD").unwrap();
        assert_eq!(hit.severity, Severity::Info);
        assert_eq!(hit.confidence, Confidence::Measured);
        assert!(hit.title.contains("5 V only"));
        assert!(hit.detail.contains("CC resistor"), "should say why");

        // The cable's rating cannot matter at 5 V for a non-PD device.
        assert!(!codes(&f).contains(&"CABLE_NOT_EMARKED"));
        // Sink-side power comparisons are meaningless while sourcing.
        assert!(!codes(&f).contains(&"PD_CONTRACT_BELOW_OFFER"));
        assert!(!codes(&f).contains(&"PD_SOURCE_BELOW_SINK_CAPABILITY"));
        assert!(!codes(&f).contains(&"PD_NO_CONTRACT"));
        assert!(
            f.iter().all(|x| x.severity <= Severity::Info),
            "a working watch charger must not produce warnings"
        );
    }

    /// A healthy 100 W charger at a full 100 W contract must produce no warning.
    /// Regression for a shipped false positive that reported "negotiated only
    /// 47 W from a supply offering 105 W" and advised buying a cable.
    #[test]
    fn a_fully_negotiated_100w_charger_is_clean() {
        let mut snap = empty_snapshot();
        let port = laptop_charger_port_100w();

        // The power-limited PPS APDO must not inflate the advertised maximum.
        let pd = port.partner.as_ref().unwrap().pd.as_ref().unwrap();
        assert_eq!(
            pd.max_source_power_mw(),
            Some(100_000),
            "power-limited PPS must not count as 105 W"
        );
        // The contract is now x now, not max x max.
        assert_eq!(
            port.power_supply.as_ref().unwrap().contract_power_mw(),
            Some(100_000)
        );

        snap.ports.push(port);
        let f = analyze(&snap);
        assert!(
            !codes(&f).contains(&"PD_CONTRACT_BELOW_OFFER"),
            "a full-power contract must not be reported as short: {:?}",
            codes(&f)
        );
        assert!(!codes(&f).contains(&"PD_NO_CONTRACT"));
        assert!(!codes(&f).contains(&"PD_SOURCE_BELOW_SINK_CAPABILITY"));
        assert!(
            f.iter().all(|x| x.severity <= Severity::Info),
            "healthy charging must not warn: {:?}",
            f.iter().map(|x| (&x.code, x.severity)).collect::<Vec<_>>()
        );
    }

    /// With no cable node but a 5 A contract, the cable is provably e-marked —
    /// PD forbids >3 A otherwise. Saying "unmarked cables are limited to 3 A"
    /// here would contradict the evidence.
    #[test]
    fn a_5a_contract_proves_the_cable_is_emarked() {
        let mut snap = empty_snapshot();
        snap.ports.push(laptop_charger_port_100w());
        let f = analyze(&snap);

        let hit = f
            .iter()
            .find(|x| x.code == "CABLE_EMARKER_NOT_REPORTED")
            .expect("a >3 A contract with no cable node must be explained");
        assert_eq!(hit.confidence, Confidence::Inferred);
        assert_eq!(hit.severity, Severity::Info);
        assert!(hit.title.contains("5 A"));
        // Must not claim the cable is limiting anything.
        assert!(!codes(&f).contains(&"CABLE_NOT_EMARKED"));
        assert!(!codes(&f).contains(&"CABLE_CURRENT_LIMIT"));
    }

    /// The official 65 W charger at a full 65 W contract. Non-power-limited PPS
    /// APDOs must not be excluded (they simply top out below the fixed rail), and
    /// a 3.25 A contract must not be over-claimed as proof of a 5 A e-marker,
    /// because a captive cable explains it just as well.
    #[test]
    fn a_fully_negotiated_65w_charger_is_clean_and_does_not_overclaim() {
        let mut snap = empty_snapshot();
        let port = official_charger_port_65w();

        let pd = port.partner.as_ref().unwrap().pd.as_ref().unwrap();
        assert_eq!(
            pd.max_source_power_mw(),
            Some(65_000),
            "the 63 W PPS must not displace the 65 W fixed rail"
        );
        assert_eq!(
            port.power_supply.as_ref().unwrap().contract_power_mw(),
            Some(65_000),
            "contract is now x now, despite current_max reading 5.72 A"
        );

        snap.ports.push(port);
        let f = analyze(&snap);
        assert!(!codes(&f).contains(&"PD_CONTRACT_BELOW_OFFER"));

        let cable = f
            .iter()
            .find(|x| x.code == "CABLE_EMARKER_NOT_REPORTED")
            .unwrap();
        assert!(
            cable.title.contains("3.25 A") && !cable.title.contains("5 A rated"),
            "must not claim a 5 A e-marker at 3.25 A: {}",
            cable.title
        );
        assert!(
            cable.detail.contains("captive"),
            "must offer the captive-cable explanation"
        );

        // A 65 W supply on a 100 W-capable port is worth noting, at Low.
        let gap = f
            .iter()
            .find(|x| x.code == "PD_SOURCE_BELOW_SINK_CAPABILITY")
            .unwrap();
        assert_eq!(gap.severity, Severity::Low);
        assert!(gap.title.contains("65 W") && gap.title.contains("100 W"));
    }

    /// At 5 A the 5 A conclusion is unambiguous and may be stated outright.
    #[test]
    fn a_5a_contract_is_stated_as_proof() {
        let mut snap = empty_snapshot();
        snap.ports.push(laptop_charger_port_100w());
        let cable = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "CABLE_EMARKER_NOT_REPORTED")
            .unwrap();
        assert!(cable.title.contains("5 A rated"), "{}", cable.title);
        assert!(!cable.detail.contains("captive"));
    }

    /// The cable notice must still appear where the rating genuinely matters.
    #[test]
    fn unmarked_cable_is_reported_when_drawing_power() {
        let mut snap = empty_snapshot();
        snap.ports.push(charging_port(100_000, None, 20_000, 3000));
        assert!(codes(&analyze(&snap)).contains(&"CABLE_NOT_EMARKED"));
    }

    /// Regression for the yes/no parsing bug: a PD-capable partner with no
    /// contract must be caught. Before the fix `supports_pd` was always None on
    /// real hardware and this rule could never fire.
    #[test]
    fn pd_capable_partner_without_a_contract_is_caught() {
        let mut snap = empty_snapshot();
        let mut port = charging_port(100_000, Some(5000), 5000, 500);
        port.power_operation_mode = Some("default".into());
        snap.ports.push(port);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "PD_NO_CONTRACT")
            .expect("PD-capable partner with no contract must be reported");
        assert_eq!(hit.severity, Severity::Medium);
    }

    #[test]
    fn unattached_port_yields_no_port_findings() {
        let mut snap = empty_snapshot();
        snap.ports.push(idle_port());
        let f = analyze(&snap);
        assert!(f.iter().all(|x| !matches!(x.subject, Subject::Port(_))));
        assert!(f.iter().all(|x| !matches!(x.subject, Subject::Cable(_))));
    }

    #[test]
    fn over_current_counter_is_reported_as_measured() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb1", 480.0);
        hub.ports.push(hub_port("usb1-port1", 2));
        snap.buses.push(hub);
        let f = analyze(&snap);
        let hit = f
            .iter()
            .find(|x| x.code == "PORT_OVER_CURRENT_COUNT")
            .unwrap();
        assert_eq!(hit.severity, Severity::High);
        assert_eq!(hit.confidence, Confidence::Measured);
    }

    #[test]
    fn findings_are_sorted_strongest_first() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb2", 10000.0);
        hub.children.push(device("2-1", " 3.20", 480.0, Some("usb2")));
        hub.ports.push(hub_port("usb2-port1", 1));
        snap.buses.push(hub);
        snap.ports.push(charging_port(100_000, None, 5000, 3000));

        let f = analyze(&snap);
        let sev: Vec<Severity> = f.iter().map(|x| x.severity).collect();
        let mut sorted = sev.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(sev, sorted, "findings must be ordered by descending severity");
    }

    /// Every bus-fault event kind the classifier can produce must reach a rule,
    /// otherwise the event is collected and then quietly ignored.
    #[test]
    fn every_bus_fault_event_produces_a_finding() {
        for (kind, expected) in [
            (EventKind::OverCurrent, "BUS_OVER_CURRENT"),
            (EventKind::InsufficientPower, "BUS_POWER_INSUFFICIENT"),
            (EventKind::InsufficientBandwidth, "BUS_BANDWIDTH_INSUFFICIENT"),
        ] {
            let mut snap = empty_snapshot();
            snap.kernel_log.events.push(KernelEvent {
                kind,
                severity: Severity::Medium,
                device: Some("1-1".into()),
                timestamp: None,
                text: "usb 1-1: synthetic fault".into(),
            });
            assert!(
                codes(&analyze(&snap)).contains(&expected),
                "{kind:?} produced no {expected} finding"
            );
        }
    }

    #[test]
    fn missing_kernel_log_is_surfaced_not_hidden() {
        let mut snap = empty_snapshot();
        snap.kernel_log = KernelLog::unavailable("permission denied");
        let f = analyze(&snap);
        assert!(codes(&f).contains(&"KERNEL_LOG_UNAVAILABLE"));
    }

    #[test]
    fn power_formatters_round_sensibly() {
        assert_eq!(watts(100_000), "100 W");
        assert_eq!(watts(65_000), "65 W");
        assert_eq!(watts(7_500), "7.5 W");
        assert_eq!(volts(20_000), "20 V");
        assert_eq!(volts(3_300), "3.3 V");
        assert_eq!(milliamps(3000), "3 A");
        assert_eq!(milliamps(2250), "2.25 A");
    }
}

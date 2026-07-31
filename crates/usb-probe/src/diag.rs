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

use std::collections::{BTreeMap, BTreeSet};

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
    let ss_failed_buses = ss_half_failed_rules(snap, &mut f);
    ss_half_idle_rules(snap, &mut f);
    phantom_device_rules(snap, &mut f);
    billboard_rules(snap, &mut f);
    hub_port_rules(snap, &mut f);

    // One physical fault should read as one finding. Without this, a single
    // loose cable produced four — three of them High, two giving contradictory
    // advice about whether to replace a cable that may be captive.
    if !ss_failed_buses.is_empty() {
        f.retain(|x| {
            if !SUPERSEDED_BY_SS_HALF_FAILED.contains(&x.code.as_str()) {
                return true;
            }
            match &x.subject {
                Subject::Device(d) => {
                    bus_of(d).is_none_or(|b| !ss_failed_buses.contains(&b))
                }
                _ => true,
            }
        });
    }

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

    // --- Sinking far less than this machine can accept, with no PD ---------
    // Checked before PARTNER_NO_PD because the two describe the same electrical
    // state and must never both fire: one calls it routine, the other a problem.
    let ceiling_mw = port.typec_advertised_ceiling_mw();
    let underpowered = port.is_sinking()
        && !port.pd_contract_active()
        // When the partner does claim PD, PD_NO_CONTRACT already covers it.
        && !partner.speaks_pd()
        && matches!((wanted_mw, ceiling_mw), (Some(w), Some(c)) if w > c * 2);

    if underpowered {
        let (want, ceil) = (wanted_mw.unwrap_or(0), ceiling_mw.unwrap_or(0));
        out.push(Finding {
            code: "SINK_UNDERPOWERED_NO_PD".into(),
            severity: Severity::Medium,
            confidence: Confidence::Measured,
            subject: Subject::Port(port.name.clone()),
            title: format!(
                "Drawing at most {} from this port, but this machine can accept {}",
                watts(ceil),
                watts(want)
            ),
            detail: "There is no PD contract, so the link is limited to 5 V at the Type-C \
                     current advertisement. Two causes are indistinguishable from here: the \
                     supply genuinely is not a PD source, or the cable is charge-only or has a \
                     failed CC line — which passes 5 V perfectly while making PD negotiation \
                     impossible, so a good PD charger then reports itself as a non-PD device. \
                     Note that without a contract the supply advertises no capabilities at all, \
                     which is why nothing about its real rating can be read here."
                .into(),
            evidence: vec![
                format!(
                    "power_operation_mode = {} ({} ceiling)",
                    port.power_operation_mode.as_deref().unwrap_or("?"),
                    watts(ceil)
                ),
                "partner supports_usb_power_delivery = no".into(),
                format!("best local sink PDO: {}", best_pdo_desc(port.local_pd.as_ref(), false)),
                "partner advertises no source capabilities".into(),
            ],
            suggestion: Some(
                "Reseat both ends, then try a known-good USB-C cable. If PD comes back, the \
                 cable was the problem."
                    .into(),
            ),
        });
    }

    // --- Attached device does not speak PD at all --------------------------
    if partner.supports_pd == Some(false) && !underpowered {
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
        // Gen 2x2 needs device, host, port and cable to support it together and
        // is genuinely rare, so single-lane at 10 Gbps is the normal case, not a
        // problem. Hubs in particular are almost never dual-lane — reporting
        // every USB 3.1 hub as "single-lane" is pure noise. Restrict to devices
        // where two lanes are plausible and the payoff would be real.
        if dev.usb_version_num.is_some_and(|v| v >= 3.2)
            && dev.tx_lanes == Some(1)
            && dev.speed.as_ref().is_some_and(|s| s.mbps <= 10_000.0)
            && !dev.has_interface_class(CLASS_HUB)
            && dev.has_interface_class(CLASS_MASS_STORAGE)
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

/// USB mass storage class code.
const CLASS_MASS_STORAGE: u8 = 0x08;
/// USB Billboard class code — a device announcing a failed Alternate Mode.
const CLASS_BILLBOARD: u8 = 0x11;
/// USB hub class code.
const CLASS_HUB: u8 = 0x09;

/// A downstream port together with the hub it belongs to.
struct PortSite<'a> {
    port: &'a HubPort,
    hub_name: &'a str,
    hub_speed_mbps: f64,
}

/// One physical socket: a USB 2.0 half and a SuperSpeed half on different buses,
/// sharing an ACPI `_PLD` location token.
struct Receptacle<'a> {
    location: &'a str,
    slow: PortSite<'a>,
    fast: PortSite<'a>,
}

impl<'a> Receptacle<'a> {
    /// Human description of where the socket physically is.
    fn where_is(&self) -> String {
        self.slow
            .port
            .physical_location
            .as_ref()
            .map(|l| l.display())
            .unwrap_or_else(|| format!("location {}", self.location))
    }
}

/// Group downstream ports into physical receptacles.
///
/// Only clean two-port groups count. Firmware emits a catch-all location token
/// shared by many unrelated ports — six of them on the machine this was built
/// against — and pairing those would attach findings to the wrong socket.
fn receptacles(snap: &Snapshot) -> Vec<Receptacle<'_>> {
    let mut groups: BTreeMap<&str, Vec<PortSite>> = BTreeMap::new();
    for dev in snap.devices() {
        let Some(speed) = dev.speed.as_ref().map(|s| s.mbps) else {
            continue;
        };
        for port in &dev.ports {
            if let Some(loc) = port.location.as_deref() {
                groups.entry(loc).or_default().push(PortSite {
                    port,
                    hub_name: &dev.sysfs_name,
                    hub_speed_mbps: speed,
                });
            }
        }
    }

    let mut out = Vec::new();
    for (location, sites) in groups {
        if sites.len() != 2 {
            continue;
        }
        let (mut slow, mut fast) = (None, None);
        for site in sites {
            if site.hub_speed_mbps <= 480.0 {
                slow.get_or_insert(site);
            } else if site.hub_speed_mbps >= 5000.0 {
                fast.get_or_insert(site);
            }
        }
        // A pair of two USB 2.0 ports has no SuperSpeed half to be missing.
        if let (Some(slow), Some(fast)) = (slow, fast) {
            out.push(Receptacle {
                location,
                slow,
                fast,
            });
        }
    }
    out
}

/// A device on the USB 2.0 half of a receptacle whose SuperSpeed half is idle.
///
/// This exists because the obvious check — "device claims USB 3 but linked at
/// 480" — cannot work. A USB 3 device carries separate descriptor sets for
/// SuperSpeed and High-Speed operation, so when it falls back it reports
/// `bcdUSB 2.10` and stops claiming USB 3 at exactly the moment we need it to.
/// Verified on one drive, same cable, two sockets: `version 3.00 / speed 5000`
/// became `version 2.10 / speed 480`.
///
/// The port topology has no such blind spot. One physical receptacle appears as
/// two logical ports sharing an ACPI `_PLD` location token — a USB 2.0 half and
/// a SuperSpeed half on different buses. A device on the slow half while the
/// fast half sits empty means SuperSpeed training never happened, whatever the
/// device claims about itself.
///
/// Restricted to mass storage on purpose. A USB 2.0 keyboard on a SuperSpeed
/// receptacle produces exactly the same topology and is entirely normal, so
/// firing for every device class would bury the signal. Storage is the class
/// where SuperSpeed is both expected and worth having.
fn ss_half_idle_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    for rec in receptacles(snap) {
        // The slow half is occupied and the fast half is empty.
        let (Some(child_name), None) = (rec.slow.port.child.as_deref(), rec.fast.port.child.as_deref())
        else {
            continue;
        };
        let Some(dev) = snap.device(child_name) else {
            continue;
        };

        // When the SuperSpeed half is actively erroring, SS_HALF_FAILED says
        // something stronger and for every device class. Don't double-report.
        if !ss_errors_on(snap, rec.fast.hub_name).is_empty() {
            continue;
        }
        if !dev.has_interface_class(CLASS_MASS_STORAGE) {
            continue;
        }

        let mut evidence = vec![
            format!(
                "{} (hub at {}) has {}",
                rec.slow.port.name,
                LinkSpeed::from_mbps(rec.slow.hub_speed_mbps).short(),
                child_name
            ),
            format!(
                "{} (hub at {}) is idle — same receptacle, location {}",
                rec.fast.port.name,
                LinkSpeed::from_mbps(rec.fast.hub_speed_mbps).short(),
                rec.location
            ),
            format!(
                "device reports USB {} — which a USB 3 device in fallback also does",
                dev.usb_version.as_deref().unwrap_or("?")
            ),
        ];
        if let Some(loc) = rec.slow.port.physical_location.as_ref() {
            evidence.push(format!("physical location: {}", loc.display()));
        }

        out.push(Finding {
            code: "SS_HALF_IDLE".into(),
            severity: Severity::Medium,
            confidence: Confidence::Heuristic,
            subject: Subject::Device(dev.sysfs_name.clone()),
            title: format!(
                "{} is on the USB 2.0 half of a SuperSpeed-capable socket",
                dev.label()
            ),
            detail: "This receptacle has a SuperSpeed half, and it is empty while the device sits \
                     on the USB 2.0 half — so the SuperSpeed pairs never trained. For storage that \
                     usually means the cable or adapter carries no SuperSpeed wiring at all, which \
                     is normal for charge-only cables and for USB 2.0-only adapters. The device's \
                     own descriptors cannot confirm this either way: a USB 3 device operating at \
                     High Speed presents its USB 2.0 descriptor set and stops advertising USB 3."
                .into(),
            evidence,
            suggestion: Some(
                "If this is a USB 3 device, replace the cable or adapter with one rated for USB 3 \
                 data. If it is genuinely a USB 2.0 device, nothing is wrong."
                    .into(),
            ),
        });
    }
}

/// Kernel events on a bus that indicate a SuperSpeed link failing to come up.
fn ss_errors_on<'a>(snap: &'a Snapshot, hub_name: &str) -> Vec<&'a KernelEvent> {
    snap.kernel_log
        .events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::LinkTrainingFailure
                    | EventKind::CableSuspect
                    | EventKind::EnumerationFailure
            ) && e.bus().as_deref() == Some(hub_name)
        })
        .collect()
}

/// A device on the USB 2.0 half of a receptacle whose SuperSpeed half is
/// *actively failing* — the strongest form of the fallback signal.
///
/// [`ss_half_idle_rules`] is restricted to mass storage because a quiet, idle
/// SuperSpeed half is perfectly normal beside a USB 2.0 keyboard. That
/// restriction is right there and wrong here: when the SuperSpeed half is
/// throwing errors, something *tried* to train at this socket and failed, so
/// the device on the slow half is a USB 3 device running degraded whatever its
/// class. The error events supply the evidence the class filter stood in for.
///
/// Distinguishes two causes that call for opposite actions:
///
/// * trained at least once, then failed — the SuperSpeed pairs are physically
///   present but the connection is intermittent, so the cable or connector is
///   likely **defective**
/// * never trained at all — the path has no SuperSpeed wiring, so it is simply
///   the **wrong cable** and nothing is broken
fn ss_half_failed_rules(snap: &Snapshot, out: &mut Vec<Finding>) -> BTreeSet<String> {
    let mut covered = BTreeSet::new();

    for rec in receptacles(snap) {
        let errors = ss_errors_on(snap, rec.fast.hub_name);
        if errors.is_empty() {
            continue;
        }
        // Only meaningful when the companion half actually carries something —
        // otherwise the socket is simply empty and errors are stale.
        let Some(child_name) = rec.slow.port.child.as_deref() else {
            continue;
        };
        let Some(dev) = snap.device(child_name) else {
            continue;
        };

        let trained = snap
            .kernel_log
            .events
            .iter()
            .any(|e| e.is_superspeed_train() && e.bus().as_deref() == Some(rec.fast.hub_name));
        let gave_up = rec.fast.port.state.as_deref() == Some("not attached");
        // Did the SuperSpeed half come back? An intermittent fault is most
        // dangerous exactly when it currently looks fine, so this is reported —
        // but the tense has to be honest about it.
        let up_now = rec
            .fast
            .port
            .child
            .as_deref()
            .and_then(|c| snap.device(c))
            .map(|d| (d.sysfs_name.clone(), d.label(), d.speed.as_ref().map(|s| s.short())));

        let (verdict, detail, suggestion) = if trained {
            (
                "the cable or connector is likely defective",
                "The SuperSpeed link trained at least once and then failed, so the SuperSpeed \
                 pairs are physically present and wired — they just will not hold a connection. \
                 That points at an intermittent contact: a loose or damaged connector, a worn \
                 plug, or a cable failing internally. A cable that merely lacked SuperSpeed \
                 wiring could never have trained at all. Note that USB 2.0 keeps working through \
                 this: it uses one differential pair with generous margins, while SuperSpeed adds \
                 two pairs at multi-gigabit rates where signal integrity is unforgiving — so the \
                 SuperSpeed half fails long before anything else shows symptoms.",
                "Reseat both ends and inspect the connectors. If the device is a hub or dock with \
                 a built-in cable, the uplink cannot be replaced and the unit needs repair or \
                 replacement.",
            )
        } else {
            (
                "the path has no SuperSpeed wiring",
                "The SuperSpeed link never trained even once, which means the SuperSpeed pairs \
                 are not reaching this socket at all. A USB 2.0-only cable or adapter does \
                 exactly this — nothing is damaged, the wiring is simply absent.",
                "Replace the cable or adapter with one rated for USB 3 data. Charge-only cables \
                 and USB 2.0 adapters cannot carry SuperSpeed however good they are.",
            )
        };

        let mut evidence = vec![
            format!(
                "{} on the SuperSpeed half: {} error event(s) this boot",
                rec.fast.port.name,
                errors.len()
            ),
            format!(
                "{} on the USB 2.0 half of the same socket is running {} ({})",
                rec.slow.port.name,
                dev.label(),
                dev.speed
                    .as_ref()
                    .map(|s| s.short())
                    .unwrap_or_else(|| "?".into())
            ),
            format!(
                "SuperSpeed {} this boot",
                if trained {
                    "trained at least once, then failed"
                } else {
                    "never trained"
                }
            ),
        ];
        if gave_up {
            evidence.push(format!(
                "{} has since given up (state: not attached)",
                rec.fast.port.name
            ));
        }
        // The errno is the actual diagnosis, so surface it in plain language.
        for e in errors.iter().rev().take(3) {
            match e.errno.and_then(errno_meaning) {
                Some(m) => evidence.push(format!("{}  [{}]", e.text, m)),
                None => evidence.push(e.text.clone()),
            }
        }

        // Tense matters: claiming a link is down while it is carrying traffic
        // would discredit everything else the tool says.
        let (title, mut detail) = match &up_now {
            Some((name, label, speed)) => {
                evidence.push(format!(
                    "SuperSpeed is up right now: {name} is {label} at {}",
                    speed.clone().unwrap_or_else(|| "?".into())
                ));
                (
                    format!(
                        "SuperSpeed at the {} socket is up now, but failed {} times earlier this \
                         boot — intermittent",
                        rec.where_is(),
                        errors.len()
                    ),
                    format!(
                        "The link is working at this moment, which is what makes this worth \
                         reporting: an intermittent fault is most misleading exactly when it looks \
                         fine. {detail}"
                    ),
                )
            }
            None => (
                format!(
                    "SuperSpeed failed at the {} socket — {}",
                    rec.where_is(),
                    verdict
                ),
                detail.to_string(),
            ),
        };

        // Absorb the narrower findings for this bus so one fault reads as one
        // finding rather than three with conflicting advice.
        covered.insert(rec.fast.hub_name.to_string());
        detail.push_str(
            "\n\nThis supersedes the individual kernel-log findings for this bus, which describe \
             the same fault one message at a time.",
        );

        out.push(Finding {
            code: "SS_HALF_FAILED".into(),
            severity: Severity::High,
            confidence: Confidence::Inferred,
            subject: Subject::Cable(rec.where_is()),
            title,
            detail,
            evidence,
            suggestion: Some(suggestion.into()),
        });
    }

    covered
}

/// Findings that [`ss_half_failed_rules`] supersedes: each describes one message
/// from the same underlying fault, and two of them give advice that contradicts
/// the combined diagnosis.
const SUPERSEDED_BY_SS_HALF_FAILED: [&str; 4] = [
    "KERNEL_BLAMED_CABLE",
    "ENUMERATION_FAILURE",
    "DEVICE_FAILED_TO_ENUMERATE",
    "LINK_TRAINING_FAILURE",
];

/// Findings for devices that appear only in the kernel log, never in sysfs.
///
/// The per-device rules iterate `snap.devices()`, so a device that failed to
/// enumerate is skipped entirely — meaning `ENUMERATION_FAILURE` could never
/// fire for a device that failed to enumerate, the only case it applies to.
/// This sweeps up what that pass leaves behind.
///
/// A device absent from sysfs is arguably worse than a degraded one: it is not
/// slow, it is unusable.
fn phantom_device_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    let mut by_device: BTreeMap<&str, Vec<&KernelEvent>> = BTreeMap::new();
    for e in &snap.kernel_log.events {
        let Some(name) = e.device.as_deref() else {
            continue;
        };
        // Present in sysfs, so the per-device pass already handled it.
        if snap.device(name).is_some() {
            continue;
        }
        if matches!(
            e.kind,
            EventKind::EnumerationFailure | EventKind::LinkTrainingFailure | EventKind::CableSuspect
        ) {
            by_device.entry(name).or_default().push(e);
        }
    }

    let sockets = receptacles(snap);
    for (name, events) in by_device {
        // Where is this bus physically, and what else is on that socket?
        let bus = events.first().and_then(|e| e.bus());
        let context = bus.as_deref().and_then(|b| {
            sockets.iter().find_map(|r| {
                if r.fast.hub_name != b {
                    return None;
                }
                let sibling = r.slow.port.child.as_deref().and_then(|c| snap.device(c));
                Some(match sibling {
                    Some(d) => format!(
                        "the SuperSpeed half of the {} socket, whose USB 2.0 half is running {}",
                        r.where_is(),
                        d.label()
                    ),
                    None => format!("the SuperSpeed half of the {} socket", r.where_is()),
                })
            })
        });

        let worst = events
            .iter()
            .map(|e| e.kind)
            .max_by_key(|k| match k {
                EventKind::EnumerationFailure => 2,
                EventKind::CableSuspect => 1,
                _ => 0,
            })
            .unwrap_or(EventKind::EnumerationFailure);

        let mut evidence = vec![format!(
            "{name} appears in the kernel log but not in sysfs — it never finished enumerating"
        )];
        if let Some(c) = &context {
            evidence.push(format!("this is {c}"));
        }
        evidence.push(format!("{} relevant event(s) this boot", events.len()));
        for e in events.iter().rev().take(3) {
            match e.errno.and_then(errno_meaning) {
                Some(m) => evidence.push(format!("{}  [{}]", e.text, m)),
                None => evidence.push(e.text.clone()),
            }
        }

        let where_ = context
            .as_deref()
            .map(|c| format!(" at {c}"))
            .unwrap_or_default();

        out.push(Finding {
            code: "DEVICE_FAILED_TO_ENUMERATE".into(),
            severity: Severity::High,
            confidence: Confidence::Measured,
            subject: Subject::Device(name.to_string()),
            title: format!("{name} tried to attach{where_} and never enumerated"),
            detail: match worst {
                EventKind::CableSuspect =>
                    "The hub driver could not enable the port and logged its bad-cable warning. \
                     The device is not merely degraded — it is absent, so nothing behind it is \
                     usable."
                        .to_string(),
                _ => "Descriptor reads failed or the device never accepted an address, so it does \
                      not exist as far as the rest of the system is concerned. Signal integrity, \
                      insufficient power, and failing device hardware all produce this."
                    .to_string(),
            },
            evidence,
            suggestion: Some(
                "Reseat the connection and try a different cable and port. If the device is \
                 behind a hub, test it directly on the machine to tell the two apart."
                    .into(),
            ),
        });
    }
}

/// USB Billboard: a device's own declaration that an Alternate Mode failed.
///
/// The class exists for exactly one purpose. A USB-C device that asked for an
/// Alternate Mode and could not enter it presents a Billboard interface to tell
/// the host so. Its presence is therefore a machine-readable failure report,
/// not an ordinary device.
fn billboard_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    for dev in snap.devices() {
        if !dev.has_interface_class(CLASS_BILLBOARD) {
            continue;
        }
        out.push(Finding {
            code: "BILLBOARD_ALT_MODE_FAILED".into(),
            severity: Severity::Medium,
            confidence: Confidence::Measured,
            subject: Subject::Device(dev.sysfs_name.clone()),
            title: format!(
                "{} reports an Alternate Mode it could not enter",
                dev.label()
            ),
            detail: "A USB Billboard device exists only to announce a failure: the attached \
                     USB-C device requested an Alternate Mode — DisplayPort, Thunderbolt, or a \
                     vendor mode — and the negotiation did not succeed, so it fell back to \
                     presenting this instead. Common causes are a cable without the required \
                     wiring, a port that does not support the mode, or a link that could not \
                     train. The specific modes it wanted live in a Billboard capability \
                     descriptor, which sysfs does not expose."
                .into(),
            evidence: vec![
                format!("{} exposes interface class 0x11 (billboard)", dev.sysfs_name),
                format!(
                    "{}:{} at {}",
                    dev.vid_pid().unwrap_or_default(),
                    dev.usb_version.as_deref().unwrap_or("?"),
                    dev.speed.as_ref().map(|s| s.short()).unwrap_or_default()
                ),
            ],
            suggestion: Some(
                "If you expected video or a docking mode from this device, the cable is the \
                 usual cause — it must carry the SuperSpeed pairs, not just power and USB 2.0."
                    .into(),
            ),
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

    // --- SS_HALF_IDLE -------------------------------------------------------

    /// The case LINK_BELOW_DEVICE_CAPABILITY is blind to: a USB 3 drive behind a
    /// USB 2.0-only adapter. It reports `version 2.10` because a USB 3 device at
    /// High Speed presents its USB 2.0 descriptor set, so only the port topology
    /// reveals the problem.
    #[test]
    fn detects_a_storage_device_stranded_on_the_usb2_half() {
        let mut snap = empty_snapshot();
        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x08));
        snap.buses.push(slow);
        snap.buses.push(fast);

        let f = analyze(&snap);
        let hit = f.iter().find(|x| x.code == "SS_HALF_IDLE").unwrap();
        assert_eq!(hit.severity, Severity::Medium);
        assert_eq!(hit.confidence, Confidence::Heuristic);
        assert!(hit.evidence.iter().any(|e| e.contains("0x80000001")));
        assert!(
            hit.evidence.iter().any(|e| e.contains("in fallback also does")),
            "must record that the descriptors cannot settle it"
        );

        // The descriptor-based rule stays silent, which is the whole point.
        assert!(!codes(&f).contains(&"LINK_BELOW_DEVICE_CAPABILITY"));
    }

    /// A USB 2.0 keyboard on a SuperSpeed socket produces identical topology and
    /// is completely normal. Firing here would bury the signal in noise.
    #[test]
    fn does_not_fire_for_a_non_storage_device_on_the_usb2_half() {
        let mut snap = empty_snapshot();
        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        // 0x03 = HID.
        slow.children
            .push(device_with_class("5-1", " 2.00", 12.0, Some("usb5"), 0x03));
        snap.buses.push(slow);
        snap.buses.push(fast);
        assert!(!codes(&analyze(&snap)).contains(&"SS_HALF_IDLE"));
    }

    #[test]
    fn does_not_fire_when_storage_is_on_the_superspeed_half() {
        let mut snap = empty_snapshot();
        let (slow, mut fast) = receptacle("0x80000001", None, Some("6-1"));
        fast.children
            .push(device_with_class("6-1", " 3.00", 5000.0, Some("usb6"), 0x08));
        snap.buses.push(slow);
        snap.buses.push(fast);
        assert!(!codes(&analyze(&snap)).contains(&"SS_HALF_IDLE"));
    }

    /// Firmware emits a catch-all location token shared by many unrelated ports —
    /// six of them on the development machine. Grouping those would pair ports
    /// with no physical relationship, so only clean two-port groups count.
    #[test]
    fn rejects_the_firmware_catch_all_location_group() {
        let mut snap = empty_snapshot();
        let (mut slow, fast) = receptacle("0x80000008", Some("5-1"), None);
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x08));
        // A third port sharing the token makes the group ambiguous.
        let mut extra = root_hub("usb7", 480.0);
        extra.ports.push(hub_port("usb7-port1", 0));
        extra.ports[0].location = Some("0x80000008".into());
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.buses.push(extra);
        assert!(!codes(&analyze(&snap)).contains(&"SS_HALF_IDLE"));
    }

    #[test]
    fn does_not_fire_on_a_receptacle_with_no_superspeed_half() {
        let mut snap = empty_snapshot();
        // Two USB 2.0 ports sharing a token: no SuperSpeed to be missing.
        let mut a = root_hub("usb5", 480.0);
        let mut b = root_hub("usb7", 480.0);
        a.ports.push(hub_port("usb5-port1", 0));
        a.ports[0].location = Some("0x80000000".into());
        a.ports[0].child = Some("5-1".into());
        b.ports.push(hub_port("usb7-port1", 0));
        b.ports[0].location = Some("0x80000000".into());
        b.ports[0].child = None;
        a.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x08));
        snap.buses.push(a);
        snap.buses.push(b);
        assert!(!codes(&analyze(&snap)).contains(&"SS_HALF_IDLE"));
    }

    // --- SS_HALF_FAILED -----------------------------------------------------

    /// The confirmed-defective case: a hub whose built-in cable had a loose
    /// connection. SuperSpeed trained once then timed out, while USB 2.0 ran
    /// perfectly throughout.
    #[test]
    fn a_superspeed_half_that_trained_then_failed_reads_as_defective() {
        let mut snap = empty_snapshot();
        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        // A hub, not storage — SS_HALF_IDLE would ignore this entirely.
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x09));
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.kernel_log.events = ss_uplink_failure_events(6, 85, true);

        let f = analyze(&snap);
        let hit = f.iter().find(|x| x.code == "SS_HALF_FAILED").unwrap();
        assert_eq!(hit.severity, Severity::High);
        assert_eq!(hit.confidence, Confidence::Inferred);
        assert!(
            hit.title.contains("likely defective"),
            "trained-then-failed means the wiring exists: {}",
            hit.title
        );
        assert!(hit.detail.contains("intermittent"));
        // Advice must cover a captive uplink, which cannot be swapped.
        assert!(hit.suggestion.as_deref().unwrap().contains("built-in cable"));
        // The errno is the diagnosis, so it must be spelled out.
        assert!(hit.evidence.iter().any(|e| e.contains("ETIMEDOUT")));
        // 85 "Cannot enable" retries plus the -110 descriptor timeout, all of
        // which are errors on that bus.
        assert!(hit.evidence.iter().any(|e| e.contains("86 error event")));
    }

    /// The other cause, needing opposite advice: a USB 2.0-only adapter. It can
    /// never train, so "defective" would be wrong and expensive.
    #[test]
    fn a_superspeed_half_that_never_trained_reads_as_wrong_cable() {
        let mut snap = empty_snapshot();
        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x08));
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.kernel_log.events = ss_uplink_failure_events(6, 12, false);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "SS_HALF_FAILED")
            .unwrap();
        assert!(hit.title.contains("no SuperSpeed wiring"), "{}", hit.title);
        assert!(!hit.detail.contains("intermittent"));
        assert!(hit.suggestion.as_deref().unwrap().contains("Replace the cable"));
    }

    /// SS_HALF_FAILED says everything SS_HALF_IDLE would, with evidence. Firing
    /// both would be two alarms for one event.
    #[test]
    fn ss_half_failed_supersedes_ss_half_idle() {
        let mut snap = empty_snapshot();
        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x08));
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.kernel_log.events = ss_uplink_failure_events(6, 85, true);

        let f = analyze(&snap);
        assert!(codes(&f).contains(&"SS_HALF_FAILED"));
        assert!(!codes(&f).contains(&"SS_HALF_IDLE"));
    }

    /// Errors with nothing plugged into the socket are stale history, not a
    /// live problem. Reporting them every boot would be noise.
    #[test]
    fn stale_errors_on_an_empty_socket_do_not_fire() {
        let mut snap = empty_snapshot();
        let (slow, fast) = receptacle("0x80000001", None, None);
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.kernel_log.events = ss_uplink_failure_events(6, 85, true);
        assert!(!codes(&analyze(&snap)).contains(&"SS_HALF_FAILED"));
    }

    // --- DEVICE_FAILED_TO_ENUMERATE ----------------------------------------

    /// The structural bug: rules iterate devices in the tree, so a device that
    /// failed to enumerate was skipped — the only case the rule applies to.
    /// Nothing on the USB 2.0 half, so SS_HALF_FAILED stays out of the way and
    /// the phantom rule is exercised alone.
    #[test]
    fn reports_a_device_that_never_reached_sysfs() {
        let mut snap = empty_snapshot();
        let (slow, fast) = receptacle("0x80000001", None, None);
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.kernel_log.events = ss_uplink_failure_events(6, 3, true);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "DEVICE_FAILED_TO_ENUMERATE")
            .expect("6-1 is in the log but not the tree");
        assert_eq!(hit.severity, Severity::High);
        assert_eq!(hit.subject, Subject::Device("6-1".into()));
        // Must locate it physically rather than print a bare device path.
        assert!(
            hit.title.contains("SuperSpeed half"),
            "bare device paths are not actionable: {}",
            hit.title
        );
        assert!(hit.evidence.iter().any(|e| e.contains("ETIMEDOUT")));
    }

    /// One physical fault must read as one finding. On real hardware a single
    /// loose hub cable produced four — three High — with two of them giving
    /// advice that contradicted the combined diagnosis.
    #[test]
    fn ss_half_failed_absorbs_the_narrower_findings_for_its_bus() {
        let mut snap = empty_snapshot();
        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x09));
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.kernel_log.events = ss_uplink_failure_events(6, 85, true);

        let f = analyze(&snap);
        assert!(codes(&f).contains(&"SS_HALF_FAILED"));
        for absorbed in [
            "KERNEL_BLAMED_CABLE",
            "ENUMERATION_FAILURE",
            "DEVICE_FAILED_TO_ENUMERATE",
        ] {
            assert!(
                !codes(&f).contains(&absorbed),
                "{absorbed} duplicates SS_HALF_FAILED for the same bus"
            );
        }
        assert_eq!(
            f.iter().filter(|x| x.severity >= Severity::High).count(),
            1,
            "one fault, one High finding: {:?}",
            codes(&f)
        );
    }

    /// Absorption is scoped to the affected bus — an unrelated fault elsewhere
    /// must still be reported.
    #[test]
    fn absorption_does_not_swallow_other_buses() {
        let mut snap = empty_snapshot();
        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x09));
        snap.buses.push(slow);
        snap.buses.push(fast);

        let mut other = root_hub("usb3", 480.0);
        other
            .children
            .push(device("3-4", " 2.00", 12.0, Some("usb3")));
        snap.buses.push(other);

        snap.kernel_log.events = ss_uplink_failure_events(6, 85, true);
        snap.kernel_log.events.push(KernelEvent {
            kind: EventKind::EnumerationFailure,
            severity: Severity::High,
            device: Some("3-4".into()),
            errno: Some(-71),
            timestamp: None,
            text: "usb 3-4: device descriptor read/64, error -71".into(),
        });

        let f = analyze(&snap);
        assert!(codes(&f).contains(&"SS_HALF_FAILED"));
        assert!(
            codes(&f).contains(&"ENUMERATION_FAILURE"),
            "bus 3's fault is unrelated and must survive"
        );
    }

    /// An intermittent link that is currently up is the most valuable thing the
    /// tool can report — but it must not claim the link is down.
    #[test]
    fn an_intermittent_link_that_recovered_is_described_in_the_right_tense() {
        let mut snap = empty_snapshot();
        let (mut slow, mut fast) = receptacle("0x80000001", Some("5-1"), Some("6-1"));
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x09));
        // The SuperSpeed half came back up, as the real hub did on reconnection.
        fast.children
            .push(device_with_class("6-1", " 3.20", 10_000.0, Some("usb6"), 0x09));
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.kernel_log.events = ss_uplink_failure_events(6, 85, true);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "SS_HALF_FAILED")
            .unwrap();
        assert!(
            hit.title.contains("up now") && hit.title.contains("intermittent"),
            "must not claim a working link is down: {}",
            hit.title
        );
        assert!(hit.title.contains("86 times"));
        assert!(hit.evidence.iter().any(|e| e.contains("up right now")));
    }

    /// A single-lane USB 3.1 hub at 10 Gbps is entirely normal; Gen 2x2 needs
    /// device, host, port and cable together and is rare.
    #[test]
    fn single_lane_hubs_are_not_reported() {
        let mut snap = empty_snapshot();
        let mut bus = root_hub("usb6", 10_000.0);
        bus.children
            .push(device_with_class("6-1", " 3.20", 10_000.0, Some("usb6"), 0x09));
        snap.buses.push(bus);
        assert!(!codes(&analyze(&snap)).contains(&"LINK_SINGLE_LANE"));
    }

    /// Devices present in sysfs are already covered by the per-device pass.
    #[test]
    fn present_devices_are_not_reported_as_phantoms() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb3", 480.0);
        let mut d = device("3-4", " 2.00", 12.0, Some("usb3"));
        d.removable = Some("removable".into());
        hub.children.push(d);
        snap.buses.push(hub);
        snap.kernel_log.events.push(KernelEvent {
            kind: EventKind::EnumerationFailure,
            severity: Severity::High,
            device: Some("3-4".into()),
            errno: Some(-71),
            timestamp: None,
            text: "usb 3-4: device descriptor read/64, error -71".into(),
        });

        let f = analyze(&snap);
        assert!(!codes(&f).contains(&"DEVICE_FAILED_TO_ENUMERATE"));
        assert!(codes(&f).contains(&"ENUMERATION_FAILURE"));
    }

    // --- BILLBOARD_ALT_MODE_FAILED -----------------------------------------

    #[test]
    fn reports_a_billboard_device_as_a_failed_alt_mode() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb5", 480.0);
        hub.children.push(billboard_device("5-1.5", Some("usb5")));
        snap.buses.push(hub);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "BILLBOARD_ALT_MODE_FAILED")
            .unwrap();
        assert_eq!(hit.severity, Severity::Medium);
        assert_eq!(hit.confidence, Confidence::Measured);
        assert!(hit.detail.contains("Alternate Mode"));
        assert!(hit.evidence.iter().any(|e| e.contains("0x11")));
    }

    #[test]
    fn ordinary_devices_are_not_billboards() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb5", 480.0);
        hub.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x08));
        snap.buses.push(hub);
        assert!(!codes(&analyze(&snap)).contains(&"BILLBOARD_ALT_MODE_FAILED"));
    }

    // --- SINK_UNDERPOWERED_NO_PD -------------------------------------------

    /// A 100 W-capable laptop reduced to a 15 W Type-C advertisement. Reported at
    /// Medium, not Info — and PARTNER_NO_PD must stand down so the two do not
    /// contradict each other.
    #[test]
    fn reports_an_underpowered_sink_and_suppresses_the_info_variant() {
        let mut snap = empty_snapshot();
        snap.ports.push(sinking_port_no_pd("3.0A"));
        let f = analyze(&snap);

        let hit = f
            .iter()
            .find(|x| x.code == "SINK_UNDERPOWERED_NO_PD")
            .unwrap();
        assert_eq!(hit.severity, Severity::Medium);
        assert_eq!(hit.confidence, Confidence::Measured);
        assert!(hit.title.contains("15 W") && hit.title.contains("100 W"));
        assert!(
            hit.detail.contains("CC line"),
            "must name the failed-cable cause, which looks identical from sysfs"
        );
        assert!(
            !codes(&f).contains(&"PARTNER_NO_PD"),
            "the Info variant must not also fire"
        );
    }

    #[test]
    fn underpowered_sink_scales_with_the_advertised_ceiling() {
        for (mode, watts) in [("default", "4.5 W"), ("1.5A", "7.5 W"), ("3.0A", "15 W")] {
            let mut snap = empty_snapshot();
            snap.ports.push(sinking_port_no_pd(mode));
            let hit = analyze(&snap)
                .into_iter()
                .find(|x| x.code == "SINK_UNDERPOWERED_NO_PD")
                .unwrap_or_else(|| panic!("no finding for mode {mode}"));
            assert!(hit.title.contains(watts), "mode {mode}: {}", hit.title);
        }
    }

    /// Sourcing to an accessory is not an underpowered sink. The watch-charger
    /// case must keep its Info wording.
    #[test]
    fn sourcing_is_never_an_underpowered_sink() {
        let mut snap = empty_snapshot();
        snap.ports.push(sourcing_port_non_pd());
        let f = analyze(&snap);
        assert!(!codes(&f).contains(&"SINK_UNDERPOWERED_NO_PD"));
        assert!(codes(&f).contains(&"PARTNER_NO_PD"));
    }

    /// When the partner does claim PD, PD_NO_CONTRACT owns the diagnosis.
    #[test]
    fn a_pd_capable_partner_is_left_to_pd_no_contract() {
        let mut snap = empty_snapshot();
        let mut port = sinking_port_no_pd("3.0A");
        port.partner.as_mut().unwrap().supports_pd = Some(true);
        snap.ports.push(port);
        let f = analyze(&snap);
        assert!(codes(&f).contains(&"PD_NO_CONTRACT"));
        assert!(!codes(&f).contains(&"SINK_UNDERPOWERED_NO_PD"));
    }

    /// A working PD contract is not underpowered, whatever the wattage.
    #[test]
    fn a_negotiated_contract_is_never_underpowered() {
        let mut snap = empty_snapshot();
        snap.ports.push(official_charger_port_65w());
        assert!(!codes(&analyze(&snap)).contains(&"SINK_UNDERPOWERED_NO_PD"));
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
                errno: None,
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

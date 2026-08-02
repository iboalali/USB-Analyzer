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

use crate::kind::{DeviceKind, Kind, KindSource};
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
    thunderbolt_rules(snap, &mut f);
    battery_rules(snap, &mut f);
    display_rules(snap, &mut f);
    urb_error_rules(snap, &mut f);
    throughput_rules(snap, &mut f);
    scsi_error_rules(snap, &mut f);
    reenumeration_rules(snap, &mut f);
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

    // Measured traffic strengthens the findings it speaks to, once the set is
    // final — so nothing later can drop a finding that has been annotated.
    corroborate_with_urb_errors(snap, &mut f);

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
    let exonerations = crate::verdict::exonerate(&snap, &findings);
    let verdicts = crate::verdict::verdicts(&snap, &findings, &exonerations);
    Report {
        snapshot: snap,
        findings,
        exonerations,
        verdicts,
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
            // Stop hedging when the battery proves the gap matters.
            let draining = snap
                .batteries
                .iter()
                .any(|b| b.not_keeping_up(snap.mains_online.unwrap_or(false)));
            out.push(Finding {
                code: "PD_SOURCE_BELOW_SINK_CAPABILITY".into(),
                severity: if draining { Severity::Medium } else { Severity::Low },
                confidence: Confidence::Measured,
                subject: Subject::Port(port.name.clone()),
                title: format!(
                    "Charger offers {} but this machine can accept {}",
                    watts(offered),
                    watts(wanted)
                ),
                detail: if draining {
                    "The supply is smaller than this port can accept, and the battery is \
                     measurably failing to gain as a result — so this is not merely a slower \
                     charge, the machine is running down while plugged in."
                        .to_string()
                } else {
                    "Not a fault — the supply is simply smaller than the port's maximum. Expect \
                     slower charging, and possible battery drain under heavy load."
                        .to_string()
                },
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
            // `asserted`, not `kind`: this creates a finding, so it may only
            // rest on what the device said about itself. See `kind`'s docs.
            && dev.kind().grounds() == Some(DeviceKind::Storage)
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
    // Only events since this device attached: a path like `5-1` is a socket
    // address, not a device identity, so the previous occupant's history must
    // not be charged to the current one.
    let (events, excluded_stale) = snap.events_since_attach(dev);
    let _ = excluded_stale;
    if events.is_empty() {
        return;
    }

    let resets = events
        .iter()
        .filter(|e| e.kind == EventKind::DeviceReset)
        .count();
    if resets >= RESET_WARN {
        // Runtime power management explains most reset storms outright, and the
        // accounting proves it rather than assuming it. A device suspended
        // ~all of its connected life logs a reset on every wake; that is the
        // designed behaviour, not a fault.
        let (severity, confidence, detail, suggestion) = if dev.autosuspend_churn() {
            let pct = dev.suspend_ratio().unwrap_or(0.0) * 100.0;
            (
                Severity::Low,
                Confidence::Measured,
                format!(
                    "Runtime power management accounts for this. The device has runtime PM \
                     enabled and has spent {pct:.1}% of its connected life suspended, with a \
                     {} ms autosuspend delay — so it is woken and re-suspended constantly, and \
                     each wake logs a reset. This is the designed behaviour, not a bad connection.",
                    dev.autosuspend_delay_ms.unwrap_or(0)
                ),
                Some(
                    "Nothing to fix unless the device actually misbehaves. If it does, raise its \
                     autosuspend delay or set power/control to 'on' — the cable is not involved."
                        .to_string(),
                ),
            )
        } else if dev.is_internal() {
            (
                Severity::Low,
                Confidence::Heuristic,
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
                Confidence::Heuristic,
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
            confidence,
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
                .chain(dev.suspend_ratio().map(|r| {
                    format!(
                        "runtime PM: control={}, suspended {:.1}% of {} s connected, {} urbs",
                        dev.power_control.as_deref().unwrap_or("?"),
                        r * 100.0,
                        dev.connected_duration_ms.unwrap_or(0) / 1000,
                        dev.urbnum.unwrap_or(0)
                    )
                }))
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

/// USB Billboard class code — a device announcing a failed Alternate Mode.
///
/// The last raw class code left in the rule engine, and it stays raw on
/// purpose: Billboard is a symptom, not an identity. Everything that asks what
/// a device *is* goes through [`crate::kind`].
const CLASS_BILLBOARD: u8 = 0x11;

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
        if dev.kind().grounds() != Some(DeviceKind::Storage) {
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
        let all_errors = ss_errors_on(snap, rec.fast.hub_name);
        if all_errors.is_empty() {
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
        // A socket outlives its occupants. Errors from whatever was plugged in
        // twenty minutes ago say nothing about what is plugged in now, and
        // blaming the current device's cable for them is simply wrong.
        let (errors, excluded) = snap.filter_since_attach(all_errors, dev);
        if errors.is_empty() {
            continue;
        }

        let attached_at = snap.uptime_s.and_then(|u| dev.attached_at_s(u));
        let trained = snap.kernel_log.events.iter().any(|e| {
            e.is_superspeed_train()
                && e.bus().as_deref() == Some(rec.fast.hub_name)
                && match (attached_at, e.monotonic_s) {
                    (Some(a), Some(t)) => t >= a,
                    _ => true,
                }
        });
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
        if excluded > 0 {
            evidence.push(format!(
                "{excluded} older error(s) on this socket predate the current device and were \
                 excluded — they belonged to whatever was plugged in before"
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
    for (name, mut events) in by_device {
        let bus = events.first().and_then(|e| e.bus());
        // The socket's current occupant, if any, and where it is.
        let socket = bus.as_deref().and_then(|b| {
            sockets.iter().find(|r| r.fast.hub_name == b)
        });
        let sibling = socket
            .and_then(|r| r.slow.port.child.as_deref())
            .and_then(|c| snap.device(c));

        // A failure that predates the socket's current occupant belonged to
        // whatever was plugged in before, and must not be reported against what
        // is there now.
        //
        // Two independent ways to date it, applied together because each covers
        // what the other misses:
        //
        // * the USB 2.0 companion of the same receptacle — but a socket holding
        //   a SuperSpeed-only device has an empty companion, so this often
        //   yields nothing;
        // * the nearest ancestor of the phantom that still exists. `6-1.2`'s
        //   parent `6-1` is decisive: if a webcam is sitting there now and it
        //   attached after these events, the events belonged to the hub that
        //   used to be there.
        let mut excluded = 0;
        let ancestor = snap.nearest_existing_ancestor(name);
        for reference in [sibling, ancestor].into_iter().flatten() {
            let (kept, dropped) = snap.filter_since_attach(events, reference);
            events = kept;
            excluded += dropped;
        }
        if events.is_empty() {
            continue;
        }

        let context = socket.map(|r| match sibling {
            Some(d) => format!(
                "the SuperSpeed half of the {} socket, whose USB 2.0 half is running {}",
                r.where_is(),
                d.label()
            ),
            None => format!("the SuperSpeed half of the {} socket", r.where_is()),
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
        evidence.push(format!("{} relevant event(s)", events.len()));
        if excluded > 0 {
            evidence.push(format!(
                "{excluded} older event(s) excluded — they predate the device now on this socket"
            ));
        }
        for e in events.iter().rev().take(3) {
            match e.errno.and_then(errno_meaning) {
                Some(m) => evidence.push(format!("{}  [{}]", e.text, m)),
                None => evidence.push(e.text.clone()),
            }
        }

        out.push(Finding {
            code: "DEVICE_FAILED_TO_ENUMERATE".into(),
            severity: Severity::High,
            confidence: Confidence::Measured,
            subject: Subject::Device(name.to_string()),
            title: match socket {
                Some(r) => format!(
                    "{name} never enumerated on the SuperSpeed half of the {} socket",
                    r.where_is()
                ),
                None => format!("{name} tried to attach and never enumerated"),
            },
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

/// Below this many transport errors, a window says nothing. One `-110` while a
/// device wakes from suspend is normal; three is a pattern.
const URB_ERROR_MIN: u64 = 3;

/// Enough completions for a rate to mean anything.
const URB_MIN_COMPLETIONS: u64 = 50;

/// A healthy link produces essentially no transport errors, so the bar for
/// "elevated" is low. Above [`URB_RATE_SEVERE`] the link is failing outright.
const URB_RATE_ELEVATED: f64 = 0.001;
const URB_RATE_SEVERE: f64 = 0.01;

/// Findings that an elevated error rate genuinely speaks to. Each already
/// claims the link is underperforming; measured errors say it is also failing.
const CORROBORATED_BY_URB_ERRORS: [&str; 4] = [
    "LINK_BELOW_DEVICE_CAPABILITY",
    "LINK_SLOW_DESPITE_CAPABLE_CABLE",
    "LINK_SINGLE_LANE",
    "DEVICE_RESET_STORM",
];

/// URB completion errors measured over a window — behaviour, not negotiated
/// state, and the only evidence in this crate that a link is failing *now*.
///
/// Only transport errors count. Stalls and driver cancellations are excluded
/// upstream, in `usbmon::classify`, because a webcam stopping its stream and a
/// device declining an unsupported control request both produce non-zero
/// statuses in bulk and neither says anything about the wire.
fn urb_error_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    let Some(traffic) = &snap.urb_traffic else {
        return;
    };
    for stats in &traffic.devices {
        if stats.transport_errors < URB_ERROR_MIN {
            continue;
        }
        let Some(rate) = stats.transport_error_rate() else {
            continue;
        };
        // A handful of errors out of a handful of transfers is not yet a rate.
        if stats.completions < URB_MIN_COMPLETIONS && stats.transport_errors < 10 {
            continue;
        }
        if rate < URB_RATE_ELEVATED {
            continue;
        }

        let dev = snap.device_at_address(stats.bus, stats.device_address);
        let who = dev
            .map(|d| format!("{} ({})", d.label(), d.sysfs_name))
            .unwrap_or_else(|| {
                format!(
                    "the device at bus {} address {}",
                    stats.bus, stats.device_address
                )
            });

        let severe = rate >= URB_RATE_SEVERE;
        let mut evidence = vec![
            format!(
                "{} of {} completions failed in transport over {:.1}s — {:.2}%",
                stats.transport_errors,
                stats.completions,
                traffic.window_ms as f64 / 1000.0,
                rate * 100.0
            ),
            format!("statuses: {}", stats.error_breakdown().join(", ")),
        ];
        if stats.transport_endpoints.len() > 1 {
            evidence.push(format!(
                "{} endpoints affected, which points at the path rather than one pipe",
                stats.transport_endpoints.len()
            ));
        }
        if stats.cancellations > 0 {
            evidence.push(format!(
                "{} driver cancellations excluded — those are routine, not faults",
                stats.cancellations
            ));
        }

        out.push(Finding {
            code: "LINK_ERROR_RATE".into(),
            severity: if severe {
                Severity::High
            } else {
                Severity::Medium
            },
            confidence: Confidence::Measured,
            subject: match dev {
                Some(d) => Subject::Device(d.sysfs_name.clone()),
                None => Subject::Host,
            },
            title: format!(
                "{who} is losing {:.2}% of its transfers to link errors",
                rate * 100.0
            ),
            detail: "These are transfers the bus failed to carry intact — CRC mismatches, \
                     babble, or no answer at all — counted as they happened rather than \
                     inferred from anything. A link that negotiated cleanly and then drops \
                     packets under load is the signature of a marginal physical path: a \
                     damaged or out-of-spec cable, a dirty or worn connector, or a hub \
                     running past what its power budget supports. It is not a driver \
                     problem, and it is not power management."
                .to_string(),
            evidence,
            suggestion: Some(
                "Swap the cable first — it is the cheapest thing in the path and the most \
                 common cause. If the errors follow the device to another port and another \
                 cable, the device is at fault; if they stay with the port, the port is."
                    .into(),
            ),
        });
    }
}

// ---------------------------------------------------------------------------
// Measured throughput
// ---------------------------------------------------------------------------

/// A rate below this is beneath what a USB 2.0 connection would have delivered
/// — the practical ceiling of High-Speed bulk, near 40 MB/s.
const USB2_PRACTICAL_BPS: f64 = 40.0e6;

/// A known-rotating disk reading below half its platter's ceiling is worth
/// mentioning. Generous, because seek patterns and the inner tracks are real.
const ROTATING_SHORTFALL: f64 = 0.5;

/// Judge a measured read rate — carefully, because the obvious rule is wrong.
///
/// The tempting version compares achieved throughput against the negotiated
/// link rate and complains when it falls short. That would condemn almost every
/// healthy drive: a 5400 rpm disk behind a 5 Gbps bridge sustains ~110 MB/s
/// against a link that allows ~450, and a cheap flash drive is not much better.
/// The bottleneck is usually the medium, and the medium is usually unknowable —
/// [`BlockDevice::medium`] returns `Unknown` for nearly everything on USB,
/// because bridges do not implement the VPD page that would say.
///
/// So the comparison is not against the link. It is against **the slowest thing
/// the medium could plausibly be**, and a finding is only made where no medium
/// explains the number:
///
/// * medium known to spin — compare against the platter's ceiling;
/// * medium unknown, SuperSpeed link — complain only below what plain USB 2.0
///   would have given, since no storage device negotiates 5 Gbps and then reads
///   slower than a High-Speed link would have allowed;
/// * medium unknown, High-Speed link — say nothing at all. A genuinely slow
///   flash drive is indistinguishable from a bad cable at these rates, and
///   guessing would mean condemning cheap hardware for being cheap.
///
/// The measurement is reported either way. Only the accusation is withheld.
fn throughput_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    for sample in &snap.throughput {
        if let Some(err) = &sample.error {
            // Distinguish "could not start" from "stopped partway": the second
            // is a hardware symptom, the first is usually a permission.
            if sample.bytes_read > 0 {
                out.push(read_error_finding(snap, sample, err));
            }
            continue;
        }

        let Some(achieved) = sample.bytes_per_second else {
            continue;
        };
        // Somebody else was using the disk, so the number is about contention
        // rather than the link. Shown, never judged.
        if sample.was_contended() {
            continue;
        }

        let Some((dev, block)) = storage_pair(snap, &sample.device) else {
            continue;
        };
        let Some(speed) = &dev.speed else {
            continue;
        };
        let link = speed.practical_bps();

        // The medium is the yardstick, and the bus almost never supplies it —
        // USB bridges omit VPD page B1h. A user who said "that one is a
        // spinning disk" gives the rule a real threshold instead of the
        // "slowest plausible medium" fallback below.
        let (medium, medium_source) = snap.medium_of(block);
        let (floor, because) = match medium.practical_ceiling_bps() {
            Some(media) => (
                media.min(link) * ROTATING_SHORTFALL,
                format!(
                    "a spinning disk should sustain around {}/s",
                    bytes_per_second(media)
                ),
            ),
            // The usual case on USB. Only an unambiguous collapse counts.
            None if link > USB2_PRACTICAL_BPS => (
                USB2_PRACTICAL_BPS,
                "no storage device negotiates SuperSpeed and then reads slower than a USB 2.0 \
                 link would have allowed, whatever it is made of"
                    .to_string(),
            ),
            // High-Speed link, unknown medium: not judgeable, and saying so is
            // better than guessing.
            None => continue,
        };

        if achieved >= floor {
            continue;
        }

        // A declaration is better evidence than the bus could give and still is
        // not a measurement, so a finding that used one cannot claim to be one.
        let declared = Kind {
            kind: DeviceKind::Storage,
            source: medium_source,
        };
        out.push(Finding {
            code: "THROUGHPUT_FAR_BELOW_LINK".into(),
            severity: Severity::Medium,
            confidence: declared.cap(Confidence::Measured),
            subject: Subject::Device(dev.sysfs_name.clone()),
            title: format!(
                "{} reads at {}/s on a link that allows {}/s",
                sample.device,
                bytes_per_second(achieved),
                bytes_per_second(link)
            ),
            detail: format!(
                "This is a measured sequential read straight from the device with the page \
                 cache bypassed, so it is what the path actually carries rather than what it \
                 negotiated. {because}. A link that trains at full speed and then delivers a \
                 fraction of it is the signature of a physical path retrying heavily — a \
                 damaged or out-of-spec cable, a worn connector, or a hub short of power. A \
                 genuinely slow drive is the other explanation, and only substitution tells \
                 the two apart."
            ),
            evidence: vec![
                format!(
                    "{} read in {:.1}s = {}/s",
                    bytes_human(sample.bytes_read),
                    sample.elapsed_ms as f64 / 1000.0,
                    bytes_per_second(achieved)
                ),
                format!("link negotiated {} ({})", speed.label, dev.sysfs_name),
                match dev.declared.as_ref().filter(|_| medium_source == KindSource::User) {
                    Some(d) => format!("medium: {} \u{2014} {}", medium.label(), d.cite()),
                    None => format!("medium: {}", medium.label()),
                },
            ],
            suggestion: Some(
                "Measure the same drive again on a different cable, then on a different \
                 port. A number that improves with the cable convicts the cable; one that \
                 follows the drive everywhere is the drive."
                    .into(),
            ),
        });
    }
}

/// A read that began and then failed. The drive answered, and then stopped
/// answering — which no amount of negotiated state would have revealed.
fn read_error_finding(snap: &Snapshot, sample: &ThroughputSample, err: &str) -> Finding {
    let subject = storage_pair(snap, &sample.device)
        .map(|(d, _)| Subject::Device(d.sysfs_name.clone()))
        .unwrap_or(Subject::Host);
    Finding {
        code: "STORAGE_READ_FAILED".into(),
        severity: Severity::High,
        confidence: Confidence::Measured,
        subject,
        title: format!(
            "{} stopped answering {} into a sequential read",
            sample.device,
            bytes_human(sample.bytes_read)
        ),
        detail: "The device served part of the read and then returned an error. That is a \
                 fault in the path or the medium, not a configuration problem: a healthy \
                 drive on a healthy cable reads from end to end without complaint."
            .to_string(),
        evidence: vec![err.to_string()],
        suggestion: Some(
            "Check the kernel log for the I/O error behind this, and re-run on another \
             cable. If it fails at the same offset every time, the medium is failing; if \
             the offset moves, suspect the path."
                .into(),
        ),
    }
}

/// The USB device a disk hangs off, together with the disk itself.
/// SCSI command errors seen *while watching*, from `/sys/block/<dev>/device/`.
///
/// The unprivileged half of what `LINK_ERROR_RATE` reaches through usbmon. It
/// gets its own code rather than folding into that one, so the two sources are
/// never conflated in evidence: one is URB status off the bus, this is the SCSI
/// layer's own accounting, and they fail in different ways.
///
/// **Only the delta is judged.** The absolute counters cannot be: two healthy
/// flash drives on the development machine both sat at exactly `ioerr_cnt = 2`
/// straight out of discovery, so a rule on the absolute value would condemn
/// every storage device on every machine. That means this fires only when the
/// caller sampled over a window — the same shape as the throughput rules.
fn scsi_error_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    for (dev, blocks) in snap.storage_devices() {
        for block in blocks {
            let Some(d) = &block.scsi_delta else { continue };
            if d.is_clean() {
                continue;
            }

            // A timeout is a command that never came back, which is a transport
            // failure rather than a device declining something. An error is
            // weaker on its own: at a low rate against real traffic it is
            // ordinary, and with no traffic at all it means nothing.
            let rate = d.error_rate();
            let severity = match (d.timeouts, rate) {
                (t, _) if t > 0 => Severity::High,
                (_, Some(r)) if r >= 0.01 => Severity::Medium,
                (_, Some(r)) if r > 0.0 => Severity::Low,
                // Errors with no requests in the window: the counter moved
                // because of something we did not see, so there is no rate to
                // judge and nothing worth saying.
                _ => continue,
            };

            let headline = if d.timeouts > 0 {
                format!(
                    "{} stopped answering {} command(s) while being watched",
                    block.label(),
                    d.timeouts
                )
            } else {
                format!(
                    "{} failed {} of {} commands while being watched",
                    block.label(),
                    d.errors,
                    d.requests
                )
            };

            let mut evidence = vec![format!(
                "over {:.1}s: {} requests, {} errors, {} timeouts",
                d.window_ms as f64 / 1000.0,
                d.requests,
                d.errors,
                d.timeouts
            )];
            if let Some(c) = &block.scsi {
                evidence.push(format!(
                    "cumulative since attach: iorequest {} ioerr {} iotmo {} \u{2014} a small \
                     non-zero ioerr is normal, it counts discovery probes too",
                    c.iorequest_cnt, c.ioerr_cnt, c.iotmo_cnt
                ));
            }

            out.push(Finding {
                code: "STORAGE_IO_ERRORS".into(),
                severity,
                // The counters are read straight from the kernel. What they
                // mean for the *cable* is the inference, and the detail says so
                // rather than the confidence pretending otherwise.
                confidence: Confidence::Measured,
                subject: Subject::Device(dev.sysfs_name.clone()),
                title: headline,
                detail: "These are the SCSI layer's own counters, not the USB bus's, and they \
                         moved during the sampling window rather than at discovery. Commands \
                         that fail or time out on a drive that enumerated cleanly point at the \
                         path rather than the negotiation — a marginal cable, a worn connector, \
                         or a hub short of power. A failing drive is the other explanation."
                    .into(),
                evidence,
                suggestion: Some(
                    "Sample again on a different cable. Counters that stay clean convict the \
                     cable; counters that follow the drive convict the drive."
                        .into(),
                ),
            });
        }
    }
}

fn storage_pair<'a>(
    snap: &'a Snapshot,
    disk: &str,
) -> Option<(&'a UsbDevice, &'a crate::model::BlockDevice)> {
    snap.storage_devices().into_iter().find_map(|(dev, blocks)| {
        blocks
            .into_iter()
            .find(|b| b.name == disk)
            .map(|b| (dev, b))
    })
}

fn bytes_per_second(bps: f64) -> String {
    bytes_human(bps as u64)
}

fn bytes_human(b: u64) -> String {
    const U: [(f64, &str); 3] = [(1e9, "GB"), (1e6, "MB"), (1e3, "kB")];
    let f = b as f64;
    for (scale, unit) in U {
        if f >= scale {
            return format!("{:.1} {unit}", f / scale);
        }
    }
    format!("{b} B")
}

// ---------------------------------------------------------------------------
// Re-enumeration cycling
// ---------------------------------------------------------------------------

/// Judge a port that was deliberately cycled.
///
/// This is the only rule in the file whose input was *produced* rather than
/// observed, and that changes what can be said. Everywhere else the tool sees
/// one sample of a link and has to hedge about whether it is typical. Here the
/// link was asked the same question twenty times, so "it answers differently
/// each time" is a fact about the link and not an inference from one reading.
///
/// A clean run is reported too. Twenty identical trainings do not prove a cable
/// is good, but they do rule out intermittency, and eliminating a hypothesis is
/// worth saying out loud — otherwise a user who ran the test learns nothing
/// from it having passed.
fn reenumeration_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    let Some(run) = &snap.reenumeration else {
        return;
    };
    if run.cycles.is_empty() {
        return;
    }

    let distribution = run.speed_distribution();
    let mut evidence: Vec<String> = distribution
        .iter()
        .map(|(label, n)| format!("{n} of {} cycles trained at {label}", run.cycles.len()))
        .collect();
    if run.failures() > 0 {
        evidence.push(format!(
            "{} of {} cycles did not come back at all",
            run.failures(),
            run.cycles.len()
        ));
    }
    evidence.push(format!("port {} ({})", run.port, run.port_path.display()));

    if !run.is_intermittent() {
        let what = distribution
            .first()
            .map(|(label, _)| label.clone())
            .unwrap_or_else(|| "the same rate".into());
        out.push(Finding {
            code: "LINK_STABLE_UNDER_CYCLING".into(),
            severity: Severity::Info,
            confidence: Confidence::Measured,
            subject: Subject::Device(run.device.clone()),
            title: format!(
                "{} trained at {what} on all {} attempts",
                run.device,
                run.cycles.len()
            ),
            detail: "The port was switched off and on repeatedly and the link came back the \
                     same way every time. That does not prove the cable is good — a fault \
                     that only appears under sustained load or at temperature would not show \
                     here — but it does rule out the intermittent training that a single \
                     reading can never distinguish from a stable one."
                .to_string(),
            evidence,
            suggestion: None,
        });
        return;
    }

    // Something varied. Which thing decides how bad it is: a link that
    // sometimes trains slower is degraded, one that sometimes does not appear
    // is failing.
    let never_returned = run.failures() > 0;
    let best = run.best_mbps().map(LinkSpeed::from_mbps);
    let title = match (&best, never_returned) {
        (_, true) => format!(
            "{} failed to re-appear on {} of {} attempts",
            run.device,
            run.failures(),
            run.cycles.len()
        ),
        (Some(best), false) => format!(
            "{} reached {} on only {} of {} attempts",
            run.device,
            best.short(),
            distribution
                .iter()
                .find(|(l, _)| *l == best.short())
                .map(|(_, n)| *n)
                .unwrap_or(0),
            run.cycles.len()
        ),
        (None, false) => format!("{} trained inconsistently", run.device),
    };

    out.push(Finding {
        code: "LINK_INTERMITTENT".into(),
        severity: if never_returned {
            Severity::High
        } else {
            Severity::Medium
        },
        confidence: Confidence::Measured,
        subject: Subject::Device(run.device.clone()),
        title,
        detail: "The same port was cycled repeatedly and did not behave the same way twice. \
                 This is the fault that no single reading can find: every individual check \
                 looks fine, and the link only fails some of the time. A device that \
                 sometimes trains and sometimes does not is almost never a firmware problem \
                 — it is a physical one, in the cable, the connector, or the socket."
            .to_string(),
        evidence,
        suggestion: Some(
            "Run this again with a different cable. Intermittency that disappears with the \
             cable is the cable; intermittency that follows the device to another port is \
             the device or its connector."
                .into(),
        ),
    });
}

/// Let measured errors strengthen the findings they speak to.
///
/// Deliberately does **not** promote anything to [`Confidence::Measured`].
/// The error counts are measured; blaming the cable for them is still an
/// inference, and a cable can only be convicted by substitution. Raising a
/// heuristic to inferred is as far as the evidence reaches.
fn corroborate_with_urb_errors(snap: &Snapshot, findings: &mut [Finding]) {
    let Some(traffic) = &snap.urb_traffic else {
        return;
    };
    for f in findings.iter_mut() {
        if !CORROBORATED_BY_URB_ERRORS.contains(&f.code.as_str()) {
            continue;
        }
        let Subject::Device(name) = &f.subject else {
            continue;
        };
        let Some(dev) = snap.device(name) else {
            continue;
        };
        let Some((bus, addr)) = dev.busnum.zip(dev.devnum) else {
            continue;
        };
        let Some(stats) = traffic.for_address(bus, addr) else {
            continue;
        };
        if stats.transport_errors < URB_ERROR_MIN {
            continue;
        }

        f.evidence.push(format!(
            "corroborated: {} transport error(s) measured over {:.1}s of live traffic ({})",
            stats.transport_errors,
            traffic.window_ms as f64 / 1000.0,
            stats.error_breakdown().join(", ")
        ));
        if f.confidence == Confidence::Heuristic {
            f.confidence = Confidence::Inferred;
        }
    }
}

/// The SVID VESA assigned to DisplayPort Alternate Mode.
pub(crate) const SVID_DISPLAYPORT: u16 = 0xff01;

/// Cross-check a DisplayPort Alt Mode against whether a picture came out.
///
/// The Type-C class can only describe a negotiation. DRM describes the result,
/// and the two disagreeing is the signature of a cable carrying power and USB
/// data but no DisplayPort lanes — a very common failure with charge-only or
/// USB 2.0-era USB-C cables, and one the user experiences as "the monitor just
/// doesn't come on".
///
/// Three guards, because a false accusation here would be easy to write:
///
/// * only the **partner's** alt modes count. A local port's own list says what
///   the port supports, and UCSI firmware reports every entry of it as
///   `active = yes` regardless of what is attached — on the machine this was
///   written against, both ports claim DisplayPort mode is active while one
///   holds a charger with zero alternate modes;
/// * the machine must actually have a DisplayPort output, or there is nothing
///   meaningful to have looked for;
/// * DRM must be readable at all.
///
/// Untested against hardware: it needs a device that negotiates DP Alt Mode,
/// which was not available when this was written. The guards are why it is
/// Inferred rather than Measured.
fn display_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    let dp_outputs: Vec<&DisplayConnector> = snap
        .displays
        .iter()
        .filter(|d| d.is_displayport() && !d.is_internal())
        .collect();
    if dp_outputs.is_empty() {
        return;
    }
    if dp_outputs.iter().any(|d| d.is_connected()) {
        return;
    }

    for port in &snap.ports {
        let Some(partner) = &port.partner else {
            continue;
        };
        if !partner
            .alt_modes
            .iter()
            .any(|m| m.svid == Some(SVID_DISPLAYPORT) && m.active == Some(true))
        {
            continue;
        }

        out.push(Finding {
            code: "DP_ALT_MODE_NO_OUTPUT".into(),
            severity: Severity::Medium,
            confidence: Confidence::Inferred,
            subject: Subject::Port(port.name.clone()),
            title: format!(
                "{} entered DisplayPort Alt Mode but no DisplayPort output is live",
                partner.kind.as_deref().unwrap_or("The attached device")
            ),
            detail: format!(
                "The attached device negotiated DisplayPort Alternate Mode, so both ends agreed \
                 to carry video. The graphics driver still sees nothing on any of its {} \
                 DisplayPort outputs. The usual cause is the cable: DisplayPort needs the \
                 high-speed pairs, and a charge-only or USB 2.0-era USB-C cable has none of \
                 them, while still carrying power and enough USB data to look fine.",
                dp_outputs.len()
            ),
            evidence: vec![
                format!(
                    "{} reports SVID ff01 (DisplayPort) active",
                    partner.sysfs_name
                ),
                format!(
                    "DisplayPort connectors, all disconnected: {}",
                    dp_outputs
                        .iter()
                        .map(|d| d.connector.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ],
            suggestion: Some(
                "Try a cable rated for USB 3.2 or Thunderbolt. If the display is behind a hub or \
                 dock, connect it directly to tell the cable and the dock apart."
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
        // Deliberately an interface check rather than `kind()`. A Billboard is
        // a symptom marker, not an identity: a dock that also exposes storage
        // classifies as storage, and asking "what is this device" would then
        // lose the failure report the class exists to carry.
        if !dev.has_interface_class(CLASS_BILLBOARD) {
            continue;
        }
        // A video adapter with nothing plugged into its output has no reason to
        // enter DisplayPort mode, and will say so exactly like one that tried
        // and failed. Checking DRM separates a fault from an empty socket — and
        // without it this fires at Medium and blames the cable of a device that
        // has no cable, which is what a Dell DA20 with an empty HDMI socket
        // produced on the machine this was written against.
        let has_drm = !snap.displays.is_empty();
        let external_display = snap
            .displays
            .iter()
            .any(|d| !d.is_internal() && d.is_connected());
        let nothing_to_drive = has_drm && !external_display;
        // A live DisplayPort output *is* DisplayPort Alt Mode working. Some
        // adapters present a Billboard permanently rather than only on failure
        // — the Dell DA20 does, and kept reporting one while driving a 1440p
        // monitor through it. Contradicting a picture on the screen is the
        // worst thing this rule could do.
        let dp_working = snap
            .displays
            .iter()
            .any(|d| d.is_displayport() && !d.is_internal() && d.is_lit());

        let mut evidence = vec![
            format!("{} exposes interface class 0x11 (billboard)", dev.sysfs_name),
            format!(
                "{}:{} at {}",
                dev.vid_pid().unwrap_or_default(),
                dev.usb_version.as_deref().unwrap_or("?"),
                dev.speed.as_ref().map(|s| s.short()).unwrap_or_default()
            ),
        ];
        if dp_working {
            evidence.push(
                "a DisplayPort output is connected and being driven, so DisplayPort Alt Mode \
                 is working despite this report"
                    .into(),
            );
        } else if nothing_to_drive {
            evidence.push(
                "no external display is connected on any output, so there may simply be \
                 nothing for it to drive"
                    .into(),
            );
        } else if external_display {
            evidence.push(
                "an external display is connected, so something was available to drive".into(),
            );
        }

        out.push(Finding {
            code: "BILLBOARD_ALT_MODE_FAILED".into(),
            // Only a fault when there was something to fail at, and not a fault
            // at all when the mode is demonstrably working.
            severity: if dp_working {
                Severity::Info
            } else if nothing_to_drive {
                Severity::Low
            } else {
                Severity::Medium
            },
            confidence: Confidence::Measured,
            subject: Subject::Device(dev.sysfs_name.clone()),
            title: format!(
                "{} reports an Alternate Mode it could not enter",
                dev.label()
            ),
            detail: if dp_working {
                "A USB Billboard device is meant to announce that an Alternate Mode was not \
                 entered — but a DisplayPort output on this machine is connected and being \
                 driven, so the mode plainly did work. Some adapters expose the Billboard \
                 interface permanently instead of only on failure, and this appears to be \
                 one of them. Nothing here is wrong; it is reported only so the interface \
                 is not mistaken for a fault later."
                    .to_string()
            } else if nothing_to_drive {
                "A USB Billboard device exists to announce that an Alternate Mode — \
                 DisplayPort, Thunderbolt, or a vendor mode — was not entered. Nothing is \
                 connected to any display output on this machine, so the most likely \
                 explanation is the ordinary one: a video adapter with an empty socket has \
                 nothing to drive and does not enter the mode. It reports the same way \
                 whether it declined for that reason or tried and failed, so this is worth \
                 re-reading once a display is attached."
                    .to_string()
            } else {
                "A USB Billboard device exists only to announce a failure: the attached \
                 USB-C device requested an Alternate Mode — DisplayPort, Thunderbolt, or a \
                 vendor mode — and the negotiation did not succeed, so it fell back to \
                 presenting this instead. Common causes are a cable without the required \
                 wiring, a port that does not support the mode, or a link that could not \
                 train. The specific modes it wanted live in a Billboard capability \
                 descriptor, which sysfs does not expose."
                    .to_string()
            },
            evidence,
            suggestion: if dp_working {
                None
            } else if nothing_to_drive {
                Some(
                    "Nothing to do unless you expected a picture. Attach a display and look \
                     again: if this is still reported with one connected, the mode genuinely \
                     failed."
                        .into(),
                )
            } else {
                Some(
                    "If you expected video or a docking mode from this device, the cable is \
                     the usual cause — it must carry the SuperSpeed pairs, not just power \
                     and USB 2.0."
                        .into(),
                )
            },
        });
    }
}

/// USB4 / Thunderbolt findings, including the one form of cable identity that
/// works on platforms whose firmware never reports a PD e-marker.
fn thunderbolt_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    let tb = &snap.thunderbolt;

    // Retimers exist only inside an active cable, so their presence *is* the
    // identification — no inference required.
    if tb.has_active_cable() {
        let mut evidence: Vec<String> = tb
            .retimers
            .iter()
            .map(|r| {
                format!(
                    "{}: vendor {} device {} firmware {}",
                    r.name,
                    r.vendor.map(|v| format!("{v:04x}")).unwrap_or_else(|| "?".into()),
                    r.device.map(|d| format!("{d:04x}")).unwrap_or_else(|| "?".into()),
                    r.nvm_version.as_deref().unwrap_or("?")
                )
            })
            .collect();
        evidence.push(
            "retimers are the signal-conditioning silicon inside an active cable".into(),
        );
        out.push(Finding {
            code: "ACTIVE_CABLE_PRESENT".into(),
            severity: Severity::Info,
            confidence: Confidence::Measured,
            subject: Subject::Cable("thunderbolt".into()),
            title: format!(
                "Active cable detected — {} retimer(s), firmware {}",
                tb.retimers.len(),
                tb.retimers
                    .iter()
                    .find_map(|r| r.nvm_version.clone())
                    .unwrap_or_else(|| "unknown".into())
            ),
            detail: "The kernel enumerated retimers on this link. Retimers only exist inside \
                     active cables, which are required to be e-marked, so this is genuine cable \
                     identity read from the cable's own silicon rather than inferred. This path \
                     is independent of PD SOP', so it works even where platform firmware never \
                     reports a cable e-marker."
                .into(),
            evidence,
            suggestion: None,
        });
    }

    // A generation-4 router is capable of 40 Gbps; report when it links slower.
    for r in tb.attached() {
        let gen = r.generation.unwrap_or(0);
        let lanes_short = r.tx_lanes == Some(1);
        if gen >= 4 && lanes_short {
            out.push(Finding {
                code: "USB4_LINK_BELOW_CAPABILITY".into(),
                severity: Severity::Medium,
                confidence: Confidence::Inferred,
                subject: Subject::Device(r.name.clone()),
                title: format!(
                    "{} is a generation {gen} router but linked with one lane",
                    r.label()
                ),
                detail: "A generation 4 router supports 40 Gbps over two lanes. Running on one \
                         lane halves the available bandwidth, and the usual cause is a cable that \
                         is not rated for the full link — Thunderbolt 4 and USB4 40 Gbps need a \
                         certified cable, and passive ones are only rated to 0.8 m."
                    .into(),
                evidence: vec![
                    format!(
                        "tx_lanes={} rx_lanes={}",
                        r.tx_lanes.unwrap_or(0),
                        r.rx_lanes.unwrap_or(0)
                    ),
                    format!(
                        "rx {} / tx {}",
                        r.rx_speed.as_deref().unwrap_or("?"),
                        r.tx_speed.as_deref().unwrap_or("?")
                    ),
                ],
                suggestion: Some(
                    "Use a certified Thunderbolt 4 or USB4 40 Gbps cable, keeping passive cables \
                     under 0.8 m."
                        .into(),
                ),
            });
        }
    }
}

/// Is the attached supply actually keeping up?
///
/// The PD contract states what is *permitted*; the battery states what is
/// *happening*. Without this the tool can only say "possible drain" — with it,
/// it can say the pack is losing ground and name the supply responsible.
fn battery_rules(snap: &Snapshot, out: &mut Vec<Finding>) {
    let mains = snap.mains_online.unwrap_or(false);
    if !mains {
        return;
    }
    for bat in &snap.batteries {
        if !bat.not_keeping_up(mains) {
            continue;
        }
        // Name the supply, since that is the thing to change.
        let contract = snap
            .ports
            .iter()
            .find(|p| p.is_sinking() && p.power_supply.as_ref().is_some_and(|s| s.is_drawing_power()))
            .and_then(|p| {
                p.power_supply
                    .as_ref()
                    .and_then(|s| s.contract_power_mw())
                    .map(|mw| (p.name.clone(), mw))
            });

        let mut evidence = vec![
            format!(
                "{}: status={}, power_now={}",
                bat.name,
                bat.status.as_deref().unwrap_or("?"),
                bat.power_now_w
                    .map(|p| format!("{p:.1} W"))
                    .unwrap_or_else(|| "not reported".into())
            ),
            "a mains supply is online".to_string(),
        ];
        if let (Some(now), Some(full)) = (bat.energy_now_wh, bat.energy_full_wh) {
            evidence.push(format!(
                "charge {:.1} Wh of {:.1} Wh ({}%)",
                now,
                full,
                bat.capacity_pct.unwrap_or(0)
            ));
        }
        if let Some((port, mw)) = &contract {
            evidence.push(format!("{port} contract: {}", watts(*mw)));
        }

        out.push(Finding {
            code: "BATTERY_DRAINING_ON_AC".into(),
            severity: Severity::Medium,
            confidence: Confidence::Measured,
            subject: Subject::Host,
            title: match &contract {
                Some((_, mw)) => format!(
                    "Battery is not gaining despite {} from the attached supply",
                    watts(*mw)
                ),
                None => "Battery is not gaining although mains power is present".to_string(),
            },
            detail: "The system is drawing at least as much as the supply provides, so the \
                     shortfall comes out of the battery. Under sustained load the machine will \
                     run down even while plugged in. The usual causes are a supply rated below \
                     what this machine can accept, a hub or dock passing through only part of \
                     what it receives, or peripherals being powered from the same budget. Note \
                     that a status of \"Charging\" with zero flow is not a contradiction — it is \
                     what the driver reports when the contract covers the load exactly and \
                     nothing is left over."
                .into(),
            evidence,
            suggestion: Some(
                "Use a supply closer to what this machine accepts, or connect the charger \
                 directly rather than through a hub that reserves part of the budget."
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

    // -----------------------------------------------------------------------
    // Measured throughput
    // -----------------------------------------------------------------------

    /// A USB disk at a given link speed, with one measured read rate.
    fn measured(mbps: f64, rotational: Option<bool>, bps: f64) -> Snapshot {
        let mut snap = empty_snapshot();
        let mut bus = root_hub("usb4", mbps);
        let mut dev = device("4-1", " 3.20", mbps, Some("usb4"));
        dev.sysfs_path = std::path::PathBuf::from("/sys/devices/pci/usb4/4-1");
        bus.children.push(dev);
        snap.buses.push(bus);
        snap.block_devices.push(BlockDevice {
            name: "sdb".into(),
            sysfs_path: std::path::PathBuf::from("/sys/devices/pci/usb4/4-1/host1/block/sdb"),
            model: None,
            vendor: None,
            size_bytes: Some(64_000_000_000),
            rotational,
            removable: Some(true),
            stats: None,
            throughput: None,
            scsi: None,
            scsi_delta: None,
        });
        snap.throughput.push(ThroughputSample {
            device: "sdb".into(),
            bytes_read: (bps * 3.0) as u64,
            elapsed_ms: 3000,
            bytes_per_second: Some(bps),
            contended_bytes: Some(0),
            error: None,
        });
        snap
    }

    // -----------------------------------------------------------------------
    // Re-enumeration cycling
    // -----------------------------------------------------------------------

    fn cycled(speeds: &[Option<f64>]) -> Snapshot {
        let mut snap = empty_snapshot();
        let mut bus = root_hub("usb4", 5000.0);
        bus.children
            .push(device("4-1", " 3.20", 5000.0, Some("usb4")));
        snap.buses.push(bus);
        snap.reenumeration = Some(ReenumerationRun {
            device: "4-1".into(),
            port: "usb4-port1".into(),
            port_path: "/sys/devices/usb4/4-0:1.0/usb4-port1".into(),
            requested_cycles: speeds.len(),
            error: None,
            cycles: speeds
                .iter()
                .enumerate()
                .map(|(index, mbps)| ReenumerationCycle {
                    index,
                    returned: mbps.is_some(),
                    returned_after_ms: 400,
                    speed_mbps: *mbps,
                    rx_lanes: Some(1),
                    tx_lanes: Some(1),
                    error: mbps.is_none().then(|| "did not come back".into()),
                })
                .collect(),
        });
        snap
    }

    /// A link that trains the same way every time is worth saying so about:
    /// the user ran a deliberate test and needs to learn something from it
    /// passing, not just from it failing.
    #[test]
    fn a_stable_link_produces_an_informational_result_not_silence() {
        let f = analyze(&cycled(&[Some(5000.0); 20]));
        let hit = f
            .iter()
            .find(|x| x.code == "LINK_STABLE_UNDER_CYCLING")
            .unwrap_or_else(|| panic!("{:?}", codes(&f)));
        assert_eq!(hit.severity, Severity::Info);
        assert!(hit.title.contains("all 20 attempts"), "{}", hit.title);
        // Info must not claim more than it can: cycling cannot clear a cable.
        assert!(hit.detail.contains("does not prove"), "{}", hit.detail);
        assert!(hit.suggestion.is_none(), "nothing to suggest about a pass");
        assert!(!codes(&f).contains(&"LINK_INTERMITTENT"));
    }

    /// The whole reason this probe exists: every individual training looks
    /// fine, and only the distribution shows the fault.
    #[test]
    fn a_link_that_sometimes_falls_back_is_caught() {
        let mut speeds = vec![Some(5000.0); 17];
        speeds.extend([Some(480.0); 3]);
        let f = analyze(&cycled(&speeds));

        let hit = f.iter().find(|x| x.code == "LINK_INTERMITTENT").unwrap();
        assert_eq!(hit.severity, Severity::Medium);
        assert_eq!(hit.confidence, Confidence::Measured);
        assert!(hit.title.contains("only 17 of 20"), "{}", hit.title);
        // The distribution is the evidence, and both rates must appear.
        let ev = hit.evidence.join(" | ");
        assert!(ev.contains("17 of 20 cycles trained at 5G"), "{ev}");
        assert!(ev.contains("3 of 20 cycles trained at 480M"), "{ev}");
        assert!(!codes(&f).contains(&"LINK_STABLE_UNDER_CYCLING"));
    }

    /// Not coming back at all is worse than coming back slow.
    #[test]
    fn a_device_that_vanishes_is_more_serious_than_one_that_downshifts() {
        let mut speeds = vec![Some(5000.0); 18];
        speeds.extend([None, None]);
        let f = analyze(&cycled(&speeds));

        let hit = f.iter().find(|x| x.code == "LINK_INTERMITTENT").unwrap();
        assert_eq!(hit.severity, Severity::High);
        assert!(
            hit.title.contains("failed to re-appear on 2 of 20"),
            "{}",
            hit.title
        );
        assert!(hit
            .evidence
            .iter()
            .any(|e| e.contains("did not come back at all")));
    }

    /// One cycle cannot disagree with itself, so it must never be reported as
    /// intermittent — and the gate keeps `--cycles` above one for this reason.
    #[test]
    fn a_single_cycle_is_never_called_intermittent() {
        for speed in [Some(5000.0), Some(480.0)] {
            let snap = cycled(&[speed]);
            assert!(!snap.reenumeration.as_ref().unwrap().is_intermittent());
            assert!(!codes(&analyze(&snap)).contains(&"LINK_INTERMITTENT"));
        }
    }

    /// The rule that would be easiest to get catastrophically wrong.
    ///
    /// Comparing achieved throughput against the *link* rate condemns almost
    /// every healthy drive — 110 MB/s over a 5 Gbps link that allows 450 is a
    /// perfectly ordinary flash drive. Only a collapse below what USB 2.0
    /// itself would have delivered counts.
    #[test]
    fn an_ordinary_drive_well_under_its_link_rate_is_not_accused() {
        // 110 MB/s on a 5 Gbps link: 24% of the link, and completely normal.
        let f = analyze(&measured(5000.0, None, 110e6));
        assert!(!codes(&f).contains(&"THROUGHPUT_FAR_BELOW_LINK"), "{:?}", codes(&f));

        // 45 MB/s: poor, still above what High-Speed would have given, still
        // explainable by a cheap drive. No accusation.
        let f = analyze(&measured(5000.0, None, 45e6));
        assert!(!codes(&f).contains(&"THROUGHPUT_FAR_BELOW_LINK"), "{:?}", codes(&f));
    }

    /// The case that is unambiguous: a SuperSpeed link delivering less than a
    /// USB 2.0 one would have. No storage medium explains that.
    #[test]
    fn a_superspeed_link_slower_than_usb2_is_reported() {
        let f = analyze(&measured(5000.0, None, 18e6));
        let hit = f
            .iter()
            .find(|x| x.code == "THROUGHPUT_FAR_BELOW_LINK")
            .unwrap_or_else(|| panic!("{:?}", codes(&f)));
        assert_eq!(hit.confidence, Confidence::Measured);
        assert_eq!(hit.subject, Subject::Device("4-1".into()));
        assert!(hit.title.contains("18.0 MB/s"), "{}", hit.title);
        // The suggestion must point at the only test that settles it.
        assert!(hit.suggestion.as_ref().unwrap().contains("different cable"));
    }

    /// On a High-Speed link an unknown medium is not judgeable at all: a slow
    /// flash drive and a bad cable look identical at these rates. Saying
    /// nothing is the correct output, not a gap.
    #[test]
    fn a_high_speed_link_with_an_unknown_medium_is_never_judged() {
        for bps in [30e6, 8e6, 2e6] {
            let f = analyze(&measured(480.0, None, bps));
            assert!(
                !codes(&f).contains(&"THROUGHPUT_FAR_BELOW_LINK"),
                "{bps} B/s judged on a 480 Mbps link: {:?}",
                codes(&f)
            );
        }
    }

    /// When the bridge does answer the medium question, the platter is the
    /// yardstick rather than the link.
    #[test]
    fn a_known_spinning_disk_is_judged_against_its_platter() {
        // 100 MB/s from a rotating disk is healthy, though it is only 22% of
        // the 5 Gbps link. `rotational: Some(true)` alone would be ignored over
        // USB, so this uses the internal-disk path to get a known medium.
        let mut snap = measured(5000.0, Some(true), 100e6);
        snap.block_devices[0].sysfs_path = "/sys/devices/pci/ata1/host1/block/sdb".into();
        assert_eq!(snap.block_devices[0].medium(), Medium::Rotating);
        // Attribution is by path, so the disk is no longer behind the USB
        // device and there is nothing to judge — which is the honest answer for
        // a SATA disk in a USB report.
        assert!(!codes(&analyze(&snap)).contains(&"THROUGHPUT_FAR_BELOW_LINK"));
    }

    // -----------------------------------------------------------------------
    // SCSI error counters
    // -----------------------------------------------------------------------

    /// A USB disk with a SCSI counter delta over a window.
    fn scsi(requests: u64, errors: u64, timeouts: u64) -> Snapshot {
        let mut snap = empty_snapshot();
        let mut bus = root_hub("usb4", 5000.0);
        let mut dev = device("4-1", " 3.20", 5000.0, Some("usb4"));
        dev.sysfs_path = std::path::PathBuf::from("/sys/devices/pci/usb4/4-1");
        bus.children.push(dev);
        snap.buses.push(bus);
        snap.block_devices.push(BlockDevice {
            name: "sdb".into(),
            sysfs_path: std::path::PathBuf::from("/sys/devices/pci/usb4/4-1/host1/block/sdb"),
            model: Some("Ultra".into()),
            vendor: Some("SanDisk".into()),
            size_bytes: Some(64_000_000_000),
            rotational: None,
            removable: Some(true),
            stats: None,
            throughput: None,
            // The real baseline from this machine: two errors from discovery.
            scsi: Some(ScsiCounters {
                iorequest_cnt: 0x243,
                iodone_cnt: 0x243,
                ioerr_cnt: 2,
                iotmo_cnt: 0,
                sampled_at_unix_ms: 1000,
            }),
            scsi_delta: Some(ScsiDelta {
                requests,
                errors,
                timeouts,
                window_ms: 3000,
            }),
        });
        snap
    }

    /// The mistake the whole design exists to avoid. Two healthy flash drives
    /// on this machine both read `ioerr_cnt = 2` straight out of discovery, so
    /// a rule on the absolute count condemns every storage device everywhere.
    #[test]
    fn the_discovery_baseline_is_never_an_accusation() {
        let snap = scsi(4000, 0, 0);
        assert_eq!(snap.block_devices[0].scsi.unwrap().ioerr_cnt, 2);
        assert!(!codes(&analyze(&snap)).contains(&"STORAGE_IO_ERRORS"));
    }

    /// Without a sampling window there is no delta, and the absolute counters
    /// are not judgeable — so a plain capture must stay silent however many
    /// errors are on the clock.
    #[test]
    fn an_unsampled_capture_never_fires_the_rule() {
        let mut snap = scsi(0, 0, 0);
        snap.block_devices[0].scsi_delta = None;
        snap.block_devices[0].scsi = Some(ScsiCounters {
            iorequest_cnt: 100_000,
            iodone_cnt: 90_000,
            ioerr_cnt: 9_000,
            iotmo_cnt: 500,
            sampled_at_unix_ms: 1000,
        });
        assert!(!codes(&analyze(&snap)).contains(&"STORAGE_IO_ERRORS"));
    }

    /// A timeout is a command that never came back — a transport failure, not
    /// a device declining an optional feature.
    #[test]
    fn a_timeout_in_the_window_is_high() {
        let f = analyze(&scsi(500, 0, 1));
        let hit = f
            .iter()
            .find(|x| x.code == "STORAGE_IO_ERRORS")
            .unwrap_or_else(|| panic!("{:?}", codes(&f)));
        assert_eq!(hit.severity, Severity::High);
        assert_eq!(hit.subject, Subject::Device("4-1".into()));
        assert!(hit.title.contains("stopped answering"), "{}", hit.title);
    }

    #[test]
    fn the_error_rate_decides_between_low_and_medium() {
        // 5 of 5000 = 0.1%: real, but ordinary.
        let low = analyze(&scsi(5000, 5, 0));
        assert_eq!(
            low.iter().find(|x| x.code == "STORAGE_IO_ERRORS").unwrap().severity,
            Severity::Low
        );
        // 100 of 5000 = 2%: worth acting on.
        let med = analyze(&scsi(5000, 100, 0));
        assert_eq!(
            med.iter().find(|x| x.code == "STORAGE_IO_ERRORS").unwrap().severity,
            Severity::Medium
        );
    }

    /// An idle window says nothing in either direction. Errors with no requests
    /// mean the counter moved because of traffic we did not see, so there is no
    /// rate to judge.
    #[test]
    fn errors_without_traffic_are_not_judged() {
        assert!(!codes(&analyze(&scsi(0, 3, 0))).contains(&"STORAGE_IO_ERRORS"));
        // ...but a timeout is a timeout whether or not we saw the traffic.
        assert!(codes(&analyze(&scsi(0, 0, 1))).contains(&"STORAGE_IO_ERRORS"));
    }

    #[test]
    fn a_clean_window_says_nothing() {
        assert!(!codes(&analyze(&scsi(20_000, 0, 0))).contains(&"STORAGE_IO_ERRORS"));
    }

    /// The evidence has to carry the caveat, or the next reader repeats the
    /// mistake this rule was written to avoid.
    #[test]
    fn the_evidence_says_a_small_nonzero_baseline_is_normal() {
        let f = analyze(&scsi(5000, 100, 0));
        let hit = f.iter().find(|x| x.code == "STORAGE_IO_ERRORS").unwrap();
        assert!(
            hit.evidence.iter().any(|e| e.contains("discovery probes")),
            "{:?}",
            hit.evidence
        );
        // And it must not be confused with the usbmon-derived rule.
        assert!(!codes(&f).contains(&"LINK_ERROR_RATE"));
    }

    /// Counters reset when a device re-enumerates, and a negative delta is not
    /// a fault — it is a new device wearing the same name.
    #[test]
    fn counters_going_backwards_produce_no_delta() {
        let before = ScsiCounters {
            iorequest_cnt: 5000,
            iodone_cnt: 5000,
            ioerr_cnt: 10,
            iotmo_cnt: 1,
            sampled_at_unix_ms: 1000,
        };
        let after = ScsiCounters {
            iorequest_cnt: 12,
            iodone_cnt: 12,
            ioerr_cnt: 2,
            iotmo_cnt: 0,
            sampled_at_unix_ms: 4000,
        };
        assert_eq!(after.delta(&before), None);
    }

    #[test]
    fn a_delta_reports_its_rate_only_when_there_was_traffic() {
        let d = ScsiDelta {
            requests: 200,
            errors: 4,
            timeouts: 0,
            window_ms: 3000,
        };
        assert_eq!(d.error_rate(), Some(0.02));
        assert!(!d.is_clean());

        let idle = ScsiDelta::default();
        assert_eq!(idle.error_rate(), None);
        assert!(idle.is_clean());
    }

    /// The case the whole override feature exists for.
    ///
    /// On a High-Speed link an unknown medium is not judgeable — the test above
    /// asserts the silence. A user who says "that one is a spinning disk"
    /// supplies the yardstick the bus cannot, and the rule can finally answer.
    #[test]
    fn a_declared_spinning_disk_gives_the_rule_a_yardstick_it_could_not_read() {
        // 8 MB/s on a High-Speed link: silence, because a cheap flash drive and
        // a bad cable are indistinguishable at these rates.
        let mut snap = measured(480.0, None, 8e6);
        assert!(!codes(&analyze(&snap)).contains(&"THROUGHPUT_FAR_BELOW_LINK"));

        // The user says it is a spinning disk. 8 MB/s from a platter that
        // should sustain ~120 MB/s is now unambiguous.
        snap.buses[0].children[0].declared = Some(crate::overrides::Declaration {
            id: "1234:5678".into(),
            unit: false,
            kind: None,
            medium: Some(Medium::Rotating),
            note: None,
        });
        let f = analyze(&snap);
        let hit = f
            .iter()
            .find(|x| x.code == "THROUGHPUT_FAR_BELOW_LINK")
            .unwrap_or_else(|| panic!("{:?}", codes(&f)));

        // A declaration is better evidence than the bus could give and is still
        // not a measurement.
        assert_eq!(hit.confidence, Confidence::Inferred);
        assert!(
            hit.evidence.iter().any(|e| e.contains("declared by you")),
            "the finding must say where the fact came from: {:?}",
            hit.evidence
        );
        assert!(
            hit.evidence.iter().any(|e| e.contains("1234:5678")),
            "and name the label so it can be found and deleted: {:?}",
            hit.evidence
        );
    }

    /// The override must not be able to invent a fault where the measurement
    /// is fine: a platter delivering platter speed is healthy.
    #[test]
    fn a_declared_spinning_disk_reading_at_platter_speed_is_not_accused() {
        let mut snap = measured(480.0, None, 40e6);
        snap.buses[0].children[0].declared = Some(crate::overrides::Declaration {
            id: "1234:5678".into(),
            unit: false,
            kind: None,
            medium: Some(Medium::Rotating),
            note: None,
        });
        // The link itself only allows ~40 MB/s, so the floor is the link, not
        // the platter, and 40 MB/s clears it.
        assert!(!codes(&analyze(&snap)).contains(&"THROUGHPUT_FAR_BELOW_LINK"));
    }

    /// A number taken while something else was hammering the disk describes
    /// the contention, not the link, and must never become a finding.
    #[test]
    fn a_contended_measurement_is_never_judged() {
        let mut snap = measured(5000.0, None, 6e6);
        // Uncontended, this would fire.
        assert!(codes(&analyze(&snap)).contains(&"THROUGHPUT_FAR_BELOW_LINK"));

        snap.throughput[0].contended_bytes = Some(500_000_000);
        assert!(snap.throughput[0].was_contended());
        assert!(
            !codes(&analyze(&snap)).contains(&"THROUGHPUT_FAR_BELOW_LINK"),
            "someone else was reading the disk"
        );
    }

    /// A read that starts and then fails is a hardware symptom. A read that
    /// never starts is usually a permission, and must not be reported as one.
    #[test]
    fn a_read_that_dies_partway_is_a_finding_but_one_that_never_started_is_not() {
        let mut snap = measured(5000.0, None, 400e6);
        snap.throughput[0].error = Some("read failed at offset 4194304: Input/output error".into());
        snap.throughput[0].bytes_read = 4_194_304;
        let f = analyze(&snap);
        let hit = f.iter().find(|x| x.code == "STORAGE_READ_FAILED").unwrap();
        assert_eq!(hit.severity, Severity::High);
        assert!(hit.evidence[0].contains("Input/output error"));

        // Nothing read at all: permission, not hardware.
        snap.throughput[0].bytes_read = 0;
        snap.throughput[0].error = Some("Permission denied (os error 13)".into());
        assert!(!codes(&analyze(&snap)).contains(&"STORAGE_READ_FAILED"));
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

    /// Runtime PM accounting turns the reset heuristic into a measurement. The
    /// real Goodix reader: 21 resets, suspended 99.8% of 12.3 h, control=auto.
    #[test]
    fn autosuspend_churn_explains_a_reset_storm_as_measured() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb3", 480.0);
        let dev = with_runtime_pm(
            device("3-4", " 2.00", 12.0, Some("usb3")),
            "auto",
            0.998,
            2000,
        );
        hub.children.push(dev);
        snap.buses.push(hub);
        snap.kernel_log = reset_log("3-4", 21);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "DEVICE_RESET_STORM")
            .unwrap();
        assert_eq!(hit.severity, Severity::Low);
        assert_eq!(
            hit.confidence,
            Confidence::Measured,
            "the accounting proves the cause rather than guessing it"
        );
        assert!(hit.detail.contains("99.8%"));
        assert!(hit.detail.contains("2000 ms"));
        assert!(hit.evidence.iter().any(|e| e.contains("control=auto")));
    }

    /// The genuinely suspicious case: power management is ruled out by the same
    /// accounting, so frequent resets stay a real problem.
    #[test]
    fn resets_without_autosuspend_remain_suspicious() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb3", 480.0);
        // control=on and never suspended, like the Bluetooth radio.
        let dev = with_runtime_pm(
            device("3-9", " 2.10", 480.0, Some("usb3")),
            "on",
            0.0,
            2000,
        );
        hub.children.push(dev);
        snap.buses.push(hub);
        snap.kernel_log = reset_log("3-9", 21);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "DEVICE_RESET_STORM")
            .unwrap();
        assert_eq!(hit.severity, Severity::High);
        assert!(hit.detail.contains("marginal connection"));
    }

    /// An internal device that also churns should report the churn, which is the
    /// specific measured cause, rather than the generic internal-device note.
    #[test]
    fn measured_churn_outranks_the_internal_device_heuristic() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb3", 480.0);
        let mut dev = with_runtime_pm(
            device("3-4", " 2.00", 12.0, Some("usb3")),
            "auto",
            0.998,
            2000,
        );
        dev.removable = Some("fixed".into());
        hub.children.push(dev);
        snap.buses.push(hub);
        snap.kernel_log = reset_log("3-4", 21);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "DEVICE_RESET_STORM")
            .unwrap();
        assert_eq!(hit.confidence, Confidence::Measured);
        assert!(hit.detail.contains("Runtime power management accounts for this"));
    }

    #[test]
    fn suspend_ratio_handles_missing_and_zero_values() {
        let d = device("3-1", " 2.00", 480.0, None);
        assert!(d.suspend_ratio().is_none(), "no accounting read");
        assert!(!d.autosuspend_churn());

        let mut d = with_runtime_pm(device("3-1", " 2.00", 480.0, None), "auto", 0.5, 2000);
        assert!((d.suspend_ratio().unwrap() - 0.5).abs() < 0.01);
        assert!(!d.autosuspend_churn(), "50% is not churn");

        d.connected_duration_ms = Some(0);
        assert!(d.suspend_ratio().is_none(), "must not divide by zero");
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

    /// A socket outlives its occupants. Errors logged by a hub that has since
    /// been unplugged must not be charged to whatever is in that socket now.
    ///
    /// Live false positive this reproduces: a Pixel plugged into the socket a
    /// defective hub had occupied was reported as having a defective cable,
    /// using errors from 20 minutes before it was attached.
    #[test]
    fn errors_predating_the_current_occupant_are_not_charged_to_it() {
        let mut snap = empty_snapshot();
        snap.uptime_s = Some(64_095.0);

        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        // Attached 82 s ago, long after the errors below.
        let mut phone = device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x06);
        phone.connected_duration_ms = Some(82_000);
        phone.product = Some("Pixel 9 Pro XL".into());
        slow.children.push(phone);
        snap.buses.push(slow);
        snap.buses.push(fast);

        // The previous occupant's failures, ~1200 s before the phone arrived.
        let mut events = ss_uplink_failure_events(6, 85, true);
        for (i, e) in events.iter_mut().enumerate() {
            e.monotonic_s = Some(62_700.0 + i as f64);
        }
        snap.kernel_log.events = events;

        let f = analyze(&snap);
        assert!(
            !codes(&f).contains(&"SS_HALF_FAILED"),
            "stale errors must not implicate the current device: {:?}",
            codes(&f)
        );
        assert!(!codes(&f).contains(&"KERNEL_BLAMED_CABLE"));
        assert!(!codes(&f).contains(&"DEVICE_FAILED_TO_ENUMERATE"));
    }

    /// The same evidence, but the device was already there when it was logged —
    /// so it genuinely is about this device and must still be reported.
    #[test]
    fn errors_after_the_device_attached_are_still_reported() {
        let mut snap = empty_snapshot();
        snap.uptime_s = Some(64_095.0);

        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        // Attached two hours ago, well before the errors.
        let mut hub = device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x09);
        hub.connected_duration_ms = Some(7_200_000);
        slow.children.push(hub);
        snap.buses.push(slow);
        snap.buses.push(fast);

        let mut events = ss_uplink_failure_events(6, 85, true);
        for (i, e) in events.iter_mut().enumerate() {
            e.monotonic_s = Some(62_700.0 + i as f64);
        }
        snap.kernel_log.events = events;

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "SS_HALF_FAILED")
            .expect("errors during this device's attachment are its own");
        assert!(hit.title.contains("likely defective"));
    }

    /// Without a time base, evidence is kept rather than silently discarded —
    /// losing a real fault is worse than an occasional stale one.
    #[test]
    fn missing_timestamps_keep_the_evidence() {
        let mut snap = empty_snapshot();
        snap.uptime_s = None;
        let (mut slow, fast) = receptacle("0x80000001", Some("5-1"), None);
        slow.children
            .push(device_with_class("5-1", " 2.10", 480.0, Some("usb5"), 0x09));
        snap.buses.push(slow);
        snap.buses.push(fast);
        snap.kernel_log.events = ss_uplink_failure_events(6, 85, true);
        assert!(codes(&analyze(&snap)).contains(&"SS_HALF_FAILED"));
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

    /// Real hardware, 2026-08-01. A hub at `6-1.2` was unplugged at monotonic
    /// 62778 s after failing to enumerate. A webcam took the same socket at
    /// 65598 s. At uptime 65700 s the tool still reported the hub's failures as
    /// a High finding — accusing hardware that had been gone for 47 minutes.
    ///
    /// The receptacle's USB 2.0 companion was empty, so the existing staleness
    /// filter had nothing to date against. The phantom's own parent did: `6-1`
    /// is in the tree and attached long after the events.
    #[test]
    fn a_phantom_is_not_charged_to_the_device_now_above_it() {
        let mut snap = empty_snapshot();
        snap.uptime_s = Some(65_700.0);

        let mut bus = root_hub("usb6", 10_000.0);
        // The webcam arrived 102 s ago, i.e. at 65598 s.
        bus.children.push(attached_ago(
            device("6-1", " 3.20", 5000.0, Some("usb6")),
            102.0,
        ));
        snap.buses.push(bus);
        snap.kernel_log.events = phantom_failure_events("6-1.2", 62_776.0);

        let f = analyze(&snap);
        assert!(
            !codes(&f).contains(&"DEVICE_FAILED_TO_ENUMERATE"),
            "events predating the current occupant of 6-1 are not about it: {:?}",
            codes(&f)
        );
    }

    /// The other half of the same rule: a phantom whose failures happened after
    /// its parent arrived is genuinely current, and must still be reported.
    #[test]
    fn a_phantom_below_a_device_that_was_already_there_is_reported() {
        let mut snap = empty_snapshot();
        snap.uptime_s = Some(65_700.0);

        let mut bus = root_hub("usb6", 10_000.0);
        // The hub has been there for an hour; the failures are two minutes old.
        bus.children.push(attached_ago(
            device("6-1", " 3.20", 5000.0, Some("usb6")),
            3600.0,
        ));
        snap.buses.push(bus);
        snap.kernel_log.events = phantom_failure_events("6-1.2", 65_580.0);

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "DEVICE_FAILED_TO_ENUMERATE")
            .expect("6-1.2 failed while 6-1 was already attached");
        assert_eq!(hit.severity, Severity::High);
        assert!(hit.evidence.iter().any(|e| e.contains("EPROTO")));
    }

    // --- LINK_ERROR_RATE ---------------------------------------------------

    fn snapshot_with_traffic(errors: u64, completions: u64) -> Snapshot {
        let mut snap = empty_snapshot();
        let mut bus = root_hub("usb2", 10_000.0);
        bus.children
            .push(device_at("2-1", 5000.0, Some("usb2"), 2, 4));
        snap.buses.push(bus);
        snap.urb_traffic = Some(urb_traffic(
            5000,
            vec![urb_stats(2, 4, completions, errors)],
        ));
        snap
    }

    /// The point of the probe: a link that negotiated cleanly and is dropping
    /// packets under load. Nothing passive can see this.
    #[test]
    fn reports_a_measured_transport_error_rate() {
        let snap = snapshot_with_traffic(40, 1000);
        let hit = analyze(&snap)
            .into_iter()
            .find(|f| f.code == "LINK_ERROR_RATE")
            .expect("4% of transfers failing");
        assert_eq!(hit.severity, Severity::High, "4% is not a warning");
        assert_eq!(hit.confidence, Confidence::Measured);
        assert_eq!(hit.subject, Subject::Device("2-1".into()));
        assert!(hit.evidence.iter().any(|e| e.contains("EPROTO")));
        // The advice must be the cheapest test first.
        assert!(hit.suggestion.as_ref().unwrap().contains("cable"));
    }

    #[test]
    fn a_low_but_real_error_rate_is_medium_not_high() {
        let snap = snapshot_with_traffic(5, 1000);
        let hit = analyze(&snap)
            .into_iter()
            .find(|f| f.code == "LINK_ERROR_RATE")
            .expect("0.5% is still elevated");
        assert_eq!(hit.severity, Severity::Medium);
    }

    /// A clean window must produce nothing, and so must a window too small to
    /// mean anything — two errors during a resume is not a failing cable.
    #[test]
    fn a_clean_or_tiny_sample_says_nothing() {
        for (errors, completions) in [(0, 5000), (2, 5000), (2, 3)] {
            let snap = snapshot_with_traffic(errors, completions);
            assert!(
                !codes(&analyze(&snap)).contains(&"LINK_ERROR_RATE"),
                "{errors} errors in {completions} completions must not fire"
            );
        }
    }

    /// The false positive the classification exists to prevent. A webcam
    /// stopping its stream cancels URBs in bulk and a device declines
    /// unsupported control requests with a stall; neither touches the wire.
    #[test]
    fn cancellations_and_stalls_never_produce_a_finding() {
        let mut snap = snapshot_with_traffic(0, 2000);
        if let Some(t) = snap.urb_traffic.as_mut() {
            t.devices[0].cancellations = 1500;
            t.devices[0].protocol_errors = 400;
            t.devices[0].by_status.insert(-2, 1500);
            t.devices[0].by_status.insert(-32, 400);
        }
        assert!(!codes(&analyze(&snap)).contains(&"LINK_ERROR_RATE"));
    }

    /// Measured errors strengthen a finding that already claims the link is
    /// underperforming — but must not promote a cable accusation to measured.
    /// The counts are measured; blaming the cable for them is not.
    #[test]
    fn errors_corroborate_without_overclaiming_confidence() {
        let mut snap = empty_snapshot();
        let mut bus = root_hub("usb3", 480.0);
        let mut dev = device_at("3-1", 12.0, Some("usb3"), 3, 7);
        dev = with_runtime_pm(dev, "on", 0.0, 2000);
        bus.children.push(dev);
        snap.buses.push(bus);
        snap.kernel_log = reset_log("3-1", 12);
        snap.urb_traffic = Some(urb_traffic(5000, vec![urb_stats(3, 7, 900, 20)]));

        let f = analyze(&snap);
        let storm = f
            .iter()
            .find(|x| x.code == "DEVICE_RESET_STORM")
            .expect("12 resets with power management ruled out");
        assert!(
            storm.evidence.iter().any(|e| e.contains("corroborated")),
            "{:?}",
            storm.evidence
        );
        // Heuristic -> Inferred, and no further: the errors are measured, but
        // blaming the cable for them is still a deduction.
        assert_eq!(storm.confidence, Confidence::Inferred);

        // Without the traffic the same snapshot yields only a heuristic, which
        // is what makes the upgrade above meaningful rather than incidental.
        let mut bare = snap.clone();
        bare.urb_traffic = None;
        let plain = analyze(&bare)
            .into_iter()
            .find(|x| x.code == "DEVICE_RESET_STORM")
            .unwrap();
        assert_eq!(plain.confidence, Confidence::Heuristic);
        assert!(!plain.evidence.iter().any(|e| e.contains("corroborated")));
    }

    /// With no probe run, nothing may be concluded from its absence.
    #[test]
    fn no_traffic_sampled_means_no_finding_either_way() {
        let mut snap = empty_snapshot();
        snap.buses.push(root_hub("usb2", 10_000.0));
        assert!(snap.urb_traffic.is_none());
        assert!(!codes(&analyze(&snap)).contains(&"LINK_ERROR_RATE"));
    }

    // --- DP_ALT_MODE_NO_OUTPUT ---------------------------------------------

    /// A dock that agreed to carry DisplayPort while the graphics driver sees
    /// nothing on any DisplayPort output. Untested against hardware — no
    /// DP Alt Mode device was available — so the guards below matter more than
    /// this happy path.
    #[test]
    fn reports_dp_alt_mode_that_produced_no_picture() {
        let mut snap = empty_snapshot();
        let mut port = charging_port(100_000, Some(5000), 20_000, 5000);
        if let Some(p) = port.partner.as_mut() {
            p.alt_modes.push(partner_alt_mode(0xff01, true));
        }
        snap.ports.push(port);
        snap.displays = vec![
            connector("eDP-1", "connected", true),
            connector("DP-1", "disconnected", false),
            connector("DP-2", "disconnected", false),
        ];

        let hit = analyze(&snap)
            .into_iter()
            .find(|f| f.code == "DP_ALT_MODE_NO_OUTPUT")
            .expect("DP mode entered, no DP output live");
        assert_eq!(hit.severity, Severity::Medium);
        assert_eq!(hit.confidence, Confidence::Inferred);
        assert!(hit.evidence.iter().any(|e| e.contains("DP-1, DP-2")));
    }

    /// The false positive this rule was written around. Every local Type-C port
    /// on the machine this was built against reports DisplayPort Alt Mode with
    /// `active = yes` — on both ports, permanently, whatever is attached. Only
    /// the partner's copy of the flag means anything.
    #[test]
    fn a_ports_own_alt_mode_list_does_not_accuse_anything() {
        let mut snap = empty_snapshot();
        let mut port = charging_port(100_000, Some(5000), 20_000, 5000);
        // The port claims it; the attached charger reports no modes at all.
        port.alt_modes.push(partner_alt_mode(0xff01, true));
        snap.ports.push(port);
        snap.displays = vec![
            connector("eDP-1", "connected", true),
            connector("DP-1", "disconnected", false),
        ];

        assert!(!codes(&analyze(&snap)).contains(&"DP_ALT_MODE_NO_OUTPUT"));
    }

    /// A picture did come out, so there is nothing to report.
    #[test]
    fn a_live_displayport_output_settles_the_question() {
        let mut snap = empty_snapshot();
        let mut port = charging_port(100_000, Some(5000), 20_000, 5000);
        if let Some(p) = port.partner.as_mut() {
            p.alt_modes.push(partner_alt_mode(0xff01, true));
        }
        snap.ports.push(port);
        snap.displays = vec![connector("DP-1", "connected", true)];

        assert!(!codes(&analyze(&snap)).contains(&"DP_ALT_MODE_NO_OUTPUT"));
    }

    /// With no DisplayPort output on the machine, and with DRM unreadable,
    /// there is nothing to have looked for — silence rather than a guess.
    #[test]
    fn no_displayport_output_and_no_drm_stay_quiet() {
        let mut base = empty_snapshot();
        let mut port = charging_port(100_000, Some(5000), 20_000, 5000);
        if let Some(p) = port.partner.as_mut() {
            p.alt_modes.push(partner_alt_mode(0xff01, true));
        }
        base.ports.push(port);

        let mut hdmi_only = base.clone();
        hdmi_only.displays = vec![connector("HDMI-A-1", "disconnected", false)];
        assert!(!codes(&analyze(&hdmi_only)).contains(&"DP_ALT_MODE_NO_OUTPUT"));

        // displays empty = /sys/class/drm was not readable.
        assert!(!codes(&analyze(&base)).contains(&"DP_ALT_MODE_NO_OUTPUT"));
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
            port: None,
            monotonic_s: None,
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
            port: None,
            monotonic_s: None,
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

    /// A Dell DA20 plugged in with an empty HDMI socket, which is what
    /// produced this on real hardware. It reports a Billboard because it has
    /// nothing to drive, and calling that a Medium fault — then blaming the
    /// cable of a device with no cable — is two wrong statements at once.
    #[test]
    fn an_adapter_with_nothing_attached_is_not_a_fault() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb5", 480.0);
        hub.children.push(billboard_device("5-1.2", Some("usb5")));
        snap.buses.push(hub);
        snap.displays = vec![
            connector("eDP-1", "connected", true),
            connector("DP-1", "disconnected", false),
            connector("HDMI-A-1", "disconnected", false),
        ];

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "BILLBOARD_ALT_MODE_FAILED")
            .unwrap();
        assert_eq!(hit.severity, Severity::Low, "an empty socket is not a fault");
        assert!(
            hit.evidence.iter().any(|e| e.contains("nothing for it to drive")),
            "{:?}",
            hit.evidence
        );
        // The advice must not send someone hunting for a cable that is not there.
        let advice = hit.suggestion.as_deref().unwrap();
        assert!(!advice.contains("cable"), "{advice}");
        assert!(advice.contains("Attach a display"), "{advice}");
    }

    /// With a display connected but not on DisplayPort, the report still means
    /// the mode was tried and failed, and the cable advice is right.
    #[test]
    fn a_billboard_with_a_display_attached_is_still_a_fault() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb5", 480.0);
        hub.children.push(billboard_device("5-1.2", Some("usb5")));
        snap.buses.push(hub);
        snap.displays = vec![
            connector("eDP-1", "connected", true),
            connector("HDMI-A-1", "connected", true),
        ];

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "BILLBOARD_ALT_MODE_FAILED")
            .unwrap();
        assert_eq!(hit.severity, Severity::Medium);
        assert!(hit.suggestion.as_deref().unwrap().contains("cable"));
    }

    /// The Dell DA20 kept reporting a Billboard while driving a 1440p monitor
    /// through DisplayPort Alt Mode. Some adapters expose the interface
    /// permanently rather than only on failure, and a rule that contradicts a
    /// picture on the screen is worse than no rule.
    #[test]
    fn a_billboard_cannot_outrank_a_working_displayport_output() {
        let mut snap = empty_snapshot();
        let mut hub = root_hub("usb5", 480.0);
        hub.children.push(billboard_device("5-1.2", Some("usb5")));
        snap.buses.push(hub);
        snap.displays = vec![
            connector("eDP-1", "connected", true),
            connector("DP-1", "connected", true),
        ];

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "BILLBOARD_ALT_MODE_FAILED")
            .unwrap();
        assert_eq!(hit.severity, Severity::Info);
        assert!(hit.suggestion.is_none(), "there is nothing to suggest");
        assert!(
            hit.evidence.iter().any(|e| e.contains("is working despite")),
            "{:?}",
            hit.evidence
        );

        // A connected-but-dark DisplayPort output is not proof of anything, so
        // the benefit of the doubt does not extend to it.
        snap.displays[1] = connector("DP-1", "connected", false);
        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "BILLBOARD_ALT_MODE_FAILED")
            .unwrap();
        assert_eq!(hit.severity, Severity::Medium);
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

    // --- BATTERY_DRAINING_ON_AC --------------------------------------------

    fn battery(status: &str, power_w: f64, pct: u32) -> Battery {
        Battery {
            name: "BAT0".into(),
            status: Some(status.into()),
            capacity_pct: Some(pct),
            energy_now_wh: Some(19.2),
            energy_full_wh: Some(77.2),
            energy_full_design_wh: Some(86.0),
            power_now_w: Some(power_w),
            voltage_now_v: Some(15.2),
            cycle_count: Some(133),
        }
    }

    /// The observed case: plugged in, "Charging", zero flow, pack losing ground.
    /// The driver reports Charging when the contract exactly covers the load, so
    /// status alone cannot be trusted.
    #[test]
    fn charging_with_zero_flow_on_mains_is_reported() {
        let mut snap = empty_snapshot();
        snap.mains_online = Some(true);
        snap.batteries.push(battery("Charging", 0.0, 25));

        let hit = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "BATTERY_DRAINING_ON_AC")
            .unwrap();
        assert_eq!(hit.severity, Severity::Medium);
        assert_eq!(hit.confidence, Confidence::Measured);
        assert!(hit.detail.contains("not a contradiction"));
    }

    #[test]
    fn discharging_on_mains_is_reported() {
        let mut snap = empty_snapshot();
        snap.mains_online = Some(true);
        snap.batteries.push(battery("Discharging", 12.0, 25));
        assert!(codes(&analyze(&snap)).contains(&"BATTERY_DRAINING_ON_AC"));
    }

    /// On battery power, discharging is simply what a battery does.
    #[test]
    fn discharging_off_mains_is_not_a_finding() {
        let mut snap = empty_snapshot();
        snap.mains_online = Some(false);
        snap.batteries.push(battery("Discharging", 12.0, 25));
        assert!(!codes(&analyze(&snap)).contains(&"BATTERY_DRAINING_ON_AC"));
    }

    #[test]
    fn a_battery_that_is_gaining_is_not_a_finding() {
        let mut snap = empty_snapshot();
        snap.mains_online = Some(true);
        snap.batteries.push(battery("Charging", 20.1, 25));
        assert!(!codes(&analyze(&snap)).contains(&"BATTERY_DRAINING_ON_AC"));
    }

    /// The power-gap finding stops hedging once the battery proves it matters.
    #[test]
    fn the_power_gap_escalates_when_the_battery_is_losing() {
        let mut snap = empty_snapshot();
        snap.mains_online = Some(true);
        snap.ports.push(official_charger_port_65w());

        let quiet = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "PD_SOURCE_BELOW_SINK_CAPABILITY")
            .unwrap();
        assert_eq!(quiet.severity, Severity::Low);
        assert!(quiet.detail.contains("Not a fault"));

        snap.batteries.push(battery("Charging", 0.0, 25));
        let loud = analyze(&snap)
            .into_iter()
            .find(|x| x.code == "PD_SOURCE_BELOW_SINK_CAPABILITY")
            .unwrap();
        assert_eq!(loud.severity, Severity::Medium);
        assert!(loud.detail.contains("running down while plugged in"));
    }

    #[test]
    fn battery_health_and_eta_are_derived_correctly() {
        let b = battery("Charging", 20.0, 25);
        // 77.2 of 86.0 Wh design.
        assert!((b.health_pct().unwrap() - 89.77).abs() < 0.1);
        // (77.2 - 19.2) / 20 W = 2.9 h.
        assert!((b.hours_to_full().unwrap() - 2.9).abs() < 0.05);
        // Not charging -> no ETA.
        let d = battery("Discharging", 20.0, 25);
        assert!(d.hours_to_full().is_none());
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
                port: None,
                monotonic_s: None,
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

//! The bottleneck chain: four stages, and which one is the limit.
//!
//! A chain is the shape of the question people actually ask — *where does the
//! power (or the speed) stop?* — laid out as a sequence of ceilings with the
//! achieved value at the end. It exists here rather than in a front end for two
//! reasons.
//!
//! **The marked stage comes from the findings, never from arithmetic here.** A
//! stage is highlighted because a [`Finding`] points at it, and the mapping from
//! code to stage is the whole of that logic (see [`marks`]). Re-deriving "the
//! cable is the limit" by comparing numbers would be a second rule engine, and
//! it would drift from the first one the moment either changed.
//!
//! **A stage with no number is the normal case.** On a UCSI platform the cable
//! stage is unknowable, and `bcdUSB` names a specification rather than a rate,
//! so the device stage is usually ambiguous too. Both are modelled as
//! [`Stage::watts`] / [`Stage::mbps`] of `None`, which a renderer must draw as
//! *unknown* — not as zero, and not by silently dropping the stage. Half the
//! value of the picture is showing where the tool cannot see.

use crate::model::{Finding, Severity, Snapshot, Subject, TypecPort, UsbDevice};

/// Which chain this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainKind {
    /// charger offers → cable carries → contract → this machine accepts
    Power,
    /// device claims → cable carries → path allows → link reached
    Data,
}

impl ChainKind {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Power => "the power chain",
            Self::Data => "the data chain",
        }
    }
}

/// Where a stage sits in its chain. Findings are attached by this, not by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKey {
    /// What the other end is willing to give (power), or claims to do (data).
    Offer,
    /// What the cable between them can carry.
    Cable,
    /// What was agreed (power), or what the topology permits (data).
    Agreed,
    /// What this end can take (power), or what was reached (data).
    Achieved,
}

/// One ceiling in the chain.
#[derive(Debug, Clone)]
pub struct Stage {
    pub key: StageKey,
    /// Short caption, e.g. `charger offers`.
    pub cap: String,
    /// The headline figure, already formatted — `100 W`, `5 Gbps`, `unknown`.
    pub value: String,
    /// The reading behind it, e.g. `20 V at 5 A · PD 3.0`.
    pub sub: String,
    /// Magnitude in mW, for a power chain. `None` means genuinely unknown.
    pub watts_mw: Option<u32>,
    /// Magnitude in Mbps, for a data chain. `None` means genuinely unknown.
    pub mbps: Option<f64>,
    /// Code of the finding that names this stage as the limit.
    pub marked_by: Option<String>,
}

impl Stage {
    /// This stage's magnitude as a fraction of the chain's widest known stage,
    /// or `None` when the stage has no number.
    ///
    /// Deliberately linear. A 480 Mbps stage beside a 10 Gbps one is a 5 %
    /// sliver, which reads as a rendering fault — but it is not one, and a log
    /// scale would flatter a USB 2 fallback into looking survivable.
    pub fn fraction(&self, max: f64) -> Option<f64> {
        if max <= 0.0 {
            return None;
        }
        let v = match (self.watts_mw, self.mbps) {
            (Some(mw), _) => mw as f64,
            (_, Some(m)) => m,
            _ => return None,
        };
        Some((v / max).clamp(0.0, 1.0))
    }
}

/// Four stages and the finding, if any, that names one of them as the limit.
#[derive(Debug, Clone)]
pub struct Chain {
    pub kind: ChainKind,
    pub subject: Subject,
    pub stages: Vec<Stage>,
    /// Code of the finding that marked a stage. `None` when nothing is marked,
    /// which is the healthy case and not the same as "no bottleneck exists".
    pub limited_by: Option<String>,
}

impl Chain {
    /// The widest known stage, for scaling the bars. Zero when nothing is known.
    pub fn max(&self) -> f64 {
        self.stages
            .iter()
            .filter_map(|s| match (s.watts_mw, s.mbps) {
                (Some(mw), _) => Some(mw as f64),
                (_, Some(m)) => Some(m),
                _ => None,
            })
            .fold(0.0, f64::max)
    }

    /// How many stages carry no figure at all.
    pub fn unknown_stages(&self) -> usize {
        self.stages
            .iter()
            .filter(|s| s.watts_mw.is_none() && s.mbps.is_none())
            .count()
    }
}

// ---------------------------------------------------------------------------
// Which finding marks which stage
// ---------------------------------------------------------------------------

/// The code → stage table. This is the only place a chain decides anything.
///
/// Codes absent from this table never mark a stage, so a new rule shows up in
/// the findings list without silently rearranging the picture. Info-severity
/// findings are excluded by [`marks`]: an Info note is *worth knowing*, and
/// pointing the "▲ the limit" marker at one would turn it into an accusation.
fn stage_for(code: &str) -> Option<(ChainKind, StageKey)> {
    use ChainKind::{Data, Power};
    use StageKey::{Achieved, Agreed, Cable, Offer};
    Some(match code {
        // --- power ---
        "CABLE_CURRENT_LIMIT" | "CABLE_VOLTAGE_EXCEEDED" => (Power, Cable),
        "PD_CONTRACT_BELOW_OFFER" | "PD_NO_CONTRACT" => (Power, Agreed),
        "PARTNER_NO_PD" | "SINK_UNDERPOWERED_NO_PD" | "PD_SOURCE_BELOW_SINK_CAPABILITY" => {
            (Power, Offer)
        }
        // --- data ---
        "CABLE_DATA_LIMIT" => (Data, Cable),
        "LINK_BELOW_DEVICE_CAPABILITY"
        | "LINK_SLOW_DESPITE_CAPABLE_CABLE"
        | "LINK_SINGLE_LANE"
        | "USB4_LINK_BELOW_CAPABILITY"
        | "SS_HALF_IDLE"
        | "SS_HALF_FAILED" => (Data, Achieved),
        _ => return None,
    })
}

/// The finding that marks `key` in a chain of `kind` about `subject`, if any.
///
/// A power chain belongs to a port, and a port's cable is a separate subject —
/// so both [`Subject::Port`] and [`Subject::Cable`] of the same name count.
fn marks<'a>(
    findings: &'a [Finding],
    kind: ChainKind,
    subject: &Subject,
    key: StageKey,
) -> Option<&'a Finding> {
    findings
        .iter()
        .filter(|f| f.severity >= Severity::Low)
        .filter(|f| same_thing(&f.subject, subject))
        .find(|f| stage_for(&f.code) == Some((kind, key)))
}

/// Two subjects naming the same physical place.
fn same_thing(a: &Subject, b: &Subject) -> bool {
    match (a, b) {
        (Subject::Port(x) | Subject::Cable(x), Subject::Port(y) | Subject::Cable(y)) => x == y,
        (Subject::Device(x), Subject::Device(y)) => x == y,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The power chain
// ---------------------------------------------------------------------------

/// The power chain for a Type-C port, or `None` when nothing is attached.
///
/// Only built while the port is *sinking*: the stages are worded from the point
/// of view of power arriving, and a port feeding a phone is a different picture
/// that the model cannot currently fill in (the UCSI source node reports zero
/// while sourcing — see [`TypecPort::is_sourcing`]).
pub fn power(port: &TypecPort, findings: &[Finding]) -> Option<Chain> {
    let partner = port.partner.as_ref()?;
    if port.is_sourcing() {
        return None;
    }
    let subject = Subject::Port(port.name.clone());
    let mark = |k: StageKey| marks(findings, ChainKind::Power, &subject, k).map(|f| f.code.clone());

    let contract = port.power_supply.as_ref();
    let contract_mw = contract.and_then(|ps| ps.contract_power_mw());
    let contract_mv = contract.and_then(|ps| ps.contract_voltage_mv());

    let stages = vec![
        offer_stage(port, partner, mark(StageKey::Offer)),
        cable_power_stage(port, contract_mv, mark(StageKey::Cable)),
        contract_stage(port, contract_mw, mark(StageKey::Agreed)),
        sink_stage(port, mark(StageKey::Achieved)),
    ];
    Some(finish(ChainKind::Power, subject, stages))
}

fn offer_stage(port: &TypecPort, partner: &crate::model::Partner, marked: Option<String>) -> Stage {
    let pd = partner.pd.as_ref();
    let mw = pd.and_then(|p| p.max_source_power_mw());
    let best = pd.and_then(|p| {
        p.source_capabilities
            .iter()
            .max_by_key(|c| c.power_mw().unwrap_or(0))
            .map(|c| c.describe())
    });
    // Without PD there is still a ceiling: the CC resistors advertise one.
    let typec_mw = port.typec_advertised_ceiling_mw();
    let (value, sub, watts_mw) = match (mw, typec_mw) {
        (Some(mw), _) => (
            crate::diag::watts(mw),
            best.unwrap_or_else(|| "PD source".into()),
            Some(mw),
        ),
        (None, Some(mw)) => (
            crate::diag::watts(mw),
            format!(
                "no PD — Type-C advertisement only ({})",
                port.power_operation_mode.as_deref().unwrap_or("default")
            ),
            Some(mw),
        ),
        (None, None) => (
            "unknown".into(),
            "the attached device did not say".into(),
            None,
        ),
    };
    Stage {
        key: StageKey::Offer,
        cap: "charger offers".into(),
        value,
        sub,
        watts_mw,
        mbps: None,
        marked_by: marked,
    }
}

/// What the cable can carry, in watts at the voltage actually in use.
///
/// The rating is a current, but the chain compares powers, so it is multiplied
/// by the contract voltage. That is the honest conversion: a 3 A cable is only
/// a 60 W cable *at 20 V*, and saying "3 A" beside "100 W" invites the reader
/// to do the multiplication wrong.
fn cable_power_stage(port: &TypecPort, contract_mv: Option<u32>, marked: Option<String>) -> Stage {
    let emarker = port
        .cable
        .as_ref()
        .and_then(|c| c.identity.as_ref())
        .and_then(|id| id.decoded.cable_current_ma);
    let inferred = port
        .power_supply
        .as_ref()
        .filter(|ps| ps.contract_requires_5a_cable())
        .and(Some(5000));

    let (ma, source) = match (emarker, inferred) {
        (Some(ma), _) => (Some(ma), "e-marker read"),
        (None, Some(ma)) => (Some(ma), "inferred — no e-marker read"),
        (None, None) => (None, "no e-marker reported"),
    };

    let watts_mw = match (ma, contract_mv) {
        (Some(ma), Some(mv)) => Some(((mv as u64 * ma as u64) / 1000) as u32),
        _ => None,
    };
    let sub = match (watts_mw, contract_mv) {
        (Some(mw), Some(mv)) => format!("{source} · {} at {}", crate::diag::watts(mw), crate::diag::volts(mv)),
        _ => source.to_string(),
    };
    Stage {
        key: StageKey::Cable,
        cap: "cable carries".into(),
        value: ma.map(crate::diag::milliamps).unwrap_or_else(|| "unknown".into()),
        sub,
        watts_mw,
        mbps: None,
        marked_by: marked,
    }
}

fn contract_stage(port: &TypecPort, mw: Option<u32>, marked: Option<String>) -> Stage {
    let ps = port.power_supply.as_ref();
    let rev = port
        .partner
        .as_ref()
        .and_then(|p| p.pd_revision_display())
        .map(|r| format!(" · PD {r}"))
        .unwrap_or_default();
    let sub = match (
        ps.and_then(|p| p.contract_voltage_mv()),
        ps.and_then(|p| p.contract_current_ma()),
    ) {
        (Some(v), Some(i)) => format!(
            "{} at {}{rev}",
            crate::diag::volts(v),
            crate::diag::milliamps(i)
        ),
        _ => format!(
            "{}{rev}",
            port.power_operation_mode.as_deref().unwrap_or("no contract")
        ),
    };
    Stage {
        key: StageKey::Agreed,
        cap: "contract".into(),
        value: mw.map(crate::diag::watts).unwrap_or_else(|| "none".into()),
        sub,
        watts_mw: mw,
        mbps: None,
        marked_by: marked,
    }
}

fn sink_stage(port: &TypecPort, marked: Option<String>) -> Stage {
    let pd = port.local_pd.as_ref();
    let mw = pd.and_then(|p| p.max_sink_power_mw());
    let best = pd.and_then(|p| {
        p.sink_capabilities
            .iter()
            .max_by_key(|c| c.power_mw().unwrap_or(0))
            .map(|c| c.describe())
    });
    Stage {
        key: StageKey::Achieved,
        cap: "this machine accepts".into(),
        value: mw.map(crate::diag::watts).unwrap_or_else(|| "unknown".into()),
        sub: best.unwrap_or_else(|| "no sink capabilities reported".into()),
        watts_mw: mw,
        mbps: None,
        marked_by: marked,
    }
}

// ---------------------------------------------------------------------------
// The data chain
// ---------------------------------------------------------------------------

/// The data chain for one USB device. `None` for a root hub, which has no
/// upstream cable and no claim of its own worth drawing.
pub fn data(snap: &Snapshot, dev: &UsbDevice, findings: &[Finding]) -> Option<Chain> {
    if dev.is_root_hub {
        return None;
    }
    let subject = Subject::Device(dev.sysfs_name.clone());
    let mark = |k: StageKey| marks(findings, ChainKind::Data, &subject, k).map(|f| f.code.clone());

    let stages = vec![
        claim_stage(dev, mark(StageKey::Offer)),
        cable_data_stage(dev, mark(StageKey::Cable)),
        path_stage(snap, dev, mark(StageKey::Agreed)),
        link_stage(dev, mark(StageKey::Achieved)),
    ];
    Some(finish(ChainKind::Data, subject, stages))
}

/// What the device says it is, which is usually not a rate.
///
/// `bcdUSB` names a *specification*, not a speed: `2.00` is claimed by 12 Mbps
/// and 480 Mbps devices alike, and `3.10` by both Gen 1 (5 Gbps) and Gen 2
/// (10 Gbps) hardware — a VIA hub on this machine declares `3.10` and links at
/// 5 Gbps into a 10 Gbps port. Only `3.0x` maps to one rate, so only `3.0x`
/// gets a bar; everything else shows the claim and no figure.
fn claim_stage(dev: &UsbDevice, marked: Option<String>) -> Stage {
    let version = dev.usb_version.clone();
    let mbps = match dev.usb_version_num {
        Some(v) if (3.0..3.1).contains(&v) => Some(5_000.0),
        _ => None,
    };
    let (value, sub) = match (&version, mbps) {
        (Some(v), Some(_)) => (format!("USB {v}"), "5 Gbps — the only rate 3.0 names".into()),
        (Some(v), None) => (
            format!("USB {v}"),
            format!("a specification, not a rate — {v} is claimed at more than one speed"),
        ),
        (None, _) => ("unknown".into(), "no version descriptor".into()),
    };
    Stage {
        key: StageKey::Offer,
        cap: "device claims".into(),
        value,
        sub,
        watts_mw: None,
        mbps,
        marked_by: marked,
    }
}

/// The cable's data rating — knowable only from a Type-C e-marker, which is to
/// say almost never on this class of hardware.
fn cable_data_stage(dev: &UsbDevice, marked: Option<String>) -> Stage {
    let _ = dev;
    Stage {
        key: StageKey::Cable,
        cap: "cable carries".into(),
        value: "unknown".into(),
        sub: "no e-marker is readable for a USB cable outside Type-C".into(),
        watts_mw: None,
        mbps: None,
        marked_by: marked,
    }
}

/// The narrowest hop between this device and its root hub.
///
/// This is the one genuinely measured ceiling in the data chain: every link on
/// the way up negotiated a rate, and the device cannot exceed the slowest of
/// them. A USB 3 drive behind a USB 2 hub is exactly this stage.
fn path_stage(snap: &Snapshot, dev: &UsbDevice, marked: Option<String>) -> Stage {
    let mut narrowest: Option<(f64, String)> = None;
    let mut cur = dev.parent.clone();
    while let Some(name) = cur {
        let Some(up) = snap.device(&name) else { break };
        if let Some(sp) = &up.speed {
            if narrowest.as_ref().is_none_or(|(m, _)| sp.mbps < *m) {
                narrowest = Some((sp.mbps, up.sysfs_name.clone()));
            }
        }
        cur = up.parent.clone();
    }
    match narrowest {
        Some((mbps, via)) => Stage {
            key: StageKey::Agreed,
            cap: "path allows".into(),
            value: rate(mbps),
            sub: format!("narrowest hop upstream is {via}"),
            watts_mw: None,
            mbps: Some(mbps),
            marked_by: marked,
        },
        None => Stage {
            key: StageKey::Agreed,
            cap: "path allows".into(),
            value: "unknown".into(),
            sub: "no upstream link rate reported".into(),
            watts_mw: None,
            mbps: None,
            marked_by: marked,
        },
    }
}

fn link_stage(dev: &UsbDevice, marked: Option<String>) -> Stage {
    match &dev.speed {
        Some(sp) => Stage {
            key: StageKey::Achieved,
            cap: "link reached".into(),
            value: rate(sp.mbps),
            sub: sp.label.clone(),
            watts_mw: None,
            mbps: Some(sp.mbps),
            marked_by: marked,
        },
        None => Stage {
            key: StageKey::Achieved,
            cap: "link reached".into(),
            value: "unknown".into(),
            sub: "the device never enumerated".into(),
            watts_mw: None,
            mbps: None,
            marked_by: marked,
        },
    }
}

/// `480 Mbps`, `5 Gbps`, `10 Gbps`.
pub fn rate(mbps: f64) -> String {
    if mbps >= 1000.0 {
        let g = mbps / 1000.0;
        if g.fract().abs() < 0.05 {
            format!("{g:.0} Gbps")
        } else {
            format!("{g:.1} Gbps")
        }
    } else if mbps.fract().abs() < 0.05 {
        format!("{mbps:.0} Mbps")
    } else {
        format!("{mbps:.1} Mbps")
    }
}

/// Collect the marked code, keeping the earliest stage when more than one fired
/// — the first constriction is the one that matters, and the ones after it are
/// consequences of it.
fn finish(kind: ChainKind, subject: Subject, stages: Vec<Stage>) -> Chain {
    let limited_by = stages.iter().find_map(|s| s.marked_by.clone());
    Chain {
        kind,
        subject,
        stages,
        limited_by,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Confidence;
    use crate::test_support as ts;

    fn finding(code: &str, severity: Severity, subject: Subject) -> Finding {
        Finding {
            code: code.into(),
            severity,
            confidence: Confidence::Measured,
            subject,
            title: code.into(),
            detail: String::new(),
            evidence: Vec::new(),
            suggestion: None,
        }
    }

    #[test]
    fn a_power_chain_has_four_stages_in_order() {
        let port = ts::laptop_charger_port_100w();
        let c = power(&port, &[]).expect("attached");
        let keys: Vec<StageKey> = c.stages.iter().map(|s| s.key).collect();
        assert_eq!(
            keys,
            vec![
                StageKey::Offer,
                StageKey::Cable,
                StageKey::Agreed,
                StageKey::Achieved
            ]
        );
    }

    #[test]
    fn a_full_contract_marks_nothing() {
        let port = ts::laptop_charger_port_100w();
        let c = power(&port, &[]).expect("attached");
        assert_eq!(c.limited_by, None);
        assert!(c.stages.iter().all(|s| s.marked_by.is_none()));
    }

    /// The marker must come from a finding, so a chain built without findings
    /// can never accuse anything however lopsided the numbers are.
    #[test]
    fn a_cable_finding_marks_the_cable_stage() {
        let port = ts::laptop_charger_port_100w();
        let f = finding(
            "CABLE_CURRENT_LIMIT",
            Severity::Medium,
            Subject::Cable(port.name.clone()),
        );
        let c = power(&port, std::slice::from_ref(&f)).expect("attached");
        assert_eq!(c.limited_by.as_deref(), Some("CABLE_CURRENT_LIMIT"));
        let cable = c.stages.iter().find(|s| s.key == StageKey::Cable).unwrap();
        assert_eq!(cable.marked_by.as_deref(), Some("CABLE_CURRENT_LIMIT"));
    }

    /// An Info finding is a note, not an accusation, and must not aim the
    /// marker at anything.
    #[test]
    fn an_info_finding_never_marks_a_stage() {
        let port = ts::laptop_charger_port_100w();
        let f = finding(
            "CABLE_CURRENT_LIMIT",
            Severity::Info,
            Subject::Cable(port.name.clone()),
        );
        let c = power(&port, &[f]).expect("attached");
        assert_eq!(c.limited_by, None);
    }

    /// A finding about a different port must not reach into this chain.
    #[test]
    fn a_finding_about_another_port_is_ignored() {
        let port = ts::laptop_charger_port_100w();
        let f = finding(
            "CABLE_CURRENT_LIMIT",
            Severity::High,
            Subject::Cable("port9".into()),
        );
        let c = power(&port, &[f]).expect("attached");
        assert_eq!(c.limited_by, None);
    }

    /// A code with no entry in the table is still a finding — it just does not
    /// rearrange the picture.
    #[test]
    fn an_unmapped_code_marks_nothing() {
        let port = ts::laptop_charger_port_100w();
        let f = finding(
            "SOMETHING_NEW",
            Severity::Critical,
            Subject::Port(port.name.clone()),
        );
        let c = power(&port, &[f]).expect("attached");
        assert_eq!(c.limited_by, None);
    }

    #[test]
    fn an_idle_port_has_no_power_chain() {
        assert!(power(&ts::idle_port(), &[]).is_none());
    }

    #[test]
    fn a_sourcing_port_has_no_power_chain() {
        assert!(power(&ts::sourcing_port_non_pd(), &[]).is_none());
    }

    /// The cable's rating is a current; the chain compares powers. 3 A at 20 V
    /// has to come out as 60 W or the bars lie.
    #[test]
    fn the_cable_stage_converts_amps_to_watts_at_the_contract_voltage() {
        // 20 V at 5 A contract, so the >3 A inference gives a 5 A cable.
        let port = ts::charging_port(100_000, Some(5000), 20_000, 5000);
        let c = power(&port, &[]).expect("attached");
        let cable = c.stages.iter().find(|s| s.key == StageKey::Cable).unwrap();
        assert_eq!(cable.watts_mw, Some(100_000));
        assert_eq!(cable.value, "5 A");
    }

    /// A 3 A contract proves nothing about the cable — 3 A is what an unmarked
    /// cable is assumed to carry — so with no e-marker the stage must stay
    /// blank rather than guessing.
    #[test]
    fn a_three_amp_contract_leaves_the_cable_unknown() {
        let port = ts::charging_port(60_000, None, 20_000, 3000);
        let c = power(&port, &[]).expect("attached");
        let cable = c.stages.iter().find(|s| s.key == StageKey::Cable).unwrap();
        assert_eq!(cable.watts_mw, None);
        assert_eq!(cable.value, "unknown");
        assert_eq!(c.unknown_stages(), 1);
    }

    /// The fault case the mockups draw: a 100 W charger behind a 3 A e-marked
    /// cable. The cable stage must read 60 W, well under the 100 W offer.
    #[test]
    fn an_emarked_three_amp_cable_caps_a_hundred_watt_charger() {
        let port = ts::charging_port(100_000, Some(3000), 20_000, 3000);
        let c = power(&port, &[]).expect("attached");
        let cable = c.stages.iter().find(|s| s.key == StageKey::Cable).unwrap();
        assert_eq!(cable.watts_mw, Some(60_000));
        assert!(cable.sub.starts_with("e-marker read"), "{}", cable.sub);
        assert!(cable.fraction(c.max()).unwrap() < 0.7);
    }

    #[test]
    fn bars_are_scaled_against_the_widest_known_stage() {
        let port = ts::laptop_charger_port_100w();
        let c = power(&port, &[]).expect("attached");
        let max = c.max();
        assert!(max > 0.0);
        for s in &c.stages {
            if let Some(f) = s.fraction(max) {
                assert!((0.0..=1.0).contains(&f), "{} -> {f}", s.cap);
            }
        }
    }

    // --- data ---

    #[test]
    fn a_root_hub_has_no_data_chain() {
        let mut snap = ts::empty_snapshot();
        snap.buses.push(ts::root_hub("usb1", 5000.0));
        let hub = snap.buses[0].clone();
        assert!(data(&snap, &hub, &[]).is_none());
    }

    /// `3.10` is claimed by 5 Gbps and 10 Gbps devices both, so it cannot be
    /// turned into a bar. Verified on this machine: a VIA hub declares 3.10 and
    /// links at 5 Gbps into a 10 Gbps port.
    #[test]
    fn an_ambiguous_bcd_usb_gets_no_bar() {
        let mut snap = ts::empty_snapshot();
        let mut hub = ts::root_hub("usb6", 10_000.0);
        hub.children.push(ts::device("6-1", "3.10", 5000.0, Some("usb6")));
        snap.buses.push(hub);
        let dev = snap.device("6-1").unwrap().clone();
        let c = data(&snap, &dev, &[]).expect("not a root hub");
        let claim = c.stages.iter().find(|s| s.key == StageKey::Offer).unwrap();
        assert_eq!(claim.mbps, None);
        assert_eq!(claim.value, "USB 3.10");
    }

    #[test]
    fn a_usb_30_claim_is_worth_five_gigabits() {
        let mut snap = ts::empty_snapshot();
        let mut hub = ts::root_hub("usb4", 10_000.0);
        hub.children.push(ts::device("4-1", "3.00", 5000.0, Some("usb4")));
        snap.buses.push(hub);
        let dev = snap.device("4-1").unwrap().clone();
        let c = data(&snap, &dev, &[]).expect("not a root hub");
        let claim = c.stages.iter().find(|s| s.key == StageKey::Offer).unwrap();
        assert_eq!(claim.mbps, Some(5_000.0));
    }

    /// The measured stage: a drive behind a slow hub is limited by the hub, and
    /// the chain has to name which hop.
    #[test]
    fn the_path_stage_is_the_narrowest_hop_upstream() {
        let mut snap = ts::empty_snapshot();
        let mut root = ts::root_hub("usb6", 10_000.0);
        let mut hub = ts::device("6-1", "2.10", 480.0, Some("usb6"));
        hub.children
            .push(ts::device("6-1.2", "3.00", 480.0, Some("6-1")));
        root.children.push(hub);
        snap.buses.push(root);
        let dev = snap.device("6-1.2").unwrap().clone();
        let c = data(&snap, &dev, &[]).expect("not a root hub");
        let path = c.stages.iter().find(|s| s.key == StageKey::Agreed).unwrap();
        assert_eq!(path.mbps, Some(480.0));
        assert!(path.sub.contains("6-1"), "{}", path.sub);
    }

    #[test]
    fn a_slow_link_finding_marks_the_link_stage() {
        let mut snap = ts::empty_snapshot();
        let mut root = ts::root_hub("usb4", 10_000.0);
        root.children
            .push(ts::device("4-1", "3.00", 480.0, Some("usb4")));
        snap.buses.push(root);
        let dev = snap.device("4-1").unwrap().clone();
        let f = finding(
            "SS_HALF_IDLE",
            Severity::Medium,
            Subject::Device("4-1".into()),
        );
        let c = data(&snap, &dev, &[f]).expect("not a root hub");
        assert_eq!(c.limited_by.as_deref(), Some("SS_HALF_IDLE"));
        let link = c
            .stages
            .iter()
            .find(|s| s.key == StageKey::Achieved)
            .unwrap();
        assert_eq!(link.marked_by.as_deref(), Some("SS_HALF_IDLE"));
    }

    /// A power code must not mark a data stage even though both chains have a
    /// stage called "cable".
    #[test]
    fn a_power_code_cannot_mark_a_data_stage() {
        let mut snap = ts::empty_snapshot();
        let mut root = ts::root_hub("usb4", 10_000.0);
        root.children
            .push(ts::device("4-1", "3.00", 5000.0, Some("usb4")));
        snap.buses.push(root);
        let dev = snap.device("4-1").unwrap().clone();
        let f = finding(
            "CABLE_CURRENT_LIMIT",
            Severity::High,
            Subject::Device("4-1".into()),
        );
        let c = data(&snap, &dev, &[f]).expect("not a root hub");
        assert_eq!(c.limited_by, None);
    }

    #[test]
    fn rates_read_the_way_people_say_them() {
        assert_eq!(rate(480.0), "480 Mbps");
        assert_eq!(rate(5000.0), "5 Gbps");
        assert_eq!(rate(10_000.0), "10 Gbps");
        assert_eq!(rate(1.5), "1.5 Mbps");
    }
}

//! Terminal rendering.
//!
//! Kept strictly separate from `usb_probe`: the library returns data, this
//! module decides how it looks. A future native UI replaces this file and
//! nothing else.

use std::fmt::Write as _;

use usb_probe::diag::{milliamps, volts, watts};
use usb_probe::model::*;
use usb_probe::kind::{DeviceKind, KindSource};
use usb_probe::usb::class_name;

const WIDTH: usize = 96;

pub struct Theme {
    pub color: bool,
    pub verbose: bool,
}

impl Theme {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }

    fn severity(&self, s: Severity) -> String {
        let label = format!("{:<8}", format!("[{}]", s.label()));
        match s {
            Severity::Critical | Severity::High => self.red(&label),
            Severity::Medium => self.yellow(&label),
            Severity::Low => self.cyan(&label),
            Severity::Info => self.dim(&label),
        }
    }

    fn heading(&self, s: &str) -> String {
        self.bold(&s.to_uppercase())
    }

    /// Colour a link speed by whether it reached SuperSpeed.
    fn speed(&self, s: Option<&LinkSpeed>) -> String {
        match s {
            Some(s) if s.mbps >= 5000.0 => self.green(&s.short()),
            Some(s) if s.mbps >= 480.0 => s.short(),
            Some(s) => self.dim(&s.short()),
            None => "?".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// The answer, before the evidence.
///
/// Findings already list what is wrong. What this adds is the sentence for
/// subjects where *nothing* is wrong, which the tool previously expressed by
/// saying nothing at all — indistinguishable, from the outside, from not having
/// looked.
///
/// So the default view leads with faults and then shows only the clean verdicts
/// that have something to cite. A `Clear` with an empty `because` is real but
/// unquotable, and printing nine of those would bury the four that mean
/// something; they collapse to a count instead, and `-v` shows them all.
pub fn verdicts(out: &mut String, report: &Report, t: &Theme) {
    if report.verdicts.is_empty() {
        return;
    }
    let (shown, silent): (Vec<_>, Vec<_>) = report
        .verdicts
        .iter()
        .partition(|v| t.verbose || v.outcome != Outcome::Clear || !v.because.is_empty());

    if shown.is_empty() && silent.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", t.heading("verdict"));

    let width = shown
        .iter()
        .map(|v| v.subject.display().chars().count())
        .max()
        .unwrap_or(0)
        .clamp(6, 16);

    for v in &shown {
        let glyph = match v.outcome {
            Outcome::Fault => t.red("✗"),
            Outcome::Minor => t.yellow("·"),
            Outcome::Clear => t.green("✓"),
        };
        let subject = v.subject.display();
        let _ = writeln!(
            out,
            "  {glyph} {:width$}  {}",
            t.bold(&subject),
            v.headline,
            // Padding is computed on the unstyled text; the escape codes in
            // `t.bold` are zero-width on screen but not to `format!`.
            width = width + (t.bold(&subject).len() - subject.chars().count())
        );
        if t.verbose && !v.because.is_empty() {
            let _ = writeln!(
                out,
                "  {:width$}  {}",
                "",
                t.dim(&v.because.join(", ")),
                width = width + 2
            );
        }
    }

    if !silent.is_empty() {
        let _ = writeln!(
            out,
            "  {} {} other subject(s) examined, nothing found",
            t.dim(" "),
            silent.len()
        );
    }
    let _ = writeln!(out);
}

/// Statements that a thing is *not* the problem.
///
/// Verbose only. These exist to make a clean verdict citable rather than to be
/// read one by one, and at Info level in a list of faults they would be pure
/// volume — the task that introduced them said so before they were written.
pub fn exonerations(out: &mut String, report: &Report, t: &Theme) {
    if !t.verbose || report.exonerations.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", t.heading("ruled out"));
    for f in &report.exonerations {
        let _ = writeln!(
            out,
            "{} {} {}",
            t.green("✓"),
            t.bold(&f.subject.display()),
            f.title
        );
        let indent = "         ";
        for line in wrap(&f.detail, WIDTH - indent.len()) {
            let _ = writeln!(out, "{indent}{}", t.dim(&line));
        }
        for (i, e) in f.evidence.iter().enumerate() {
            let branch = if i + 1 == f.evidence.len() {
                "└"
            } else {
                "├"
            };
            let _ = writeln!(out, "{indent}{} {}", t.dim(branch), t.dim(e));
        }
        let _ = writeln!(
            out,
            "{indent}{}",
            t.dim(&format!("{} · {}", f.code, f.confidence.label()))
        );
        let _ = writeln!(out);
    }
}

pub fn findings(out: &mut String, report: &Report, t: &Theme) {
    let _ = writeln!(out, "{}", t.heading("findings"));
    if report.findings.is_empty() {
        let _ = writeln!(out, "  {} nothing to report\n", t.green("✓"));
        return;
    }

    for f in &report.findings {
        let _ = writeln!(
            out,
            "{} {} {}",
            t.severity(f.severity),
            t.bold(&f.subject.display()),
            f.title
        );

        let indent = "         ";
        for line in wrap(&f.detail, WIDTH - indent.len()) {
            let _ = writeln!(out, "{indent}{}", t.dim(&line));
        }

        if t.verbose && !f.evidence.is_empty() {
            for (i, e) in f.evidence.iter().enumerate() {
                let branch = if i + 1 == f.evidence.len() {
                    "└"
                } else {
                    "├"
                };
                let _ = writeln!(out, "{indent}{} {}", t.dim(branch), t.dim(e));
            }
        }
        if let Some(s) = &f.suggestion {
            for (i, line) in wrap(s, WIDTH - indent.len() - 2).into_iter().enumerate() {
                let prefix = if i == 0 { "→ " } else { "  " };
                let _ = writeln!(out, "{indent}{}{}", t.cyan(prefix), line);
            }
        }
        let _ = writeln!(out, "{indent}{}", t.dim(&format!("{} · {}", f.code, f.confidence.label())));
        let _ = writeln!(out);
    }
}

// ---------------------------------------------------------------------------
// Type-C ports
// ---------------------------------------------------------------------------

pub fn ports(out: &mut String, snap: &Snapshot, t: &Theme) {
    let _ = writeln!(out, "{}", t.heading("usb-c ports"));
    if snap.ports.is_empty() {
        let _ = writeln!(
            out,
            "  {}\n",
            t.dim("no Type-C ports exposed (no typec driver, or none on this machine)")
        );
        return;
    }

    for p in &snap.ports {
        let loc = p
            .physical_location
            .as_ref()
            .map(|l| format!("  {}", t.dim(&l.display())))
            .unwrap_or_default();
        let _ = writeln!(out, "  {}{}", t.bold(&p.name), loc);

        row(out, t, "roles", &roles_line(p));
        row(out, t, "usb / rev", &revision_line(p));

        match &p.partner {
            Some(pt) => {
                row(out, t, "attached", &partner_line(pt));
                if let Some(id) = &pt.identity {
                    if t.verbose {
                        row(out, t, "", &identity_line(id));
                    }
                }
            }
            None => row(out, t, "attached", &t.dim("nothing")),
        }

        row(out, t, "contract", &contract_line(p, t));
        row(out, t, "cable", &cable_line(p, t));

        if !p.alt_modes.is_empty() {
            row(out, t, "port supports", &alt_modes_line(&p.alt_modes, false));
        }
        if let Some(pt) = &p.partner {
            if !pt.alt_modes.is_empty() {
                row(out, t, "partner modes", &alt_modes_line(&pt.alt_modes, true));
            }
            // What the attached supply can actually deliver, with the profile
            // currently in use marked. Shown unconditionally: when charging is
            // the question, this is the answer.
            if let Some(pd) = &pt.pd {
                if !pd.source_capabilities.is_empty() {
                    row(out, t, "charger max", &supply_headline(pd, t));
                    for line in pdo_lines(&pd.source_capabilities, p.power_supply.as_ref(), t) {
                        row(out, t, "", &line);
                    }
                }
                // What the attached device will accept. Relevant whenever this
                // machine is the one supplying — then *it* is the sink.
                if !pd.sink_capabilities.is_empty() {
                    row(
                        out,
                        t,
                        "device accepts",
                        &format!(
                            "up to {}   {}",
                            pd.max_sink_power_mw()
                                .map(watts)
                                .unwrap_or_else(|| "?".into()),
                            t.dim(&pdo_line(&pd.sink_capabilities))
                        ),
                    );
                }
            }
        }
        // The machine's own appetite, shown whenever something is attached so
        // the two numbers can be compared side by side.
        if let Some(pd) = &p.local_pd {
            if p.is_attached() && !pd.sink_capabilities.is_empty() {
                row(
                    out,
                    t,
                    "this machine",
                    &format!(
                        "accepts up to {}   {}",
                        pd.max_sink_power_mw()
                            .map(watts)
                            .unwrap_or_else(|| "?".into()),
                        t.dim(&pdo_line(&pd.sink_capabilities))
                    ),
                );
            }
            if t.verbose && !pd.source_capabilities.is_empty() {
                row(out, t, "offers", &pdo_line(&pd.source_capabilities));
            }
        }
        let _ = writeln!(out);
    }

    // Socket positions come from ACPI, and firmware gets them wrong. On the
    // machine this was written against, `_PLD` puts one USB-C port on the right
    // panel when both are on the left, and calls the right-hand USB-A
    // receptacle a left-panel one. The *grouping* of ports into receptacles has
    // held up against every test; only the panel names have not, and a wrong
    // one sends someone to look at the wrong side of the laptop.
    if snap.ports.iter().any(|p| p.physical_location.is_some()) {
        let _ = writeln!(
            out,
            "  {}\n",
            t.dim("socket positions come from ACPI _PLD — some firmware names the wrong panel")
        );
    }
}

/// Power Delivery objects the kernel exposed that no Type-C port refers to.
///
/// The capture keeps these so that nothing sysfs offered is silently dropped;
/// this prints them so that promise is real rather than notional. On a normal
/// machine it prints nothing. When it does print, it means one of two things:
/// firmware exposed a PD object for something outside the Type-C class, or this
/// tool failed to follow a port's link to it — and either is worth seeing.
pub fn orphan_pd(out: &mut String, snap: &Snapshot, t: &Theme) {
    if snap.orphan_pd.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", t.heading("unattached power delivery objects"));
    let _ = writeln!(
        out,
        "  {}",
        t.dim("exposed by the kernel but not reachable from any Type-C port")
    );

    for pd in &snap.orphan_pd {
        let _ = writeln!(
            out,
            "  {}{}",
            t.bold(&pd.name),
            pd.revision
                .as_ref()
                .map(|r| t.dim(&format!("  PD {r}")))
                .unwrap_or_default()
        );
        if !pd.source_capabilities.is_empty() {
            row(
                out,
                t,
                "offers",
                &format!(
                    "up to {}   {}",
                    pd.max_source_power_mw()
                        .map(watts)
                        .unwrap_or_else(|| "?".into()),
                    t.dim(&pdo_line(&pd.source_capabilities))
                ),
            );
        }
        if !pd.sink_capabilities.is_empty() {
            row(
                out,
                t,
                "accepts",
                &format!(
                    "up to {}   {}",
                    pd.max_sink_power_mw()
                        .map(watts)
                        .unwrap_or_else(|| "?".into()),
                    t.dim(&pdo_line(&pd.sink_capabilities))
                ),
            );
        }
        if pd.source_capabilities.is_empty() && pd.sink_capabilities.is_empty() {
            row(out, t, "", &t.dim("no capabilities reported"));
        }
    }
    let _ = writeln!(out);
}

fn row(out: &mut String, t: &Theme, label: &str, value: &str) {
    let _ = writeln!(out, "    {} {}", t.dim(&format!("{label:<14}")), value);
}

fn roles_line(p: &TypecPort) -> String {
    let mut parts = Vec::new();
    if let Some(r) = &p.data_role {
        parts.push(format!("data={}", r.display()));
    }
    if let Some(r) = &p.power_role {
        parts.push(format!("power={}", r.display()));
    }
    if let Some(v) = p.vconn_source {
        parts.push(format!("vconn={}", if v { "yes" } else { "no" }));
    }
    if let Some(o) = &p.orientation {
        parts.push(format!("orientation={o}"));
    }
    parts.join("  ")
}

fn revision_line(p: &TypecPort) -> String {
    let cap = p
        .usb_capability
        .as_ref()
        .map(|c| c.raw.clone())
        .unwrap_or_else(|| "?".into());
    let mut s = format!("{cap:<14}");
    if let Some(r) = &p.pd_revision {
        s.push_str(&format!(" PD {r}"));
    }
    if let Some(r) = &p.typec_revision {
        s.push_str(&format!("  Type-C {r}"));
    }
    if !p.supported_accessory_modes.is_empty() {
        s.push_str(&format!(
            "  accessories: {}",
            p.supported_accessory_modes.join(",")
        ));
    }
    s
}

fn partner_line(pt: &Partner) -> String {
    let mut parts = Vec::new();
    if let Some(k) = &pt.kind {
        parts.push(k.clone());
    }
    if let Some(m) = &pt.accessory_mode {
        parts.push(format!("{m} accessory"));
    }
    match pt.supports_pd {
        // Plenty of UCSI firmware reports `0.0` for the partner revision even
        // when PD is active; say that rather than printing a bogus version.
        Some(true) => parts.push(match pt.pd_revision_display() {
            Some(r) => format!("PD {r}"),
            None => "PD (revision not reported)".to_string(),
        }),
        // Worth spelling out: it is the reason the link is stuck at 5 V.
        Some(false) => parts.push("no PD (Type-C current advertisement only)".into()),
        None => {}
    }
    if let Some(id) = &pt.identity {
        if let Some(vid) = id.decoded.vendor_id {
            let mut s = format!("vid {vid:04x}");
            if let Some(pid) = id.decoded.product_id {
                s.push_str(&format!(":{pid:04x}"));
            }
            parts.push(s);
        }
        if let Some(pt_name) = &id.decoded.product_type {
            parts.push(pt_name.clone());
        }
    }
    if parts.is_empty() {
        "attached (no details reported)".into()
    } else {
        parts.join(", ")
    }
}

fn identity_line(id: &Identity) -> String {
    let mut parts = Vec::new();
    if let Some(x) = id.decoded.xid {
        parts.push(format!("XID {x}"));
    }
    if let Some(c) = &id.decoded.partner_device_capability {
        if !c.is_empty() {
            parts.push(c.join("/"));
        }
    }
    if let Some(s) = &id.decoded.partner_max_speed {
        parts.push(s.clone());
    }
    if let Some(v) = id.id_header {
        parts.push(format!("id_header {}", v.hex));
    }
    parts.join(", ")
}

fn contract_line(p: &TypecPort, t: &Theme) -> String {
    let mode = p.power_operation_mode.as_deref().unwrap_or("?");
    let tag = t.dim(&format!("[{mode}]"));
    let Some(ps) = &p.power_supply else {
        return format!("{mode} {}", t.dim("(no power_supply node)"));
    };

    // While this machine is the source, the ucsi-source-psy node describes
    // incoming power — so its online=0 and current_now=0 say nothing about what
    // is flowing out. Report the advertised limit and be explicit that the
    // outgoing draw is not measurable here.
    if p.is_sourcing() {
        // The `*_max` pair is the port's capability range, not what it is
        // offering — the same trap that once turned a 100 W contract into
        // 47 W, still live on this side of the branch. A real reading: a port
        // advertising Type-C `3.0A`, which means 5 V at 3 A, reported
        // voltage_max 13.4 V and current_max 5.72 A. Multiplying those would
        // have claimed 76 W out of a 15 W port.
        //
        // What the port is actually offering is the Type-C advertisement,
        // unless a PD contract has superseded it.
        let limit = if p.pd_contract_active() {
            match (ps.contract_voltage_mv(), ps.contract_current_ma()) {
                (Some(v), Some(i)) => {
                    let mut s = t.bold(&format!("supplying {} at {}", volts(v), milliamps(i)));
                    if let Some(mw) = ps.contract_power_mw() {
                        s.push_str(&format!(" ({})", watts(mw)));
                    }
                    s
                }
                _ => t.bold("supplying under a PD contract, terms not reported"),
            }
        } else if let Some(mw) = p.typec_advertised_ceiling_mw() {
            t.bold(&format!("offering up to {}", watts(mw)))
        } else {
            t.bold("supplying, limit not reported")
        };
        return format!(
            "{limit}  {tag}  {}",
            t.dim("Type-C current advertisement; outgoing draw is not measurable from this node")
        );
    }

    if !ps.is_drawing_power() {
        let why = if p.is_attached() {
            "attached but drawing no power"
        } else {
            "nothing attached"
        };
        return format!("{mode}  {}", t.dim(why));
    }

    // The contract is voltage_now x current_now. The *_max fields are a
    // capability range, not a cap, so they are shown separately and labelled.
    let mut s = t.bold(&format!(
        "{} at {}",
        ps.contract_voltage_mv().map(volts).unwrap_or_else(|| "?V".into()),
        ps.contract_current_ma()
            .map(milliamps)
            .unwrap_or_else(|| "?A".into())
    ));
    if let Some(mw) = ps.contract_power_mw() {
        s.push_str(&format!(" ({})", watts(mw)));
    }
    s.push_str(&format!("  {tag}"));
    if let Some((lo, hi)) = ps.voltage_range_mv() {
        s.push_str(&format!(
            "  {}",
            t.dim(&format!("range {}-{}", volts(lo), volts(hi)))
        ));
    }
    s
}

fn cable_line(p: &TypecPort, t: &Theme) -> String {
    let Some(c) = &p.cable else {
        let Some(partner) = &p.partner else {
            return t.dim("—");
        };
        // Don't imply a concern the rule engine has already dismissed.
        // A >3 A contract is only legal over a 5 A e-marked cable, so in that
        // case the rating is known even though sysfs has no cable node.
        if let Some(ma) = p
            .power_supply
            .as_ref()
            .filter(|ps| ps.contract_requires_5a_cable())
            .and_then(|ps| ps.contract_current_ma())
        {
            return t.dim(&format!(
                "e-marker not reported — carrying {}, so 5 A rated or captive to the charger",
                milliamps(ma)
            ));
        }
        let non_pd_at_5v = partner.supports_pd == Some(false) && !p.pd_contract_active();
        return t.dim(if non_pd_at_5v {
            "no e-marker — not required at 5 V"
        } else {
            "no e-marker reported — capability unknown, 3 A limit applies"
        });
    };
    let mut parts = Vec::new();
    if let Some(k) = &c.kind {
        parts.push(k.clone());
    }
    if let Some(pt) = &c.plug_type {
        parts.push(pt.clone());
    }
    if let Some(id) = &c.identity {
        let d = &id.decoded;
        if let Some(ma) = d.cable_current_ma {
            parts.push(milliamps(ma));
        }
        if let Some(mv) = d.cable_max_voltage_mv {
            parts.push(format!("max {}", volts(mv)));
        }
        if let Some(s) = &d.cable_max_speed {
            parts.push(s.clone());
        }
        if let Some(l) = &d.cable_latency {
            parts.push(l.clone());
        }
        if let Some(term) = &d.cable_termination {
            parts.push(term.clone());
        }
        if let Some(vid) = d.vendor_id {
            parts.push(format!("vid {vid:04x}"));
        }
        if let Some(v) = id.product_type_vdo1 {
            parts.push(t.dim(&format!("vdo1 {}", v.hex)));
        }
    }
    parts.join(", ")
}

/// One line of alternate modes.
///
/// `show_state` exists because the `active` flag only means what it says on the
/// **partner's** modes. A local port's alt-mode objects describe what the port
/// is capable of, and on UCSI firmware they are reported `active = yes`
/// unconditionally — this machine claims Lenovo, Thunderbolt and DisplayPort
/// modes are all simultaneously active on both ports while a charger with zero
/// alternate modes is attached to one and nothing to the other. Printing that
/// as "active" would assert something the data does not support, so for the
/// local port only the mode list is shown.
fn alt_modes_line(modes: &[AltMode], show_state: bool) -> String {
    modes
        .iter()
        .map(|m| {
            let svid = m
                .svid
                .map(|s| format!("{s:04x}"))
                .unwrap_or_else(|| "????".into());
            let name = m
                .svid_name
                .as_deref()
                .map(|n| format!(" {n}"))
                .unwrap_or_default();
            let state = match (show_state, m.active) {
                (true, Some(true)) => " active",
                (true, Some(false)) => " inactive",
                _ => "",
            };
            format!("{svid}.{}{}{}", m.mode.unwrap_or(0), name, state)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// One-line summary of what an attached supply can deliver.
fn supply_headline(pd: &PowerDelivery, t: &Theme) -> String {
    let max = pd
        .max_source_power_mw()
        .map(watts)
        .unwrap_or_else(|| "unknown".into());
    let fixed = pd
        .source_capabilities
        .iter()
        .filter(|p| p.kind == PdoKind::FixedSupply)
        .count();
    let pps = pd
        .source_capabilities
        .iter()
        .filter(|p| p.kind == PdoKind::ProgrammableSupply)
        .count();
    let mut s = t.bold(&max);
    s.push_str(&t.dim(&format!("  ({fixed} fixed profile{}", plural(fixed))));
    if pps > 0 {
        s.push_str(&t.dim(&format!(", {pps} PPS range{}", plural(pps))));
    }
    s.push_str(&t.dim(")"));
    s
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Each profile the supply advertises, one per line, with the one currently in
/// effect marked. Knowing *which* profile is active is the difference between
/// "the charger can do 100 W" and "you are getting 100 W".
fn pdo_lines(pdos: &[Pdo], contract: Option<&PortPowerSupply>, t: &Theme) -> Vec<String> {
    let active = contract.and_then(|c| Some((c.contract_voltage_mv()?, c.contract_current_ma()?)));
    pdos.iter()
        .map(|p| {
            let is_active = matches!(
                (active, p.voltage_mv, p.current_ma),
                (Some((av, ai)), Some(v), Some(i)) if av == v && ai == i
            );
            let desc = p.describe();
            if is_active {
                format!("{} {}", t.green(&format!("{desc:<34}")), t.green("← in use"))
            } else {
                format!("{}{desc}", t.dim("  "))
            }
        })
        .collect()
}

fn pdo_line(pdos: &[Pdo]) -> String {
    pdos.iter()
        .map(|p| p.describe())
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Per-storage-device view: what speed the link got, why it is what it is, and
/// what is actually moving through it.
pub fn storage(out: &mut String, report: &Report, t: &Theme) {
    let snap = &report.snapshot;
    let devices = snap.storage_devices();
    if devices.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", t.heading("storage"));

    for (usb, blocks) in devices {
        for b in blocks {
            let _ = writeln!(
                out,
                "  {} {}  {}",
                t.bold(&b.name),
                b.label(),
                t.dim(&format!(
                    "{}  {}  via {}",
                    b.size_bytes.map(bytes_human).unwrap_or_else(|| "?".into()),
                    b.medium().label(),
                    usb.sysfs_name
                ))
            );

            // Link speed, and the practical ceiling it implies.
            let speed = usb.speed.as_ref();
            let link = speed.map(|s| s.label.clone()).unwrap_or_else(|| "?".into());
            let bus_ceiling = speed.map(|s| practical_bps(s.mbps));
            row(
                out,
                t,
                "link",
                &format!(
                    "{}{}",
                    t.speed(speed),
                    t.dim(&format!(
                        "  {link}{}",
                        bus_ceiling
                            .map(|c| format!("  — up to ~{}/s in practice", bytes_human(c as u64)))
                            .unwrap_or_default()
                    ))
                ),
            );

            // The binding constraint: bus or platter.
            if let (Some(bus), Some(media)) = (bus_ceiling, b.media_ceiling_bps()) {
                let (limiter, note) = if media < bus {
                    ("media", "the drive is slower than the link, so the bus is not the limit")
                } else {
                    ("link", "the link is slower than the drive can go")
                };
                row(
                    out,
                    t,
                    "limited by",
                    &format!(
                        "{}  {}",
                        t.bold(limiter),
                        t.dim(&format!(
                            "drive ~{}/s vs link ~{}/s — {note}",
                            bytes_human(media as u64),
                            bytes_human(bus as u64)
                        ))
                    ),
                );
            }

            // Why the link is not faster, taken from the findings for this
            // device rather than re-derived here.
            let reasons: Vec<&Finding> = report
                .findings
                .iter()
                .filter(|f| {
                    matches!(&f.subject, Subject::Device(d) if *d == usb.sysfs_name)
                        || matches!(&f.code[..], "SS_HALF_FAILED" | "SS_HALF_IDLE")
                })
                .filter(|f| f.code.starts_with("SS_") || f.code.starts_with("LINK_"))
                .collect();
            for f in &reasons {
                row(out, t, "why", &format!("{} {}", t.yellow("▸"), f.title));
            }
            if reasons.is_empty() && usb.linked_below_superspeed() {
                row(
                    out,
                    t,
                    "why",
                    &t.dim(
                        "no cause identified — the device may genuinely be USB 2.0, which its \
                         descriptors cannot distinguish from a fallback",
                    ),
                );
            }

            // What is actually moving.
            match (&b.throughput, &b.stats) {
                (Some(tp), _) if !tp.is_idle() => row(
                    out,
                    t,
                    "now",
                    &format!(
                        "{}  {}",
                        t.green(&format!("{}/s", bytes_human(tp.total_bps() as u64))),
                        t.dim(&format!(
                            "read {}/s  write {}/s  over {} ms",
                            bytes_human(tp.read_bps as u64),
                            bytes_human(tp.write_bps as u64),
                            tp.interval_ms
                        ))
                    ),
                ),
                (Some(tp), _) => row(
                    out,
                    t,
                    "now",
                    &t.dim(&format!("idle (sampled {} ms)", tp.interval_ms)),
                ),
                (None, Some(_)) => row(
                    out,
                    t,
                    "now",
                    &t.dim("not sampled — pass --sample 1000 to measure live throughput"),
                ),
                _ => {}
            }
            if let Some(s) = &b.stats {
                row(
                    out,
                    t,
                    "since boot",
                    &t.dim(&format!(
                        "read {}  written {}  ({} in flight)",
                        bytes_human(s.total_read_bytes()),
                        bytes_human(s.total_written_bytes()),
                        s.ios_in_flight
                    )),
                );
            }
            let _ = writeln!(out);
        }
    }
}

/// Realistic sustained throughput for a link rate. The rule engine judges
/// measured rates against the same figures, so there is one definition.
use usb_probe::model::practical_bps;

fn bytes_human(b: u64) -> String {
    const U: [(f64, &str); 4] = [(1e12, "TB"), (1e9, "GB"), (1e6, "MB"), (1e3, "kB")];
    let f = b as f64;
    for (scale, unit) in U {
        if f >= scale {
            let v = f / scale;
            return if v >= 100.0 {
                format!("{v:.0} {unit}")
            } else {
                format!("{v:.1} {unit}")
            };
        }
    }
    format!("{b} B")
}

// ---------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------

pub fn battery(out: &mut String, snap: &Snapshot, t: &Theme) {
    if snap.batteries.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", t.heading("battery"));
    let mains = snap.mains_online.unwrap_or(false);

    for b in &snap.batteries {
        let state = match b.status.as_deref() {
            Some("Charging") => t.green("charging"),
            Some("Discharging") if mains => t.red("discharging on mains"),
            Some("Discharging") => "discharging".to_string(),
            Some(s) => s.to_string(),
            None => "?".into(),
        };
        let _ = writeln!(
            out,
            "  {} {}  {}",
            t.bold(&b.name),
            state,
            t.dim(&format!(
                "mains {}",
                if mains { "online" } else { "offline" }
            ))
        );

        row(
            out,
            t,
            "charge",
            &format!(
                "{}%  {}",
                b.capacity_pct.unwrap_or(0),
                t.dim(&format!(
                    "{:.1} Wh of {:.1} Wh",
                    b.energy_now_wh.unwrap_or(0.0),
                    b.energy_full_wh.unwrap_or(0.0)
                ))
            ),
        );

        // The number that says whether the supply is keeping up.
        let flow = match b.power_now_w {
            Some(p) if p > 0.1 && b.is_charging() => t.green(&format!("+{p:.1} W into the pack")),
            Some(p) if p > 0.1 => t.red(&format!("-{p:.1} W out of the pack")),
            Some(_) if b.not_keeping_up(mains) => {
                t.red("0 W — plugged in but the pack is not gaining")
            }
            Some(_) => t.dim("0 W"),
            None => t.dim("not reported"),
        };
        let eta = b
            .hours_to_full()
            .map(|h| t.dim(&format!("  ~{h:.1} h to full")))
            .unwrap_or_default();
        row(out, t, "flow", &format!("{flow}{eta}"));

        if let Some(h) = b.health_pct() {
            row(
                out,
                t,
                "health",
                &format!(
                    "{h:.0}%  {}",
                    t.dim(&format!(
                        "{:.1} Wh of {:.1} Wh design{}",
                        b.energy_full_wh.unwrap_or(0.0),
                        b.energy_full_design_wh.unwrap_or(0.0),
                        b.cycle_count
                            .map(|c| format!(", {c} cycles"))
                            .unwrap_or_default()
                    ))
                ),
            );
        }
    }
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// USB4 / Thunderbolt
// ---------------------------------------------------------------------------

pub fn thunderbolt(out: &mut String, snap: &Snapshot, t: &Theme) {
    let tb = &snap.thunderbolt;
    if tb.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", t.heading("usb4 / thunderbolt"));

    for d in &tb.domains {
        let mut parts = Vec::new();
        if let Some(s) = &d.security {
            parts.push(format!("security={s}"));
        }
        if d.iommu_dma_protection == Some(true) {
            parts.push("IOMMU DMA protection".into());
        }
        let _ = writeln!(out, "  {} {}", t.bold(&d.name), t.dim(&parts.join("  ")));
    }

    for r in &tb.routers {
        let role = if r.is_host { "host" } else { "device" };
        let mut line = format!(
            "  {} {}  {}",
            t.bold(&r.name),
            t.dim(role),
            r.label()
        );
        if let Some(g) = r.generation {
            line.push_str(&t.dim(&format!("  gen {g}")));
        }
        if let Some(v) = &r.usb4_version {
            line.push_str(&t.dim(&format!("  USB4 {v}")));
        }
        if let (Some(tx), Some(rx)) = (&r.tx_speed, &r.rx_speed) {
            line.push_str(&format!("  {}", t.green(&format!("tx {tx} / rx {rx}"))));
        }
        if let (Some(tx), Some(rx)) = (r.tx_lanes, r.rx_lanes) {
            line.push_str(&t.dim(&format!("  lanes tx{tx}/rx{rx}")));
        }
        let _ = writeln!(out, "{line}");
        if t.verbose && !r.usb4_ports.is_empty() {
            let _ = writeln!(out, "      {}", t.dim(&r.usb4_ports.join(" ")));
        }
    }

    // The headline: a retimer only exists inside an active cable, so this is
    // cable identity read from the cable itself.
    if tb.has_active_cable() {
        for r in &tb.retimers {
            let _ = writeln!(
                out,
                "  {} {}  {}",
                t.bold("active cable"),
                t.green(&format!(
                    "firmware {}",
                    r.nvm_version.as_deref().unwrap_or("?")
                )),
                t.dim(&format!(
                    "{}  vendor {}  device {}",
                    r.name,
                    r.vendor.map(|v| format!("{v:04x}")).unwrap_or_else(|| "?".into()),
                    r.device.map(|d| format!("{d:04x}")).unwrap_or_else(|| "?".into())
                ))
            );
        }
    } else if !tb.routers.is_empty() {
        let _ = writeln!(
            out,
            "  {}",
            t.dim("no retimers — nothing attached over an active cable")
        );
    }
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// Displays
// ---------------------------------------------------------------------------

/// What the GPU says is plugged in, which is the only independent check on a
/// DisplayPort Alt Mode claim.
///
/// Connected and driven are separated deliberately. A monitor that has gone to
/// sleep still reads `connected`, and reporting that as "in use" would make the
/// tool wrong about the one thing this section exists to settle.
pub fn displays(out: &mut String, snap: &Snapshot, t: &Theme) {
    if snap.displays.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", t.heading("displays"));

    let (attached, empty): (Vec<_>, Vec<_>) =
        snap.displays.iter().partition(|d| d.is_connected());

    for d in &attached {
        let state = if d.is_lit() {
            t.green("driven")
        } else {
            t.dim("attached but not being driven")
        };
        let _ = writeln!(
            out,
            "  {} {}{}  {}",
            t.bold(&format!("{:<12}", d.connector)),
            d.label(),
            if d.is_internal() {
                t.dim("  built-in panel")
            } else {
                String::new()
            },
            state
        );

        if let Some(id) = &d.display {
            if let Some(m) = &id.preferred_mode {
                row(
                    out,
                    t,
                    "native",
                    &format!(
                        "{}  {}",
                        t.bold(&m.describe()),
                        t.dim(&format!(
                            "pixel clock {:.1} MHz",
                            m.pixel_clock_khz as f64 / 1000.0
                        ))
                    ),
                );
            }
            row(out, t, "identity", &t.dim(&identity_of(id)));
        }
        if !d.modes.is_empty() {
            // `modes` lists one line per mode, so the same resolution repeats
            // once per refresh rate it supports — and the file does not say
            // which rate, so the repeats carry no information.
            let mut sizes: Vec<&str> = Vec::new();
            for m in &d.modes {
                if !sizes.contains(&m.as_str()) {
                    sizes.push(m);
                }
            }
            let shown = sizes.iter().take(4).copied().collect::<Vec<_>>().join(", ");
            let more = sizes.len().saturating_sub(4);
            row(
                out,
                t,
                "offers",
                &t.dim(&if more > 0 {
                    format!("{shown}, +{more} more")
                } else {
                    shown
                }),
            );
        }
    }

    if attached.is_empty() {
        let _ = writeln!(out, "  {}", t.dim("no display attached to any output"));
    }
    if !empty.is_empty() {
        let names: Vec<&str> = empty.iter().map(|d| d.connector.as_str()).collect();
        let _ = writeln!(
            out,
            "  {}",
            t.dim(&if t.verbose {
                format!("nothing attached: {}", names.join(", "))
            } else {
                format!("{} other output(s) with nothing attached", names.len())
            })
        );
    }
    // The one thing sysfs cannot answer, said once so it is never implied.
    let _ = writeln!(
        out,
        "  {}",
        t.dim("the mode being scanned out is not exposed by sysfs — only what is offered")
    );
    let _ = writeln!(out);
}

fn identity_of(id: &DisplayIdentity) -> String {
    let mut parts = Vec::new();
    match (&id.manufacturer_name, &id.manufacturer) {
        (Some(n), Some(c)) => parts.push(format!("{n} ({c})")),
        (None, Some(c)) => parts.push(c.clone()),
        _ => {}
    }
    if let Some(p) = id.product_code {
        parts.push(format!("product {p:04x}"));
    }
    if let Some(s) = &id.serial_text {
        parts.push(format!("serial {s}"));
    }
    if let Some(y) = id.year {
        parts.push(format!("made {y}"));
    }
    if let Some(v) = &id.edid_version {
        parts.push(format!("EDID {v}"));
    }
    parts.join("  ")
}

// ---------------------------------------------------------------------------
// USB device tree
// ---------------------------------------------------------------------------

pub fn devices(out: &mut String, snap: &Snapshot, t: &Theme) {
    let _ = writeln!(out, "{}", t.heading("usb topology"));
    for bus in &snap.buses {
        let _ = writeln!(
            out,
            "  {} {}  {}  {}",
            t.bold(&bus.sysfs_name),
            t.speed(bus.speed.as_ref()),
            t.dim(&format!("USB {}", bus.usb_version.as_deref().unwrap_or("?"))),
            t.dim(&bus.label())
        );
        for (i, child) in bus.children.iter().enumerate() {
            device_tree(out, child, "  ", i + 1 == bus.children.len(), t);
        }
        if t.verbose {
            for p in &bus.ports {
                let _ = writeln!(out, "    {}", t.dim(&hub_port_line(p)));
            }
        }
        if bus.children.is_empty() && !t.verbose {
            let _ = writeln!(out, "    {}", t.dim("(nothing attached)"));
        }
    }
    let _ = writeln!(out);
}

fn device_tree(out: &mut String, dev: &UsbDevice, prefix: &str, last: bool, t: &Theme) {
    let branch = if last { "└─" } else { "├─" };
    let drivers = drivers_of(dev);
    let _ = writeln!(
        out,
        "{prefix}{} {} {:<6} {}  {}{}",
        t.dim(branch),
        t.bold(&format!("{:<8}", dev.sysfs_name)),
        t.speed(dev.speed.as_ref()),
        dev.label(),
        t.dim(&dev.vid_pid().unwrap_or_default()),
        drivers
    );

    if t.verbose {
        let child_prefix = format!("{prefix}{}   ", if last { " " } else { "│" });
        let _ = writeln!(out, "{child_prefix}{}", t.dim(&device_detail(dev)));
        for p in &dev.ports {
            let _ = writeln!(out, "{child_prefix}{}", t.dim(&hub_port_line(p)));
        }
    }

    let child_prefix = format!("{prefix}{}  ", if last { " " } else { "│" });
    for (i, c) in dev.children.iter().enumerate() {
        device_tree(out, c, &child_prefix, i + 1 == dev.children.len(), t);
    }
}

fn drivers_of(dev: &UsbDevice) -> String {
    let mut names: Vec<&str> = dev
        .interfaces
        .iter()
        .filter_map(|i| i.driver.as_deref())
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        if dev.interfaces.is_empty() {
            String::new()
        } else {
            "  [no driver]".to_string()
        }
    } else {
        format!("  {}", names.join(","))
    }
}

fn device_detail(dev: &UsbDevice) -> String {
    let kind = dev.kind();
    let mut parts = Vec::new();
    // What it is, before what it implements. The raw class codes stay below as
    // the evidence for it.
    //
    // A kind the user set must never look like one the device declared (§8): a
    // stored belief they cannot see is one they cannot correct, and this is the
    // only place a stale label would show up.
    if kind.is_known() {
        parts.push(match dev.declared.as_ref().filter(|_| kind.source == KindSource::User) {
            Some(d) => format!("{} (set by you, {})", kind.kind.label(), d.id),
            None => kind.kind.label().to_string(),
        });
    }
    if let Some(note) = dev.declared.as_ref().and_then(|d| d.note.as_deref()) {
        parts.push(format!("\u{201c}{note}\u{201d}"));
    }
    if let Some(m) = dev.declared.as_ref().and_then(|d| d.medium) {
        parts.push(format!("medium {} (set by you)", m.label()));
    }
    parts.push(format!("USB {}", dev.usb_version.as_deref().unwrap_or("?")));
    if let (Some(tx), Some(rx)) = (dev.tx_lanes, dev.rx_lanes) {
        parts.push(format!("lanes tx{tx}/rx{rx}"));
    }
    if let Some(ma) = dev.max_power_ma {
        parts.push(format!("draws {ma}mA"));
    }
    if dev.self_powered == Some(true) {
        parts.push("self-powered".into());
    }
    if let Some(c) = dev.device_class {
        // The raw class is evidence for the kind, so it is worth printing —
        // unless it is the same statement twice, which on a hub gave "hub" the
        // kind, "hub" the class and "if0:hub" the interface.
        if c != 0 && DeviceKind::from_class(c) != Some(kind.kind) {
            parts.push(class_name(c).to_string());
        }
    }
    for i in &dev.interfaces {
        parts.push(format!(
            "if{}:{}{}",
            i.number.unwrap_or(0),
            i.class.map(class_name).unwrap_or("?"),
            i.driver
                .as_deref()
                .map(|d| format!("→{d}"))
                .unwrap_or_else(|| "→none".into())
        ));
    }
    parts.join("  ")
}

fn hub_port_line(p: &HubPort) -> String {
    let mut parts = vec![format!("{}: {}", p.name, p.state.as_deref().unwrap_or("?"))];
    if let Some(c) = &p.connect_type {
        parts.push(c.clone());
    }
    if let Some(n) = p.over_current_count {
        if n > 0 {
            parts.push(format!("over-current x{n}"));
        }
    }
    if let Some(l) = &p.location {
        parts.push(format!("loc {l}"));
    }
    if let Some(pl) = &p.physical_location {
        parts.push(pl.display());
    }
    if let Some(c) = &p.connector {
        parts.push(format!("connector {c}"));
    }
    if let Some(c) = &p.child {
        parts.push(format!("→ {c}"));
    }
    parts.join("  ")
}

// ---------------------------------------------------------------------------
// Header / summary
// ---------------------------------------------------------------------------

pub fn header(out: &mut String, snap: &Snapshot, t: &Theme) {
    let h = &snap.host;
    let _ = writeln!(
        out,
        "{} {} {}",
        t.bold("usbdiag"),
        t.dim("·"),
        t.dim(&format!(
            "{} {} · kernel {} · typec via {}",
            h.sys_vendor.as_deref().unwrap_or("?"),
            h.product_name.as_deref().unwrap_or("?"),
            h.kernel_release.as_deref().unwrap_or("?"),
            if h.typec_drivers.is_empty() {
                "none".to_string()
            } else {
                h.typec_drivers.join(",")
            }
        ))
    );

    let log = &snap.kernel_log;
    let src = match log.source {
        KernelLogSource::DevKmsg => "/dev/kmsg",
        KernelLogSource::Journalctl => "journalctl",
        KernelLogSource::Dmesg => "dmesg",
        KernelLogSource::Unavailable => "unavailable",
    };
    let _ = writeln!(
        out,
        "{}",
        t.dim(&format!(
            "  kernel log: {src} ({} usb events){}",
            log.events.len(),
            log.note
                .as_deref()
                .map(|n| format!(" — {n}"))
                .unwrap_or_default()
        ))
    );
    let _ = writeln!(out);
}

/// URB traffic measured over a window, when the privileged probe ran.
///
/// Shown whenever there is traffic, not only when something is wrong: seeing
/// zero transport errors across thousands of completions is a real result, and
/// the only one this tool can offer that says a link is healthy rather than
/// merely well-negotiated.
pub fn urb_traffic(out: &mut String, snap: &Snapshot, t: &Theme) {
    let Some(traffic) = &snap.urb_traffic else {
        return;
    };
    let _ = writeln!(out, "{}", t.heading("link error counts"));
    let _ = writeln!(
        out,
        "  {}",
        t.dim(&format!(
            "{} completions over {:.1}s from {}",
            traffic.total_completions(),
            traffic.window_ms as f64 / 1000.0,
            traffic
                .source
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "usbmon".into())
        ))
    );
    if traffic.unparsed > 0 {
        let _ = writeln!(
            out,
            "  {}",
            t.yellow(&format!(
                "{} line(s) not understood — the usbmon format may have changed",
                traffic.unparsed
            ))
        );
    }

    for s in &traffic.devices {
        let who = snap
            .device_at_address(s.bus, s.device_address)
            .map(|d| format!("{}  {}", d.sysfs_name, d.label()))
            .unwrap_or_else(|| format!("bus {} address {}", s.bus, s.device_address));

        let rate = s.transport_error_rate().unwrap_or(0.0);
        let verdict = if s.transport_errors == 0 {
            t.green("no transport errors")
        } else {
            t.red(&format!(
                "{} transport error(s), {:.2}%",
                s.transport_errors,
                rate * 100.0
            ))
        };
        let _ = writeln!(out, "  {} {}", t.bold(&who), verdict);

        let mut detail = vec![format!("{} completions", s.completions)];
        if let Some(bps) = s.throughput_bps(traffic.window_ms) {
            if bps > 1024.0 {
                detail.push(format!("{}/s", bytes_human(bps as u64)));
            }
        }
        // Named so they are visibly excluded rather than quietly missing.
        if s.cancellations > 0 {
            detail.push(format!("{} cancelled by the driver (routine)", s.cancellations));
        }
        if s.protocol_errors > 0 {
            detail.push(format!(
                "{} stall(s){}",
                s.protocol_errors,
                if s.non_control_stalls > 0 {
                    format!(", {} on a data endpoint", s.non_control_stalls)
                } else {
                    ", all on endpoint 0 (routine)".into()
                }
            ));
        }
        row(out, t, "", &t.dim(&detail.join("  ")));

        if !s.error_breakdown().is_empty() {
            row(out, t, "statuses", &t.dim(&s.error_breakdown().join(", ")));
        }
    }
    let _ = writeln!(out);
}

/// What this run could and could not attempt.
///
/// Verbose only. On an ordinary desktop every active probe is out of reach and
/// saying so on every run would be noise — but when someone is asking why a
/// cable finding is only *inferred*, this is the answer, and it names the exact
/// command that would change it.
pub fn capabilities(out: &mut String, snap: &Snapshot, t: &Theme) {
    if !t.verbose {
        return;
    }
    let c = &snap.capabilities;
    let _ = writeln!(out, "{}", t.heading("probe capabilities"));
    let _ = writeln!(
        out,
        "  {}",
        t.dim(&format!(
            "running as uid {}{}",
            c.effective_uid
                .map(|u| u.to_string())
                .unwrap_or_else(|| "?".into()),
            if c.is_root() { " (root)" } else { "" }
        ))
    );

    for (name, iface) in [
        ("usbmon", &c.usbmon),
        ("usbfs", &c.usbfs),
        ("disks", &c.block_read),
        ("ports", &c.port_control),
    ] {
        let mark = if iface.is_usable() {
            t.green("✓")
        } else {
            t.dim("·")
        };
        row(
            out,
            t,
            name,
            &format!("{mark} {}", t.dim(iface.availability.explain())),
        );
    }

    let _ = writeln!(out, "  {}", t.dim("probes"));
    for p in usb_probe::caps::PROBES {
        // The reason is already spelled out against the interface above; here
        // only which interface is missing is worth repeating.
        let (mark, note) = match c.blocker(p) {
            None => (t.green("✓"), String::new()),
            Some(_) => (t.dim("·"), format!("needs {}", p.needs.label())),
        };
        let line = format!(
            "    {mark} {} {} {}",
            t.bold(&format!("{:<16}", p.name)),
            t.dim(&format!("{:<24}", format!("[{}]", p.class.label()))),
            t.dim(&note),
        );
        let _ = writeln!(out, "{}", line.trim_end());
    }
    let _ = writeln!(out);
}

/// Measured read rates from the throughput probe.
///
/// Always shows the number, including when the rule engine declined to judge
/// it. "62 MB/s, medium unknown, no conclusion drawn" is a useful thing to have
/// on screen; a measurement suppressed because it could not be interpreted is
/// not.
pub fn throughput(out: &mut String, snap: &Snapshot, t: &Theme) {
    if snap.throughput.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", t.heading("measured throughput"));

    for s in &snap.throughput {
        let owner = snap
            .storage_devices()
            .into_iter()
            .find(|(_, b)| b.iter().any(|b| b.name == s.device))
            .map(|(d, _)| format!("{}  {}", d.sysfs_name, d.label()));

        let _ = writeln!(
            out,
            "  {} {}",
            t.bold(&s.device),
            t.dim(&owner.unwrap_or_else(|| "not attributed to a USB device".into()))
        );

        match (&s.error, s.bytes_per_second) {
            (Some(e), _) => row(out, t, "failed", &t.red(e)),
            (None, Some(bps)) => {
                row(
                    out,
                    t,
                    "read",
                    &format!(
                        "{} in {:.1}s  =  {}",
                        bytes_human(s.bytes_read),
                        s.elapsed_ms as f64 / 1000.0,
                        t.bold(&format!("{}/s", bytes_human(bps as u64)))
                    ),
                );
                if let Some(dev) = snap
                    .storage_devices()
                    .into_iter()
                    .find(|(_, b)| b.iter().any(|b| b.name == s.device))
                    .map(|(d, _)| d)
                {
                    if let Some(speed) = &dev.speed {
                        let ceiling = speed.practical_bps();
                        row(
                            out,
                            t,
                            "link allows",
                            &t.dim(&format!(
                                "~{}/s at {}  —  {:.0}% of it",
                                bytes_human(ceiling as u64),
                                speed.label,
                                bps / ceiling * 100.0
                            )),
                        );
                    }
                }
            }
            (None, None) => row(out, t, "read", &t.dim("nothing")),
        }

        // A contended sample is not wrong, it just is not about the link. Say
        // so next to the number rather than quietly dropping it.
        if s.was_contended() {
            let other = s.contended_bytes.unwrap_or(0);
            row(
                out,
                t,
                "contended",
                &t.yellow(&format!(
                    "{} was served to something else during the window — this number is \
                     about load, not the link",
                    bytes_human(other)
                )),
            );
        }
        let _ = writeln!(out);
    }
}

/// The per-cycle record from the re-enumeration probe.
///
/// Shows every attempt, not just the summary. A distribution like "18 at 5G, 2
/// at 480M" is the finding, but seeing *which* attempts fell back is what tells
/// someone whether it degrades over time or fails at random.
pub fn reenumeration(out: &mut String, snap: &Snapshot, t: &Theme) {
    let Some(run) = &snap.reenumeration else {
        return;
    };
    let _ = writeln!(out, "{}", t.heading("port cycling"));
    if let Some(e) = &run.error {
        let _ = writeln!(out, "  {}\n", t.red(e));
        return;
    }
    let _ = writeln!(
        out,
        "  {} {}",
        t.bold(&run.device),
        t.dim(&format!("via {} — {} cycles", run.port, run.cycles.len()))
    );

    for c in &run.cycles {
        let outcome = match (c.returned, &c.speed_mbps) {
            (true, Some(mbps)) => {
                let speed = LinkSpeed::from_mbps(*mbps);
                let lanes = match (c.rx_lanes, c.tx_lanes) {
                    (Some(rx), Some(tx)) if rx > 1 || tx > 1 => format!("  {rx}x{tx} lanes"),
                    _ => String::new(),
                };
                let text = format!("{}{lanes}  after {} ms", speed.short(), c.returned_after_ms);
                // Anything below the best seen is the interesting row.
                match run.best_mbps() {
                    Some(best) if *mbps < best => t.yellow(&text),
                    _ => t.green(&text),
                }
            }
            _ => t.red(c.error.as_deref().unwrap_or("did not return")),
        };
        let _ = writeln!(
            out,
            "    {} {}",
            t.dim(&format!("{:>3}", c.index + 1)),
            outcome
        );
    }

    let _ = writeln!(out);
    for (label, n) in run.speed_distribution() {
        row(out, t, &label, &format!("{n} of {}", run.cycles.len()));
    }
    if run.failures() > 0 {
        row(
            out,
            t,
            "did not return",
            &t.red(&format!("{} of {}", run.failures(), run.cycles.len())),
        );
    }
    let _ = writeln!(out);
}

/// The probe catalogue: what exists, what this machine allows, and what each
/// one would do to the bus.
///
/// Unimplemented probes are listed rather than hidden. A gap that is visible
/// gets closed; one that is merely absent does not.
pub fn probes(out: &mut String, snap: &Snapshot, t: &Theme) {
    let c = &snap.capabilities;
    let _ = writeln!(out, "{}", t.heading("probes"));
    let _ = writeln!(
        out,
        "  {}",
        t.dim(&format!(
            "running as uid {}{}",
            c.effective_uid
                .map(|u| u.to_string())
                .unwrap_or_else(|| "?".into()),
            if c.is_root() { " (root)" } else { "" },
        ))
    );
    for (name, iface) in [
        ("usbmon", &c.usbmon),
        ("usbfs", &c.usbfs),
        ("disks", &c.block_read),
        ("ports", &c.port_control),
    ] {
        let mark = if iface.is_usable() {
            t.green("✓")
        } else {
            t.dim("·")
        };
        let mut lines = wrap(iface.availability.explain(), 60).into_iter();
        let _ = writeln!(
            out,
            "  {mark} {} {}",
            t.dim(&format!("{name:<7}")),
            t.dim(&lines.next().unwrap_or_default())
        );
        for line in lines {
            let _ = writeln!(out, "            {}", t.dim(&line));
        }
    }
    let _ = writeln!(out);

    for p in usb_probe::caps::PROBES {
        let (mark, status) = match (p.implemented, c.blocker(p)) {
            (false, _) => (t.dim("·"), t.dim("not implemented yet")),
            (true, Some(_)) => (t.dim("·"), t.yellow(&format!("needs {}", p.needs.label()))),
            (true, None) => (t.green("✓"), t.green("ready")),
        };
        let _ = writeln!(
            out,
            "  {mark} {} {}  {status}",
            t.bold(&format!("{:<15}", p.name)),
            t.dim(&format!("{:<24}", format!("[{}]", p.class.label()))),
        );
        for line in wrap(p.summary, 68) {
            let _ = writeln!(out, "      {}", t.dim(&line));
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(
        out,
        "  {}",
        t.dim("Passive probes run in every ordinary scan. Naming a read-only probe")
    );
    let _ = writeln!(
        out,
        "  {}",
        t.dim("is enough to run it; a disruptive one needs --target, --yes and --force,")
    );
    let _ = writeln!(
        out,
        "  {}",
        t.dim("and is refused outright on a disk holding a mounted filesystem.")
    );
    let _ = writeln!(out);
}

pub fn summary(out: &mut String, report: &Report, t: &Theme) {
    let mut counts: Vec<(Severity, usize)> = Vec::new();
    for s in [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
        Severity::Info,
    ] {
        let n = report.findings.iter().filter(|f| f.severity == s).count();
        if n > 0 {
            counts.push((s, n));
        }
    }
    if counts.is_empty() {
        return;
    }
    let text = counts
        .iter()
        .map(|(s, n)| format!("{n} {}", s.label().to_lowercase()))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(out, "{} {}", t.dim("summary:"), text);
}

/// The one honest caveat, printed once so it is never implied otherwise.
pub fn caveat(out: &mut String, t: &Theme) {
    let _ = writeln!(
        out,
        "{}",
        t.dim(
            "note: cable conclusions are inferred from e-markers and negotiated state. Signal\n\
             integrity, CC-line voltages and unmarked-cable ratings are not measurable in software."
        )
    );
}

// ---------------------------------------------------------------------------

/// Word-wrap to a width, preserving nothing clever — plain greedy fill.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_width_and_keeps_all_words() {
        let text = "the quick brown fox jumps over the lazy dog";
        let lines = wrap(text, 16);
        assert!(lines.iter().all(|l| l.chars().count() <= 16), "{lines:?}");
        assert_eq!(lines.join(" "), text);
    }

    #[test]
    fn wrap_handles_a_word_longer_than_the_width() {
        let lines = wrap("short supercalifragilisticexpialidocious", 10);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn theme_without_color_emits_no_escapes() {
        let t = Theme {
            color: false,
            verbose: false,
        };
        assert_eq!(t.bold("x"), "x");
        assert!(!t.severity(Severity::High).contains('\x1b'));
    }

    #[test]
    fn theme_with_color_emits_escapes() {
        let t = Theme {
            color: true,
            verbose: false,
        };
        assert!(t.severity(Severity::High).contains("\x1b[31m"));
    }

    /// Real values from this ThinkPad's right-hand port while sourcing. The
    /// port advertises Type-C `3.0A` — 5 V at 3 A, 15 W — and its power_supply
    /// node reports `voltage_max` 13.4 V with `current_max` 5.72 A, which
    /// multiplies out to 76 W it is in no position to deliver. Those fields
    /// are the port's capability range, and reading them as an offer is the
    /// same mistake that once turned a 100 W contract into 47 W.
    #[test]
    fn a_sourcing_port_reports_its_advertisement_not_its_capability_range() {
        let port = TypecPort {
            name: "port1".into(),
            power_role: Some(RoleField::parse("sink [source]")),
            power_operation_mode: Some("3.0A".into()),
            power_supply: Some(PortPowerSupply {
                name: "ucsi-source-psy-USBC000:002".into(),
                online: Some(false),
                voltage_now_mv: Some(0),
                current_now_ma: Some(0),
                voltage_min_mv: Some(5_000),
                voltage_max_mv: Some(13_400),
                current_max_ma: Some(5_720),
                usb_type: None,
            }),
            ..Default::default()
        };
        assert!(port.is_sourcing());

        let line = contract_line(
            &port,
            &Theme {
                color: false,
                verbose: false,
            },
        );
        assert!(line.contains("15 W"), "the advertisement is the offer: {line}");
        assert!(!line.contains("13.4"), "a capability range is not an offer: {line}");
        assert!(!line.contains("5.72"), "a capability range is not an offer: {line}");
        assert!(
            line.contains("not measurable"),
            "outgoing draw is unknowable here and must be said: {line}"
        );
    }

    fn fixed_pdo(index: u32, mv: u32, ma: u32) -> Pdo {
        Pdo {
            index,
            kind: PdoKind::FixedSupply,
            role: PdoRole::Source,
            voltage_mv: Some(mv),
            min_voltage_mv: None,
            max_voltage_mv: None,
            current_ma: Some(ma),
            power_mw_field: None,
            flags: Default::default(),
            peak_current: None,
            fast_role_swap_current: None,
        }
    }

    /// `Snapshot::orphan_pd` exists so that a PD object no port claims is not
    /// silently discarded. That is only true if something renders it.
    #[test]
    fn an_unattached_pd_object_is_rendered() {
        let mut snap = Snapshot::default();
        snap.orphan_pd.push(PowerDelivery {
            name: "pd7".into(),
            revision: Some("3.0".into()),
            source_capabilities: vec![fixed_pdo(1, 5000, 3000), fixed_pdo(2, 20_000, 5000)],
            sink_capabilities: Vec::new(),
        });

        let t = Theme {
            color: false,
            verbose: false,
        };
        let mut out = String::new();
        orphan_pd(&mut out, &snap, &t);

        assert!(out.contains("pd7"), "{out}");
        assert!(out.contains("PD 3.0"), "{out}");
        // The headline capability must survive, not just the name.
        assert!(out.contains("100 W"), "{out}");
    }

    /// "Connected" and "being driven" must never be collapsed into one word: a
    /// sleeping monitor reads connected, and calling that "in use" would make
    /// the section wrong about the only thing it exists to settle.
    #[test]
    fn a_sleeping_monitor_is_not_reported_as_driven() {
        let mut snap = Snapshot::default();
        snap.displays.push(DisplayConnector {
            name: "card1-HDMI-A-1".into(),
            connector: "HDMI-A-1".into(),
            connector_id: Some(108),
            status: Some("connected".into()),
            enabled: Some(false),
            dpms: Some("Off".into()),
            modes: vec!["2560x1440".into(), "2560x1440".into(), "1920x1080".into()],
            display: Some(DisplayIdentity {
                manufacturer: Some("GSM".into()),
                manufacturer_name: Some("LG".into()),
                name: Some("LG ULTRAGEAR".into()),
                preferred_mode: Some(DisplayMode {
                    width: 2560,
                    height: 1440,
                    refresh_hz: 120.0,
                    pixel_clock_khz: 497_750,
                }),
                ..Default::default()
            }),
        });
        snap.displays
            .push(usb_probe::model::DisplayConnector {
                name: "card1-DP-1".into(),
                connector: "DP-1".into(),
                connector_id: None,
                status: Some("disconnected".into()),
                enabled: Some(false),
                dpms: None,
                modes: Vec::new(),
                display: None,
            });

        let mut out = String::new();
        displays(
            &mut out,
            &snap,
            &Theme {
                color: false,
                verbose: false,
            },
        );

        assert!(out.contains("LG ULTRAGEAR"), "{out}");
        assert!(out.contains("2560x1440 @ 120 Hz"), "{out}");
        assert!(out.contains("attached but not being driven"), "{out}");
        assert!(snap.displays[0].is_connected() && !snap.displays[0].is_lit());
        // Repeated resolutions carry no information and are folded.
        assert!(out.contains("2560x1440, 1920x1080"), "{out}");
        assert!(out.contains("1 other output"), "{out}");
    }

    #[test]
    fn no_unattached_pd_objects_prints_nothing() {
        let mut out = String::new();
        orphan_pd(
            &mut out,
            &Snapshot::default(),
            &Theme {
                color: false,
                verbose: false,
            },
        );
        assert!(out.is_empty());
    }
}

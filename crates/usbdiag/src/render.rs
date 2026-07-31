//! Terminal rendering.
//!
//! Kept strictly separate from `usb_probe`: the library returns data, this
//! module decides how it looks. A future native UI replaces this file and
//! nothing else.

use std::fmt::Write as _;

use usb_probe::diag::{milliamps, volts, watts};
use usb_probe::model::*;
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
            row(out, t, "alt modes", &alt_modes_line(&p.alt_modes));
        }
        if let Some(pt) = &p.partner {
            if !pt.alt_modes.is_empty() {
                row(out, t, "partner modes", &alt_modes_line(&pt.alt_modes));
            }
            if let Some(pd) = &pt.pd {
                if !pd.source_capabilities.is_empty() {
                    row(out, t, "source caps", &pdo_line(&pd.source_capabilities));
                }
            }
        }
        if t.verbose {
            if let Some(pd) = &p.local_pd {
                if !pd.sink_capabilities.is_empty() {
                    row(out, t, "accepts", &pdo_line(&pd.sink_capabilities));
                }
                if !pd.source_capabilities.is_empty() {
                    row(out, t, "offers", &pdo_line(&pd.source_capabilities));
                }
            }
        }
        let _ = writeln!(out);
    }
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
        let limit = match (ps.voltage_max_mv.or(ps.voltage_now_mv), ps.current_max_ma) {
            (Some(v), Some(i)) if v > 0 && i > 0 => {
                let mut s = t.bold(&format!("supplying up to {} at {}", volts(v), milliamps(i)));
                if let Some(mw) = ps.contract_power_mw() {
                    s.push_str(&format!(" ({})", watts(mw)));
                }
                s
            }
            _ => t.bold("supplying (limit not reported)"),
        };
        return format!(
            "{limit}  {tag}  {}",
            t.dim("outgoing current is not measurable from this node")
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

fn alt_modes_line(modes: &[AltMode]) -> String {
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
            let state = match m.active {
                Some(true) => " active",
                Some(false) => " inactive",
                None => "",
            };
            format!("{svid}.{}{}{}", m.mode.unwrap_or(0), name, state)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn pdo_line(pdos: &[Pdo]) -> String {
    pdos.iter()
        .map(|p| p.describe())
        .collect::<Vec<_>>()
        .join(", ")
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
    let mut parts = vec![format!("USB {}", dev.usb_version.as_deref().unwrap_or("?"))];
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
        if c != 0 {
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
}

//! The sidebar: the machine, its Type-C ports, and the device tree.
//!
//! Two rules from `docs/01-gui-concept.md` shape this file.
//!
//! **Every dot carries its sentence.** §8 forbids a coloured mark without text,
//! and a tree of bare dots would break that while looking perfectly normal. So
//! each row has a one-line reason under the name — the subject's verdict
//! headline where there is one, and a plain fact where there is not.
//!
//! **Hubs collapse.** A hub between the user and the device they came for is
//! noise; it becomes `via 2 hubs` on the device's own row. A hub with nothing
//! interesting behind it is the exception — then the hub *is* the device, and
//! hiding it would leave a bus looking empty when it is not.

use relm4::gtk::{self, prelude::*};
use relm4::Sender;
use usb_probe::model::{Outcome, Report, Severity, Subject, TypecPort, UsbDevice, Verdict};

use crate::findings;
use crate::Msg;

/// USB class code for a hub.
const CLASS_HUB: u8 = 0x09;

/// Replace the sidebar's contents.
pub fn build(
    container: &gtk::Box,
    report: &Report,
    selected: Option<&Subject>,
    show_hubs: bool,
    sender: &Sender<Msg>,
) {
    clear(container);
    let snap = &report.snapshot;

    // --- the machine itself ---
    container.append(&group_heading("System", None));
    container.append(&row(
        report,
        &Subject::Host,
        &host_name(report),
        snap.host.kernel_release.as_deref().unwrap_or(""),
        selected,
        sender,
    ));

    // --- Type-C ports ---
    if !snap.ports.is_empty() {
        container.append(&group_heading("USB-C ports", None));
        for p in &snap.ports {
            container.append(&row(
                report,
                &Subject::Port(p.name.clone()),
                &port_name(p),
                &port_meta(p),
                selected,
                sender,
            ));
        }
    }

    // --- devices, by bus ---
    let toggle = gtk::Button::with_label(if show_hubs { "hide hubs" } else { "show hubs" });
    toggle.add_css_class("flat");
    toggle.add_css_class("link");
    {
        let sender = sender.clone();
        toggle.connect_clicked(move |_| sender.emit(Msg::ToggleHubs));
    }
    container.append(&group_heading("Devices", Some(&toggle)));

    let mut silent_buses = 0;
    for bus in &snap.buses {
        let shown: Vec<&UsbDevice> = snap
            .subtree(&bus.sysfs_name)
            .into_iter()
            .filter(|d| !d.is_root_hub)
            .filter(|d| show_hubs || !is_hub(d) || !has_real_descendant(report, d))
            .collect();
        if shown.is_empty() {
            silent_buses += 1;
            continue;
        }
        container.append(&bus_heading(bus));
        for d in shown {
            container.append(&row(
                report,
                &Subject::Device(d.sysfs_name.clone()),
                &d.label(),
                &device_meta(d),
                selected,
                sender,
            ));
        }
    }
    if silent_buses > 0 {
        let note = gtk::Label::new(Some(&format!(
            "{silent_buses} controller{} with nothing attached",
            if silent_buses == 1 { "" } else { "s" }
        )));
        note.add_css_class("footnote");
        note.set_xalign(0.0);
        container.append(&note);
    }
}

// ---------------------------------------------------------------------------
// rows
// ---------------------------------------------------------------------------

fn row(
    report: &Report,
    subject: &Subject,
    name: &str,
    meta: &str,
    selected: Option<&Subject>,
    sender: &Sender<Msg>,
) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("flat");
    btn.add_css_class("row");
    if selected == Some(subject) {
        btn.add_css_class("sel");
    }

    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    let (why, class) = reason(report, subject);
    outer.append(&findings::dot(class));

    let body = gtk::Box::new(gtk::Orientation::Vertical, 1);
    body.set_hexpand(true);

    let line1 = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let n = gtk::Label::new(Some(name));
    n.add_css_class("name");
    n.set_xalign(0.0);
    n.set_ellipsize(gtk::pango::EllipsizeMode::End);
    line1.append(&n);
    if !meta.is_empty() {
        let m = gtk::Label::new(Some(meta));
        m.add_css_class("meta");
        m.set_hexpand(true);
        m.set_xalign(1.0);
        m.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line1.append(&m);
    }
    body.append(&line1);

    let w = gtk::Label::new(Some(&why));
    w.add_css_class("why");
    if class != "none" && class != "info" {
        w.add_css_class(class);
    }
    w.set_xalign(0.0);
    // Two lines, not one. The sentence is a finding's title quoted verbatim,
    // and those are written for a list that names the subject separately — so
    // they often open with the device's own name and run past a single line.
    // Truncating the quote is worse than spending a second line on it.
    w.set_wrap(true);
    w.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    w.set_lines(2);
    w.set_ellipsize(gtk::pango::EllipsizeMode::End);
    body.append(&w);

    outer.append(&body);
    btn.set_child(Some(&outer));

    let sender = sender.clone();
    let subject = subject.clone();
    btn.connect_clicked(move |_| sender.emit(Msg::Select(subject.clone())));
    btn
}

/// The sentence beside the dot, and the dot's class.
///
/// A verdict that cites nothing is a weaker statement than one that does, so it
/// does not get to speak: the row falls back to a plain fact and a hollow dot.
/// Saying "Nothing wrong found" in green on a subject no rule even looked at
/// would be a claim the data does not support.
fn reason(report: &Report, subject: &Subject) -> (String, &'static str) {
    let verdict = report.verdict_for(subject);
    let worst = findings::about(&report.findings, subject)
        .first()
        .map(|f| f.severity);

    let class = match (worst, verdict.map(|v| v.outcome)) {
        (Some(s), _) if s >= Severity::Low => findings::severity_class(s),
        (_, Some(Outcome::Clear)) if verdict.is_some_and(cited) => "ok",
        (Some(Severity::Info), _) => "info",
        _ => "none",
    };

    let text = match verdict {
        Some(v) if cited(v) => v.headline.clone(),
        _ => fact(report, subject),
    };
    (text, class)
}

fn cited(v: &Verdict) -> bool {
    !v.because.is_empty()
}

/// A plain, checkable fact for a subject nothing has been concluded about.
fn fact(report: &Report, subject: &Subject) -> String {
    let snap = &report.snapshot;
    match subject {
        Subject::Host => snap
            .host
            .typec_drivers
            .first()
            .map(|d| format!("Type-C via {d}"))
            .unwrap_or_else(|| "no Type-C driver loaded".into()),
        Subject::Port(name) | Subject::Cable(name) => {
            let Some(p) = snap.ports.iter().find(|p| &p.name == name) else {
                return String::new();
            };
            match &p.partner {
                None => "nothing attached".into(),
                Some(pt) if pt.speaks_pd() => "attached, speaks PD".into(),
                Some(_) => "attached, does not speak PD".into(),
            }
        }
        Subject::Device(name) => {
            let Some(d) = snap.device(name) else {
                return String::new();
            };
            if let Some(b) = snap.storage_on(d).first() {
                return format!("storage \u{00b7} {}", b.label());
            }
            if is_hub(d) && d.children.is_empty() {
                return "nothing attached".into();
            }
            match hops(snap, d) {
                0 => d
                    .speed
                    .as_ref()
                    .map(|s| s.label.clone())
                    .unwrap_or_else(|| "attached".into()),
                n => format!("via {n} hub{}", if n == 1 { "" } else { "s" }),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// headings and labels
// ---------------------------------------------------------------------------

fn group_heading(title: &str, trailing: Option<&gtk::Button>) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.add_css_class("group-hd");
    let l = gtk::Label::new(Some(&title.to_uppercase()));
    l.add_css_class("group-title");
    l.set_xalign(0.0);
    l.set_hexpand(true);
    b.append(&l);
    if let Some(t) = trailing {
        b.append(t);
    }
    b
}

fn bus_heading(bus: &UsbDevice) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.add_css_class("bus");
    let name = gtk::Label::new(Some(&bus.sysfs_name));
    name.add_css_class("mono");
    name.set_xalign(0.0);
    b.append(&name);
    if let Some(s) = &bus.speed {
        let sp = gtk::Label::new(Some(&s.short()));
        sp.set_hexpand(true);
        sp.set_xalign(0.0);
        b.append(&sp);
    }
    let driver = bus
        .interfaces
        .iter()
        .find_map(|i| i.driver.clone())
        .unwrap_or_default();
    if !driver.is_empty() {
        let d = gtk::Label::new(Some(&driver));
        d.add_css_class("dim");
        d.set_xalign(1.0);
        b.append(&d);
    }
    b
}

fn host_name(report: &Report) -> String {
    let h = &report.snapshot.host;
    match (&h.sys_vendor, &h.product_name) {
        (Some(v), Some(p)) => format!("{} {}", v.trim(), p.trim()),
        (_, Some(p)) => p.trim().to_string(),
        _ => "This machine".into(),
    }
}

fn port_name(p: &TypecPort) -> String {
    match p.physical_location.as_ref().map(|l| l.display()) {
        Some(loc) if !loc.is_empty() => format!("{}  \u{00b7} {loc}", p.name),
        _ => p.name.clone(),
    }
}

/// Power in or out, which is the number people look for on a port row.
///
/// Only while something is attached. An empty socket still advertises a Type-C
/// current on its CC resistors, and printing that as "4.5 W in" claims power is
/// arriving when nothing is plugged in.
fn port_meta(p: &TypecPort) -> String {
    if !p.is_attached() {
        return String::new();
    }
    let Some(mw) = p
        .power_supply
        .as_ref()
        .and_then(|ps| ps.contract_power_mw())
        .or_else(|| p.typec_advertised_ceiling_mw())
    else {
        return String::new();
    };
    let dir = if p.is_sourcing() { "out" } else { "in" };
    format!("{} {dir}", usb_probe::diag::watts(mw))
}

fn device_meta(d: &UsbDevice) -> String {
    d.speed.as_ref().map(|s| s.short()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// topology
// ---------------------------------------------------------------------------

pub fn is_hub(d: &UsbDevice) -> bool {
    d.device_class == Some(CLASS_HUB) || d.has_interface_class(CLASS_HUB)
}

/// How many hubs sit between this device and its root hub.
fn hops(snap: &usb_probe::model::Snapshot, d: &UsbDevice) -> usize {
    let mut n = 0;
    let mut cur = d.parent.clone();
    while let Some(name) = cur {
        let Some(up) = snap.device(&name) else { break };
        if up.is_root_hub {
            break;
        }
        if is_hub(up) {
            n += 1;
        }
        cur = up.parent.clone();
    }
    n
}

/// Is there anything behind this hub that is not itself a hub?
fn has_real_descendant(report: &Report, hub: &UsbDevice) -> bool {
    report
        .snapshot
        .subtree(&hub.sysfs_name)
        .into_iter()
        .any(|d| d.sysfs_name != hub.sysfs_name && !is_hub(d))
}

pub fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

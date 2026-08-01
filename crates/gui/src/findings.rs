//! Finding rows, and the shared vocabulary for turning model enums into CSS.
//!
//! The honesty rules from `docs/01-gui-concept.md` §8 are enforced here rather
//! than remembered:
//!
//! * [`row`] is the only way a finding reaches the screen, and it always emits
//!   the title, so a coloured dot can never appear without its sentence.
//! * It always emits the confidence chip, so confidence is never collapsed into
//!   a colour.
//! * [`Confidence::Heuristic`] adds a class to the *card*, not a step down the
//!   severity scale — suspicion is a different kind of statement, not a milder
//!   one.

use relm4::gtk::{self, prelude::*};
use usb_probe::model::{Confidence, Finding, Severity, Subject};

/// Severity as a dot class. Kept apart from confidence on purpose.
pub fn severity_class(s: Severity) -> &'static str {
    match s {
        Severity::Critical | Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low => "low",
        Severity::Info => "info",
    }
}

pub fn confidence_class(c: Confidence) -> &'static str {
    match c {
        Confidence::Measured => "measured",
        Confidence::Inferred => "inferred",
        Confidence::Heuristic => "heuristic",
    }
}

/// A small pill of text, e.g. `measured` or `port0 cable`.
pub fn chip(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("chip");
    for c in classes {
        l.add_css_class(c);
    }
    l.set_valign(gtk::Align::Center);
    l
}

/// A coloured dot. Never call this without putting a sentence beside it.
pub fn dot(class: &str) -> gtk::Box {
    let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    d.add_css_class("dot");
    d.add_css_class(class);
    d.set_valign(gtk::Align::Start);
    d.set_size_request(9, 9);
    d
}

/// One finding, as it appears in the *Findings* card.
///
/// `show_subject` is set when the pane can hold statements about more than one
/// subject — a port's pane carries its cable's findings too, and an untagged
/// cable accusation reads as an accusation of the port.
pub fn row(f: &Finding, show_subject: bool) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    outer.add_css_class("finding");
    if f.confidence == Confidence::Heuristic {
        // §8: a different kind of card, not a lower severity.
        outer.add_css_class("heuristic-card");
    }

    outer.append(&dot(severity_class(f.severity)));

    let body = gtk::Box::new(gtk::Orientation::Vertical, 3);
    body.set_hexpand(true);

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let title = gtk::Label::new(Some(&f.title));
    title.add_css_class("finding-title");
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    head.append(&title);
    if show_subject {
        head.append(&chip(&f.subject.display(), &["subject"]));
    }
    head.append(&chip(
        f.confidence.label(),
        &[confidence_class(f.confidence)],
    ));
    body.append(&head);

    if !f.detail.is_empty() {
        body.append(&paragraph(&f.detail, &["dim"]));
    }

    if !f.evidence.is_empty() {
        let ev = gtk::Label::new(Some(&f.evidence.join("\n")));
        ev.add_css_class("evidence");
        ev.set_xalign(0.0);
        ev.set_wrap(true);
        ev.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        ev.set_selectable(true);
        body.append(&ev);
    }

    if let Some(s) = &f.suggestion {
        // "might be defective" must not become "is": the suggestion is shown as
        // the model wrote it, with no imperative styling added on top.
        body.append(&paragraph(s, &["suggestion"]));
    }

    outer.append(&body);
    outer
}

/// One exoneration, as it appears in the *Ruled out* card.
///
/// Visually a different thing from a finding: a tick rather than a severity
/// dot, because an exoneration has no severity — every one of them is Info by
/// construction, and drawing it on the fault scale would be a category error.
pub fn cleared_row(f: &Finding, show_subject: bool) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    outer.add_css_class("cleared-row");

    let tick = gtk::Label::new(Some("\u{2713}"));
    tick.add_css_class("tick");
    tick.set_valign(gtk::Align::Start);
    outer.append(&tick);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 3);
    body.set_hexpand(true);

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let title = gtk::Label::new(Some(&f.title));
    title.add_css_class("finding-title");
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    head.append(&title);
    if show_subject {
        head.append(&chip(&f.subject.display(), &["subject"]));
    }
    head.append(&chip(
        f.confidence.label(),
        &[confidence_class(f.confidence)],
    ));
    body.append(&head);

    if !f.detail.is_empty() {
        body.append(&paragraph(&f.detail, &["dim"]));
    }

    outer.append(&body);
    outer
}

/// A wrapping body paragraph.
pub fn paragraph(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_max_width_chars(64);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

/// A card: a titled surface with an optional right-aligned note.
pub fn card(title: &str, note: Option<&str>, classes: &[&str]) -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    outer.add_css_class("card");
    for c in classes {
        outer.add_css_class(c);
    }

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let h = gtk::Label::new(Some(title));
    h.add_css_class("card-title");
    h.set_xalign(0.0);
    head.append(&h);
    if let Some(n) = note {
        let r = gtk::Label::new(Some(n));
        r.add_css_class("rank");
        r.set_hexpand(true);
        r.set_xalign(1.0);
        r.set_ellipsize(gtk::pango::EllipsizeMode::End);
        head.append(&r);
    }
    outer.append(&head);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    outer.append(&body);
    (outer, body)
}

/// Findings for one subject, worst first. A port's pane also carries the
/// statements about its cable, since `Subject::Cable` has no row of its own.
pub fn about<'a>(findings: &'a [Finding], subject: &Subject) -> Vec<&'a Finding> {
    let mut out: Vec<&Finding> = findings
        .iter()
        .filter(|f| covers(subject, &f.subject))
        .collect();
    out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    out
}

/// Does a pane about `pane` show statements about `f`?
pub fn covers(pane: &Subject, f: &Subject) -> bool {
    match (pane, f) {
        // The cable is the port's, and it is what people came to ask about.
        (Subject::Port(a), Subject::Port(b) | Subject::Cable(b)) => a == b,
        (Subject::Device(a), Subject::Device(b)) => a == b,
        (Subject::Cable(a), Subject::Cable(b)) => a == b,
        (Subject::Host, Subject::Host) => true,
        _ => false,
    }
}

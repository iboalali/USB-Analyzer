//! The detail pane for the selected subject.
//!
//! The order is the point. `docs/02-prior-art.md` established that the
//! bottleneck chain is the one thing three other projects already draw — one of
//! them in this exact toolkit — and that the findings are what none of them
//! has. So the pane runs
//!
//! 1. **verdict** — one sentence, with the codes it rests on;
//! 2. **ruled out** — the exonerations, which on a healthy machine *are* the
//!    content;
//! 3. **findings** — worst first;
//! 4. **evidence** — the chain, under a heading that says it supports a
//!    statement above rather than being one;
//! 5. **what cannot be answered here.**
//!
//! Nothing in this file composes a sentence about the hardware. Every headline
//! is a finding's title and every paragraph is a finding's detail, both
//! verbatim — the one exception is the fixed text for a subject where no rule
//! fired at all, which says exactly that and claims nothing else.

use relm4::gtk::{self, prelude::*};
use usb_probe::chain;
use usb_probe::kind::KindSource;
use usb_probe::model::{
    Finding, KernelLogSource, Outcome, Report, Severity, Subject, Verdict,
};

use crate::findings;

/// What a verdict of "nothing fired" is allowed to say. Deliberately not a
/// clean bill of health: no rule looking at something is not the same as every
/// rule passing over it.
const NOTHING_FIRED: &str = "No rule fired for this subject, in either direction. \
That is weaker than a clean bill of health — it means nothing was concluded here, \
not that everything was checked and passed.";

pub fn build(
    container: &gtk::Box,
    report: &Report,
    subject: &Subject,
    // Only the host pane uses it, and only for the probe catalogue — but it is
    // passed in rather than reached for, so this file keeps having no state.
    probes: &crate::probes::Panel,
    sender: &relm4::Sender<crate::Msg>,
) {
    crate::sidebar::clear(container);

    let mine = findings::about(&report.findings, subject);
    let cleared = findings::about(&report.exonerations, subject);
    // A port's pane carries its cable's statements, so tag them.
    let mixed = matches!(subject, Subject::Port(_));

    container.append(&verdict_block(report, subject, &mine));

    if let Subject::Device(name) = subject {
        if let Some(dev) = report.snapshot.device(name) {
            container.append(&kind_card(dev, sender));
        }
    }

    if !cleared.is_empty() {
        let (card, body) = findings::card(
            "Ruled out",
            Some(&format!(
                "{} check{} that could have failed",
                cleared.len(),
                if cleared.len() == 1 { "" } else { "s" }
            )),
            &["cleared"],
        );
        for f in &cleared {
            body.append(&findings::cleared_row(f, mixed));
        }
        container.append(&card);
    }

    let (card, body) = findings::card("Findings", Some(&rank(&mine)), &[]);
    if mine.is_empty() {
        body.append(&findings::paragraph(
            "Nothing fired for this subject.",
            &["dim"],
        ));
    }
    for f in &mine {
        body.append(&findings::row(f, mixed));
    }
    container.append(&card);

    for c in chains(report, subject) {
        container.append(&chain_card(c));
    }

    let limits = cannot_answer(report, subject);
    if !limits.is_empty() {
        let (card, body) = findings::card("What cannot be answered here", None, &["silent"]);
        for l in limits {
            body.append(&findings::paragraph(&l, &["dim"]));
        }
        container.append(&card);
    }

    // Last, and only on the host: what the tool is permitted to do is a property
    // of the machine, not of a device. Directly after the limits above on
    // purpose — that card says what could not be determined and this one says
    // what would determine it.
    if matches!(subject, Subject::Host) {
        if let Some(card) = crate::probes::card(&report.snapshot, probes, sender) {
            container.append(&card);
        }
    }
}

// ---------------------------------------------------------------------------
// what this is, and correcting it
// ---------------------------------------------------------------------------

/// The device's kind, where it came from, and a control to change it.
///
/// §8: *a device's kind shows where it came from*, and the override is editable
/// in place. A stored override the user cannot see is a belief they cannot
/// correct, and it will outlive their memory of setting it — so when one is
/// applied this card also says what the device itself claimed, right next to
/// it, and offers to put it back.
fn kind_card(dev: &usb_probe::model::UsbDevice, sender: &relm4::Sender<crate::Msg>) -> gtk::Box {
    let kind = dev.kind();
    let declared = dev.declared.clone();
    let overridden = kind.source == KindSource::User;

    let (card, body) = findings::card(
        "What this is",
        Some(kind.source.label()),
        if overridden { &["declared"] } else { &[] },
    );

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);

    let name = gtk::Label::new(Some(kind.kind.label()));
    name.add_css_class("kindname");
    name.set_xalign(0.0);
    name.set_hexpand(true);
    row.append(&name);

    // The control. A plain dropdown of every kind, with the current one
    // selected; changing it writes a label, and "as the device says" clears it.
    let names: Vec<&str> = std::iter::once(AS_DECLARED)
        .chain(crate::KINDS.iter().map(|k| k.label()))
        .collect();
    let choose = gtk::DropDown::from_strings(&names);
    choose.set_valign(gtk::Align::Center);
    choose.set_tooltip_text(Some(
        "Say what this is. Stored against its vendor/product id and applied \
         whenever it is plugged in again.",
    ));
    let selected = match overridden {
        true => crate::KINDS.iter().position(|k| *k == kind.kind).map_or(0, |i| i + 1),
        false => 0,
    };
    choose.set_selected(selected as u32);
    {
        let sender = sender.clone();
        let name = dev.sysfs_name.clone();
        // No "skip the first notify" guard here, and deliberately not.
        // `set_selected` above runs *before* this handler is connected, so
        // building the row cannot fire it. A guard would have nothing to skip
        // on construction and would instead swallow the user's first real
        // choice — which is exactly what it did until this was found by
        // clicking the control and watching the file not change.
        //
        // What is needed instead is the no-op check: the pane is rebuilt on
        // every capture, and re-selecting the value already stored must not
        // rewrite the file with a new timestamp.
        let current = selected;
        choose.connect_selected_notify(move |d| {
            let idx = d.selected() as usize;
            if idx == current {
                return;
            }
            let want = (idx > 0).then(|| crate::KINDS[idx - 1]);
            sender.emit(crate::Msg::SetKind {
                device: name.clone(),
                kind: want,
            });
        });
    }
    row.append(&choose);
    body.append(&row);

    if overridden {
        let says = dev.declared_kind();
        let mut line = format!(
            "You set this. The device itself reports {}.",
            says.kind.label()
        );
        if let Some(d) = &declared {
            line.push_str(&format!(" Stored against {} ({}).", d.id, d.scope_label()));
        }
        body.append(&findings::paragraph(&line, &["dim"]));
    }

    if let Some(note) = declared.as_ref().and_then(|d| d.note.as_deref()) {
        body.append(&findings::paragraph(
            &format!("\u{201c}{note}\u{201d}"),
            &["dim"],
        ));
    }

    if let Some(m) = declared.as_ref().and_then(|d| d.medium) {
        body.append(&findings::paragraph(
            &format!(
                "Medium: {} \u{2014} set by you. The bus does not report this, and the \
                 throughput rule uses it as a yardstick.",
                m.label()
            ),
            &["dim"],
        ));
    }

    card
}

/// The dropdown's first entry: not a kind, but "stop overriding".
pub const AS_DECLARED: &str = "as the device says";

// ---------------------------------------------------------------------------
// verdict
// ---------------------------------------------------------------------------

fn verdict_block(report: &Report, subject: &Subject, mine: &[&Finding]) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    outer.add_css_class("verdict");

    let v = worst_verdict(report, subject);
    let outcome = v.map(|v| v.outcome).unwrap_or(Outcome::Clear);
    outer.add_css_class(match outcome {
        Outcome::Fault => "fault",
        Outcome::Minor => "minor",
        Outcome::Clear => "clear",
    });

    let glyph = gtk::Label::new(Some(match outcome {
        Outcome::Fault | Outcome::Minor => "!",
        Outcome::Clear => "\u{2713}",
    }));
    glyph.add_css_class("glyph");
    glyph.set_valign(gtk::Align::Start);
    outer.append(&glyph);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_hexpand(true);

    let headline = gtk::Label::new(Some(
        v.map(|v| v.headline.as_str())
            .unwrap_or(Verdict::NOTHING_FOUND),
    ));
    headline.add_css_class("headline");
    headline.set_xalign(0.0);
    headline.set_wrap(true);
    headline.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    body.append(&headline);

    // The explanation is the detail of the finding the headline was taken
    // from — never a sentence composed here.
    let detail = v
        .and_then(|v| quoted_from(report, subject, &v.headline))
        .map(|f| f.detail.clone());
    match detail {
        Some(d) if !d.is_empty() => body.append(&findings::paragraph(&d, &["dim"])),
        _ if mine.is_empty() && v.is_none_or(|v| v.because.is_empty()) => {
            body.append(&findings::paragraph(NOTHING_FIRED, &["dim"]))
        }
        _ => {}
    }

    if let Some(v) = v {
        if !v.because.is_empty() {
            let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            chips.add_css_class("because");
            for code in &v.because {
                chips.append(&findings::chip(code, &["code"]));
            }
            body.append(&chips);
        }
    }

    outer.append(&body);
    outer
}

/// The verdict this pane should lead with.
///
/// A port's pane covers the port *and* its cable, and those get separate
/// verdicts. Leading with the port's "all clear" while the cable's verdict says
/// something is wrong would bury the answer, so the worst one wins and ties go
/// to the subject the pane is actually about.
fn worst_verdict<'a>(report: &'a Report, subject: &Subject) -> Option<&'a Verdict> {
    let rank = |o: Outcome| match o {
        Outcome::Fault => 2,
        Outcome::Minor => 1,
        Outcome::Clear => 0,
    };
    report
        .verdicts
        .iter()
        .filter(|v| findings::covers(subject, &v.subject))
        .max_by_key(|v| (rank(v.outcome), (&v.subject == subject) as u8))
}

/// The finding or exoneration a headline was quoted from.
fn quoted_from<'a>(report: &'a Report, subject: &Subject, headline: &str) -> Option<&'a Finding> {
    report
        .findings
        .iter()
        .chain(report.exonerations.iter())
        .filter(|f| findings::covers(subject, &f.subject))
        .find(|f| f.title == headline)
}

fn rank(mine: &[&Finding]) -> String {
    match mine.iter().map(|f| f.severity).max() {
        None => "nothing fired".into(),
        Some(Severity::Info) => "nothing above info".into(),
        Some(s) => format!("worst is {}", s.label().to_lowercase()),
    }
}

// ---------------------------------------------------------------------------
// evidence: the chains
// ---------------------------------------------------------------------------

fn chains(report: &Report, subject: &Subject) -> Vec<chain::Chain> {
    let snap = &report.snapshot;
    match subject {
        Subject::Port(name) | Subject::Cable(name) => snap
            .ports
            .iter()
            .find(|p| &p.name == name)
            .and_then(|p| chain::power(p, &report.findings))
            .into_iter()
            .collect(),
        Subject::Device(name) => snap
            .device(name)
            .and_then(|d| chain::data(snap, d, &report.findings))
            .into_iter()
            .collect(),
        Subject::Host => Vec::new(),
    }
}

fn chain_card(c: chain::Chain) -> gtk::Box {
    let note = match &c.limited_by {
        Some(code) => format!("marked stage comes from {code}"),
        None => "supporting".into(),
    };
    let (card, body) = findings::card(
        &format!("Evidence \u{00b7} {}", c.kind.title()),
        Some(&note),
        &["supporting"],
    );
    let unknown = c.unknown_stages();
    body.append(&crate::chain::area(c));
    // A dashed bar has to be explained or it reads as a rendering fault.
    let tail = if unknown > 0 {
        format!(
            "A dashed stage is one this platform cannot report; {unknown} of them here. \
             The chain is shown because a statement above rests on it, not because it is the point."
        )
    } else {
        "The chain is shown because a statement above rests on it, not because it is the point."
            .to_string()
    };
    body.append(&findings::paragraph(&tail, &["dim", "chain-note"]));
    card
}

// ---------------------------------------------------------------------------
// what cannot be answered
// ---------------------------------------------------------------------------

/// Silence is not an answer (§8): where a question is simply out of reach, the
/// pane says so rather than leaving a gap the user reads as "fine".
fn cannot_answer(report: &Report, subject: &Subject) -> Vec<String> {
    let snap = &report.snapshot;
    let mut out = Vec::new();

    if let Subject::Port(name) | Subject::Cable(name) = subject {
        if let Some(p) = snap.ports.iter().find(|p| &p.name == name) {
            if p.is_attached() && p.cable.is_none() {
                out.push(
                    "This platform does not expose the cable's identity, so nothing here was read \
                     off the cable itself. Any statement about what it can carry is deduced from \
                     the contract, which cannot tell a cable rated exactly at the contract from \
                     one rated higher."
                        .into(),
                );
            }
        }
    }

    if let Subject::Device(name) = subject {
        if snap.device(name).is_some_and(|d| !snap.storage_on(d).is_empty())
            && snap.throughput.is_empty()
        {
            out.push(
                "No throughput was measured. The link rate above is what was negotiated, not what \
                 the device achieves — `usbdiag probe throughput` measures the difference."
                    .into(),
            );
        }
    }

    if snap.kernel_log.source == KernelLogSource::Unavailable {
        let why = snap
            .kernel_log
            .note
            .as_deref()
            .unwrap_or("the kernel log could not be read");
        out.push(format!(
            "Reset and error history is unavailable, so anything that happened before now is \
             invisible here: {why}."
        ));
    }

    if !snap.capabilities.usbmon.is_usable() {
        out.push(format!(
            "Transport error rates were not read: {}. Without them a link that works but retries \
             constantly looks the same as one that does not.",
            snap.capabilities.usbmon.availability.explain()
        ));
    }

    out
}

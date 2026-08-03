//! The probe catalogue, on the host pane.
//!
//! `docs/01-gui-concept.md` §9 promised this needs nothing new: *"the catalogue
//! JSON already carries every probe's class, `ready` flag and `blocker`, so rows
//! can be greyed with the reason in the tooltip without the GUI knowing anything
//! about capabilities."* That held. [`Snapshot::capabilities`] is already in the
//! model and already read by `detail.rs`, so this file adds no plumbing, no
//! detection and no second opinion about what the machine allows —
//! [`Capabilities::blocker`] is asked, and its answer is displayed.
//!
//! # It belongs to the machine, so it lives on the host pane
//!
//! Not a new view and not a header button. What the tool is *allowed* to do is a
//! property of this computer, and the sidebar already has a row for that. Putting
//! it there costs no navigation and no second view model — the thing §5 warned
//! against — and it lands where somebody already goes to ask "what is this
//! machine".
//!
//! It comes last in the pane, after *what cannot be answered here*, because
//! together those two read as one thought: here is what could not be determined,
//! and here is what would determine it.
//!
//! # This runs nothing
//!
//! The viewer reads. Every row is inert, and the card says so in its first line
//! and names the CLI command instead. That is not a placeholder for a missing
//! button — until `pkexec` escalation lands (§9's second item) the app has no
//! honest way to run a probe, and a row that looks clickable and does nothing is
//! worse than a row that admits it. What the panel adds today is the answer to
//! *"what could this tool do here, and why can it not right now"*, which the GUI
//! could not previously answer at all: `cannot_answer` already tells the user
//! that `usbdiag probe throughput` measures the difference, without ever saying
//! whether it could run on this machine.
//!
//! # No dots
//!
//! A dot in this app means severity, and every one is drawn beside a finding's
//! sentence. A probe that cannot run here is not a fault — nothing is wrong with
//! a laptop that will not let a stranger read raw disks — so readiness is a chip
//! and never a dot. Reusing the severity vocabulary would quietly grade the
//! machine.

use relm4::gtk::{self, prelude::*};
use usb_probe::caps::{ProbeClass, Remedy, PROBES};
use usb_probe::model::Snapshot;

use crate::findings;

/// The catalogue, or `None` when there is nothing worth saying.
///
/// Never actually `None` today — the registry is a compile-time constant with
/// five entries — but the signature says the card is optional so that an empty
/// registry renders as absence rather than as an empty box with a title.
pub fn card(snap: &Snapshot) -> Option<gtk::Box> {
    if PROBES.is_empty() {
        return None;
    }
    let caps = &snap.capabilities;
    let ready = PROBES
        .iter()
        .filter(|p| p.implemented && caps.blocker(p).is_none())
        .count();

    let (outer, body) = findings::card(
        "Active probes",
        Some(&format!("{ready} of {} can run here", PROBES.len())),
        &["silent"],
    );

    body.append(&findings::paragraph(
        "This viewer only reads. Nothing below is run from the app — `usbdiag probe NAME` \
         runs one from a terminal, and asks for root only at that point.",
        &["dim"],
    ));

    // Registry order, which escalates by invasiveness: passive, then privileged
    // reads, then the one that takes a device off the bus. Sorting by
    // availability instead would put the disruptive probe above the harmless
    // ones whenever it happened to be usable.
    for p in PROBES {
        body.append(&row(p, snap));
    }

    Some(outer)
}

fn row(p: &usb_probe::caps::ProbeInfo, snap: &Snapshot) -> gtk::Box {
    let blocker = snap.capabilities.blocker(p);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 3);
    outer.add_css_class("probe-row");
    if blocker.is_some() || !p.implemented {
        outer.add_css_class("unavailable");
    }

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let name = gtk::Label::new(Some(p.name));
    name.add_css_class("probe-name");
    name.set_xalign(0.0);
    head.append(&name);

    head.append(&findings::chip(p.class.label(), &[class_css(p.class)]));

    // "Not built yet" is a fact about this program and comes first, because a
    // probe that does not exist cannot be blocked by anything.
    //
    // Everything else names *what is missing*, from `Remedy`, rather than the
    // flat "unavailable here" this started as. The distinction is not cosmetic:
    // "needs something to run it on" is fixed by plugging a disk in, with no
    // password and no reboot, and reads as a completely different instruction
    // from "needs privilege". Collapsing them hid the one blocker on this
    // machine a user could clear in five seconds.
    let (state, css) = if !p.implemented {
        ("not built yet".to_string(), "blocked")
    } else {
        match snap.capabilities.remedy(p) {
            Remedy::Nothing => ("ready".to_string(), "ready"),
            r => (format!("needs {}", r.label()), "blocked"),
        }
    };
    head.append(&findings::chip(&state, &[css]));
    outer.append(&head);

    outer.append(&findings::paragraph(p.summary, &["dim"]));

    // The library's own sentence, verbatim. It repeats the probe's name, which
    // is mildly redundant under a row already titled with it — worth keeping
    // anyway, because the alternative is the GUI reassembling the reason from
    // `needs` and the interface's availability, and then having its own opinion
    // about why something is blocked.
    if let Some(why) = blocker {
        outer.append(&findings::paragraph(&why, &["dim", "blocker"]));
    }

    outer
}

fn class_css(c: ProbeClass) -> &'static str {
    match c {
        // Deliberately the confidence palette rather than the severity one: a
        // disruptive probe is not a fault, it is a stronger kind of action.
        ProbeClass::Passive => "measured",
        ProbeClass::PrivilegedRead => "inferred",
        ProbeClass::Disruptive => "heuristic",
    }
}

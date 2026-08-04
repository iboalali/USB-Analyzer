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
//! # What may be run from here, and what may not
//!
//! A row offers a run when — and only when — **privilege is the whole obstacle**
//! ([`Remedy::Privilege`]), the probe needs no target, and there is a `usbdiag`
//! on this machine that [`escalate::Helper`] is willing to run as root. Every
//! other row explains itself instead:
//!
//! * `needs a kernel module` is not a password problem. Authenticating would
//!   succeed and the probe would still fail, so no button appears — loading a
//!   module is a different act and deserves its own consent.
//! * `needs something to run it on` is fixed by plugging a drive in.
//! * a **disruptive** probe must be pointed at one device, so it cannot be
//!   offered from a panel about the machine. The row says so, and names the
//!   command.
//! * escalation refused for a user-writable binary says which binary and how to
//!   fix it, because "unavailable" would send someone looking for a bug.
//!
//! The app itself never becomes root: each run is a separate process that ends
//! with its answer. See [`usb_probe::escalate`].
//!
//! # No dots
//!
//! A dot in this app means severity, and every one is drawn beside a finding's
//! sentence. A probe that cannot run here is not a fault — nothing is wrong with
//! a laptop that will not let a stranger read raw disks — so readiness is a chip
//! and never a dot. Reusing the severity vocabulary would quietly grade the
//! machine.

use relm4::gtk::{self, prelude::*};
use usb_probe::caps::{ProbeClass, ProbeInfo, Remedy, PROBES};
use usb_probe::escalate::{self, Helper, Unavailable};
use usb_probe::model::{Measured, Snapshot};

use crate::{findings, Msg};

/// Everything the panel needs that is not in the snapshot.
pub struct Panel<'a> {
    /// Measurements this session has paid for, for provenance on the row that
    /// produced each one.
    pub carried: &'a [Measured],
    /// The probe running right now, and whether it has been asked to stop.
    pub running: Option<(&'a str, bool)>,
    /// What the last run of each probe said, where it left something to say.
    pub notes: &'a std::collections::HashMap<&'static str, String>,
}

/// The catalogue, or `None` when there is nothing worth saying.
///
/// Never actually `None` today — the registry is a compile-time constant with
/// five entries — but the signature says the card is optional so that an empty
/// registry renders as absence rather than as an empty box with a title.
pub fn card(snap: &Snapshot, panel: &Panel, sender: &relm4::Sender<Msg>) -> Option<gtk::Box> {
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

    // Asked once per rebuild rather than cached, so installing usbdiag while the
    // window is open is noticed at the next refresh instead of at the next
    // launch. It is a handful of `stat` calls.
    let escalation = Helper::find();

    // Four states, and each says only what is true of itself. An earlier draft
    // had one sentence ending "— see below" for both halves of the failure, and
    // on this machine it pointed at nothing: nothing here is waiting on
    // privilege, so the install message was correctly suppressed and the
    // reference dangled.
    let offers = PROBES.iter().any(|p| offerable(p, snap));
    body.append(&findings::paragraph(
        match (escalation.is_ok(), offers) {
            (true, true) => "A probe that only lacks privilege can be run from here: you are \
                             asked for your password, and only that one probe runs as root. The \
                             app never does.",
            (true, false) => "Nothing below can be started from here — each row says what stands \
                              in its way.",
            (false, true) => "One of these needs only privilege, and could be run from here — but \
                              not from this install:",
            (false, false) => "This viewer only reads, and nothing below can be started from here \
                               either — each row says what stands in its way. `usbdiag probe \
                               NAME` runs one from a terminal.",
        },
        &["dim"],
    ));

    // The reason, and only where somebody could act on it: a machine whose probes
    // are all blocked by a missing module or an absent disk is not waiting on an
    // install, and saying so there would send them off to fix the wrong thing.
    if let (Err(why), true) = (&escalation, offers) {
        body.append(&findings::paragraph(&why.message(), &["dim", "blocker"]));
    }

    // Registry order, which escalates by invasiveness: passive, then privileged
    // reads, then the one that takes a device off the bus. Sorting by
    // availability instead would put the disruptive probe above the harmless
    // ones whenever it happened to be usable.
    for p in PROBES {
        body.append(&row(p, snap, panel, escalation.as_ref(), sender));
    }

    Some(outer)
}

/// Whether root is the whole answer for this probe, and it can be run without
/// naming a device.
///
/// Deliberately independent of whether a helper exists: the panel needs to tell
/// "nothing to offer here" apart from "something to offer, and no way to run it".
fn offerable(p: &ProbeInfo, snap: &Snapshot) -> bool {
    p.implemented
        && p.class != ProbeClass::Disruptive
        && snap.capabilities.remedy(p) == Remedy::Privilege
}

fn row(
    p: &'static ProbeInfo,
    snap: &Snapshot,
    panel: &Panel,
    escalation: Result<&Helper, &Unavailable>,
    sender: &relm4::Sender<Msg>,
) -> gtk::Box {
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

    // Provenance for a reading that cost a password, on the row that produced
    // it. `Measured` is kept by the model rather than read from
    // `Snapshot::carried`, because the report adopted straight from a probe
    // measured its numbers itself and so carries nothing.
    if let Some(m) = panel.carried.iter().find(|m| m.probe == p.name) {
        head.append(&findings::chip(
            &format!("measured {}", ago(m.age_ms(snap))),
            &["measured"],
        ));
    }

    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    head.append(&spacer);

    if let Some(action) = action(p, snap, panel, escalation, sender) {
        head.append(&action);
    }
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

    // Why there is no button on a probe that is otherwise ready to go. Only for
    // the disruptive one: everywhere else the readiness chip has already said it.
    if p.implemented && p.class == ProbeClass::Disruptive {
        outer.append(&findings::paragraph(
            &format!(
                "Cycling a port has to be aimed at one device, so it is not offered from a panel \
                 about the machine: `usbdiag probe {} --target NAME`.",
                p.name
            ),
            &["dim"],
        ));
    }

    if let Some(note) = panel.notes.get(p.name) {
        outer.append(&findings::paragraph(note, &["dim", "probe-note"]));
    }

    outer
}

/// The button, where there is one.
fn action(
    p: &'static ProbeInfo,
    snap: &Snapshot,
    panel: &Panel,
    escalation: Result<&Helper, &Unavailable>,
    sender: &relm4::Sender<Msg>,
) -> Option<gtk::Widget> {
    // Running: the same row grows the way to stop it. Stopping is asking — an
    // unprivileged parent cannot kill a root child — so once asked, the label
    // says what is true and the button goes quiet.
    if let Some((running, asked_to_stop)) = panel.running {
        if running == p.name {
            let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let spinner = gtk::Spinner::new();
            spinner.start();
            box_.append(&spinner);
            let stop = gtk::Button::with_label(if asked_to_stop { "stopping…" } else { "Stop" });
            stop.add_css_class("flat");
            stop.set_sensitive(!asked_to_stop);
            let s = sender.clone();
            stop.connect_clicked(move |_| s.emit(Msg::StopProbe));
            box_.append(&stop);
            return Some(box_.upcast());
        }
    }

    if !offerable(p, snap) || escalation.is_err() {
        return None;
    }

    let button = gtk::Button::with_label("Run as root…");
    button.add_css_class("flat");
    button.add_css_class("run-probe");
    // The ellipsis is not decoration: it promises a dialog before anything
    // happens, and there is one.
    button.set_tooltip_text(Some("Describes what it will do, then asks for your password"));
    // One at a time. Two probes at once would race on the same bus and neither
    // measurement would mean anything.
    button.set_sensitive(panel.running.is_none());
    let s = sender.clone();
    button.connect_clicked(move |_| s.emit(Msg::RunProbe(p.name)));
    Some(button.upcast())
}

/// Rounded, because the exact age of a measurement is never the point — only
/// whether it still describes what is on screen.
fn ago(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=1 => "just now".into(),
        2..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
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

/// The dialog shown before a probe is run, and the consent it collects.
///
/// Ours rather than polkit's, because polkit authenticates *who you are* and
/// never says what is about to happen. The password prompt that follows this one
/// cannot describe a probe, so this has to.
pub fn confirm(
    preview: &usb_probe::probe::Preview,
    helper: &Helper,
    req: &usb_probe::probe::Request,
    parent: &impl IsA<gtk::Widget>,
    sender: &relm4::Sender<Msg>,
) {
    use relm4::adw::{self, prelude::*};

    let name = preview.probe.name;
    let body = format!(
        "{}\n\nYou will be asked for your password, and this is what runs:\n\n{}",
        preview.what_it_does(),
        helper.command_line(req)
    );

    let dialog = adw::AlertDialog::new(Some(&format!("Run {name} as root?")), Some(&body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("run", "Run as root");
    dialog.set_response_appearance("run", adw::ResponseAppearance::Suggested);
    // Cancel is both the default and what Escape means: the safe answer should
    // be the one you get by not deciding.
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let s = sender.clone();
    dialog.connect_response(None, move |_, response| {
        if response == "run" {
            s.emit(Msg::ProbeConfirmed(name));
        }
    });
    dialog.present(Some(parent));
}

/// What an escalated run asks for, when the front end has no reason to ask for
/// anything else.
///
/// Five seconds: long enough for `urb-errors` to see traffic on an idle bus and
/// for a disk to reach a steady read rate, short enough to sit through while a
/// window is blocked. There is no control for it yet, and inventing one before
/// anybody has wanted a different number would be the wrong order.
pub const WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// The request behind the button. Never a target: the panel only offers probes
/// that do not need one, and [`offerable`] is what keeps that true.
pub fn request_for(p: &'static ProbeInfo) -> usb_probe::probe::Request<'static> {
    usb_probe::probe::Request::new(p.name, WINDOW)
}

/// Start a probe as root. The answer arrives later as [`crate::Cmd::Probed`].
///
/// Here rather than in `main.rs` so the whole escalation path — find, spawn, hand
/// the stopper back, wait — reads in one place next to the panel that offers it.
///
/// `Err` is the one case that cannot be reported through a command, because
/// nothing was started: the helper vanished between the dialog and the answer.
pub fn spawn(
    p: &'static ProbeInfo,
    sender: &relm4::ComponentSender<crate::AppModel>,
) -> Result<(), String> {
    let helper = Helper::find().map_err(|e| e.message())?;
    let input = sender.input_sender().clone();
    sender.spawn_command(move |out| {
        let req = request_for(p);
        let outcome = match helper.spawn(&req) {
            // The stopper goes back to the GTK thread *before* the wait, which
            // is the whole reason this is not a oneshot: this thread is about to
            // block until the password prompt is answered and the probe is done.
            Ok(run) => {
                input.emit(Msg::ProbeStarted(p.name, run.stopper()));
                run.wait()
            }
            Err(e) => escalate::Outcome::Failed(format!("usbdiag could not be started: {e}")),
        };
        out.emit(crate::Cmd::Probed {
            probe: p.name,
            outcome: Box::new(outcome),
        });
    });
    Ok(())
}

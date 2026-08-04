//! `usbdiag-gui` — a native GTK4 viewer for `usb-probe`.
//!
//! **Viewer only.** Device tree, detail pane, findings and the bottleneck
//! chain, updating live from udev. No probes, no privilege, no `pkexec`. The
//! window opens and is useful with zero privileges, which is the whole point:
//! running a diagnostic as root so it can read a counter is not a trade worth
//! making for a window that sits open on a desktop. See
//! `docs/01-gui-concept.md` §1 and §3.
//!
//! Reading links the library rather than shelling out to the CLI: typed data,
//! no subprocess per refresh, no JSON round trip. The JSON API exists for the
//! privileged half, which is out of v1 by design.

mod chain;
mod detail;
mod findings;
mod monitor;
mod probes;
mod sidebar;

use std::cell::Cell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use relm4::adw::{self, prelude::*};
use relm4::gtk::{self, glib};
use relm4::{Component, ComponentParts, ComponentSender, RelmApp, WorkerController};

use usb_probe::kind::DeviceKind;
use usb_probe::model::{KernelLog, Outcome, Report, Severity, Subject};
use usb_probe::overrides::{self, Overrides};
use usb_probe::Options;

/// D-Bus / window-class identity. The CLI stays `usbdiag`.
const APP_ID: &str = "com.iboalali.usbdiag";

/// The kinds offered in the override dropdown, in the order they appear.
///
/// `Unknown` is deliberately absent: "I don't know what this is" is what the
/// tool already says on its own, and letting someone store it as a correction
/// would be a label that asserts nothing while still overriding the class code.
pub const KINDS: [DeviceKind; 14] = [
    DeviceKind::Hub,
    DeviceKind::Keyboard,
    DeviceKind::Mouse,
    DeviceKind::InputDevice,
    DeviceKind::Storage,
    DeviceKind::Audio,
    DeviceKind::Camera,
    DeviceKind::Imaging,
    DeviceKind::Printer,
    DeviceKind::Network,
    DeviceKind::SmartcardReader,
    DeviceKind::Billboard,
    DeviceKind::Wireless,
    DeviceKind::Diagnostic,
];

/// How long a kernel log may be reused before it is read again.
///
/// Reading it is by far the most expensive part of a capture — on a machine
/// with `kernel.dmesg_restrict=1` it is a `journalctl` spawn, which costs more
/// than every sysfs read put together. A view that refreshes on every uevent
/// must not spawn a process on every uevent. The cost is bounded and
/// one-directional: log-derived findings lag, but nothing becomes wrong, only
/// late. This is the policy the CLI's watch loop already settled on.
const LOG_REFRESH: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum Msg {
    /// The monitor's subprocess handle, kept so that shutdown can end it. The
    /// worker thread cannot do it itself — see [`monitor::Out::Started`].
    MonitorStarted(usb_probe::monitor::Stopper),
    /// The monitor finished a wait. `events` of 0 is a bare fallback tick.
    Woke { events: usize },
    Source { live: bool, note: Option<String> },
    Select(Subject),
    ToggleHubs,
    /// Explicit refresh, which also re-reads the kernel log.
    Refresh,
    DismissBanner,
    /// Store (or clear, with `None`) what a device is.
    SetKind {
        device: String,
        kind: Option<DeviceKind>,
    },
}

/// Opening window size. Settable from the command line because the narrow
/// presentation is the same widget tree under an `AdwBreakpoint`, and the only
/// way to see it — or to screenshot it — is to actually open a narrow window.
#[derive(Debug, Clone, Copy)]
struct Size {
    width: i32,
    height: i32,
}

impl Default for Size {
    fn default() -> Self {
        Self {
            width: 1120,
            height: 760,
        }
    }
}

#[derive(Debug)]
enum Cmd {
    Captured(Box<Report>),
}

struct AppModel {
    report: Report,
    size: Size,
    selected: Subject,
    show_hubs: bool,
    live: bool,
    note: Option<String>,

    /// A capture is in flight; a second one must not be started under it.
    capturing: bool,
    /// Something changed while a capture was in flight, so run another.
    again: bool,
    /// The previous kernel log, kept so most captures can skip re-reading it.
    cached_log: Option<KernelLog>,
    log_read_at: Instant,
    /// `Snapshot::fingerprint` of what is on screen. It excludes time, I/O
    /// counters and sub-0.5 W battery drift, so the view stays still while
    /// nothing is happening.
    fingerprint: Option<u64>,
    /// Findings already seen, so an arriving fault can be told from a standing
    /// one. Keyed by code and subject, since the same code on two ports is two
    /// different problems.
    seen: HashSet<(String, String)>,
    banner_text: Option<String>,

    /// Bumped whenever the panes need rebuilding. `update_view` runs on every
    /// message, and rebuilding two widget trees for a message that changed
    /// nothing is exactly the flicker the fingerprint exists to prevent.
    revision: u64,
    rendered: Cell<u64>,
    /// Set when a selection should push the content page in collapsed mode.
    show_content: Cell<bool>,

    _monitor: WorkerController<monitor::MonitorWorker>,
    /// Ends the monitor's `udevadm` subprocess at shutdown.
    ///
    /// Held here rather than left to the worker because the worker's thread is
    /// permanently inside a blocking wait and so is never dropped; this lives on
    /// the GTK thread, which does get to run. See [`Component::shutdown`].
    stopper: Option<usb_probe::monitor::Stopper>,
}

impl AppModel {
    /// Where to start: the worst thing on the machine, so the window opens on
    /// the answer rather than on an arbitrary row.
    fn worst_subject(report: &Report) -> Subject {
        let rank = |o: Outcome| match o {
            Outcome::Fault => 2,
            Outcome::Minor => 1,
            Outcome::Clear => 0,
        };
        report
            .verdicts
            .iter()
            .max_by_key(|v| rank(v.outcome))
            .filter(|v| v.outcome != Outcome::Clear)
            .map(|v| v.subject.clone())
            .or_else(|| {
                report
                    .findings
                    .iter()
                    .max_by_key(|f| f.severity)
                    .filter(|f| f.severity >= Severity::Medium)
                    .map(|f| f.subject.clone())
            })
            .unwrap_or(Subject::Host)
    }

    /// Every applied label in a report, for telling "the user just relabelled
    /// something" apart from "nothing happened".
    fn declarations_of(&self, report: &Report) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = report
            .snapshot
            .devices()
            .into_iter()
            .filter_map(|d| {
                d.declared
                    .as_ref()
                    .map(|x| (d.sysfs_name.clone(), format!("{:?}", x)))
            })
            .collect();
        out.sort();
        out
    }

    fn exists(&self, s: &Subject) -> bool {
        let snap = &self.report.snapshot;
        match s {
            Subject::Host => true,
            Subject::Port(n) | Subject::Cable(n) => snap.ports.iter().any(|p| &p.name == n),
            Subject::Device(n) => snap.device(n).is_some(),
        }
    }

    fn start_capture(&mut self, sender: &ComponentSender<Self>, fresh_log: bool) {
        if self.capturing {
            self.again = true;
            return;
        }
        self.capturing = true;
        let reuse = if fresh_log || self.log_read_at.elapsed() >= LOG_REFRESH {
            self.log_read_at = Instant::now();
            None
        } else {
            self.cached_log.clone()
        };
        sender.spawn_oneshot_command(move || {
            let snap = usb_probe::capture_with_log(Options::default(), reuse);
            Cmd::Captured(Box::new(usb_probe::diag::report(snap)))
        });
    }

    /// Adopt a capture, and notice anything that arrived while we were looking.
    fn adopt(&mut self, report: Report) {
        let keys: HashSet<(String, String)> = report
            .findings
            .iter()
            .map(|f| (f.code.clone(), f.subject.display()))
            .collect();

        // Only after the first capture: on startup everything is "new", and a
        // banner announcing a fault that has been there since boot would be a
        // lie about when it happened.
        if self.fingerprint.is_some() {
            self.banner_text = report
                .findings
                .iter()
                .filter(|f| f.severity >= Severity::High)
                .find(|f| !self.seen.contains(&(f.code.clone(), f.subject.display())))
                .map(|f| format!("{} \u{2014} {}", f.subject.display(), f.title));
        }
        self.seen = keys;

        self.report = report;
        if !self.exists(&self.selected) {
            self.selected = Self::worst_subject(&self.report);
        }
        self.revision += 1;
    }

    /// Write a label for a device, then re-read so the change is shown as the
    /// tool sees it rather than as the UI hoped.
    ///
    /// This is the only thing in the GUI that writes anything, and it happens
    /// only because someone changed a dropdown.
    fn set_kind(&mut self, device: &str, kind: Option<DeviceKind>, sender: &ComponentSender<Self>) {
        let Some(dev) = self.report.snapshot.device(device) else {
            return;
        };
        // Model scope. The per-unit escape hatch is a CLI flag rather than a
        // second control here: choosing between "this model" and "this one" is
        // a question most people should not have to answer, and the default is
        // right far more often.
        let Some(id) = overrides::model_id(dev) else {
            self.banner_text =
                Some(format!("{device} reports no vendor/product id, so it cannot be labelled"));
            return;
        };

        let mut store = Overrides::load();
        let existing = store.devices.iter().find(|o| o.id == id).cloned();
        match kind {
            Some(k) => store.set(overrides::Override {
                id: id.clone(),
                kind: Some(k),
                medium: existing.as_ref().and_then(|o| o.medium),
                note: existing.as_ref().and_then(|o| o.note.clone()),
                set_at_unix_ms: now_ms(),
            }),
            // "as the device says" clears only the kind, keeping a medium or a
            // note the user set separately — those are different assertions.
            None => match existing {
                Some(o) if o.medium.is_some() || o.note.is_some() => store.set(overrides::Override {
                    kind: None,
                    ..o
                }),
                _ => {
                    store.forget(&id);
                }
            },
        }

        if let Err(e) = store.save() {
            self.banner_text = Some(format!("Could not save the label: {e}"));
            return;
        }
        self.start_capture(sender, false);
    }

    fn title(&self) -> String {
        let snap = &self.report.snapshot;
        match &self.selected {
            Subject::Host => "This machine".into(),
            Subject::Port(n) | Subject::Cable(n) => n.clone(),
            Subject::Device(n) => snap.device(n).map(|d| d.label()).unwrap_or_else(|| n.clone()),
        }
    }

    fn live_tooltip(&self) -> String {
        match (&self.note, self.live) {
            (Some(n), _) => n.clone(),
            (None, true) => "Updating from udev events".into(),
            (None, false) => "No event source; polling".into(),
        }
    }

    fn subtitle(&self) -> String {
        let snap = &self.report.snapshot;
        match &self.selected {
            Subject::Host => {
                let h = &snap.host;
                [h.sys_vendor.as_deref(), h.product_name.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            Subject::Port(n) | Subject::Cable(n) => snap
                .ports
                .iter()
                .find(|p| &p.name == n)
                .map(|p| {
                    let mut bits = Vec::new();
                    if let Some(loc) = p.physical_location.as_ref().map(|l| l.display()) {
                        if !loc.is_empty() {
                            bits.push(loc);
                        }
                    }
                    if let Some(r) = p.power_role.as_ref().map(|r| r.display()) {
                        bits.push(r);
                    }
                    bits.join(" \u{00b7} ")
                })
                .unwrap_or_default(),
            Subject::Device(n) => snap
                .device(n)
                .map(|d| {
                    let mut bits = vec![n.clone()];
                    // With its provenance, per §8: a kind the user set must not
                    // look like one the device declared.
                    let kind = d.kind();
                    if kind.is_known() {
                        bits.push(kind.describe());
                    }
                    if let Some(s) = &d.speed {
                        bits.push(s.label.clone());
                    }
                    bits.join(" \u{00b7} ")
                })
                .unwrap_or_else(|| n.clone()),
        }
    }
}

/// Let the icon theme find this app's icon when it has not been installed.
///
/// GTK resolves a window icon by *name*, through the theme's search path. An
/// installed app is fine — `install-local.sh` puts the file in
/// `~/.local/share/icons/hicolor` — but a `cargo run` build has its icon sitting
/// in `data/icons/` where nothing looks, so the shell falls back to a generic
/// one and the app appears to have no icon at all.
///
/// Resolved from the executable rather than the working directory: `cargo run`
/// and `./target/debug/usbdiag-gui` are launched from anywhere, and the binary
/// knows where it is. An installed binary has no `data/icons` above it, so the
/// check simply fails and the real theme answers.
fn find_our_icon(root: &adw::ApplicationWindow) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // target/<profile>/usbdiag-gui — three parents up is the repository root.
    let Some(repo) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) else {
        return;
    };
    let icons = repo.join("data/icons");
    if icons.join("hicolor/scalable/apps").is_dir() {
        // `display` is ambiguous here: an ApplicationWindow is both a Root and a
        // Widget, and both traits offer one. Either answers; name a trait so the
        // compiler does not have to guess.
        let display = gtk::prelude::WidgetExt::display(root);
        gtk::IconTheme::for_display(&display).add_search_path(&icons);
    }
}

#[relm4::component]
impl Component for AppModel {
    /// The first capture, plus the window size to open at.
    type Init = (Report, Size);
    type Input = Msg;
    type Output = ();
    type CommandOutput = Cmd;

    view! {
        adw::ApplicationWindow {
            set_title: Some("usbdiag"),
            set_default_width: model.size.width,
            set_default_height: model.size.height,
            set_width_request: 360,
            set_height_request: 420,

            #[name = "split"]
            adw::NavigationSplitView {
                set_min_sidebar_width: 320.0,
                set_max_sidebar_width: 420.0,
                set_sidebar_width_fraction: 0.33,

                #[wrap(Some)]
                set_sidebar = &adw::NavigationPage {
                    set_title: "usbdiag",

                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        add_top_bar = &adw::HeaderBar {
                            #[wrap(Some)]
                            set_title_widget = &adw::WindowTitle {
                                set_title: "usbdiag",
                                #[watch]
                                set_subtitle: &host_line(&model.report),
                            },
                        },

                        #[wrap(Some)]
                        set_content = &gtk::ScrolledWindow {
                            set_hscrollbar_policy: gtk::PolicyType::Never,
                            set_vexpand: true,

                            #[name = "sidebar_box"]
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_spacing: 2,
                                add_css_class: "sidebar-list",
                            },
                        },
                    },
                },

                #[wrap(Some)]
                set_content = &adw::NavigationPage {
                    #[watch]
                    set_title: &model.title(),

                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        add_top_bar = &adw::HeaderBar {
                            #[wrap(Some)]
                            set_title_widget = &adw::WindowTitle {
                                #[watch]
                                set_title: &model.title(),
                                #[watch]
                                set_subtitle: &model.subtitle(),
                            },

                            pack_end = &gtk::Button {
                                set_icon_name: "view-refresh-symbolic",
                                add_css_class: "flat",
                                set_tooltip_text: Some("Read everything again, including the kernel log"),
                                connect_clicked => Msg::Refresh,
                            },

                            pack_end = &gtk::Box {
                                set_spacing: 6,
                                set_valign: gtk::Align::Center,
                                add_css_class: "live",
                                #[watch]
                                set_tooltip_text: Some(&model.live_tooltip()),

                                #[name = "live_dot"]
                                gtk::Box {
                                    add_css_class: "dot",
                                    set_size_request: (8, 8),
                                    set_valign: gtk::Align::Center,
                                    #[watch]
                                    set_css_classes: if model.live {
                                        &["dot", "ok"]
                                    } else {
                                        &["dot", "info"]
                                    },
                                },
                                gtk::Label {
                                    #[watch]
                                    set_label: if model.live { "live" } else { "polling" },
                                },
                            },
                        },

                        #[wrap(Some)]
                        set_content = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,

                            #[name = "banner"]
                            adw::Banner {
                                #[watch]
                                set_title: model.banner_text.as_deref().unwrap_or(""),
                                #[watch]
                                set_revealed: model.banner_text.is_some(),
                                set_button_label: Some("Dismiss"),
                                connect_button_clicked => Msg::DismissBanner,
                            },

                            gtk::ScrolledWindow {
                                set_hscrollbar_policy: gtk::PolicyType::Never,
                                set_vexpand: true,

                                #[name = "detail_box"]
                                gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    set_spacing: 14,
                                    add_css_class: "detail",
                                },
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(
        (report, size): Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Ask for our own icon by name, and make sure the name can be resolved
        // from a build tree. Without both, the shell shows a generic icon.
        find_our_icon(&root);
        root.set_icon_name(Some(APP_ID));

        let monitor = monitor::MonitorWorker::builder()
            .detach_worker(())
            .forward(sender.input_sender(), |out| match out {
                monitor::Out::Started(stopper) => Msg::MonitorStarted(stopper),
                monitor::Out::Source { live, note } => Msg::Source { live, note },
                monitor::Out::Woke { events } => Msg::Woke { events },
            });

        let selected = Self::worst_subject(&report);
        let seen = report
            .findings
            .iter()
            .map(|f| (f.code.clone(), f.subject.display()))
            .collect();

        let model = AppModel {
            report,
            size,
            selected,
            show_hubs: false,
            live: false,
            note: None,
            capturing: false,
            again: false,
            cached_log: None,
            log_read_at: Instant::now(),
            fingerprint: None,
            seen,
            banner_text: None,
            revision: 1,
            rendered: Cell::new(0),
            show_content: Cell::new(false),
            _monitor: monitor,
            stopper: None,
        };

        let widgets = view_output!();

        // Adaptive from the first commit, per §5: below ~500 sp the split view
        // collapses to one pane at a time. Retrofitting this later would mean
        // rewriting every container, and the narrow window is how the tool gets
        // used while cables are being swapped.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            500.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&widgets.split, "collapsed", Some(&true.to_value()));
        root.add_breakpoint(breakpoint);

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            Msg::MonitorStarted(stopper) => self.stopper = Some(stopper),
            Msg::Woke { events } => self.start_capture(&sender, events > 0),
            Msg::Refresh => self.start_capture(&sender, true),
            Msg::Source { live, note } => {
                if self.live != live || self.note != note {
                    self.live = live;
                    self.note = note;
                }
            }
            Msg::Select(s) => {
                if self.selected != s {
                    self.selected = s;
                    self.revision += 1;
                }
                self.show_content.set(true);
            }
            Msg::ToggleHubs => {
                self.show_hubs = !self.show_hubs;
                self.revision += 1;
            }
            Msg::DismissBanner => self.banner_text = None,
            Msg::SetKind { device, kind } => self.set_kind(&device, kind, &sender),
        }
    }

    /// End the `udevadm monitor` subprocess.
    ///
    /// The one place in this program that can. The worker owning the monitor is
    /// always inside a blocking wait, so its thread is killed at exit without
    /// running destructors and `Monitor`'s `Drop` never fires — which orphaned a
    /// `udevadm monitor` on every single run, closing the window included.
    ///
    /// This covers the exits GTK tells us about. A `SIGKILL`, or a `SIGTERM` with
    /// no handler, still leaves the child behind: nothing in-process can catch
    /// those, and the usual answer — `PR_SET_PDEATHSIG` — needs `libc`, which
    /// `usb-probe` deliberately does not depend on.
    fn shutdown(&mut self, _widgets: &mut Self::Widgets, _output: relm4::Sender<Self::Output>) {
        if let Some(stopper) = self.stopper.take() {
            stopper.stop();
        }
    }

    fn update_cmd(&mut self, cmd: Self::CommandOutput, sender: ComponentSender<Self>, _: &Self::Root) {
        let Cmd::Captured(report) = cmd;
        self.capturing = false;

        // Reclaiming the log costs a clone of the event vector. The CLI moves
        // it instead, because it is finished with the report; here the report
        // stays on screen.
        self.cached_log = Some(report.snapshot.kernel_log.clone());

        // The fingerprint covers the hardware, and a label is not hardware — so
        // a capture taken right after one is written would otherwise be
        // discarded as "nothing changed" and the correction would not appear.
        let fingerprint = report.snapshot.fingerprint();
        let labels_changed = self.declarations_of(&report) != self.declarations_of(&self.report);
        if self.fingerprint != Some(fingerprint) || labels_changed {
            self.fingerprint = Some(fingerprint);
            self.adopt(*report);
        }

        if std::mem::take(&mut self.again) {
            self.start_capture(&sender, false);
        }
    }

    /// Spliced onto the end of the generated `update_view`.
    ///
    /// The two panes are rebuilt rather than diffed: the tree is small, and a
    /// factory per row would buy incremental updates at the price of a lot of
    /// machinery for a list that is a dozen items long. What it must not do is
    /// rebuild for a message that changed nothing — that would undo the
    /// stillness the fingerprint gate exists to provide — hence `revision`.
    fn post_view() {
        if self.show_content.take() {
            widgets.split.set_show_content(true);
        }
        if self.rendered.get() != self.revision {
            self.rendered.set(self.revision);
            sidebar::build(
                &widgets.sidebar_box,
                &self.report,
                Some(&self.selected),
                self.show_hubs,
                sender.input_sender(),
            );
                detail::build(
                &widgets.detail_box,
                &self.report,
                &self.selected,
                sender.input_sender(),
            );
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn host_line(report: &Report) -> String {
    let h = &report.snapshot.host;
    match (&h.sys_vendor, &h.product_name) {
        (Some(v), Some(p)) => format!("{} {}", v.trim(), p.trim()),
        (_, Some(p)) => p.trim().to_string(),
        _ => "this machine".into(),
    }
}

const USAGE: &str = "\
usbdiag-gui — native viewer for USB-C cable and Power Delivery diagnosis

Usage: usbdiag-gui [options]

  --width N        opening window width  (default 1120)
  --height N       opening window height (default 760)
  -h, --help       this text
  -V, --version    version

Reads sysfs and the kernel log. Needs no privileges, and has none of the
probes — those live in the `usbdiag probe` CLI, behind an explicit consent gate.
";

/// Parse the handful of options. `Err` is a message; `Ok(None)` means the
/// caller asked for help and there is nothing to run.
fn parse(argv: &[String]) -> Result<Option<Size>, String> {
    let mut size = Size::default();
    let mut it = argv.iter().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<i32, String> {
            it.next()
                .ok_or_else(|| format!("{name} needs a number"))?
                .parse()
                .map_err(|_| format!("{name} needs a number"))
        };
        match arg.as_str() {
            "--width" => size.width = value("--width")?,
            "--height" => size.height = value("--height")?,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("usbdiag-gui {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    if size.width < 360 || size.height < 420 {
        return Err("--width/--height are below the window's minimum (360x420)".into());
    }
    Ok(Some(size))
}

fn main() -> glib::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let size = match parse(&argv) {
        Ok(Some(s)) => s,
        Ok(None) => return glib::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("usbdiag-gui: {e}");
            eprint!("{USAGE}");
            return glib::ExitCode::FAILURE;
        }
    };

    // The first capture happens before the window exists, so it opens with real
    // content rather than flashing an empty tree. It costs one kernel-log read.
    let report = usb_probe::report(Options::default());

    // Hand GTK only the program name: our options are already consumed, and
    // `GtkApplication` rejects any it does not recognise.
    let app = RelmApp::new(APP_ID).with_args(vec![argv[0].clone()]);
    relm4::set_global_css(include_str!("style.css"));
    app.run::<AppModel>((report, size));
    glib::ExitCode::SUCCESS
}

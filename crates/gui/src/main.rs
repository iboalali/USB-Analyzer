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
mod sidebar;

use std::cell::Cell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use relm4::adw::{self, prelude::*};
use relm4::gtk::{self, glib};
use relm4::{Component, ComponentParts, ComponentSender, RelmApp, WorkerController};

use usb_probe::model::{KernelLog, Outcome, Report, Severity, Subject};
use usb_probe::Options;

/// D-Bus / window-class identity. The CLI stays `usbdiag`.
const APP_ID: &str = "com.iboalali.usbdiag";

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
    /// The monitor finished a wait. `events` of 0 is a bare fallback tick.
    Woke { events: usize },
    Source { live: bool, note: Option<String> },
    Select(Subject),
    ToggleHubs,
    /// Explicit refresh, which also re-reads the kernel log.
    Refresh,
    DismissBanner,
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
                    let speed = d.speed.as_ref().map(|s| s.label.clone()).unwrap_or_default();
                    format!("{n} \u{00b7} {speed}")
                })
                .unwrap_or_else(|| n.clone()),
        }
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
        let monitor = monitor::MonitorWorker::builder()
            .detach_worker(())
            .forward(sender.input_sender(), |out| match out {
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
        }
    }

    fn update_cmd(&mut self, cmd: Self::CommandOutput, sender: ComponentSender<Self>, _: &Self::Root) {
        let Cmd::Captured(report) = cmd;
        self.capturing = false;

        // Reclaiming the log costs a clone of the event vector. The CLI moves
        // it instead, because it is finished with the report; here the report
        // stays on screen.
        self.cached_log = Some(report.snapshot.kernel_log.clone());

        let fingerprint = report.snapshot.fingerprint();
        if self.fingerprint != Some(fingerprint) {
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
            detail::build(&widgets.detail_box, &self.report, &self.selected);
        }
    }
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

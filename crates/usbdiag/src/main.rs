//! `usbdiag` — USB-C cable and Power Delivery diagnostics for Linux.
//!
//! All the logic lives in the `usb-probe` library; this binary only parses
//! arguments, calls it, and formats the result.

mod render;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use usb_probe::model::{KernelLog, Report, Severity};
use usb_probe::monitor::{Monitor, Source};
use usb_probe::{kernel, Options};

use render::Theme;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Diag,
    Ports,
    Devices,
    All,
    Watch,
    Json,
}

#[derive(Debug)]
struct Args {
    command: Command,
    json: bool,
    verbose: bool,
    color: Option<bool>,
    raw_log: bool,
    interval_ms: u64,
    sample_ms: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            command: Command::All,
            json: false,
            verbose: false,
            color: None,
            raw_log: false,
            interval_ms: 2000,
            sample_ms: 0,
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&argv) {
        Ok(Some(a)) => a,
        // --help / --version already printed.
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("usbdiag: {e}\nTry 'usbdiag --help'.");
            return ExitCode::from(2);
        }
    };

    let opts = Options {
        kernel: kernel::Options {
            include_unclassified: args.raw_log,
            ..Default::default()
        },
        storage_sample_ms: args.sample_ms,
    };

    if args.command == Command::Watch {
        return watch(&args, opts);
    }

    let report = usb_probe::report(opts);
    let text = format_report(&report, &args);
    print!("{text}");
    let _ = std::io::stdout().flush();

    exit_code(&report)
}

/// Exit non-zero when something actionable was found, so scripts and CI can gate
/// on it: 0 = clean or informational, 1 = a medium-or-worse finding.
fn exit_code(report: &Report) -> ExitCode {
    match report.worst_severity() {
        Some(s) if s >= Severity::Medium => ExitCode::from(1),
        _ => ExitCode::SUCCESS,
    }
}

fn format_report(report: &Report, args: &Args) -> String {
    if args.json {
        return match args.command {
            // `json` emits everything; other commands emit their own slice.
            Command::Ports => json_of(&report.snapshot.ports),
            Command::Devices => json_of(&report.snapshot.buses),
            Command::Diag => json_of(&report.findings),
            _ => json_of(report),
        };
    }

    let theme = Theme {
        color: args
            .color
            .unwrap_or_else(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()),
        verbose: args.verbose,
    };

    let mut out = String::new();
    render::header(&mut out, &report.snapshot, &theme);
    match args.command {
        Command::Ports => {
            render::ports(&mut out, &report.snapshot, &theme);
            render::orphan_pd(&mut out, &report.snapshot, &theme);
            render::battery(&mut out, &report.snapshot, &theme);
            render::thunderbolt(&mut out, &report.snapshot, &theme);
        }
        Command::Devices => {
            render::devices(&mut out, &report.snapshot, &theme);
            render::storage(&mut out, report, &theme);
        }
        Command::Diag => {
            render::findings(&mut out, report, &theme);
            render::summary(&mut out, report, &theme);
        }
        Command::All | Command::Watch => {
            render::ports(&mut out, &report.snapshot, &theme);
            render::orphan_pd(&mut out, &report.snapshot, &theme);
            render::battery(&mut out, &report.snapshot, &theme);
            render::thunderbolt(&mut out, &report.snapshot, &theme);
            render::devices(&mut out, &report.snapshot, &theme);
            render::storage(&mut out, report, &theme);
            render::findings(&mut out, report, &theme);
            render::summary(&mut out, report, &theme);
            render::caveat(&mut out, &theme);
        }
        Command::Json => unreachable!("Json implies --json"),
    }
    out
}

fn json_of<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_string_pretty(v) {
        Ok(mut s) => {
            s.push('\n');
            s
        }
        Err(e) => format!("{{\"error\":\"{e}\"}}\n"),
    }
}

/// A burst of uevents ends when this long passes with no further event.
const DEBOUNCE_MS: u64 = 250;

/// Never coalesce for longer than this. A device in a reset loop emits events
/// continuously and must not be able to hold the display still.
const DEBOUNCE_MAX_MS: u64 = 1500;

/// How often to re-read the kernel log when nothing has happened. Sysfs is
/// cheap and re-read every cycle; the log is a process spawn, so it gets its own
/// slower cadence — and is read immediately whenever a real event arrives.
const LOG_REFRESH_MS: u64 = 10_000;

/// Re-render whenever the observed state changes. Useful for plugging and
/// unplugging: it shows the PD contract settling in real time.
///
/// Driven by uevents where they are available, so a plug shows up immediately
/// rather than up to one poll interval later. `--interval` remains the fallback:
/// the longest this will go without looking at sysfs regardless of events.
fn watch(args: &Args, opts: Options) -> ExitCode {
    let mut args = Args {
        command: Command::All,
        ..*args
    };
    // Colour is on by default here since watch is inherently interactive.
    if args.color.is_none() {
        args.color = Some(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
    }

    let mut monitor = Monitor::start();
    let mut last: Option<u64> = None;
    let mut cached_log: Option<KernelLog> = None;
    let mut log_read_at = Instant::now();
    // Nothing has been read yet, so the first pass reads everything.
    let mut events = 1;

    loop {
        // An event is exactly when new log lines are worth having; otherwise the
        // cached copy stands until it ages out.
        let stale = log_read_at.elapsed() >= Duration::from_millis(LOG_REFRESH_MS);
        let reuse = if events > 0 || stale {
            log_read_at = Instant::now();
            None
        } else {
            cached_log.take()
        };

        let report = usb_probe::diag::report(usb_probe::capture_with_log(opts, reuse));

        // Live throughput is meant to move, so when it was asked for, repaint on
        // every pass. Otherwise repaint only when the state actually differs.
        let fingerprint = report.snapshot.fingerprint();
        if last != Some(fingerprint) || args.sample_ms > 0 {
            last = Some(fingerprint);
            let text = format_report(&report, &args);
            // Clear screen and home the cursor, then repaint.
            print!("\x1b[2J\x1b[H{text}");
            println!("{}", watch_status(&monitor, &args));
            let _ = std::io::stdout().flush();
        }

        // Reclaim the log rather than cloning it: the report is finished with.
        cached_log = Some(report.snapshot.kernel_log);

        events = monitor.wait_for_change(
            Duration::from_millis(args.interval_ms),
            Duration::from_millis(DEBOUNCE_MS),
            Duration::from_millis(DEBOUNCE_MAX_MS),
        );
    }
}

fn watch_status(monitor: &Monitor, args: &Args) -> String {
    let how = match monitor.source() {
        Source::Udev => format!("udev events, {}ms fallback", args.interval_ms),
        Source::TimerOnly => format!(
            "polling every {}ms — {}",
            args.interval_ms,
            monitor.note().unwrap_or("no event source")
        ),
    };
    format!("watching — press Ctrl-C to stop ({how})")
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn parse(argv: &[String]) -> Result<Option<Args>, String> {
    let mut args = Args::default();
    let mut saw_command = false;
    let mut i = 0;

    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "-h" | "--help" => {
                print!("{}", help());
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("usbdiag {VERSION}");
                return Ok(None);
            }
            "-j" | "--json" => args.json = true,
            "-v" | "--verbose" => args.verbose = true,
            "--no-color" => args.color = Some(false),
            "--color" => args.color = Some(true),
            "--raw-log" => args.raw_log = true,
            "--sample" => {
                i += 1;
                let v = argv.get(i).ok_or("--sample needs a value in milliseconds")?;
                args.sample_ms = v
                    .parse()
                    .map_err(|_| format!("not a number of milliseconds: {v}"))?;
                if args.sample_ms < 100 {
                    return Err("--sample must be at least 100 ms to measure anything".into());
                }
            }
            "--interval" => {
                i += 1;
                let v = argv.get(i).ok_or("--interval needs a value in milliseconds")?;
                args.interval_ms = v
                    .parse()
                    .map_err(|_| format!("not a number of milliseconds: {v}"))?;
                if args.interval_ms < 100 {
                    return Err("--interval must be at least 100 ms".into());
                }
            }
            _ if a.starts_with('-') => return Err(format!("unknown option: {a}")),
            _ => {
                if saw_command {
                    return Err(format!("unexpected argument: {a}"));
                }
                args.command = match a {
                    "diag" => Command::Diag,
                    "ports" => Command::Ports,
                    "devices" | "tree" => Command::Devices,
                    "all" => Command::All,
                    "watch" => Command::Watch,
                    "json" => Command::Json,
                    _ => return Err(format!("unknown command: {a}")),
                };
                saw_command = true;
            }
        }
        i += 1;
    }

    if args.command == Command::Json {
        args.json = true;
    }
    Ok(Some(args))
}

fn help() -> String {
    format!(
        "\
usbdiag {VERSION} — USB-C cable and Power Delivery diagnostics for Linux

USAGE
    usbdiag [OPTIONS] [COMMAND]

COMMANDS
    all        Ports, topology and findings (default)
    ports      Type-C ports: roles, PD contract, cable e-marker, alt modes
    devices    USB topology, plus storage speed, why, and live throughput
    diag       Findings only
    watch      Re-render on change, driven by uevents — plug a cable in and
               watch it negotiate
    json       Full snapshot and findings as JSON

OPTIONS
    -j, --json          JSON output (also narrows to the command's own data)
    -v, --verbose       Show evidence, per-port detail and interface bindings
        --raw-log       Keep kernel lines that matched no known pattern
        --color         Force ANSI colour
        --no-color      Disable ANSI colour (also honours NO_COLOR)
        --interval MS   Fallback refresh for watch, default 2000. Watch is
                        event-driven; this is the longest it will go without
                        looking anyway
        --sample MS     Measure live storage throughput over this window
                        (costs exactly this much wall-clock; off by default)
    -h, --help          This text
    -V, --version       Version

EXIT STATUS
    0   nothing actionable found
    1   at least one medium-or-worse finding
    2   bad usage

NOTES
    Runs unprivileged. On systems with kernel.dmesg_restrict=1 the kernel ring
    buffer is read via journalctl; if neither that nor /dev/kmsg is readable, the
    reset-history rules are skipped and that is reported as a finding.

    watch uses 'udevadm monitor' for change notification, and falls back to
    plain polling at --interval if udevadm is not available.

    Cable capability can only be read from an e-marker chip. Signal integrity and
    the true rating of an unmarked cable need a hardware analyzer.
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Option<Args>, String> {
        parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn defaults_to_the_all_command() {
        let a = p(&[]).unwrap().unwrap();
        assert_eq!(a.command, Command::All);
        assert!(!a.json);
    }

    #[test]
    fn parses_commands_and_flags() {
        let a = p(&["ports", "-v", "--json"]).unwrap().unwrap();
        assert_eq!(a.command, Command::Ports);
        assert!(a.verbose && a.json);

        let a = p(&["tree"]).unwrap().unwrap();
        assert_eq!(a.command, Command::Devices);
    }

    #[test]
    fn json_command_implies_json_output() {
        let a = p(&["json"]).unwrap().unwrap();
        assert!(a.json);
    }

    #[test]
    fn flags_may_precede_the_command() {
        let a = p(&["--no-color", "diag"]).unwrap().unwrap();
        assert_eq!(a.command, Command::Diag);
        assert_eq!(a.color, Some(false));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&["--nope"]).is_err());
        assert!(p(&["nope"]).is_err());
        assert!(p(&["ports", "devices"]).is_err());
        assert!(p(&["--interval"]).is_err());
        assert!(p(&["--interval", "abc"]).is_err());
        assert!(p(&["--interval", "10"]).is_err());
    }

    #[test]
    fn interval_is_accepted() {
        let a = p(&["watch", "--interval", "500"]).unwrap().unwrap();
        assert_eq!(a.interval_ms, 500);
        assert_eq!(a.command, Command::Watch);
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(p(&["--help"]).unwrap().is_none());
        assert!(p(&["-V"]).unwrap().is_none());
    }

    #[test]
    fn exit_code_reflects_worst_finding() {
        use usb_probe::model::*;
        let snap = usb_probe::capture(Options::default());
        let clean = Report {
            snapshot: snap.clone(),
            findings: vec![],
        };
        assert_eq!(format!("{:?}", exit_code(&clean)), format!("{:?}", ExitCode::SUCCESS));

        let info_only = Report {
            snapshot: snap.clone(),
            findings: vec![Finding {
                code: "X".into(),
                severity: Severity::Info,
                confidence: Confidence::Measured,
                subject: Subject::Host,
                title: "t".into(),
                detail: "d".into(),
                evidence: vec![],
                suggestion: None,
            }],
        };
        assert_eq!(
            format!("{:?}", exit_code(&info_only)),
            format!("{:?}", ExitCode::SUCCESS)
        );

        let bad = Report {
            snapshot: snap,
            findings: vec![Finding {
                code: "Y".into(),
                severity: Severity::High,
                confidence: Confidence::Measured,
                subject: Subject::Host,
                title: "t".into(),
                detail: "d".into(),
                evidence: vec![],
                suggestion: None,
            }],
        };
        assert_eq!(format!("{:?}", exit_code(&bad)), format!("{:?}", ExitCode::from(1)));
    }

    /// The renderers must survive a real capture without panicking, in both
    /// plain and verbose modes.
    #[test]
    fn renders_a_real_capture() {
        let report = usb_probe::report(Options::default());
        for verbose in [false, true] {
            let args = Args {
                command: Command::All,
                color: Some(false),
                verbose,
                ..Default::default()
            };
            let text = format_report(&report, &args);
            assert!(text.contains("USB-C PORTS"));
            assert!(text.contains("USB TOPOLOGY"));
            assert!(text.contains("FINDINGS"));
            assert!(!text.contains('\x1b'), "no colour when disabled");
        }
    }

    #[test]
    fn json_output_is_valid_json() {
        let report = usb_probe::report(Options::default());
        for command in [Command::Json, Command::Ports, Command::Devices, Command::Diag] {
            let args = Args {
                command,
                json: true,
                ..Default::default()
            };
            let text = format_report(&report, &args);
            serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or_else(|e| panic!("{command:?} produced invalid JSON: {e}"));
        }
    }
}

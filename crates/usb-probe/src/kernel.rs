//! Kernel ring buffer, filtered down to USB events that matter for diagnosis.
//!
//! This is the layer that catches the failures sysfs cannot show: sysfs reports
//! the state a link *settled into*, while the ring buffer records the fights it
//! had getting there. Repeated resets on a device that "works" are the classic
//! marginal-cable signature.
//!
//! Three sources, tried in order:
//!
//! 1. `/dev/kmsg` — best, but blocked by `kernel.dmesg_restrict=1` unless root.
//! 2. `journalctl -k -b 0` — usually works unprivileged if the user can read the
//!    journal, which is the common desktop configuration.
//! 3. `dmesg` — last resort, same restriction as `/dev/kmsg`.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;

use crate::model::{EventKind, KernelEvent, KernelLog, KernelLogSource, Severity};

/// Linux `O_NONBLOCK`; hardcoded to avoid a libc dependency.
const O_NONBLOCK: i32 = 0o4000;

/// Guard against an endless loop if the ring buffer keeps being overwritten.
const MAX_RECORDS: usize = 200_000;

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Keep lines that matched no known pattern (for a raw view).
    pub include_unclassified: bool,
    /// Cap on retained events, newest kept. 0 means unlimited.
    pub limit: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            include_unclassified: false,
            limit: 4000,
        }
    }
}

/// Collect and classify USB-relevant kernel messages.
pub fn collect(opts: Options) -> KernelLog {
    let mut notes = Vec::new();

    match read_dev_kmsg() {
        Ok(lines) => return build(KernelLogSource::DevKmsg, lines, opts, None),
        Err(e) => notes.push(format!("/dev/kmsg: {e}")),
    }

    match read_journalctl() {
        Ok(lines) => {
            return build(
                KernelLogSource::Journalctl,
                lines,
                opts,
                Some(format!(
                    "read via journalctl ({}); run as root for /dev/kmsg",
                    notes.join(", ")
                )),
            )
        }
        Err(e) => notes.push(format!("journalctl: {e}")),
    }

    match read_dmesg() {
        Ok(lines) => {
            return build(
                KernelLogSource::Dmesg,
                lines,
                opts,
                Some(format!("read via dmesg ({})", notes.join(", "))),
            )
        }
        Err(e) => notes.push(format!("dmesg: {e}")),
    }

    KernelLog::unavailable(format!(
        "no kernel log source available: {}. Reset/enumeration history will be missing; \
         re-run with sudo or add your user to the systemd-journal group.",
        notes.join("; ")
    ))
}

/// A source-independent log line: optional timestamp plus the message body.
type Line = (Option<String>, String);

fn build(
    source: KernelLogSource,
    lines: Vec<Line>,
    opts: Options,
    note: Option<String>,
) -> KernelLog {
    let mut events: Vec<KernelEvent> = lines
        .into_iter()
        .filter(|(_, text)| is_usb_related(text))
        .filter_map(|(timestamp, text)| {
            let (kind, device) = classify(&text);
            if kind == EventKind::Other && !opts.include_unclassified {
                return None;
            }
            Some(KernelEvent {
                severity: severity_of(kind),
                kind,
                device,
                timestamp,
                text,
            })
        })
        .collect();

    if opts.limit > 0 && events.len() > opts.limit {
        events.drain(..events.len() - opts.limit);
    }

    KernelLog {
        source,
        note,
        events,
    }
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

fn read_dev_kmsg() -> Result<Vec<Line>, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open("/dev/kmsg")
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    for _ in 0..MAX_RECORDS {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(line) = parse_kmsg_record(&String::from_utf8_lossy(&buf[..n])) {
                    out.push(line);
                }
            }
            // Ring buffer drained.
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            // EPIPE: records were overwritten while reading; the next read resumes.
            Err(e) if e.raw_os_error() == Some(32) => continue,
            Err(e) => {
                if out.is_empty() {
                    return Err(e.to_string());
                }
                break;
            }
        }
    }
    Ok(out)
}

/// `<prio>,<seq>,<usec_since_boot>,<flags>[,..];<message>`, plus indented
/// continuation lines we don't need.
fn parse_kmsg_record(record: &str) -> Option<Line> {
    let (meta, rest) = record.split_once(';')?;
    let text = rest.lines().next()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let usec: u64 = meta.split(',').nth(2)?.parse().ok()?;
    let secs = usec / 1_000_000;
    let frac = usec % 1_000_000;
    Some((Some(format!("[{secs:>6}.{frac:06}]")), text))
}

fn read_journalctl() -> Result<Vec<Line>, String> {
    let out = Command::new("journalctl")
        .args(["-k", "-b", "0", "--no-pager", "-o", "short-precise"])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().filter_map(parse_journal_line).collect())
}

/// `Jul 31 22:52:14.531674 hostname kernel: usb 3-4: reset ...`
fn parse_journal_line(line: &str) -> Option<Line> {
    let (head, text) = line.split_once(" kernel: ")?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // Drop the trailing hostname token from the head to leave the timestamp.
    let timestamp = head
        .rsplit_once(' ')
        .map(|(ts, _host)| ts.to_string())
        .unwrap_or_else(|| head.to_string());
    Some((Some(timestamp), text.to_string()))
}

fn read_dmesg() -> Result<Vec<Line>, String> {
    let out = Command::new("dmesg")
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().filter_map(parse_dmesg_line).collect())
}

/// `[ 1234.567890] usb 3-4: reset ...`
fn parse_dmesg_line(line: &str) -> Option<Line> {
    if let Some(rest) = line.strip_prefix('[') {
        if let Some((ts, text)) = rest.split_once(']') {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            return Some((Some(format!("[{}]", ts.trim())), text.to_string()));
        }
    }
    let t = line.trim();
    if t.is_empty() {
        None
    } else {
        Some((None, t.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

fn is_usb_related(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const PREFIXES: [&str; 9] = [
        "usb ", "usb-storage", "hub ", "xhci", "ehci", "uas ", "typec", "ucsi", "tcpm",
    ];
    PREFIXES.iter().any(|p| lower.starts_with(p))
        || lower.contains("usb cable")
        || lower.contains("usb device")
}

/// Map a message to an event kind and the device it concerns.
fn classify(text: &str) -> (EventKind, Option<String>) {
    let lower = text.to_ascii_lowercase();
    let (prefix, _) = text.split_once(": ").unwrap_or((text, ""));
    let device = extract_device(prefix);

    // Order matters: the kernel's explicit cable blame wins over everything, and
    // enumeration failures are checked before resets because a failing
    // enumeration also logs resets.
    let kind = if lower.contains("maybe the usb cable is bad") {
        EventKind::CableSuspect
    } else if lower.contains("device descriptor read")
        || lower.contains("unable to enumerate usb device")
        || lower.contains("device not accepting address")
        || lower.contains("device not responding to setup address")
        || lower.contains("no response for device descriptor")
        || lower.contains("can't read configurations")
    {
        EventKind::EnumerationFailure
    } else if lower.contains("over-current") || lower.contains("overcurrent") {
        EventKind::OverCurrent
    } else if lower.contains("insufficient available bus power")
        || lower.contains("not enough power")
    {
        EventKind::InsufficientPower
    } else if lower.contains("no bandwidth for new device")
        || lower.contains("not enough bandwidth")
    {
        EventKind::InsufficientBandwidth
    } else if lower.contains("host controller not responding")
        || lower.contains("hc died")
        || lower.contains("host not responding")
        || lower.contains("host system error")
        || lower.contains("assume dead")
    {
        EventKind::HostControllerFailure
    } else if lower.contains("connect-debounce failed")
        || lower.contains("cannot enable")
        || lower.contains("link is stuck")
        || lower.contains("warm reset")
    {
        EventKind::LinkTrainingFailure
    } else if lower.contains("reset") && lower.contains("usb device") {
        EventKind::DeviceReset
    } else if lower.contains("usb disconnect") {
        EventKind::Disconnect
    } else if lower.starts_with("typec") || lower.starts_with("ucsi") || lower.starts_with("tcpm") {
        EventKind::TypecEvent
    } else {
        EventKind::Other
    };

    (kind, device)
}

/// Pull a normalized USB device path out of a message prefix.
///
/// `usb 3-4` -> `3-4`, `usb 3-4-port2` -> `3-4`, `usb-storage 3-5:1.0` -> `3-5`,
/// `hub 3-0:1.0` -> `usb3`, `xhci_hcd 0000:c4:00.4` -> None.
fn extract_device(prefix: &str) -> Option<String> {
    let token = prefix.split_whitespace().last()?;

    // Interface suffix, e.g. `3-5:1.0`.
    let token = token.split(':').next()?;
    // Hub port suffix, e.g. `3-4-port2`.
    let token = match token.find("-port") {
        Some(i) => &token[..i],
        None => token,
    };

    if token.starts_with("usb") && token[3..].chars().all(|c| c.is_ascii_digit()) {
        return Some(token.to_string());
    }

    let (bus, path) = token.split_once('-')?;
    if bus.is_empty() || !bus.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if path.is_empty() || !path.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    // `3-0` is the root hub's own interface; name it as the bus.
    if path == "0" {
        return Some(format!("usb{bus}"));
    }
    Some(token.to_string())
}

fn severity_of(kind: EventKind) -> Severity {
    match kind {
        EventKind::CableSuspect | EventKind::EnumerationFailure => Severity::High,
        EventKind::OverCurrent | EventKind::HostControllerFailure => Severity::High,
        EventKind::LinkTrainingFailure
        | EventKind::InsufficientPower
        | EventKind::InsufficientBandwidth => Severity::Medium,
        // A single reset is normal; the rule engine escalates on repetition.
        EventKind::DeviceReset => Severity::Low,
        EventKind::Disconnect | EventKind::TypecEvent | EventKind::Other => Severity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kmsg_record() {
        let rec = "6,1234,98765432,-;usb 3-4: reset full-speed USB device number 2 using xhci_hcd\n";
        let (ts, text) = parse_kmsg_record(rec).unwrap();
        assert_eq!(ts.as_deref(), Some("[    98.765432]"));
        assert!(text.starts_with("usb 3-4: reset"));
    }

    /// Real `journalctl -k -o short-precise` output, which is the fallback source
    /// on any system with `kernel.dmesg_restrict=1`. Note the hostname token
    /// between the timestamp and `kernel:` — the parser has to drop it.
    #[test]
    fn parses_journal_line() {
        let line = "Jul 31 22:52:14.531674 host-with-a-long-name kernel: usb 3-4: reset full-speed USB device number 2 using xhci_hcd";
        let (ts, text) = parse_journal_line(line).unwrap();
        assert_eq!(ts.as_deref(), Some("Jul 31 22:52:14.531674"));
        assert_eq!(
            text,
            "usb 3-4: reset full-speed USB device number 2 using xhci_hcd"
        );
    }

    #[test]
    fn parses_dmesg_line() {
        let (ts, text) = parse_dmesg_line("[   12.345678] usb 1-1: USB disconnect, device number 2").unwrap();
        assert_eq!(ts.as_deref(), Some("[12.345678]"));
        assert!(text.contains("USB disconnect"));
    }

    #[test]
    fn extracts_device_paths() {
        assert_eq!(extract_device("usb 3-4"), Some("3-4".into()));
        assert_eq!(extract_device("usb 3-5.1"), Some("3-5.1".into()));
        assert_eq!(extract_device("usb 3-4-port2"), Some("3-4".into()));
        assert_eq!(extract_device("usb-storage 3-5:1.0"), Some("3-5".into()));
        assert_eq!(extract_device("hub 3-0:1.0"), Some("usb3".into()));
        assert_eq!(extract_device("usb usb3"), Some("usb3".into()));
        assert_eq!(extract_device("xhci_hcd 0000"), None);
    }

    #[test]
    fn classifies_the_reset_storm_seen_on_this_machine() {
        let (kind, dev) = classify("usb 3-4: reset full-speed USB device number 2 using xhci_hcd");
        assert_eq!(kind, EventKind::DeviceReset);
        assert_eq!(dev.as_deref(), Some("3-4"));
        assert_eq!(severity_of(kind), Severity::Low);
    }

    #[test]
    fn classifies_enumeration_failure_before_reset() {
        let (kind, dev) = classify("usb 2-1: device descriptor read/64, error -71");
        assert_eq!(kind, EventKind::EnumerationFailure);
        assert_eq!(dev.as_deref(), Some("2-1"));
    }

    #[test]
    fn classifies_explicit_cable_blame() {
        let (kind, _) = classify(
            "usb usb2-port1: Cannot enable. Maybe the USB cable is bad?",
        );
        assert_eq!(kind, EventKind::CableSuspect);
        assert_eq!(severity_of(kind), Severity::High);
    }

    #[test]
    fn classifies_host_controller_death() {
        let (kind, dev) =
            classify("xhci_hcd 0000:c4:00.4: xHCI host controller not responding, assume dead");
        assert_eq!(kind, EventKind::HostControllerFailure);
        assert_eq!(dev, None);
    }

    #[test]
    fn classifies_power_and_bandwidth_limits() {
        assert_eq!(
            classify("usb 1-1: rejected 1 configuration due to insufficient available bus power").0,
            EventKind::InsufficientPower
        );
        assert_eq!(
            classify("usb 2-1: Not enough bandwidth for new device state").0,
            EventKind::InsufficientBandwidth
        );
    }

    #[test]
    fn ignores_unrelated_lines() {
        assert!(!is_usb_related("wlp194s0: associated"));
        assert!(is_usb_related("usb 3-4: reset"));
        assert!(is_usb_related("ucsi_acpi USBC000:00: PPM init failed"));
    }

    #[test]
    fn unclassified_lines_are_dropped_by_default() {
        let lines = vec![
            (None, "usb 3-4: SerialNumber: ABC123".to_string()),
            (None, "usb 3-4: reset full-speed USB device number 2".to_string()),
        ];
        let log = build(KernelLogSource::Dmesg, lines.clone(), Options::default(), None);
        assert_eq!(log.events.len(), 1);

        let raw = build(
            KernelLogSource::Dmesg,
            lines,
            Options {
                include_unclassified: true,
                limit: 0,
            },
            None,
        );
        assert_eq!(raw.events.len(), 2);
    }

    #[test]
    fn limit_keeps_the_newest_events() {
        let lines: Vec<Line> = (0..10)
            .map(|i| (None, format!("usb 3-{i}: reset full-speed USB device number {i}")))
            .collect();
        let log = build(
            KernelLogSource::Dmesg,
            lines,
            Options {
                include_unclassified: false,
                limit: 3,
            },
            None,
        );
        assert_eq!(log.events.len(), 3);
        assert_eq!(log.events[2].device.as_deref(), Some("3-9"));
    }
}

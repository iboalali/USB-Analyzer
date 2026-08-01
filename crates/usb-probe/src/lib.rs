//! Read-only USB / USB-C / Power Delivery inspection for Linux, with a
//! diagnostic rule engine aimed at cable and charging problems.
//!
//! # Scope
//!
//! Everything here comes from sysfs and the kernel ring buffer. That means the
//! crate can tell you, with certainty, what a link **negotiated** and what each
//! side **advertised** — and it can read a cable's e-marker when one exists.
//!
//! It cannot measure the cable electrically. Signal integrity, eye diagrams,
//! CC-line voltages, and the true rating of an unmarked cable are invisible to
//! software and need a hardware analyzer. Findings about cables are therefore
//! marked [`Confidence::Inferred`] or [`Confidence::Heuristic`], never
//! [`Confidence::Measured`], unless they come straight off an e-marker or a
//! hardware counter.
//!
//! # Usage
//!
//! ```no_run
//! let report = usb_probe::report(usb_probe::Options::default());
//! for f in &report.findings {
//!     println!("[{}] {}", f.severity.label(), f.title);
//! }
//! ```
//!
//! Nothing here requires root. Without root the kernel ring buffer may be
//! unreadable on systems with `kernel.dmesg_restrict=1`, which disables the
//! reset-history rules; that is reported as a finding rather than hidden.

pub mod block;
pub mod diag;
pub mod drm;
pub mod kernel;
pub mod model;
pub mod monitor;
pub mod pd;
pub mod sysfs;
pub mod thunderbolt;
pub mod typec;
pub mod usb;
pub mod vdo;

#[cfg(test)]
mod test_support;

pub use model::*;

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// What to include in a capture.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub kernel: kernel::Options,
    /// Milliseconds to sample block I/O for a live throughput figure. 0 skips
    /// it — the measurement costs exactly this much wall-clock, since a rate
    /// cannot be derived from cumulative counters without a time base.
    pub storage_sample_ms: u64,
}

/// Read the current state of the system.
pub fn capture(opts: Options) -> Snapshot {
    capture_with_log(opts, None)
}

/// Read the current state, optionally reusing a kernel log read earlier.
///
/// Reading the log is by far the most expensive part of a capture: on a machine
/// with `kernel.dmesg_restrict=1` it is a `journalctl` process spawn, which
/// costs more than everything else here put together. A live view that refreshes
/// sysfs on every uevent can hand the previous log back in and re-read it on its
/// own, slower cadence.
///
/// The cost of doing that is bounded and one-directional: findings derived from
/// the log lag by however long the caller waits. Nothing becomes *wrong*, only
/// late — log events are append-only, so a stale copy is a prefix of the truth.
pub fn capture_with_log(opts: Options, log: Option<KernelLog>) -> Snapshot {
    let ports = typec::read_ports();
    let (batteries, mains_online) = pd::read_batteries();

    // Any PD object not already reachable from a port, so nothing is lost.
    let referenced: BTreeSet<String> = ports
        .iter()
        .flat_map(|p| {
            [
                p.local_pd.as_ref().map(|pd| pd.name.clone()),
                p.partner
                    .as_ref()
                    .and_then(|pt| pt.pd.as_ref())
                    .map(|pd| pd.name.clone()),
            ]
        })
        .flatten()
        .collect();
    let orphan_pd = pd::read_all()
        .into_iter()
        .filter(|(name, _)| !referenced.contains(name))
        .map(|(_, v)| v)
        .collect();

    Snapshot {
        captured_at_unix_ms: now_ms(),
        host: read_host(),
        buses: usb::read_buses(),
        ports,
        thunderbolt: thunderbolt::read(),
        block_devices: if opts.storage_sample_ms > 0 {
            block::sample(std::time::Duration::from_millis(opts.storage_sample_ms))
        } else {
            block::read()
        },
        batteries,
        displays: drm::read(),
        mains_online,
        uptime_s: read_uptime_s(),
        orphan_pd,
        kernel_log: log.unwrap_or_else(|| kernel::collect(opts.kernel)),
    }
}

/// Capture and analyze in one step.
pub fn report(opts: Options) -> Report {
    diag::report(capture(opts))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Seconds since boot, from `/proc/uptime`.
fn read_uptime_s() -> Option<f64> {
    sysfs::read_str("/proc/uptime")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn read_host() -> Host {
    Host {
        kernel_release: sysfs::read_str("/proc/sys/kernel/osrelease"),
        product_name: sysfs::read_str("/sys/class/dmi/id/product_name"),
        sys_vendor: sysfs::read_str("/sys/class/dmi/id/sys_vendor"),
        typec_drivers: read_typec_drivers(),
    }
}

/// Loaded modules that implement Type-C support. Which one is in use decides how
/// much PD detail the kernel can expose, so it is worth reporting.
fn read_typec_drivers() -> Vec<String> {
    let Some(modules) = sysfs::read_str("/proc/modules") else {
        return Vec::new();
    };
    let mut out: Vec<String> = modules
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|m| {
            ["typec", "ucsi", "tcpm", "tcpci", "fusb302", "pd_"]
                .iter()
                .any(|k| m.contains(k))
        })
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model must round-trip through JSON, since that is the wire format for
    /// any out-of-process front end.
    #[test]
    fn snapshot_round_trips_through_json() {
        let snap = test_support::empty_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.host.kernel_release, snap.host.kernel_release);
    }

    #[test]
    fn vdo_serializes_as_hex_string() {
        let v = Vdo::new(0x2108_2042);
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"0x21082042\""), "got {json}");
        let back: Vdo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.raw, 0x2108_2042);
    }

    #[test]
    fn report_round_trips_through_json() {
        let mut snap = test_support::empty_snapshot();
        snap.ports.push(test_support::charging_port(100_000, Some(3000), 20_000, 3000));
        let rep = diag::report(snap);
        assert!(!rep.findings.is_empty());
        let json = serde_json::to_string(&rep).unwrap();
        let back: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(back.findings.len(), rep.findings.len());
        assert_eq!(back.worst_severity(), rep.worst_severity());
    }

    /// Handing a log back in must use it verbatim, not merge or re-read it —
    /// that is the whole point of the call, and a silent re-read would put the
    /// `journalctl` spawn back into every cycle of a live view.
    #[test]
    fn a_supplied_kernel_log_is_used_as_is() {
        let supplied = KernelLog {
            source: KernelLogSource::Dmesg,
            note: Some("supplied by the caller".into()),
            events: Vec::new(),
        };
        let snap = capture_with_log(Options::default(), Some(supplied));
        assert_eq!(snap.kernel_log.source, KernelLogSource::Dmesg);
        assert_eq!(
            snap.kernel_log.note.as_deref(),
            Some("supplied by the caller")
        );
        assert!(snap.kernel_log.events.is_empty());
    }

    /// A real capture must not panic on this machine, whatever it finds.
    #[test]
    fn capture_on_the_host_does_not_panic() {
        let snap = capture(Options::default());
        let _ = diag::analyze(&snap);
        // Every bus is a root hub by construction.
        assert!(snap.buses.iter().all(|b| b.is_root_hub));
    }
}

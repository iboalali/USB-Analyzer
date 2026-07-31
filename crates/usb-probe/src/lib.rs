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

pub mod diag;
pub mod kernel;
pub mod model;
pub mod pd;
pub mod sysfs;
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
}

/// Read the current state of the system.
pub fn capture(opts: Options) -> Snapshot {
    let ports = typec::read_ports();

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
        orphan_pd,
        kernel_log: kernel::collect(opts.kernel),
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

    /// A real capture must not panic on this machine, whatever it finds.
    #[test]
    fn capture_on_the_host_does_not_panic() {
        let snap = capture(Options::default());
        let _ = diag::analyze(&snap);
        // Every bus is a root hub by construction.
        assert!(snap.buses.iter().all(|b| b.is_root_hub));
    }
}

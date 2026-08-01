//! URB error accounting from usbmon — what the bus is *doing*, not what it
//! agreed to do.
//!
//! # Why this is the probe that matters
//!
//! Everything else in this crate reads negotiated state. A marginal cable
//! usually negotiates perfectly and then fails under load, so every passive
//! check reports a healthy link right up until the transfer dies. usbmon sees
//! each transfer complete, with its status, which makes it the only source of
//! evidence in software that observes behaviour rather than intent — and the
//! only thing that can move a cable finding from inferred to measured.
//!
//! # Not every error is a bus error
//!
//! This is the whole difficulty, and getting it wrong would produce exactly the
//! kind of confident false accusation this tool exists to avoid. Three classes,
//! kept apart:
//!
//! * **Transport errors** — `EPROTO`, `EILSEQ`, `EOVERFLOW`, `ETIMEDOUT`. The
//!   packet was mangled, babbled or never answered. This is the class that
//!   implicates the physical link.
//! * **Protocol refusals** — `EPIPE`, a stall. On endpoint 0 this is routine:
//!   it is how a device says "I don't support that request", and a normal boot
//!   produces plenty. On a bulk or interrupt endpoint it is a halt condition
//!   and more interesting, so the endpoint is recorded.
//! * **Cancellations** — `ENOENT`, `ECONNRESET`, `ESHUTDOWN`. The driver
//!   unlinked the URB. A webcam stopping its stream cancels a great many in a
//!   burst; counting those as faults would condemn every healthy camera on the
//!   machine.
//!
//! # Access
//!
//! The text interface lives under debugfs and is root-only, as is the binary
//! one. The binary API would need ioctls and therefore libc, so only the text
//! API is read here — it is stable, documented, and enough for counting.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::model::{UrbStats, UrbStatusClass, UrbTraffic};

/// Linux `O_NONBLOCK`; hardcoded to avoid a libc dependency.
const O_NONBLOCK: i32 = 0o4000;

/// How long to wait between reads when the stream has nothing to say.
const IDLE_POLL: Duration = Duration::from_millis(10);

/// The all-buses text stream. Per-bus streams are `<busnum>u`.
pub const TEXT_API_ALL: &str = "/sys/kernel/debug/usb/usbmon/0u";

/// Watch the URB stream for a window and account what completed.
///
/// The window is the measurement: usbmon is a live stream with no history, so
/// nothing is learned about traffic that already happened. A device that is
/// idle during the window produces no evidence either way, which the caller
/// must not confuse with evidence of health.
pub fn sample(path: &Path, window: Duration) -> std::io::Result<UrbTraffic> {
    let mut file = OpenOptions::new()
        .read(true)
        // Without this a quiet bus blocks the read forever and the window
        // becomes meaningless.
        .custom_flags(O_NONBLOCK)
        .open(path)?;

    let started = Instant::now();
    let mut acc = Accumulator::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut partial = String::new();

    while started.elapsed() < window {
        match file.read(&mut buf) {
            Ok(0) => std::thread::sleep(IDLE_POLL),
            Ok(n) => {
                partial.push_str(&String::from_utf8_lossy(&buf[..n]));
                // Keep whatever follows the last newline for the next read.
                if let Some(cut) = partial.rfind('\n') {
                    let complete: String = partial.drain(..=cut).collect();
                    for line in complete.lines() {
                        acc.feed(line);
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(IDLE_POLL),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }

    Ok(acc.finish(
        started.elapsed().as_millis() as u64,
        Some(path.to_path_buf()),
    ))
}

/// One parsed completion. Submissions carry no outcome and are not represented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub bus: u32,
    pub device_address: u32,
    pub endpoint: u8,
    /// Transfer type letter: `C`ontrol, `B`ulk, `I`nterrupt, i`S`ochronous.
    pub transfer: char,
    /// 0 on success, otherwise a negative errno.
    pub status: i32,
    pub length: u64,
}

/// Parse one line of the usbmon text stream.
///
/// ```text
/// b5ef4900 3575914555 S Ci:1:001:0 s a3 00 0000 0001 0004 4 <
/// b5ef4900 3575914560 C Ci:1:001:0 0 4 = 01050000
/// f1a2b300 3575920001 C Bi:2:004:1 -71 512 <
/// ```
///
/// Field 3 is the event: `S` submission, `C` callback, `E` submission error.
/// Only `C` and `E` carry an outcome — a submission's status word is `-115`
/// (`EINPROGRESS`) or, for control transfers, the letter `s` followed by the
/// eight setup bytes. Counting either as a failure would make every control
/// transfer on the machine look like an error.
pub fn parse_line(line: &str) -> Option<Completion> {
    let mut f = line.split_whitespace();
    let _urb_tag = f.next()?;
    let _timestamp_us = f.next()?;

    let event = f.next()?;
    if event != "C" && event != "E" {
        return None;
    }

    // `Bi:2:004:1` — type, direction, bus, device address, endpoint.
    let pipe = f.next()?;
    let mut parts = pipe.split(':');
    let kind = parts.next()?;
    let mut chars = kind.chars();
    let transfer = chars.next()?;
    // `C`ontrol, `B`ulk, `I`nterrupt, and isochronous — which the kernel writes
    // as `Z`. `S` is accepted alongside it because the documentation and the
    // implementation have differed on that letter; accepting both costs
    // nothing and cannot collide, since the event letter is a different field.
    if !matches!(transfer, 'C' | 'B' | 'I' | 'Z' | 'S') {
        return None;
    }
    let bus: u32 = parts.next()?.parse().ok()?;
    let device_address: u32 = parts.next()?.parse().ok()?;
    let endpoint: u8 = parts.next()?.parse().ok()?;

    // The status word. Isochronous completions extend it with interval, start
    // frame and error count, colon separated; the status is always first.
    let status_word = f.next()?;
    let status: i32 = status_word.split(':').next()?.parse().ok()?;

    // Length is present on completions; be tolerant if a variant omits it.
    let length = f.next().and_then(|v| v.parse().ok()).unwrap_or(0);

    Some(Completion {
        bus,
        device_address,
        endpoint,
        transfer,
        status,
        length,
    })
}

/// Which of the three classes a completion status belongs to.
///
/// Errno values are the kernel's, negated as usbmon reports them.
pub fn classify(status: i32) -> UrbStatusClass {
    match status {
        0 => UrbStatusClass::Success,

        // The physical link failed to carry the packet intact.
        -71 => UrbStatusClass::Transport,  // EPROTO   — bit stuffing, no response
        -75 => UrbStatusClass::Transport,  // EOVERFLOW — babble
        -84 => UrbStatusClass::Transport,  // EILSEQ   — CRC mismatch
        -110 => UrbStatusClass::Transport, // ETIMEDOUT

        // The device answered, and the answer was "no".
        -32 => UrbStatusClass::Protocol, // EPIPE — stall

        // The driver took the URB back. Routine.
        -2 => UrbStatusClass::Cancelled,    // ENOENT
        -104 => UrbStatusClass::Cancelled,  // ECONNRESET
        -108 => UrbStatusClass::Cancelled,  // ESHUTDOWN — device gone
        -115 => UrbStatusClass::Cancelled,  // EINPROGRESS — should not reach here

        // A short read is usually the driver asking for more than was offered.
        -121 => UrbStatusClass::Other, // EREMOTEIO
        _ => UrbStatusClass::Other,
    }
}

#[derive(Default)]
struct Accumulator {
    by_device: BTreeMap<(u32, u32), UrbStats>,
    unparsed: usize,
    lines: u64,
}

impl Accumulator {
    fn feed(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        self.lines += 1;
        let Some(c) = parse_line(line) else {
            // Submissions are the common non-completion and are not a parse
            // failure; anything else is, and is counted so a format change
            // shows up instead of silently zeroing the result.
            if !is_submission(line) {
                self.unparsed += 1;
            }
            return;
        };

        let stats = self
            .by_device
            .entry((c.bus, c.device_address))
            .or_insert_with(|| UrbStats {
                bus: c.bus,
                device_address: c.device_address,
                ..Default::default()
            });

        stats.completions += 1;
        stats.bytes += c.length;
        *stats.by_status.entry(c.status).or_insert(0) += 1;
        match classify(c.status) {
            UrbStatusClass::Success => {}
            UrbStatusClass::Transport => {
                stats.transport_errors += 1;
                stats.transport_endpoints.insert(c.endpoint);
            }
            UrbStatusClass::Protocol => {
                stats.protocol_errors += 1;
                if c.endpoint != 0 {
                    stats.non_control_stalls += 1;
                }
            }
            UrbStatusClass::Cancelled => stats.cancellations += 1,
            UrbStatusClass::Other => stats.other += 1,
        }
    }

    fn finish(self, window_ms: u64, source: Option<std::path::PathBuf>) -> UrbTraffic {
        UrbTraffic {
            window_ms,
            lines_read: self.lines,
            unparsed: self.unparsed,
            source,
            devices: self.by_device.into_values().collect(),
        }
    }
}

fn is_submission(line: &str) -> bool {
    line.split_whitespace().nth(2) == Some("S")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The submission/completion distinction. A control submission's status
    /// field is the letter `s`, and a non-control submission's is -115; reading
    /// either as an outcome would make ordinary traffic look catastrophic.
    #[test]
    fn only_completions_carry_an_outcome() {
        assert!(parse_line("b5ef4900 3575914555 S Ci:1:001:0 s a3 00 0000 0001 0004 4 <").is_none());
        assert!(parse_line("f1a2b300 3575919000 S Bi:2:004:1 -115 512 <").is_none());

        let c = parse_line("b5ef4900 3575914560 C Ci:1:001:0 0 4 = 01050000").unwrap();
        assert_eq!(c.transfer, 'C');
        assert_eq!((c.bus, c.device_address, c.endpoint), (1, 1, 0));
        assert_eq!(c.status, 0);
        assert_eq!(c.length, 4);
    }

    #[test]
    fn parses_a_bulk_transport_error() {
        let c = parse_line("f1a2b300 3575920001 C Bi:2:004:1 -71 512 <").unwrap();
        assert_eq!(c.transfer, 'B');
        assert_eq!(c.status, -71);
        assert_eq!(classify(c.status), UrbStatusClass::Transport);
    }

    /// Isochronous completions append interval, start frame and error count to
    /// the status word. The status is still the first field.
    ///
    /// Both spellings of the isochronous type letter are accepted, so a webcam
    /// cannot go unaccounted because of which one this kernel emits.
    #[test]
    fn handles_the_isochronous_status_word() {
        for letter in ['Z', 'S'] {
            let c = parse_line(&format!("d1000000 100 C {letter}i:1:005:1 -84:1:1234 192 <"))
                .unwrap_or_else(|| panic!("isochronous spelled {letter}"));
            assert_eq!(c.status, -84);
            assert_eq!(c.endpoint, 1);
        }
        assert_eq!(classify(-84), UrbStatusClass::Transport);
    }

    /// The false positive this classification exists to prevent: a webcam
    /// stopping its stream cancels URBs in bulk, and a stall on endpoint 0 is
    /// how a device declines an unsupported request. Neither is a bus fault.
    #[test]
    fn cancellations_and_control_stalls_are_not_transport_errors() {
        assert_eq!(classify(-2), UrbStatusClass::Cancelled);
        assert_eq!(classify(-104), UrbStatusClass::Cancelled);
        assert_eq!(classify(-108), UrbStatusClass::Cancelled);
        assert_eq!(classify(-32), UrbStatusClass::Protocol);

        let mut acc = Accumulator::default();
        for _ in 0..50 {
            acc.feed("aaaa0000 100 C Bi:2:004:1 -2 512 <");
        }
        for _ in 0..10 {
            acc.feed("aaaa0000 100 C Ci:2:004:0 -32 0 <");
        }
        let t = acc.finish(1000, None);
        let d = &t.devices[0];
        assert_eq!(d.completions, 60);
        assert_eq!(d.cancellations, 50);
        assert_eq!(d.protocol_errors, 10);
        assert_eq!(d.transport_errors, 0);
        // Endpoint 0 stalls do not count as endpoint halts.
        assert_eq!(d.non_control_stalls, 0);
        assert_eq!(d.transport_error_rate(), Some(0.0));
    }

    #[test]
    fn accounts_errors_per_device_and_endpoint() {
        let mut acc = Accumulator::default();
        // A healthy device on bus 1.
        for _ in 0..200 {
            acc.feed("bbbb0000 100 C Bi:1:003:2 0 4096 <");
        }
        // A struggling one on bus 2.
        for _ in 0..90 {
            acc.feed("cccc0000 100 C Bi:2:004:1 0 4096 <");
        }
        for _ in 0..10 {
            acc.feed("cccc0000 100 C Bi:2:004:1 -71 0 <");
        }
        acc.feed("cccc0000 100 E Bo:2:004:2 -110 0 <");

        let t = acc.finish(2000, None);
        assert_eq!(t.devices.len(), 2);
        assert_eq!(t.unparsed, 0);

        let good = t.for_address(1, 3).unwrap();
        assert_eq!(good.transport_errors, 0);
        assert_eq!(good.bytes, 200 * 4096);

        let bad = t.for_address(2, 4).unwrap();
        assert_eq!(bad.transport_errors, 11);
        assert_eq!(bad.completions, 101);
        assert!((bad.transport_error_rate().unwrap() - 11.0 / 101.0).abs() < 1e-9);
        // Both the IN and the OUT endpoint were implicated.
        assert_eq!(bad.transport_endpoints.len(), 2);
        assert_eq!(bad.by_status.get(&-71), Some(&10));
        assert_eq!(bad.by_status.get(&-110), Some(&1));
    }

    /// A format change must be visible rather than quietly producing zeroes.
    #[test]
    fn unrecognised_lines_are_counted_not_ignored() {
        let mut acc = Accumulator::default();
        acc.feed("this is not a usbmon line at all");
        acc.feed("b5ef4900 3575914555 S Ci:1:001:0 s a3 00 0000 0001 0004 4 <");
        let t = acc.finish(100, None);
        assert_eq!(t.unparsed, 1, "the submission is expected, the garbage is not");
        assert_eq!(t.lines_read, 2);
    }

    #[test]
    fn sampling_a_missing_path_is_an_error_not_a_panic() {
        let e = sample(
            Path::new("/nonexistent/usbmon/0u"),
            Duration::from_millis(10),
        );
        assert!(e.is_err());
    }
}

//! Change notification, so a live view can wait for something to happen
//! instead of asking the same question twice a second.
//!
//! # Why a subprocess and not a netlink socket
//!
//! The direct route is a `NETLINK_KOBJECT_UEVENT` socket bound to the udev
//! multicast group. It needs no privileges — systemd-udevd broadcasts to a group
//! every user can read; only the raw *kernel* group needs root. But Rust's
//! standard library exposes no way to call `socket(2)` with `AF_NETLINK`, so it
//! would mean taking a `libc` dependency and writing `unsafe` FFI. This crate
//! deliberately has no dependency beyond `serde`, and already reads the kernel
//! log by spawning `journalctl`, so the cheaper answer is consistent with what
//! is already here: spawn `udevadm monitor --udev` and read its stdout.
//!
//! The tradeoffs are real and worth stating:
//!
//! * it needs `udevadm` on `PATH` — present wherever systemd is, absent on a
//!   minimal container or a non-systemd system;
//! * it costs one long-lived child process;
//! * it reports the same events a netlink subscriber would, because it *is* a
//!   netlink subscriber — one written in C by someone else.
//!
//! When `udevadm` cannot be started, or dies, the monitor degrades to a plain
//! timeout and the caller keeps polling. Nothing breaks; it just gets slower to
//! notice. That fallback is also why [`Monitor::wait_for_change`] always takes a
//! deadline: a missed event must never be able to wedge a display.
//!
//! # Debouncing
//!
//! One physical act produces a burst. Plugging in a charger renegotiates the PD
//! contract several times as it settles, and each step is a uevent; plugging in
//! a hub emits one event per downstream device. Repainting on each would thrash.
//! [`Monitor::wait_for_change`] therefore returns only after a quiet period, and
//! caps how long it will keep coalescing so a continuous storm still updates.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The subsystems whose events can change anything this crate reports.
///
/// `power_supply` is included because a charger's contract shows up there, and
/// `block` because a storage device's arrival is what makes the storage view
/// appear at all.
pub const SUBSYSTEMS: [&str; 5] = ["usb", "typec", "power_supply", "thunderbolt", "block"];

/// Where wake-ups are coming from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Live uevents via `udevadm monitor`.
    Udev,
    /// No event source; the caller's timeout is the only thing waking it.
    TimerOnly,
}

/// A device uevent, reduced to the three fields that matter here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEvent {
    /// `add` | `remove` | `change` | `bind` | `unbind` | `move`.
    pub action: String,
    /// Kernel device path, e.g. `/devices/pci0000:00/.../usb5/5-1`.
    pub devpath: String,
    /// `usb`, `typec`, `power_supply`, ...
    pub subsystem: String,
}

/// The outcome of a single wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake {
    Event(DeviceEvent),
    Timeout,
}

pub struct Monitor {
    rx: Option<Receiver<DeviceEvent>>,
    /// Shared so that [`Monitor::stopper`] can hand out the ability to end the
    /// child without owning the monitor. See [`Stopper`] for why that is needed.
    child: Arc<Mutex<Option<Child>>>,
    source: Source,
    note: Option<String>,
}

/// The ability to end the monitor's subprocess, detached from the monitor.
///
/// [`Monitor`] kills its child on drop, which is correct and, on its own, not
/// enough: a caller that parks the monitor on a thread blocked in
/// [`Monitor::wait_for_change`] never drops it, because the process exits with
/// that thread still inside the call. Destructors do not run for a thread that
/// is killed at exit, so the `udevadm monitor` child is orphaned and reparented
/// to init — once per run of the program.
///
/// That is exactly the shape of `usbdiag-gui`'s worker, which is where this was
/// found: closing the window left a stray `udevadm monitor` behind every single
/// time. A `Stopper` can be held by whatever *does* get a chance to run at
/// shutdown — the GTK thread — while the monitor itself stays on the worker.
///
/// Cheap to clone, safe to call more than once, and a no-op when the monitor was
/// a timer with no subprocess at all.
#[derive(Debug, Clone)]
pub struct Stopper {
    child: Arc<Mutex<Option<Child>>>,
}

impl Stopper {
    /// End the subprocess now. Idempotent.
    ///
    /// Returns whether there was a live child to end, which is the only way a
    /// caller — or a test — can tell "cleaned up" from "there was nothing to
    /// clean up".
    pub fn stop(&self) -> bool {
        kill_child(&self.child)
    }
}

/// Kill and reap, leaving nothing behind to kill twice. True if a child was
/// there.
fn kill_child(slot: &Arc<Mutex<Option<Child>>>) -> bool {
    // A poisoned lock still has to be cleaned up: a panic elsewhere is no reason
    // to leak a process, so the guard is taken either way.
    let mut guard = match slot.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.take() {
        Some(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
        None => false,
    }
}

impl Monitor {
    /// Start watching [`SUBSYSTEMS`]. Never fails: if no event source can be
    /// opened the monitor is a timer, and [`Monitor::source`] says so.
    pub fn start() -> Self {
        Self::start_with(&SUBSYSTEMS)
    }

    pub fn start_with(subsystems: &[&str]) -> Self {
        match spawn_udevadm(subsystems) {
            Ok((child, stdout)) => {
                let m = Self::from_stream(stdout);
                *m.child.lock().expect("fresh mutex") = Some(child);
                m
            }
            Err(e) => Self {
                rx: None,
                child: Arc::default(),
                source: Source::TimerOnly,
                note: Some(format!("udevadm monitor unavailable ({e}); falling back to polling")),
            },
        }
    }

    /// A handle that can end the subprocess from somewhere else.
    ///
    /// Give this to whatever will still be running at shutdown. See [`Stopper`].
    pub fn stopper(&self) -> Stopper {
        Stopper {
            child: Arc::clone(&self.child),
        }
    }

    /// Build a monitor over any stream of `udevadm monitor` formatted lines.
    ///
    /// The subprocess path goes through here, and so could a netlink socket if
    /// this crate ever grows one — the parsing and debouncing above are the same
    /// either way. Reading happens on its own thread, so the stream may block.
    pub fn from_stream(stream: impl Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                if let Some(ev) = parse_line(&line) {
                    // A send failure means the Monitor is gone; so are we.
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            rx: Some(rx),
            // Empty, not absent: `start_with` fills it when there is a
            // subprocess, and `from_stream` on its own has none to fill.
            child: Arc::default(),
            source: Source::Udev,
            note: None,
        }
    }

    pub fn source(&self) -> Source {
        self.source
    }

    /// Why there is no event source, when there isn't one.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// Wait for one event, or until `timeout` elapses.
    pub fn wait(&mut self, timeout: Duration) -> Wake {
        let Some(rx) = &self.rx else {
            std::thread::sleep(timeout);
            return Wake::Timeout;
        };
        match rx.recv_timeout(timeout) {
            Ok(ev) => Wake::Event(ev),
            Err(RecvTimeoutError::Timeout) => Wake::Timeout,
            Err(RecvTimeoutError::Disconnected) => {
                // The reader thread ended, so the event source is gone. Say so
                // once and carry on as a timer rather than spin on a dead
                // channel — a lost monitor must degrade, not wedge.
                self.rx = None;
                self.source = Source::TimerOnly;
                self.note = Some("event stream ended; falling back to polling".into());
                Wake::Timeout
            }
        }
    }

    /// Block until something changes, absorbing the burst that follows it.
    ///
    /// Returns after `quiet` has passed with no further event, or after
    /// `max_coalesce` regardless — a device stuck in a reset loop emits events
    /// forever and must not be able to hold the display still.
    ///
    /// The return value is how many events were absorbed; `0` means nothing
    /// happened and `fallback` simply expired. A caller can use that to decide
    /// how much work the next refresh deserves: after real events, re-read
    /// everything; after a bare timeout, the cheap parts are usually enough.
    pub fn wait_for_change(
        &mut self,
        fallback: Duration,
        quiet: Duration,
        max_coalesce: Duration,
    ) -> usize {
        if self.wait(fallback) == Wake::Timeout {
            return 0;
        }
        let mut seen = 1;
        let deadline = Instant::now() + max_coalesce;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return seen;
            }
            match self.wait(quiet.min(left)) {
                Wake::Event(_) => seen += 1,
                Wake::Timeout => return seen,
            }
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // Dropping the receiver ends the reader thread on its next send; killing
        // the child unblocks that read straight away.
        //
        // Correct, and not sufficient by itself — a monitor parked on a thread
        // that is still inside `wait_for_change` when the process exits is never
        // dropped at all. [`Stopper`] exists for that case.
        let _ = kill_child(&self.child);
    }
}

fn spawn_udevadm(subsystems: &[&str]) -> std::io::Result<(Child, std::process::ChildStdout)> {
    let mut cmd = Command::new("udevadm");
    cmd.arg("monitor").arg("--udev");
    for s in subsystems {
        cmd.arg(format!("--subsystem-match={s}"));
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "no stdout from udevadm")
    })?;
    Ok((child, stdout))
}

/// Parse one line of `udevadm monitor` output.
///
/// ```text
/// KERNEL[64095.123456] add      /devices/pci0000:00/.../usb5/5-1 (usb)
/// UDEV  [64095.234567] add      /devices/pci0000:00/.../usb5/5-1 (usb)
/// ```
///
/// Note the tag: `KERNEL` is glued to the timestamp while `UDEV` is padded away
/// from it, so the two cannot be split the same way.
///
/// Anything else — the banner it prints on startup, blank separators — is not an
/// event and is skipped.
fn parse_line(line: &str) -> Option<DeviceEvent> {
    let rest = line
        .strip_prefix("UDEV")
        .or_else(|| line.strip_prefix("KERNEL"))?;
    let mut f = rest.split_whitespace();
    // The timestamp is discarded: it is udev's own clock, and the caller cares
    // that something happened, not exactly when. It is still parsed, because it
    // is what distinguishes an event line from the banner.
    let ts = f.next()?;
    if !ts.starts_with('[') || !ts.ends_with(']') {
        return None;
    }
    let action = f.next()?.to_string();
    let devpath = f.next()?.to_string();
    let subsystem = f.next()?.trim_matches(['(', ')']).to_string();
    if subsystem.is_empty() {
        return None;
    }
    Some(DeviceEvent {
        action,
        devpath,
        subsystem,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_udev_event_line() {
        let ev = parse_line(
            "UDEV  [64095.123456] add      /devices/pci0000:00/0000:00:14.0/usb5/5-1 (usb)",
        )
        .expect("should parse");
        assert_eq!(ev.action, "add");
        assert_eq!(ev.devpath, "/devices/pci0000:00/0000:00:14.0/usb5/5-1");
        assert_eq!(ev.subsystem, "usb");

        let ev = parse_line("KERNEL[12.5] change   /devices/platform/USBC000:00/typec/port0 (typec)")
            .expect("should parse");
        assert_eq!(ev.action, "change");
        assert_eq!(ev.subsystem, "typec");
    }

    /// The banner udevadm prints before any event must not look like one.
    #[test]
    fn ignores_non_event_lines() {
        assert!(parse_line("monitor will print the received events for:").is_none());
        assert!(parse_line("UDEV - the event which udev sends out after rule processing").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("UDEV").is_none());
        assert!(parse_line("UDEV  [1.0] add").is_none());
    }

    /// The whole point: one physical act produces a burst, and the burst must
    /// arrive as a single reason to redraw.
    #[test]
    fn a_burst_of_events_coalesces_into_one_wake_up() {
        let stream = "monitor will print the received events for:\n\
             UDEV  [1.100] add      /devices/pci0000:00/usb5/5-1 (usb)\n\
             UDEV  [1.140] change   /devices/platform/USBC000:00/typec/port0 (typec)\n\
             UDEV  [1.190] change   /devices/platform/USBC000:00/typec/port0 (typec)\n";
        let mut m = Monitor::from_stream(std::io::Cursor::new(stream));

        let n = m.wait_for_change(
            Duration::from_secs(2),
            Duration::from_millis(50),
            Duration::from_millis(500),
        );
        assert_eq!(n, 3, "all three events belong to one repaint");

        // The stream has ended. The next wait must time out on schedule rather
        // than block forever or spin on the dead channel.
        let started = Instant::now();
        let n = m.wait_for_change(
            Duration::from_millis(80),
            Duration::from_millis(20),
            Duration::from_millis(200),
        );
        assert_eq!(n, 0);
        assert_eq!(m.source(), Source::TimerOnly);
        assert!(m.note().is_some(), "the downgrade must explain itself");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A monitor with no subprocess has nothing to stop, and saying so must not
    /// be an error — `from_stream` is the netlink-shaped path with no child.
    #[test]
    fn stopping_a_monitor_with_no_subprocess_is_a_harmless_no_op() {
        let m = Monitor::from_stream(std::io::Cursor::new(Vec::new()));
        let s = m.stopper();
        assert!(!s.stop(), "there was no child to end");
        assert!(!s.stop(), "and still none the second time");
    }

    /// The leak this exists for: the GUI's worker thread is always inside a
    /// blocking wait when the process exits, so `Monitor`'s `Drop` never runs and
    /// the child is orphaned. A `Stopper` held elsewhere is the way out, so it
    /// must work while the monitor is still very much alive.
    #[test]
    fn a_stopper_ends_the_subprocess_without_owning_the_monitor() {
        let m = Monitor::start();
        if m.source() != Source::Udev {
            eprintln!("no udevadm here; nothing to stop");
            return;
        }
        let s = m.stopper();
        assert!(s.stop(), "the live child should have been ended");
        assert!(!s.stop(), "idempotent: nothing left to end");
        // The monitor is still owned here, and dropping it must not double-kill.
        drop(m);
    }

    /// Starting must never fail, whatever the machine has installed.
    #[test]
    fn start_always_yields_a_usable_monitor() {
        let mut m = Monitor::start();
        match m.source() {
            Source::Udev => assert!(m.note().is_none()),
            Source::TimerOnly => assert!(m.note().is_some(), "a fallback must explain itself"),
        }
        // Nothing is being plugged in during a test run, so this is the
        // fallback path: it must return promptly rather than block.
        let started = Instant::now();
        let n = m.wait_for_change(
            Duration::from_millis(120),
            Duration::from_millis(20),
            Duration::from_millis(200),
        );
        assert_eq!(n, 0, "no events expected in a test run");
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

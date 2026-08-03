//! The udev monitor, as a Relm4 worker.
//!
//! [`usb_probe::monitor::Monitor`] blocks on reads from a `udevadm monitor`
//! child process, which is exactly what must not happen on the GTK thread. A
//! worker owns that thread instead and forwards debounced wake-ups.
//!
//! The debounce is not reimplemented here. `wait_for_change` already coalesces
//! the burst that follows one physical act — a charger renegotiates its
//! contract several times as it settles, a hub emits one event per downstream
//! device — and caps how long it will keep coalescing so a device stuck in a
//! reset loop cannot hold the display still. That logic is tested in
//! `usb-probe`; duplicating it in the UI would give it a second, untested copy.

use std::time::Duration;

use relm4::{ComponentSender, Worker};
use usb_probe::monitor::{Monitor, Source};

/// Fallback poll, for the case where `udevadm` is unavailable and for state
/// that changes without a uevent (battery drift, I/O counters).
const FALLBACK_MS: u64 = 2000;
/// Same values the CLI's watch loop uses.
const QUIET_MS: u64 = 250;
const MAX_COALESCE_MS: u64 = 1500;

#[derive(Debug)]
pub enum In {
    /// Do one blocking wait and report the result. Self-sent, so the worker
    /// loops without the main thread having to drive it.
    Wait,
}

#[derive(Debug)]
pub enum Out {
    /// The handle that ends the `udevadm monitor` subprocess, sent once.
    ///
    /// **This worker cannot clean up after itself.** Its `update` re-queues
    /// `In::Wait` forever, so the thread is always inside a blocking wait when
    /// the process exits — and a thread killed at exit runs no destructors, so
    /// `Monitor`'s own `Drop` never fires and the child is orphaned. Handing the
    /// stopper to the GTK thread, which *does* get to run at shutdown, is the
    /// fix. Closing the window used to leak one `udevadm monitor` every time.
    Started(usb_probe::monitor::Stopper),
    /// What the event source turned out to be, sent once the monitor starts.
    Source {
        live: bool,
        note: Option<String>,
    },
    /// A wait finished. `events` is how many uevents were absorbed; `0` means
    /// the fallback timer simply expired and nothing happened.
    Woke { events: usize },
}

#[derive(Default)]
pub struct MonitorWorker {
    monitor: Option<Monitor>,
}

impl Worker for MonitorWorker {
    type Init = ();
    type Input = In;
    type Output = Out;

    fn init(_: Self::Init, sender: ComponentSender<Self>) -> Self {
        // Nothing is started here: `Worker::init` runs on the *calling* thread,
        // which is the GTK thread, and starting the monitor spawns a process.
        // The first `Wait` does it instead, on the worker's own thread.
        sender.input(In::Wait);
        Self::default()
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>) {
        let In::Wait = message;

        let monitor = self.monitor.get_or_insert_with(|| {
            let m = Monitor::start();
            // Before anything else: the GTK thread needs this to clean up, and
            // every pass after this one is spent blocked inside the wait below.
            let _ = sender.output(Out::Started(m.stopper()));
            let _ = sender.output(Out::Source {
                live: m.source() == Source::Udev,
                note: m.note().map(str::to_string),
            });
            m
        });

        let events = monitor.wait_for_change(
            Duration::from_millis(FALLBACK_MS),
            Duration::from_millis(QUIET_MS),
            Duration::from_millis(MAX_COALESCE_MS),
        );

        // A monitor that loses its event source degrades to a timer and says so
        // once. Re-reading `source()` each pass is how the window finds out.
        let _ = sender.output(Out::Source {
            live: monitor.source() == Source::Udev,
            note: monitor.note().map(str::to_string),
        });
        let _ = sender.output(Out::Woke { events });

        sender.input(In::Wait);
    }
}

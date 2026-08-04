//! Stopping a probe that is already running.
//!
//! # A front end cannot kill what it started
//!
//! Privileged probes are meant to be launched as `pkexec usbdiag probe …`, which
//! makes the child a **root** process owned by an unprivileged parent. That
//! parent may not signal it. So the obvious Cancel button — send `SIGTERM` — does
//! not work, and cannot be made to work from that side.
//!
//! Worse, the same asymmetry means the child outlives its parent. Close the
//! window during `reenumerate` and a root process keeps switching a port off and
//! on with nobody watching. It restores the port when it finishes, so it heals,
//! but nothing can stop it.
//!
//! # So the child agrees to be stopped
//!
//! A cancellable probe holds a [`Cancel`] and checks it at the points where
//! stopping is safe — between cycles, between chunks — never mid-operation. That
//! is the whole trick: cooperation instead of force, so the port is always left
//! up and every partial result is a real one.
//!
//! [`Cancel::on_stdin_eof`] is the transport. The parent keeps the child's stdin
//! and closes it to cancel; if the parent dies instead, the kernel closes it, and
//! both look identical from here — a read of zero bytes. No signals, no `libc`,
//! no pid to track, and it covers the case nobody remembers to handle, which is
//! the parent crashing.
//!
//! **It is opt-in for a reason.** On a terminal, stdin is the keyboard: watching
//! it would swallow what the user types and never see EOF until they press
//! Ctrl-D. So an interactive run must not enable this, and `usbdiag` only does so
//! when asked with `--stop-on-eof`.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A flag a long-running probe agrees to watch.
///
/// Cheap to clone; every clone observes the same flag.
#[derive(Debug, Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
}

impl Cancel {
    /// A token nothing ever sets. The default for a plain CLI run.
    pub fn never() -> Self {
        Self::default()
    }

    /// Stop when this process's stdin reaches end of file.
    ///
    /// Spawns one reader thread. Any byte that arrives is discarded — the pipe is
    /// a liveness signal and not a channel, so there is no protocol to get wrong
    /// and nothing to parse.
    ///
    /// A read error counts as a stop for the same reason EOF does: the parent's
    /// end is no longer there, so nobody is waiting for the answer.
    pub fn on_stdin_eof() -> Self {
        let me = Self::default();
        let flag = Arc::clone(&me.flag);
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match std::io::stdin().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            flag.store(true, Ordering::SeqCst);
        });
        me
    }

    /// Ask for a stop. For an in-process caller, and for tests.
    pub fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Whether a stop has been asked for. Checked between units of work.
    pub fn stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Sleep for `total`, giving up early if a stop arrives. Returns whether the
    /// full time was slept.
    ///
    /// A probe that waits out a window would otherwise ignore a cancel for the
    /// whole window — a thirty-second sample being uncancellable for thirty
    /// seconds is indistinguishable, to the person waiting, from a cancel that
    /// does not work. The slice is small enough to feel immediate and long enough
    /// that the wakeups cost nothing.
    pub fn sleep(&self, total: Duration) -> bool {
        const SLICE: Duration = Duration::from_millis(50);
        let deadline = Instant::now() + total;
        while Instant::now() < deadline {
            if self.stopped() {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(left.min(SLICE));
        }
        !self.stopped()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_token_never_stops() {
        let c = Cancel::never();
        assert!(!c.stopped());
        assert!(!c.stopped(), "and asking twice does not change it");
    }

    #[test]
    fn every_clone_watches_the_same_flag() {
        let a = Cancel::never();
        let b = a.clone();
        assert!(!b.stopped());
        a.stop();
        assert!(b.stopped(), "a probe holding a clone must see the stop");
    }

    #[test]
    fn stopping_is_idempotent() {
        let c = Cancel::never();
        c.stop();
        c.stop();
        assert!(c.stopped());
    }
}

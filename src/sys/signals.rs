//! Wiring quit signals to flags, so a process can exit *normally*.
//!
//! # Why a signal may not do the work itself
//!
//! A signal handler runs on an arbitrary thread, possibly while a mutex is held by
//! the thread it interrupted. Logging takes a mutex, tunnel shutdown allocates —
//! neither is allowed there. So the handler may only do what is async-signal-safe:
//! one atomic store. This module does exactly that, and both processes then poll
//! the flag from a normal thread:
//!
//! * the TUI's event loop checks it every frame ([`crate::main`]);
//! * the headless server's watcher thread checks it and asks the server to stop
//!   ([`crate::server::start`]).
//!
//! # Why not `process::exit`
//!
//! `process::exit` skips `Drop`, which skips stopping the tunnels, releasing the
//! discovery files and restoring the terminal. Every exit path here is a normal
//! `return`, which is why the flag exists at all.
//!
//! Belongs here: registering the platform's quit signals onto two atomics.
//! Does not belong: deciding what to do about them (each process decides).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use crate::eventlog;
use crate::sys;

/// Signals that make this process quit cleanly, as
/// `(set the quit flag, remember which signal)`.
///
/// The second atomic is not redundant: the TUI is gone by the time the exit status
/// is produced, so an exit code of 0 cannot tell `Ctrl-C` from `kill`, and the log
/// line wants the signal's name.
pub struct QuitFlags {
    /// Flipped by any quit signal.
    pub quit: Arc<AtomicBool>,
    /// Holds the number of the signal that arrived, or `0` for none.
    pub received: Arc<AtomicUsize>,
}

impl QuitFlags {
    /// Create the flags and register every quit signal onto them.
    ///
    /// A signal that cannot be registered is **not** fatal — the other shortcuts
    /// still work — but it is logged, because the consequence is subtle: that
    /// shortcut now kills the process without running any `Drop`, leaving tunnels
    /// up and the terminal in raw mode.
    pub fn install() -> QuitFlags {
        let flags = QuitFlags { quit: Arc::new(AtomicBool::new(false)), received: Arc::new(AtomicUsize::new(0)) };
        flags.register();
        flags
    }

    fn register(&self) {
        for sig in sys::quit_signals() {
            let sig = *sig;
            let registered = signal_hook::flag::register_usize(sig, self.received.clone(), sig as usize)
                .and_then(|_| signal_hook::flag::register(sig, self.quit.clone()));
            if let Err(e) = registered {
                logging_warn_unavailable(sig, &e.to_string());
            }
        }
    }

    /// Whether a quit signal has arrived.
    pub fn quit(&self) -> bool {
        self.quit.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The signal that arrived, or `0` for none.
    pub fn signal(&self) -> i32 {
        self.received.load(std::sync::atomic::Ordering::Relaxed) as i32
    }
}

/// Record that one shortcut will not work on this platform/session.
fn logging_warn_unavailable(sig: i32, msg: &str) {
    eventlog::signal_unavailable(&sys::signal_name(sig), msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A signal this platform lets `signal_hook` register and `raise`, but which is
    /// not one of [`sys::quit_signals`], so the flags flipped here cannot interfere
    /// with the test harness.
    #[cfg(unix)]
    const SCRATCH_SIGNAL: i32 = signal_hook::consts::SIGUSR1;
    /// Windows has no `SIGUSR1`. `SIGABRT` is a real CRT signal that is not in
    /// `quit_signals`; the test registers a handler **before** raising it, so the
    /// default abort action never runs.
    #[cfg(windows)]
    const SCRATCH_SIGNAL: i32 = signal_hook::consts::SIGABRT;

    /// Registration must work on this platform's own quit signals, and a registered
    /// signal must flip both flags.
    #[test]
    fn a_registered_signal_flips_both_flags() {
        let quit = Arc::new(AtomicBool::new(false));
        let received = Arc::new(AtomicUsize::new(0));
        let sig = SCRATCH_SIGNAL;
        assert!(
            signal_hook::flag::register_usize(sig, received.clone(), sig as usize).is_ok(),
            "flag::register_usize must accept a real signal"
        );
        assert!(signal_hook::flag::register(sig, quit.clone()).is_ok());
        let _ = signal_hook::low_level::raise(sig);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && received.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(received.load(std::sync::atomic::Ordering::Relaxed), sig as usize);
        assert!(quit.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn fresh_flags_report_no_signal() {
        let flags = QuitFlags { quit: Arc::new(AtomicBool::new(false)), received: Arc::new(AtomicUsize::new(0)) };
        assert!(!flags.quit());
        assert_eq!(flags.signal(), 0, "0 means `no signal arrived`");
    }
}

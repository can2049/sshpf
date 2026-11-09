//! When the server stops: counting clients, the grace window, and the order of
//! shutdown.
//!
//! # The requirement this encodes
//!
//! "Every forward must stop when the last client closes" — because the server is
//! the only thing holding the tunnels, and leaving them running after the user's
//! last window is gone is a resource leak the user cannot see or stop.
//!
//! But closing a window is also the most common accident there is (wrong window,
//! Ctrl-W, a shell that exits). So the count reaching zero does not stop anything
//! immediately: it starts a **grace window**, and any new client inside it cancels
//! the shutdown. That is the whole mechanism, and it is why a user can close the
//! last window and reopen it without dropping every connection.
//!
//! # Two different grace windows, deliberately
//!
//! | trigger | window | why |
//! |---|---|---|
//! | the last client said `Bye` | short (500 ms) | the user confirmed; only a slip needs covering |
//! | a client's socket just ended | long (5 s) | a crash or `SIGKILL` gave nobody a chance to object |
//!
//! Sharing one constant would either make deliberate exits feel sluggish or make
//! a crashed terminal kill every tunnel before it can be reopened.
//!
//! # What `Bye` is, and is not
//!
//! `Bye { last_exit: true }` means *"the user was told the consequence and agreed"*.
//! It is a **proposal**: the server recounts and decides. A client's snapshot can
//! be seconds old, and letting a stale view stop someone else's forwards would be
//! the worst failure this whole design exists to prevent. `last_exit` therefore
//! only selects the **short** window; it never forces a shutdown.
//!
//! Belongs here: the client count, the grace state machine, and the shutdown order.
//! Does not belong: the socket ([`super::election`]), the frames
//! ([`super::broadcast`]), or what a command did ([`super::commands`]).

use std::time::{Duration, Instant};

/// How long to wait after the last client left on purpose.
pub const GRACE_CONFIRMED: Duration = Duration::from_millis(500);

/// How long to wait after the last client vanished without saying goodbye.
pub const GRACE_UNEXPECTED: Duration = Duration::from_secs(5);

/// Why the count reached zero, which selects the grace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The departing client had sent `Bye`.
    Confirmed,
    /// The socket ended with no `Bye` (crash, `SIGKILL`, closed terminal).
    Unexpected,
}

impl Trigger {
    /// The window for this trigger.
    pub fn grace(self) -> Duration {
        match self {
            Trigger::Confirmed => GRACE_CONFIRMED,
            Trigger::Unexpected => GRACE_UNEXPECTED,
        }
    }

    /// Stable token for the log.
    pub fn name(self) -> &'static str {
        match self {
            Trigger::Confirmed => "last_exit",
            Trigger::Unexpected => "eof",
        }
    }
}

/// The countdown that decides whether the server is allowed to exit.
#[derive(Debug, Default)]
pub struct Grace {
    /// When the count hit zero, if it currently is zero.
    deadline: Option<Instant>,
    /// Which window is running, for the log and for tests.
    trigger: Option<Trigger>,
}

impl Grace {
    /// A client departed, leaving `remaining` attached.
    ///
    /// Returns `true` when this departure started a countdown (i.e. it was the
    /// last one), so the caller can log it exactly once instead of every tick.
    pub fn departed(&mut self, remaining: usize, trigger: Trigger) -> bool {
        if remaining > 0 {
            // Others are still attached: whatever countdown existed is irrelevant.
            self.deadline = None;
            self.trigger = None;
            return false;
        }
        self.deadline = Some(Instant::now() + trigger.grace());
        self.trigger = Some(trigger);
        true
    }

    /// A client arrived. Returns `true` when this cancelled a running countdown.
    pub fn arrived(&mut self) -> bool {
        let was_waiting = self.deadline.take().is_some();
        self.trigger = None;
        was_waiting
    }

    /// Whether the countdown has expired.
    pub fn expired(&self) -> bool {
        self.deadline.is_some_and(|at| Instant::now() >= at)
    }

    /// Whether a countdown is running.
    pub fn waiting(&self) -> bool {
        self.deadline.is_some()
    }

    /// The window that is running, for the log and for tests.
    pub fn trigger(&self) -> Option<Trigger> {
        self.trigger
    }

    /// The remaining time, for diagnostics; zero when nothing is running.
    pub fn remaining(&self) -> Duration {
        self.deadline.map(|at| at.saturating_duration_since(Instant::now())).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_departure_with_others_attached_starts_nothing() {
        let mut grace = Grace::default();
        assert!(!grace.departed(2, Trigger::Unexpected), "two clients still attached is not the last one");
        assert!(!grace.waiting(), "no countdown may run while clients remain");
    }

    #[test]
    fn the_last_departure_starts_a_countdown() {
        let mut grace = Grace::default();
        assert!(grace.departed(0, Trigger::Confirmed));
        assert!(grace.waiting());
        assert!(!grace.expired(), "the window must not be already over");
        assert_eq!(grace.trigger(), Some(Trigger::Confirmed));
    }

    #[test]
    fn a_confirmed_exit_waits_less_than_a_crash() {
        // The two windows encode different knowledge: "the user said go" versus
        // "nobody knows what happened". Sharing one value would break one of them.
        assert!(GRACE_CONFIRMED < GRACE_UNEXPECTED, "a confirmed exit must not wait as long as a crash");
    }

    #[test]
    fn a_new_client_cancels_a_running_countdown() {
        let mut grace = Grace::default();
        assert!(grace.departed(0, Trigger::Unexpected));
        assert!(grace.arrived(), "a client inside the window must cancel the shutdown");
        assert!(!grace.waiting(), "and no countdown may survive it");
        assert!(!grace.expired(), "an expired countdown would have shut the server down");
    }

    #[test]
    fn a_cancelled_countdown_does_not_report_as_having_been_waiting() {
        let mut grace = Grace::default();
        assert!(!grace.arrived(), "nothing was running, so nothing was cancelled");
    }

    #[test]
    fn a_later_departure_with_others_clears_an_older_countdown() {
        // Sequence: last client leaves (countdown), someone reconnects, then that
        // someone leaves while a third is attached. The old countdown must not
        // still be ticking: it belonged to a different configuration of clients.
        let mut grace = Grace::default();
        assert!(grace.departed(0, Trigger::Confirmed));
        assert!(grace.arrived());
        assert!(!grace.departed(2, Trigger::Confirmed), "two attached is still not the last one");
        assert!(!grace.waiting());
    }

    #[test]
    fn the_trigger_names_are_the_documented_tokens() {
        assert_eq!(Trigger::Confirmed.name(), "last_exit");
        assert_eq!(Trigger::Unexpected.name(), "eof");
    }
}

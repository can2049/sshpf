//! When the server stops: the client count, the grace window, and the shutdown
//! sequence.
//!
//! # The requirement, stated as a rule
//!
//! **The server lives exactly as long as its clients, unless `--keep-server`.**
//! Closing the last window therefore stops every forward, including ones another
//! (now closed) window started. That is the intended meaning, and it is also the
//! most surprising thing this feature does — which is why the client warns about it
//! before sending `Bye` and why the window below exists.
//!
//! # Why a grace window instead of an immediate exit
//!
//! A disconnect is not always a decision. A terminal can be killed, a window
//! closed by accident, a client can crash. Exiting the instant the count hits zero
//! would turn every accident into "all my forwards silently stopped". So the count
//! reaching zero *arms* a timer instead of exiting; anything that reconnects during
//! it calls the shutdown off.
//!
//! # Two windows, because two causes need different patience
//!
//! | cause | window | why |
//! |---|---|---|
//! | polite `Bye{last_exit:true}` | [`BYE_GRACE`] (~0.5 s) | the user said yes to a prompt that already explained it; just long enough to cover a `Bye` that crossed a simultaneous connect |
//! | socket EOF with no `Bye` | [`ABRUPT_GRACE`] (~5 s) | an accident; long enough to reopen a terminal and attach again |
//!
//! Both are cancelled by the count becoming non-zero, and neither applies under
//! `--keep-server`.
//!
//! Belongs here: the count-down decision, arming and cancelling the window, and
//! the ordered teardown.
//! Does not belong: counting clients itself ([`super::state`]) or writing frames
//! ([`super::session`]).

use std::time::Duration;

use crate::eventlog;

use super::state::Shared;

/// How long the server waits after a client's deliberate goodbye.
pub const BYE_GRACE: Duration = Duration::from_millis(500);

/// How long the server waits after a socket died without a goodbye.
pub const ABRUPT_GRACE: Duration = Duration::from_secs(5);

/// Called whenever a client's count dropped, with the count that remains.
///
/// `last_exit` is the departed client's *proposal* that it was the last. It is
/// never trusted on its own — the count decides — but it does choose the shorter
/// window, because a user who confirmed a prompt deserves a prompt answer.
pub fn on_client_left(shared: &Shared, remaining: u32, last_exit: bool) {
    if remaining > 0 {
        // Someone is still here. If a shutdown had been armed, it has just been
        // cancelled by definition (the count left zero), so say so once.
        return;
    }
    if shared.keep_server() {
        // `--keep-server`: the whole point is to outlive the clients.
        return;
    }
    let grace = if last_exit { BYE_GRACE } else { ABRUPT_GRACE };
    let trigger = if last_exit { "last_exit" } else { "eof" };
    arm_shutdown(shared.clone(), grace, trigger);
}

/// Wait out the grace window, then stop the server unless someone came back.
///
/// Runs on its own thread so the session that reported the departure can finish
/// closing its socket.
fn arm_shutdown(shared: Shared, grace: Duration, trigger: &'static str) {
    // Serialise the arm: two clients leaving at once must not start two timers, and
    // a timer started for a count that is no longer zero would stop a server that
    // has just gained a client again.
    if shared.grace_armed.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    eventlog::server_grace_started(grace.as_millis(), trigger);
    let timer = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("sshpf-server-grace".to_string())
        .spawn(move || wait_out_grace(&timer, grace, trigger));
    if let Err(e) = spawned {
        // Without the timer the server would live forever, which is the opposite of
        // the requirement; the honest response is to stop now.
        crate::logging::error("server.grace_thread_failed", &[("msg", &e.to_string())]);
        shared.begin_stopping();
    }
}

/// Poll until the grace window elapses, calling off the shutdown if a client
/// returns.
fn wait_out_grace(shared: &Shared, grace: Duration, trigger: &'static str) {
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if shared.client_count() > 0 {
            cancel(shared);
            return;
        }
        if shared.is_stopping() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Re-check after the last poll: a client may have attached between it and the
    // deadline, and stopping with a client connected is the one outcome that must
    // never happen.
    if shared.client_count() > 0 {
        cancel(shared);
        return;
    }
    eventlog::server_shutdown_begin(shared.client_count(), shared.active_count(), trigger);
    shared.begin_stopping();
}

/// Call the armed shutdown off because a client came back.
fn cancel(shared: &Shared) {
    shared.grace_armed.store(false, std::sync::atomic::Ordering::SeqCst);
    eventlog::server_grace_cancelled(shared.client_count());
}

/// The ordered teardown, run once the accept loop has exited.
///
/// Order matters and is the same one `main` uses for the single-process build:
/// tunnels first, then the discovery files, then the persisted state — so a crash
/// during shutdown cannot leave a client dialling a port that no longer answers
/// while the tunnels are still up.
///
/// **Does not record `server.shutdown_begin`.** Whoever decided to stop already
/// did: the grace timer names its `trigger` (`last_exit` / `eof`), and a signal or a
/// natural end records `trigger=signal` / `trigger=process` on the way here. Two
/// identical lines for one shutdown would make the log look like two events.
pub fn shutdown(shared: Shared, trigger: super::ShutdownTrigger) {
    // A signal or a natural exit has not yet said why; the grace timer has.
    if !shared.is_stopping() {
        eventlog::server_shutdown_begin(shared.client_count(), shared.active_count(), trigger.name());
    }
    // A `let _ =` because the app's own shutdown must run even if the lock is
    // poisoned; `with_app` recovers from that.
    shared.with_app(|app| app.shutdown());
    // Publishing the final state is what lets a client that is still watching see
    // "everything stopped" rather than a frozen table.
    let _ = shared.publish_if_changed();
    shared.begin_stopping();
}

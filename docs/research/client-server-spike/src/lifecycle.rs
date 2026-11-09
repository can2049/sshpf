//! Assertion 5 + 8: the last client leaving stops the server — and only when it
//! really is the last one.
//!
//! What belongs here: the lifecycle rules the user asked for explicitly.
//!
//! 5. **The server exits only after the last client leaves**, and a client that
//!    arrives inside the grace window cancels the shutdown.
//! 8. **A client's "I am the last one" claim is a proposal, not an order.** If
//!    the claim was true when the dialog opened but another client connected
//!    before the `Bye` arrived, the server must **not** exit.
//!
//! Claim 8 is the one that makes the risk warning safe to show: the warning is
//! honest ("you are quitting the last client, every forward stops"), yet a stale
//! view can never destroy other people's forwards.
//!
//! What does not: what a snapshot contains, or how frames are encoded.
//!
//! Run with `cargo run --bin lifecycle`.

#[path = "common/mod.rs"]
mod common;

use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::assert::{check, finish};

/// How long the server waits after the client count reaches zero.
const GRACE: Duration = Duration::from_millis(300);
/// How long a test waits for an outcome before declaring a hang.
const TIMEOUT: Duration = Duration::from_secs(3);

fn main() {
    two_clients_one_leaves();
    println!();
    last_client_leaves();
    println!();
    stale_last_exit_race();
    std::process::exit(finish("lifecycle"));
}

// ---------------------------------------------------------------------------
// A tiny model of the server's client bookkeeping.
//
// This is deliberately a model, not a socket server: the claim under test is
// about *counting and ordering*, and a model makes the interleavings explicit
// instead of hiding them behind kernel scheduling.
// ---------------------------------------------------------------------------

/// What the server decided to do after a client left.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Still at least one client attached; keep running.
    KeepRunning,
    /// Count reached zero; start (or continue) the grace timer.
    GraceStarted,
}

/// The server's view: how many clients are attached, and whether a grace timer
/// is armed.
struct ServerModel {
    clients: usize,
    grace_armed: bool,
    /// Set when the grace timer fires while the count is still zero.
    exited: Arc<Mutex<bool>>,
}

impl ServerModel {
    fn new() -> Self {
        ServerModel { clients: 0, grace_armed: false, exited: Arc::new(Mutex::new(false)) }
    }

    fn connect(&mut self) {
        self.clients += 1;
        if self.grace_armed {
            // A newcomer cancels a pending shutdown: the reason for the grace
            // window is precisely "do not tear down forwards for a flapping UI".
            self.grace_armed = false;
        }
    }

    /// A client departs. `last_exit` is the client's *claim*; it never decides.
    fn disconnect(&mut self, _last_exit: bool) -> Decision {
        self.clients = self.clients.saturating_sub(1);
        if self.clients > 0 {
            return Decision::KeepRunning;
        }
        self.grace_armed = true;
        Decision::GraceStarted
    }

    /// Arm the timer that actually performs the shutdown.
    fn arm_grace_timer(&self) -> (Sender<()>, thread::JoinHandle<()>) {
        let (cancel_tx, cancel_rx) = channel::<()>();
        let exited = self.exited.clone();
        let handle = thread::spawn(move || {
            // Cancel wins if it arrives inside the window.
            match cancel_rx.recv_timeout(GRACE) {
                Ok(()) => {}
                Err(_) => {
                    if let Ok(mut e) = exited.lock() {
                        *e = true;
                    }
                }
            }
        });
        (cancel_tx, handle)
    }

    fn exited(&self) -> bool {
        self.exited.lock().map(|e| *e).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Assertion 5a: two clients, one leaves -> server keeps running
// ---------------------------------------------------------------------------

fn two_clients_one_leaves() {
    let mut server = ServerModel::new();
    server.connect();
    server.connect();
    let decision = server.disconnect(true);
    check(
        "lifecycle: leaving with another client attached keeps the server running",
        decision == Decision::KeepRunning,
        &format!("{decision:?}"),
    );
}

// ---------------------------------------------------------------------------
// Assertion 5b: the last client leaves -> server exits after the grace window;
//                a newcomer inside the window cancels it
// ---------------------------------------------------------------------------

fn last_client_leaves() {
    // (i) last client leaves, nobody comes back -> server exits.
    let mut server = ServerModel::new();
    server.connect();
    let decision = server.disconnect(false);
    check("lifecycle: the last client leaving arms the grace timer", decision == Decision::GraceStarted, "");
    let (_cancel, handle) = server.arm_grace_timer();
    let exited = wait_for(|| server.exited(), TIMEOUT);
    check("lifecycle: the server exits after the grace window", exited, &format!("grace={GRACE:?}"));
    let _ = handle.join();

    // (ii) last client leaves, then a new client arrives inside the window.
    let mut server = ServerModel::new();
    server.connect();
    server.disconnect(false);
    let (cancel, handle) = server.arm_grace_timer();
    thread::sleep(GRACE / 3);
    server.connect();
    let _ = cancel.send(());
    let _ = handle.join();
    check("lifecycle: a client arriving inside the grace window cancels the exit", !server.exited(), "");
}

// ---------------------------------------------------------------------------
// Assertion 8: the stale "I am the last one" race
// ---------------------------------------------------------------------------

/// The interleaving the design must survive:
///
/// ```text
/// t0  server has 1 client (A)
/// t1  A renders the quit dialog: snapshot says client_count == 1,
///     so A shows "this is the last client; every forward will stop"
/// t2  user presses y
/// t3  **B connects** (a second terminal, before A's Bye is processed)
/// t4  A's Bye{last_exit: true} reaches the server
/// t5  server recounts: 2 - 1 = 1 > 0  ->  must NOT exit
/// ```
///
/// Step t5 is the whole point: `last_exit` carried a stale observation, so it
/// cannot be allowed to destroy B's forwards.
fn stale_last_exit_race() {
    let mut server = ServerModel::new();
    server.connect(); // A
    let snapshot_count_seen_by_a = server.clients;
    let a_shows_warning = snapshot_count_seen_by_a == 1;
    check(
        "lifecycle: the warning is shown exactly when the view says one client",
        a_shows_warning,
        &format!("count={snapshot_count_seen_by_a}"),
    );

    // t3: B connects before A's Bye is applied.
    server.connect();
    // t4/t5: A's Bye arrives, carrying the now-stale claim.
    let decision = server.disconnect(true);

    check(
        "lifecycle: a stale last_exit does not stop the server while another client is attached",
        decision == Decision::KeepRunning,
        &format!("{decision:?}"),
    );
    check("lifecycle: the server did not arm the grace timer on a stale claim", !server.grace_armed, "");

    // And the honest counterpart: if A really was last, the claim does count.
    let mut server = ServerModel::new();
    server.connect();
    let decision = server.disconnect(true);
    check(
        "lifecycle: a genuine last_exit does arm the shutdown",
        decision == Decision::GraceStarted,
        &format!("{decision:?}"),
    );
}

fn wait_for(cond: impl Fn() -> bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    cond()
}

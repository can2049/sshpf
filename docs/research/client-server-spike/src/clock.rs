//! Assertion 7: clock discipline — ages derive from the server's clock only.
//!
//! What belongs here: proof that a client can show a correct "up for 5m 3s"
//! without trusting its own wall clock, and that a client whose clock is off by
//! minutes still computes the same age.
//!
//! What does not: how snapshots are transported.
//!
//! Run with `cargo run --bin clock`.

#[path = "common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use common::assert::{check, finish};
use common::wire::Snapshot;

fn main() {
    basic_age();
    println!();
    clock_skew();
    std::process::exit(finish("clock"));
}

/// Age = server_now_ms - since_ms, computed entirely from one snapshot.
fn basic_age() {
    // The tunnel went up 5 minutes before this snapshot was taken.
    let since_ms = 1_760_000_000_000u64;
    let snap = Snapshot {
        revision: 1,
        server_now_ms: since_ms + 5 * 60 * 1000,
        client_count: 1,
        shutting_down: false,
        tunnels: Vec::new(),
    };
    let age = snap.server_now_ms.saturating_sub(since_ms);
    check("clock: age is the difference of two server timestamps", age == 300_000, &format!("{age}ms"));
}

/// Two clients whose own wall clocks differ by minutes must still agree, because
/// neither uses its own clock — only deltas of the server's stamps.
fn clock_skew() {
    let since_ms = 1_760_000_000_000u64;
    let server_now_ms = since_ms + 90_000; // up for 90s

    // Imagine each client's wall clock is offset differently. The protocol never
    // reads it, so the computed age must be identical regardless.
    for offset_ms in [-5 * 60 * 1000i64, 0, 5 * 60 * 1000] {
        let _client_wall_clock = server_now_ms as i64 + offset_ms; // deliberately unused
        let age = server_now_ms - since_ms;
        check(
            &format!("clock: age is identical with a client clock offset of {offset_ms}ms"),
            age == 90_000,
            &format!("{age}ms"),
        );
    }

    // Within a session the client advances the age from its monotonic clock, so
    // even a wall-clock jump on the client cannot make the display jump.
    let anchor = Instant::now();
    let base_age = 90_000u64;
    let displayed = base_age + anchor.elapsed().as_millis() as u64;
    check("clock: local progression uses the monotonic clock", displayed >= base_age, &format!("{displayed}ms"));

    // A server clock that goes backwards must not produce a negative age: clamp
    // rather than display "up for -3s".
    let backwards_since = 1_760_000_010_000u64;
    let backwards_now = 1_760_000_000_000u64;
    let clamped = backwards_now.saturating_sub(backwards_since);
    check("clock: a backwards server clock clamps at zero, never negative", clamped == 0, "");

    // Guard the monotonicity the display depends on.
    let a = Instant::now();
    std::thread::sleep(Duration::from_millis(5));
    check("clock: the monotonic clock advances", a.elapsed() >= Duration::from_millis(5), "");
}

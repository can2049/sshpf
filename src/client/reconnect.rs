//! Waiting for a server that is not there yet, and backing off when it goes away.
//!
//! # The two waits this file owns
//!
//! They look similar and are not:
//!
//! | situation | budget | interval | why |
//! |---|---|---|---|
//! | a server was just spawned | ~5 s | 50 ms | a process needs a moment to bind, write its port file and start accepting; the user is watching an empty table and should not wait longer than a heartbeat |
//! | an established link dropped | 300 ms doubling to 5 s | — | a server that keeps dying must not be hammered, and the user must be able to see that a retry is happening |
//!
//! # Why the first attach does not retry
//!
//! If nothing answers on the very first attempt, the caller must be told: a silently
//! retrying client looks frozen, and "cannot reach the server" is a sentence `main`
//! can put on the screen. Only *after* a successful handshake does the client own a
//! connection whose loss is worth hiding behind a retry loop.
//!
//! Belongs here: the polling loop for a spawned server, and the backoff schedule.
//! Does not belong: the handshake ([`super::connect`]) or starting the server
//! ([`super::spawn`]).

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crate::help;
use crate::ipc::Endpoint;

use super::connect::{AttachError, Connection, attach};

/// How long a freshly spawned server is given to start accepting.
///
/// Generous against a cold start (the process has to load, read its config and bind)
/// but short enough that a server which failed to start is reported while the user is
/// still looking at the screen.
pub const SPAWN_BUDGET: Duration = Duration::from_secs(5);

/// How often the spawn wait re-tries the connection.
///
/// Fine enough that a fast server is attached to almost immediately, coarse enough
/// that five seconds of waiting is a hundred syscalls rather than a spin.
pub const SPAWN_POLL: Duration = Duration::from_millis(50);

/// The first reconnect delay after a dropped link.
pub const BACKOFF_FIRST: Duration = Duration::from_millis(300);

/// The longest reconnect delay.
///
/// Capped rather than doubling forever: a server restarted by hand should be picked
/// up within a few seconds, and a user watching a dead status line deserves a retry
/// that visibly keeps happening.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Wait for a just-spawned server, attaching as soon as it accepts.
///
/// Every failure that a booting child can still outgrow is expected here: "nobody
/// home" while it has not bound yet, and a timeout while it holds the lock but is
/// still starting up. Anything a *peer that answered* said — a refused token, a
/// protocol mismatch — is returned immediately, because waiting longer cannot fix it.
pub fn wait_for_spawn(endpoint: &Endpoint) -> Result<Connection, AttachError> {
    wait_within(endpoint, SPAWN_BUDGET, SPAWN_POLL)
}

/// [`wait_for_spawn`] with an explicit budget, so the timeout path is testable
/// without a five-second test.
pub fn wait_within(endpoint: &Endpoint, budget: Duration, poll: Duration) -> Result<Connection, AttachError> {
    let deadline = Instant::now() + budget;
    loop {
        match attach(endpoint) {
            Ok(connection) => return Ok(connection),
            // A refusal is final; a booting child may still answer, so keep waiting.
            Err(e) if !e.worth_waiting_out() => return Err(e),
            Err(_) if Instant::now() >= deadline => return Err(not_ready(budget)),
            Err(_) => thread::sleep(poll),
        }
    }
}

/// The error for "a server was started but never became reachable".
fn not_ready(budget: Duration) -> AttachError {
    AttachError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        help::bi(
            &format!("服务端在 {} 秒内没有就绪，请查看日志或手动运行 sshpf --server。", budget.as_secs()),
            &format!(
                "The server did not become ready within {} s; check the log or run `sshpf --server` by hand.",
                budget.as_secs()
            ),
        ),
    ))
}

/// The delay before reconnect attempt number `attempt` (1-based).
///
/// 300 ms, 600 ms, 1.2 s, 2.4 s, then 5 s forever. A pure function so the shape can
/// be asserted without sleeping.
pub fn backoff(attempt: u32) -> Duration {
    // `checked_shl` keeps a hand-built attempt count from overflowing into a shift
    // panic; the cap makes the result irrelevant past it anyway.
    let doublings = attempt.saturating_sub(1).min(8);
    let factor = 1u32.checked_shl(doublings).unwrap_or(u32::MAX);
    BACKOFF_FIRST.checked_mul(factor).unwrap_or(BACKOFF_MAX).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc;
    use std::path::Path;

    fn endpoint_in(dir: &Path, tag: &str) -> Endpoint {
        let mut endpoint = ipc::endpoint_for(dir.join("config.toml").as_path());
        endpoint.dir = dir.to_path_buf();
        endpoint.port_path = dir.join(format!("{tag}.port"));
        endpoint.token_path = dir.join(format!("{tag}.token"));
        endpoint.lock_path = dir.join(format!("{tag}.lock"));
        endpoint
    }

    #[test]
    fn the_backoff_doubles_then_stops_growing() {
        assert_eq!(backoff(1), BACKOFF_FIRST, "the first retry is prompt");
        assert_eq!(backoff(2), BACKOFF_FIRST * 2);
        assert_eq!(backoff(3), BACKOFF_FIRST * 4);
        // Capped: a long outage retries at a steady, visible pace.
        assert_eq!(backoff(20), BACKOFF_MAX);
        assert_eq!(backoff(u32::MAX), BACKOFF_MAX, "a hand-built count must not overflow");
        assert_eq!(backoff(0), BACKOFF_FIRST, "zero is treated as the first attempt");
    }

    #[test]
    fn every_delay_is_within_the_documented_bounds() {
        for attempt in 0..40 {
            let delay = backoff(attempt);
            assert!(delay >= BACKOFF_FIRST, "attempt {attempt}: {delay:?} is too eager");
            assert!(delay <= BACKOFF_MAX, "attempt {attempt}: {delay:?} is too slow");
        }
    }

    #[test]
    fn waiting_for_a_server_that_never_starts_reports_a_timeout() {
        // Nothing is listening and nothing will be: the budget must expire rather
        // than loop forever. No child is spawned, so nothing can leak.
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "cold");
        let err = wait_within(&endpoint, Duration::from_millis(150), Duration::from_millis(10))
            .expect_err("no server will appear");
        let text = err.message();
        assert!(text.contains("没有就绪"), "{text}");
        assert!(text.contains("did not become ready"), "both languages: {text}");
    }

    #[test]
    fn a_reachable_server_is_attached_to_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "live");
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "t").expect("token");
        let asked = endpoint.config_path.to_string_lossy().into_owned();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            let welcome = ipc::ServerMsg::Welcome {
                proto_version: ipc::PROTO_VERSION,
                config_path: asked,
                server_pid: 99,
                snapshot: ipc::Snapshot::default(),
            };
            let _ = ipc::write_message(&mut stream, &welcome);
            // Keep the socket open while the client finishes reading.
            thread::sleep(Duration::from_millis(100));
        });
        let connection = wait_within(&endpoint, Duration::from_secs(2), SPAWN_POLL).expect("attach");
        assert_eq!(connection.server_pid(), Some(99));
        let _ = server.join();
    }

    #[test]
    fn a_refusal_is_returned_at_once_rather_than_waited_out() {
        // A server that answers and refuses us will answer the same way in 50 ms, so
        // waiting the whole budget would only delay the message.
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "refused");
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "t").expect("token");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            let denial = ipc::ServerMsg::Error {
                code: "bad_token".into(),
                error: ipc::WireError {
                    code: "bad_token".into(), zh: "令牌不匹配".into(), en: "bad token".into()
                },
            };
            let _ = ipc::write_message(&mut stream, &denial);
        });
        let started = Instant::now();
        let err =
            wait_within(&endpoint, Duration::from_secs(30), Duration::from_millis(10)).expect_err("a refusal is final");
        assert!(started.elapsed() < Duration::from_secs(10), "it must not consume the budget");
        assert!(!err.may_spawn());
    }

    #[test]
    fn the_spawn_budget_is_short_enough_to_report_a_failure_promptly() {
        // Five seconds is the documented promise to the user: longer would look like
        // a hang, shorter would refuse a slow cold start.
        assert!(SPAWN_BUDGET >= Duration::from_secs(3), "{SPAWN_BUDGET:?} is too tight for a cold start");
        assert!(SPAWN_BUDGET <= Duration::from_secs(10), "{SPAWN_BUDGET:?} leaves a user waiting too long");
        assert!(SPAWN_POLL * 10 < SPAWN_BUDGET, "the poll interval must fit many times into the budget");
    }

    /// A child that has elected itself holds the lock but may not have bound yet, so
    /// the first dials in that window time out and classify as `Io`. Treating an `Io`
    /// as final aborted the wait on the spot and reported a timeout instead of the
    /// server that was starting behind it — so the wait must outlast it.
    #[test]
    fn a_booting_server_that_holds_the_lock_is_waited_for_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "booting");
        // The shape of a child mid-boot: it holds the lock, but no listener yet.
        let held = ipc::try_become_singleton(&endpoint.lock_path).expect("acquire");
        assert!(held.is_some(), "the test must actually hold the lock");

        let started = Instant::now();
        let err = wait_within(&endpoint, Duration::from_millis(300), Duration::from_millis(10))
            .expect_err("a server that never binds must still be reported");
        // It must have waited out the budget rather than returning on the first dial.
        assert!(started.elapsed() >= Duration::from_millis(250), "it must not give up on the first timeout");
        // And then say the server was started but never became ready, not "unreachable".
        let text = err.message();
        assert!(text.contains("没有就绪"), "{text}");
        assert!(!err.may_spawn(), "a server exists; spawning another cannot help");
    }

    /// The distinction that must survive the change: an answer that *refuses* us is
    /// final even while waiting, and stays distinguishable from a booting child.
    #[test]
    fn a_refusal_still_ends_the_wait_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "still-refused");
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "t").expect("token");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            let denial = ipc::ServerMsg::Error {
                code: "bad_token".into(),
                error: ipc::WireError {
                    code: "bad_token".into(), zh: "令牌不匹配".into(), en: "bad token".into()
                },
            };
            let _ = ipc::write_message(&mut stream, &denial);
        });
        let started = Instant::now();
        let err = wait_within(&endpoint, Duration::from_secs(30), Duration::from_millis(10))
            .expect_err("a refusal is final even mid-wait");
        assert!(started.elapsed() < Duration::from_secs(5), "a refusal must not consume the budget");
        assert!(!err.worth_waiting_out(), "a peer that answered will answer the same way again");
    }
}

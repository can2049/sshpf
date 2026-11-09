//! Periodic liveness probing of a live SSH session.
//!
//! # Why this exists
//!
//! A local `-L`/`-D` listener cannot tell that its SSH session died. The kernel
//! keeps accepting on the bound port, so `accept()` keeps returning sockets and
//! the status stays `●` while every forwarded connection fails. That is exactly
//! what a laptop looks like after sleep/resume: the TCP connection is half open,
//! the peer is gone, and nothing on our side ever finds out. A `-R` tunnel has
//! the same problem from the server's side.
//!
//! The fix is to ask the question the transport cannot answer passively: send a
//! `keepalive@openssh.com` [global request](russh::client::Handle::send_global_request)
//! that *wants a reply*, and wait for it. A session that answers is alive; one
//! that does not has been staying `●` on a dead connection.
//!
//! # What counts as dead
//!
//! | observed | [`StaleCause`] | meaning |
//! |---|---|---|
//! | the reply did not arrive within [`PING_TIMEOUT`] | `PingTimeout` | the link is gone (resume, network change, black-holed route) |
//! | sending the request failed | `PingError` | the session's write side is already closed |
//! | `Handle::is_closed()` | `SessionClosed` | russh has already torn the session down |
//!
//! A server that answers with `REQUEST_FAILURE` is *alive*: `send_global_request`
//! reports that as `Err(RequestDenied)`, which means bytes went out and came
//! back. Only a missing answer, in the shape of our own timeout, is a death
//! certificate — this is the distinction that keeps a server which simply does
//! not implement the extension from being killed every interval.
//!
//! # Why the built-in keepalive is not enough
//!
//! [`connect`](super::connect) already configures russh's own keepalive
//! (`keepalive_interval = 15s`, `keepalive_max = 3`), so a wedged session does
//! eventually produce `Error::KeepaliveTimeout` — but that error ends the
//! *session* task, and nothing downstream ever looks at it. The serve loops only
//! await `accept()` (or `pending()` for `-R`), so they never observe the session
//! going away: the listener keeps accepting onto a dead session, which is
//! precisely the "shows `●` but does not forward" symptom. A passive timeout
//! buried in a task nobody reads is not a detection path; an explicit probe
//! raced *inside* the serve loop is.
//!
//! # Where the verdict goes
//!
//! Nowhere new: a dead session ends this run's driver with
//! [`FailureKind::StaleConnection`] (see [`super::driver`]), which travels the
//! normal path — `Exited { reason }` → `was_up = true` → backoff retry. Reusing
//! the existing supervision is what keeps "a stale link" from needing a state
//! machine of its own, and it is why `auto_restart` governs it like any other
//! drop.
//!
//! # What does not belong here
//!
//! Reconnecting, retry policy, status changes, rendering: those live in
//! `app::events`. This module only answers "is this session still alive?".

use std::time::Duration;

use anyhow::Result;
use russh::client;

use crate::model::FailureKind;
use crate::ssh::client::handler::Handler;

/// How long a probe may take to be answered before the session is declared dead.
///
/// Kept well under [`MIN_HEALTH_CHECK_SECS`](crate::config::MIN_HEALTH_CHECK_SECS)
/// so one slow probe cannot overlap the next interval, and low enough that a
/// resumed machine notices within seconds rather than at the next tick.
pub const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// Name of the request we send. OpenSSH's own keepalive, so a stock `sshd`
/// answers it with `REQUEST_SUCCESS` or `REQUEST_FAILURE` instead of ignoring an
/// unknown extension.
const KEEPALIVE_REQUEST: &str = "keepalive@openssh.com";

/// Why a session was judged dead.
///
/// The ASCII names are the `cause=` values in `event=tunnel.health_failed`, so
/// they are a log contract: do not reword them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleCause {
    /// The keepalive went unanswered for [`PING_TIMEOUT`].
    PingTimeout,
    /// The request could not even be sent (write side already closed).
    PingError,
    /// russh had already marked the session closed.
    SessionClosed,
}

impl StaleCause {
    /// Stable ASCII token for the `cause=` log field.
    pub fn name(self) -> &'static str {
        match self {
            StaleCause::PingTimeout => "ping_timeout",
            StaleCause::PingError => "ping_error",
            StaleCause::SessionClosed => "session_closed",
        }
    }

    /// Chinese half of the user-facing detail, for the log line and the panel.
    fn detail(self) -> &'static str {
        match self {
            StaleCause::PingTimeout => "探活超时，SSH 会话已失效（睡眠唤醒或网络中断后常见）",
            StaleCause::PingError => "探活请求无法发出，SSH 会话已关闭",
            StaleCause::SessionClosed => "SSH 会话已被关闭",
        }
    }
}

/// Probe the session once.
///
/// `Ok(())` means "the peer answered, the path is alive". `Err(cause)` means the
/// session must be torn down; the caller turns that into a
/// [`FailureKind::StaleConnection`] exit.
pub(crate) async fn probe(handle: &client::Handle<Handler>) -> Result<(), StaleCause> {
    if handle.is_closed() {
        return Err(StaleCause::SessionClosed);
    }
    // `want_reply = true` is the whole point: without it the request is fire and
    // forget and can never detect a black-holed connection.
    let sent = tokio::time::timeout(PING_TIMEOUT, handle.send_global_request(KEEPALIVE_REQUEST, &[], true)).await;
    match sent {
        // A transport-level failure (send error, disconnect instead of a reply)
        // means the session is gone.
        Err(_elapsed) => Err(StaleCause::PingTimeout),
        Ok(Err(russh::Error::RequestDenied)) => {
            // The server answered, and said no. It is alive: OpenSSH replies
            // `REQUEST_FAILURE` to `keepalive@openssh.com`, which russh surfaces
            // as `RequestDenied`. Treating that as death would kill a perfectly
            // healthy session every interval.
            Ok(())
        }
        Ok(Err(_other)) => Err(StaleCause::PingError),
        Ok(Ok(_reply)) => Ok(()),
    }
}

/// The run-ending error for a dead session.
///
/// Carried as a typed cause (the same trick as
/// [`HostKeyRejected`](super::classify::HostKeyRejected)) so
/// [`classify`](super::classify) selects [`FailureKind::StaleConnection`] by
/// type rather than by guessing from the message text — "探活超时" would
/// otherwise be classified as a plain `Timeout`, which is the wrong story for
/// the table and the retry log.
#[derive(Debug)]
pub(crate) struct StaleSession {
    pub(crate) cause: StaleCause,
}

impl StaleSession {
    pub(crate) fn new(cause: StaleCause) -> Self {
        StaleSession { cause }
    }
}

impl std::fmt::Display for StaleSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.cause.detail())
    }
}

impl std::error::Error for StaleSession {}

/// Render a [`StaleCause`] as the failure that ends the run.
///
/// Lives here rather than in `driver` so the two halves of the verdict — the
/// ASCII `cause` in the log and the `FailureKind` in the status table — cannot
/// drift apart.
pub(crate) fn stale_failure(cause: StaleCause) -> crate::model::FailureInfo {
    crate::model::FailureInfo::with_short(
        FailureKind::StaleConnection,
        FailureKind::StaleConnection.short(),
        cause.detail(),
    )
}

/// Poll the session until it fails, or `forever()` never resolves.
///
/// Returns the cause of death instead of looping by itself so the driver keeps
/// one obvious place where a run ends.
pub(crate) async fn watch(handle: std::sync::Arc<client::Handle<Handler>>, interval: Duration) -> StaleCause {
    loop {
        tokio::time::sleep(interval).await;
        if let Err(cause) = probe(&handle).await {
            return cause;
        }
    }
}

/// Turn a probe verdict into the `anyhow` error the driver returns.
///
/// Typed (see [`StaleSession`]) so the supervisor's classification cannot fall
/// back to text matching.
pub(crate) fn stale_error(cause: StaleCause) -> anyhow::Error {
    anyhow::Error::new(StaleSession::new(cause))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cause_has_a_stable_ascii_token() {
        for cause in [StaleCause::PingTimeout, StaleCause::PingError, StaleCause::SessionClosed] {
            let name = cause.name();
            assert!(name.is_ascii(), "{cause:?} leaked a non-ASCII token: {name}");
            assert!(!name.is_empty());
            assert!(!name.contains(' '), "log values are space separated: {name}");
        }
        // Pinned: these are log contract values (`cause=`), not internal names.
        assert_eq!(StaleCause::PingTimeout.name(), "ping_timeout");
        assert_eq!(StaleCause::PingError.name(), "ping_error");
        assert_eq!(StaleCause::SessionClosed.name(), "session_closed");
    }

    /// The failure surfaced to the table/panel must be the retryable, clearly
    /// named one — a stale link self-heals once the network is back.
    #[test]
    fn a_stale_session_is_a_retryable_failure_with_a_readable_detail() {
        for cause in [StaleCause::PingTimeout, StaleCause::PingError, StaleCause::SessionClosed] {
            let info = stale_failure(cause);
            assert_eq!(info.kind, FailureKind::StaleConnection, "{cause:?}");
            assert!(info.kind.is_retryable(), "{cause:?} must schedule a reconnect");
            assert!(!info.detail.is_empty(), "{cause:?} has no detail");
        }
    }

    /// The probe interval must never be shorter than the probe itself: two
    /// probes in flight on one session would report two overlapping verdicts.
    #[test]
    fn the_probe_timeout_fits_inside_the_shortest_interval() {
        assert!(
            PING_TIMEOUT <= Duration::from_secs(crate::config::MIN_HEALTH_CHECK_SECS),
            "PING_TIMEOUT ({PING_TIMEOUT:?}) exceeds the minimum interval"
        );
    }
}

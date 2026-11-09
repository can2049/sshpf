//! Turning a wire snapshot back into the live types the renderer draws.
//!
//! The only interesting part is time. Every status on the wire carries an
//! absolute millisecond stamp on the **server's** wall clock; the renderer wants
//! an `Instant` on **this** process's monotonic clock. The conversion therefore
//! uses only a *difference* (`stamp - server_now`) applied to a locally captured
//! `Instant`, so any fixed offset between the two machines cancels out — a server
//! whose clock is five minutes fast produces exactly the same age as one that
//! agrees with us.
//!
//! Belongs here: `Snapshot`/`WireStatus`/`WireFailure`/`WireLogLine` → live types.
//! Does not belong: when to apply them (`super::inbound`), what they mean
//! ([`crate::ui`]), or how they are encoded ([`crate::ipc::codec`]).

use std::time::{Duration, Instant};

use crate::ipc::{WireLogLine, WireStatus, failure_kind_from_token, level_from_token};
use crate::model::{FailureInfo, LogLine, TunnelStatus, TunnelView};

/// Build the live view for one wire tunnel, turning server-clock stamps back into
/// local `Instant`s relative to this frame.
pub(super) fn live_view(wire: crate::ipc::WireTunnel, now_ms: i64, local_now: Instant) -> TunnelView {
    let mut view = TunnelView::new(wire.cfg);
    view.status = live_status(wire.status, now_ms, local_now);
    view.retries = wire.retries;
    for line in wire.log_tail {
        view.log.push(live_line(line));
    }
    view
}

/// Rebuild a status: ages and countdowns are recomputed against the local clock
/// using only the *difference* from the snapshot's own clock, so a fixed offset
/// between the two machines cancels out.
fn live_status(status: WireStatus, now_ms: i64, local_now: Instant) -> TunnelStatus {
    match status {
        WireStatus::Stopped => TunnelStatus::Stopped,
        WireStatus::Starting { since_ms } => TunnelStatus::Starting { since: shift_past(since_ms, now_ms, local_now) },
        WireStatus::Up { since_ms } => TunnelStatus::Up { since: shift_past(since_ms, now_ms, local_now) },
        WireStatus::Retrying { attempt, next_at_ms, reason } => TunnelStatus::Retrying {
            attempt,
            next_at: shift_future(next_at_ms, now_ms, local_now),
            reason: live_failure(reason),
        },
        WireStatus::Failed { reason } => TunnelStatus::Failed { reason: live_failure(reason) },
    }
}

/// A stamp in the past becomes a local `Instant` that many ms ago.
fn shift_past(at_ms: i64, now_ms: i64, local_now: Instant) -> Instant {
    let age = now_ms.saturating_sub(at_ms).max(0);
    local_now.checked_sub(Duration::from_millis(u64::try_from(age).unwrap_or(0))).unwrap_or(local_now)
}

/// A stamp in the future becomes a local `Instant` that many ms ahead.
fn shift_future(at_ms: i64, now_ms: i64, local_now: Instant) -> Instant {
    let ahead = at_ms.saturating_sub(now_ms).max(0);
    local_now.checked_add(Duration::from_millis(u64::try_from(ahead).unwrap_or(0))).unwrap_or(local_now)
}

/// Rebuild the failure, keeping the server's wording and stable kind token.
fn live_failure(wire: crate::ipc::WireFailure) -> FailureInfo {
    let kind = failure_kind_from_token(&wire.kind);
    FailureInfo::with_short(kind, wire.short, wire.detail)
}

/// Rebuild one log line; the timestamp is already formatted by the server.
fn live_line(wire: WireLogLine) -> LogLine {
    LogLine { time: wire.time, level: level_from_token(&wire.level), text: wire.text }
}

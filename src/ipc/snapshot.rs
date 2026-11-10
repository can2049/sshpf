//! The wire mirror of the live state: what a client renders from, and the only
//! thing that ever crosses the socket.
//!
//! # Why a mirror instead of the real types
//!
//! The live `TunnelStatus` stores `Instant` for `since` / `next_at`. An `Instant`
//! is a monotonic-clock reading whose origin is per-process, so it cannot be
//! sent to another process and mean anything. Every time here is therefore an
//! integer millisecond stamp on the **server's** wall clock, and a client derives
//! ages from differences against [`Snapshot::server_now_ms`].
//!
//! Keeping a separate mirror also means the on-the-wire shape can evolve without
//! constraining an internal refactor: [`Snapshot`] is a protocol contract,
//! `TunnelView` is an implementation detail.
//!
//! Belongs here: the serialisable structs, and the conversions from the live
//! types.
//! Does not belong: encoding (`codec`), framing (`frame`), or which message
//! carries them (`msg`).

use crate::model::{FailureInfo, FailureKind, LogLevel, LogLine, TunnelConfig, TunnelStatus, TunnelView};

/// How many log lines travel inside a snapshot.
///
/// Matches `ui::layout::DETAIL_LOG_TAIL`, which is all the detail panel ever
/// renders. Measured: a 50-tunnel snapshot is ~94 KiB with 15-line tails but
/// ~5.2 MiB with full 1000-line rings — the latter exceeds the frame limit
/// outright, so truncation is a protocol requirement, not an optimisation.
/// **Must equal `ui::layout::DETAIL_LOG_TAIL`**, which is all the detail panel
/// renders. It is duplicated rather than imported because `ui` is a *higher*
/// layer than `ipc`: reaching up for it would be a reverse dependency. The
/// equality is asserted from the `ui` side, which may legally look down here.
pub const SNAPSHOT_LOG_TAIL: usize = 15;

/// One tunnel's state, as it travels over the socket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WireTunnel {
    /// The persisted definition, reused unchanged so the wire format cannot
    /// drift from `config.toml`.
    pub cfg: TunnelConfig,
    /// Lifecycle state; see [`WireStatus`].
    pub status: WireStatus,
    /// Automatic reconnect attempts so far, for the detail panel.
    pub retries: u32,
    /// The tail of this tunnel's log, newest last, at most
    /// [`SNAPSHOT_LOG_TAIL`] entries.
    #[serde(default)]
    pub log_tail: Vec<WireLogLine>,
}

/// One log line, with the level as a lowercase ASCII token.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WireLogLine {
    /// Pre-formatted `HH:MM:SS`, produced by the server so a client never has to
    /// read its own clock to render a timestamp.
    pub time: String,
    /// `debug` / `info` / `warn` / `error`.
    pub level: String,
    pub text: String,
}

/// Lifecycle state with every time as a server-clock millisecond stamp.
///
/// `Option` is used for at most one level: TOML has no `null` and a nested
/// `Option<Option<_>>` is lossy, so a two-state field is expressed by presence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "s")]
pub enum WireStatus {
    /// Not running.
    #[serde(rename = "stopped")]
    Stopped,
    /// Connecting, not yet forwarding.
    #[serde(rename = "starting")]
    Starting { since_ms: i64 },
    /// Forwarding is live.
    #[serde(rename = "up")]
    Up { since_ms: i64 },
    /// Waiting for the next automatic attempt.
    #[serde(rename = "retrying")]
    Retrying { attempt: u32, next_at_ms: i64, reason: WireFailure },
    /// Terminal failure; the user has to act.
    #[serde(rename = "failed")]
    Failed { reason: WireFailure },
}

/// A classified failure, with the ASCII kind token as the stable identifier.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WireFailure {
    /// Stable ASCII token from `FailureKind::name()` (e.g. `port_in_use`).
    ///
    /// Deliberately not the Chinese `short()` string: that wording may be
    /// reworded between versions, which would silently change the meaning of an
    /// otherwise identical message.
    pub kind: String,
    /// Chinese short phrase, for the table column.
    pub short: String,
    /// Raw diagnostic text, shown verbatim in the detail panel.
    pub detail: String,
}

/// The whole authoritative state of a server at one revision.
///
/// Sent in full on every change rather than as a delta: a full snapshot makes an
/// out-of-order, duplicated or dropped frame harmless, because the next one
/// restates everything.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    /// Strictly increasing; a client ignores anything not newer than what it
    /// already applied.
    pub revision: u64,
    /// The server's wall clock when this snapshot was produced (epoch ms).
    pub server_now_ms: i64,
    /// How many clients are attached, **including** the one being served.
    ///
    /// This is the server's own count, never a number a client reported: it
    /// decides whether quitting would stop every forward for everyone.
    pub client_count: u32,
    /// True when the server will not exit just because its clients did.
    pub keep_server: bool,
    /// Live tunnels, in table order.
    #[serde(default)]
    pub tunnels: Vec<WireTunnel>,
}

impl Snapshot {
    /// Build a wire snapshot from the live views, stamping every time.
    ///
    /// `now_ms` is passed in rather than read here so the caller can make one
    /// clock reading serve a whole snapshot: two rows must not disagree about
    /// what "now" is.
    pub fn from_views(
        revision: u64,
        now_ms: i64,
        client_count: u32,
        keep_server: bool,
        views: &[TunnelView],
    ) -> Snapshot {
        let tunnels = views.iter().map(|view| wire_tunnel(view, now_ms)).collect();
        Snapshot { revision, server_now_ms: now_ms, client_count, keep_server, tunnels }
    }

    /// Rebuild live views from a snapshot, on the client side.
    ///
    /// The inverse of [`Snapshot::from_views`]: each server-clock stamp is turned
    /// back into a local [`Instant`](std::time::Instant), so the renderer's
    /// "running for 3m" arithmetic works unchanged. Only the *difference* between
    /// a stamp and [`Self::server_now_ms`] is used, which is what makes this
    /// immune to clock skew between the two processes.
    ///
    /// The log ring is truncated to the tail the snapshot carried; that is all a
    /// client is ever sent, so the ring holds less history than the server's.
    pub fn to_views(&self) -> Vec<TunnelView> {
        self.tunnels.iter().map(|t| view_from_wire(t, self.server_now_ms)).collect()
    }
}

/// Rebuild one live view, converting server-clock stamps back into local
/// [`Instant`](std::time::Instant)s relative to `now_ms`.
fn view_from_wire(wire: &WireTunnel, now_ms: i64) -> TunnelView {
    let mut log = crate::model::LogRing::default();
    for line in &wire.log_tail {
        log.push(LogLine::new(level_from_token(&line.level), line.text.clone()));
    }
    TunnelView { cfg: wire.cfg.clone(), status: status_from_wire(&wire.status, now_ms), log, retries: wire.retries }
}

/// Turn one stamped status back into a local `Instant`-based one.
///
/// `unwind` orients the conversion: a start time is in the past, a retry deadline
/// in the future, so each direction is named rather than guessed from the sign.
fn status_from_wire(status: &WireStatus, now_ms: i64) -> TunnelStatus {
    match status {
        WireStatus::Stopped => TunnelStatus::Stopped,
        WireStatus::Starting { since_ms } => TunnelStatus::Starting { since: instant_of(*since_ms, now_ms) },
        WireStatus::Up { since_ms } => TunnelStatus::Up { since: instant_of(*since_ms, now_ms) },
        WireStatus::Retrying { attempt, next_at_ms, reason } => TunnelStatus::Retrying {
            attempt: *attempt,
            next_at: instant_of(*next_at_ms, now_ms),
            reason: failure_from_wire(reason),
        },
        WireStatus::Failed { reason } => TunnelStatus::Failed { reason: failure_from_wire(reason) },
    }
}

/// Map a server-clock stamp onto a local `Instant`.
///
/// `now_ms` is the server's clock at snapshot time and `Instant::now()` is the
/// local receipt time, so the pair means the same "when" in two process-local
/// clocks. The arithmetic is saturating both ways: a stamp far in either
/// direction must degrade to "a long time ago" / "a long time from now" rather
/// than overflow.
fn instant_of(stamp_ms: i64, now_ms: i64) -> std::time::Instant {
    let now = std::time::Instant::now();
    let delta = stamp_ms.saturating_sub(now_ms);
    if delta >= 0 {
        let ahead = std::time::Duration::from_millis(u64::try_from(delta).unwrap_or(u64::MAX));
        now.checked_add(ahead).unwrap_or(now)
    } else {
        let behind = std::time::Duration::from_millis(u64::try_from(delta.saturating_neg()).unwrap_or(u64::MAX));
        now.checked_sub(behind).unwrap_or(now)
    }
}

/// Rebuild a classified failure from its wire form.
fn failure_from_wire(wire: &WireFailure) -> FailureInfo {
    FailureInfo::with_short(failure_kind_from_token(&wire.kind), wire.short.clone(), wire.detail.clone())
}

/// Project one live view onto the wire, truncating its log to the tail.
fn wire_tunnel(view: &TunnelView, now_ms: i64) -> WireTunnel {
    WireTunnel {
        cfg: view.cfg.clone(),
        status: wire_status(&view.status, now_ms),
        retries: view.retries,
        log_tail: wire_log_tail(&view.log),
    }
}

/// Convert a status, turning each `Instant` into an age in milliseconds.
///
/// A clock that moved backwards yields a negative age rather than a panic; the
/// renderer clamps it, which is strictly better than refusing to send state.
fn wire_status(status: &TunnelStatus, now_ms: i64) -> WireStatus {
    match status {
        TunnelStatus::Stopped => WireStatus::Stopped,
        TunnelStatus::Starting { since } => WireStatus::Starting { since_ms: stamp(*since, now_ms) },
        TunnelStatus::Up { since } => WireStatus::Up { since_ms: stamp(*since, now_ms) },
        TunnelStatus::Retrying { attempt, next_at, reason } => WireStatus::Retrying {
            attempt: *attempt,
            next_at_ms: stamp(*next_at, now_ms),
            reason: wire_failure(reason),
        },
        TunnelStatus::Failed { reason } => WireStatus::Failed { reason: wire_failure(reason) },
    }
}

/// Turn an `Instant` into an absolute server-clock stamp.
///
/// The live value is an elapsed-time reading; the wire value is "when, on the
/// server's clock". Future instants (a scheduled retry) come out ahead of
/// `now_ms`, which is exactly what a countdown needs.
fn stamp(at: std::time::Instant, now_ms: i64) -> i64 {
    let now = std::time::Instant::now();
    let delta = if at >= now {
        // A future instant: the difference is a delay to add.
        i64::try_from((at - now).as_millis()).unwrap_or(i64::MAX)
    } else {
        // A past instant: the difference is an age to subtract.
        -i64::try_from((now - at).as_millis()).unwrap_or(i64::MAX)
    };
    now_ms.saturating_add(delta)
}

fn wire_failure(info: &FailureInfo) -> WireFailure {
    WireFailure { kind: info.kind.name().to_string(), short: info.short.clone(), detail: info.detail.clone() }
}

/// Keep only the last [`SNAPSHOT_LOG_TAIL`] lines, oldest first.
fn wire_log_tail(ring: &crate::model::LogRing) -> Vec<WireLogLine> {
    ring.tail(SNAPSHOT_LOG_TAIL).iter().map(wire_line).collect()
}

fn wire_line(line: &LogLine) -> WireLogLine {
    WireLogLine { time: line.time.clone(), level: level_token(line.level).to_string(), text: line.text.clone() }
}

/// The lowercase ASCII token for a log level.
pub fn level_token(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

/// Parse a level token back, defaulting to `info` for an unknown one.
///
/// Unknown levels become `info` rather than an error: a newer server adding a
/// level must not make an older client drop the line entirely.
pub fn level_from_token(token: &str) -> LogLevel {
    match token {
        "debug" => LogLevel::Debug,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    }
}

/// Map an ASCII kind token back to a `FailureKind`, falling back to `Unknown`.
pub fn failure_kind_from_token(token: &str) -> FailureKind {
    for kind in ALL_FAILURE_KINDS {
        if kind.name() == token {
            return *kind;
        }
    }
    FailureKind::Unknown
}

/// Every failure kind, so a token can be resolved without a hand-written table
/// that would silently omit a newly added variant.
const ALL_FAILURE_KINDS: &[FailureKind] = &[
    FailureKind::PortInUse,
    FailureKind::PermissionDenied,
    FailureKind::AuthFailed,
    FailureKind::DnsFailure,
    FailureKind::ConnectionRefused,
    FailureKind::Timeout,
    FailureKind::HostKeyMismatch,
    FailureKind::RemoteForwardFailed,
    FailureKind::TargetUnreachable,
    FailureKind::StaleConnection,
    FailureKind::SpawnFailed,
    FailureKind::Exited,
    FailureKind::Unsupported,
    FailureKind::Unknown,
];

#[cfg(test)]
mod tests;

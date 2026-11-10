//! Live state of a running tunnel: its status, its log ring, and the view the
//! renderer reads.
//!
//! A `TunnelView` is definition (`TunnelConfig`) + live state + the per-tunnel
//! log. The status is rebuilt from scratch on every start/stop, so it holds no
//! history of its own; the log ring is where history goes, and it is bounded by
//! construction (an unbounded ring is a memory leak with a nice name).
//!
//! Belongs here: lifecycle states, log lines/rings, their accessors.
//! Does not belong: spawning ssh (`tunnel`), formatting rows (`ui`).

use std::collections::VecDeque;
use std::time::Instant;

use super::failure::FailureInfo;
use super::msg::now_hms;
use super::tunnel::TunnelConfig;

/// Live status of a tunnel. Rebuilt from scratch on every start/stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelStatus {
    /// Not running (user intent off, or stopped by hand).
    Stopped,
    /// ssh spawned, waiting for the forwarding-ready marker.
    Starting { since: Instant },
    /// Forwarding is listening and the session is established.
    Up { since: Instant },
    /// Waiting for the next automatic reconnect attempt.
    Retrying { attempt: u32, next_at: Instant, reason: FailureInfo },
    /// Terminal failure; no automatic retry.
    Failed { reason: FailureInfo },
}

impl TunnelStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, TunnelStatus::Up { .. } | TunnelStatus::Starting { .. })
    }

    pub fn is_running_or_pending(&self) -> bool {
        !matches!(self, TunnelStatus::Stopped)
    }

    /// Stable lowercase key used by the UI for styling.
    pub fn key(&self) -> &'static str {
        match self {
            TunnelStatus::Stopped => "stopped",
            TunnelStatus::Starting { .. } => "starting",
            TunnelStatus::Up { .. } => "up",
            TunnelStatus::Retrying { .. } => "retrying",
            TunnelStatus::Failed { .. } => "failed",
        }
    }

    pub fn failure(&self) -> Option<&FailureInfo> {
        match self {
            TunnelStatus::Failed { reason } => Some(reason),
            TunnelStatus::Retrying { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// One line in a tunnel's per-tunnel log ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Pre-formatted `HH:MM:SS`.
    pub time: String,
    pub level: LogLevel,
    pub text: String,
}

impl LogLine {
    pub fn new(level: LogLevel, text: impl Into<String>) -> Self {
        LogLine { time: now_hms(), level, text: text.into() }
    }
}

/// Bounded FIFO of log lines.
#[derive(Debug, Clone)]
pub struct LogRing {
    buf: VecDeque<LogLine>,
    cap: usize,
}

impl LogRing {
    pub fn new(cap: usize) -> Self {
        // A zero-capacity ring would never evict: `push` pops only when
        // `len == cap`, so `cap == 0` grows without bound. The bound is the
        // whole point of this type, so enforce it here rather than at every
        // call site.
        let cap = cap.max(1);
        LogRing { buf: VecDeque::with_capacity(cap.min(1024)), cap }
    }

    pub fn push(&mut self, line: LogLine) {
        while self.buf.len() >= self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(line);
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &LogLine> {
        self.buf.iter()
    }

    pub fn last(&self) -> Option<&LogLine> {
        self.buf.back()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// How many lines this ring holds before evicting.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let skip = self.buf.len().saturating_sub(n);
        self.buf.iter().skip(skip).cloned().collect()
    }
}

impl Default for LogRing {
    fn default() -> Self {
        LogRing::new(1000)
    }
}

/// A tunnel as presented by the UI: definition + live state + its log.
#[derive(Debug, Clone)]
pub struct TunnelView {
    pub cfg: TunnelConfig,
    pub status: TunnelStatus,
    pub log: LogRing,
    /// Number of automatic reconnect attempts made so far (for the detail panel).
    pub retries: u32,
}

impl TunnelView {
    pub fn new(cfg: TunnelConfig) -> Self {
        let running = cfg.enabled;
        TunnelView {
            cfg,
            status: if running { TunnelStatus::Starting { since: Instant::now() } } else { TunnelStatus::Stopped },
            log: LogRing::new(1000),
            retries: 0,
        }
    }

    /// Short reason shown in the LAST ERROR column, `None` when healthy.
    pub fn last_error(&self) -> Option<String> {
        self.status.failure().map(|f| f.short.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardKind;
    use crate::model::fixtures::sample;
    use crate::model::{FailureInfo, FailureKind};
    use std::time::Instant;

    #[test]
    fn status_helpers_agree_with_the_variant() {
        let up = TunnelStatus::Up { since: Instant::now() };
        assert!(up.is_active());
        assert!(up.is_running_or_pending());
        assert_eq!(up.key(), "up");
        assert!(up.failure().is_none());

        let starting = TunnelStatus::Starting { since: Instant::now() };
        assert!(starting.is_active());
        assert_eq!(starting.key(), "starting");

        let stopped = TunnelStatus::Stopped;
        assert!(!stopped.is_active());
        assert!(!stopped.is_running_or_pending());
        assert_eq!(stopped.key(), "stopped");

        let info = FailureInfo::new(FailureKind::AuthFailed, "detail");
        let failed = TunnelStatus::Failed { reason: info.clone() };
        assert!(!failed.is_active());
        // A failed tunnel is not "running", but it is not "off" either: the user
        // asked for it to run and it is not running, so retry/stop apply.
        assert!(failed.is_running_or_pending());
        assert_eq!(failed.key(), "failed");
        assert_eq!(failed.failure(), Some(&info));

        let retrying = TunnelStatus::Retrying { attempt: 3, next_at: Instant::now(), reason: info };
        assert_eq!(retrying.key(), "retrying");
        assert!(retrying.failure().is_some());
    }

    #[test]
    fn tunnel_view_has_no_error_when_healthy() {
        let mut cfg = sample(ForwardKind::Local);
        cfg.enabled = false;
        let mut v = TunnelView::new(cfg);
        v.status = TunnelStatus::Up { since: Instant::now() };
        assert!(v.last_error().is_none());

        v.status =
            TunnelStatus::Failed { reason: FailureInfo::with_short(FailureKind::AuthFailed, "认证失败", "stderr") };
        assert_eq!(v.last_error().as_deref(), Some("认证失败"));
    }

    #[test]
    fn log_ring_bounded() {
        let mut r = LogRing::new(3);
        for i in 0..10 {
            r.push(LogLine::new(LogLevel::Info, format!("{i}")));
        }
        assert_eq!(r.len(), 3);
        let tail: Vec<_> = r.iter().map(|l| l.text.clone()).collect();
        assert_eq!(tail, vec!["7", "8", "9"]);
    }

    #[test]
    fn log_ring_tail_and_clear() {
        let mut r = LogRing::new(5);
        for i in 0..8 {
            r.push(LogLine::new(LogLevel::Info, format!("{i}")));
        }
        assert_eq!(r.len(), 5);
        let tail: Vec<_> = r.tail(2).into_iter().map(|l| l.text).collect();
        assert_eq!(tail, vec!["6", "7"]);
        // Asking for more than we have returns everything, oldest first.
        assert_eq!(r.tail(100).len(), 5);
        assert_eq!(r.last().map(|l| l.text.as_str()), Some("7"));
        r.clear();
        assert!(r.is_empty());
        assert!(r.last().is_none());
        assert_eq!(r.tail(3).len(), 0);
    }

    #[test]
    fn log_line_timestamps_are_hms() {
        let l = LogLine::new(LogLevel::Warn, "x");
        assert_eq!(l.time.len(), 8, "expected HH:MM:SS, got {}", l.time);
        assert_eq!(l.time.matches(':').count(), 2);
    }

    #[test]
    fn a_zero_capacity_log_ring_still_evicts() {
        // `push` evicted only when `len == cap`, so a `cap` of 0 never evicted
        // and the "bounded" ring grew without limit.
        let mut r = LogRing::new(0);
        for i in 0..10 {
            r.push(LogLine::new(LogLevel::Info, format!("{i}")));
        }
        assert_eq!(r.len(), 1, "an empty-cap ring must still hold at most one line");
        assert_eq!(r.last().map(|l| l.text.as_str()), Some("9"));
    }

    #[test]
    fn a_log_ring_push_with_a_cap_of_one_keeps_the_newest() {
        let mut r = LogRing::new(1);
        r.push(LogLine::new(LogLevel::Info, "a"));
        r.push(LogLine::new(LogLevel::Info, "b"));
        assert_eq!(r.len(), 1);
        assert_eq!(r.last().map(|l| l.text.as_str()), Some("b"));
    }
}

//! Unit tests for `snapshot.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use std::time::{Duration, Instant};

fn config(id: u64) -> TunnelConfig {
    TunnelConfig {
        id,
        name: format!("t{id}"),
        kind: crate::model::ForwardKind::Local,
        ssh_host: "host".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 8080,
        remote_host: "127.0.0.1".into(),
        remote_port: 80,
        enabled: true,
        auto_restart: true,
        ssh_options: vec![],
        note: String::new(),
        created_at: 0,
    }
}

fn view(id: u64, status: TunnelStatus) -> TunnelView {
    let mut view = TunnelView::new(config(id));
    view.status = status;
    view
}

fn round_trip(snapshot: &Snapshot) -> Snapshot {
    let text = toml::to_string(snapshot).expect("a snapshot must encode");
    toml::from_str(&text).expect("a snapshot must decode")
}

#[test]
fn a_snapshot_round_trips_through_toml() {
    let views = vec![view(1, TunnelStatus::Up { since: Instant::now() })];
    let snap = Snapshot::from_views(7, 1_700_000_000_000, 2, false, &views);
    assert_eq!(round_trip(&snap), snap);
}

#[test]
fn every_status_variant_round_trips() {
    let statuses = [
        TunnelStatus::Stopped,
        TunnelStatus::Starting { since: Instant::now() },
        TunnelStatus::Up { since: Instant::now() },
        TunnelStatus::Retrying {
            attempt: 3,
            next_at: Instant::now() + Duration::from_secs(30),
            reason: FailureInfo::new(FailureKind::Timeout, "read timed out"),
        },
        TunnelStatus::Failed { reason: FailureInfo::new(FailureKind::PortInUse, "port busy") },
    ];
    for status in statuses {
        let views = vec![view(1, status)];
        let snap = Snapshot::from_views(1, 1_700_000_000_000, 1, false, &views);
        assert_eq!(round_trip(&snap), snap, "status did not survive the wire");
    }
}

#[test]
fn a_failure_travels_as_its_ascii_token_not_its_wording() {
    let reason = FailureInfo::new(FailureKind::DnsFailure, "could not resolve host");
    let views = vec![view(1, TunnelStatus::Failed { reason })];
    let snap = Snapshot::from_views(1, 0, 1, false, &views);
    match &snap.tunnels[0].status {
        WireStatus::Failed { reason } => {
            assert_eq!(reason.kind, "dns_failure", "the stable token must be the wire identity");
            assert!(!reason.detail.is_empty());
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn an_unknown_kind_token_degrades_to_unknown_instead_of_failing() {
    assert_eq!(failure_kind_from_token("a_kind_from_the_future"), FailureKind::Unknown);
}

#[test]
fn every_failure_kind_has_a_token_that_maps_back_to_itself() {
    for kind in ALL_FAILURE_KINDS {
        assert_eq!(failure_kind_from_token(kind.name()), *kind, "{} did not round-trip", kind.name());
    }
}

#[test]
fn the_log_tail_is_truncated_to_the_configured_length() {
    let mut view = view(1, TunnelStatus::Up { since: Instant::now() });
    // Push more lines than a snapshot may carry.
    for i in 0..(SNAPSHOT_LOG_TAIL + 40) {
        view.log.push(LogLine::new(LogLevel::Info, format!("line {i}")));
    }
    let snap = Snapshot::from_views(1, 0, 1, false, &[view]);
    assert_eq!(snap.tunnels[0].log_tail.len(), SNAPSHOT_LOG_TAIL);
    // Newest last, so the last entry is the most recent line.
    let last = snap.tunnels[0].log_tail.last().expect("a non-empty tail");
    assert!(last.text.contains(&format!("line {}", SNAPSHOT_LOG_TAIL + 39)), "{}", last.text);
}

#[test]
fn log_levels_survive_as_ascii_tokens() {
    for level in [LogLevel::Debug, LogLevel::Info, LogLevel::Warn, LogLevel::Error] {
        assert_eq!(level_from_token(level_token(level)), level);
    }
    // An unknown token must not lose the line.
    assert_eq!(level_from_token("trace"), LogLevel::Info);
}

#[test]
fn a_past_instant_becomes_a_stamp_at_or_before_now() {
    let now_ms = 1_700_000_000_000;
    let views = vec![view(1, TunnelStatus::Up { since: Instant::now() })];
    let snap = Snapshot::from_views(1, now_ms, 1, false, &views);
    match &snap.tunnels[0].status {
        WireStatus::Up { since_ms } => {
            // `Instant::now()` was just taken, so the stamp sits at "now".
            assert!(*since_ms <= now_ms, "a past instant must not be in the future: {since_ms} > {now_ms}");
            assert!(*since_ms > now_ms - 5_000, "and it must be close to now");
        }
        other => panic!("expected Up, got {other:?}"),
    }
}

#[test]
fn a_future_instant_becomes_a_stamp_after_now() {
    let now_ms = 1_700_000_000_000;
    let views = vec![view(
        1,
        TunnelStatus::Retrying {
            attempt: 1,
            next_at: Instant::now() + Duration::from_secs(30),
            reason: FailureInfo::new(FailureKind::Timeout, "timeout"),
        },
    )];
    let snap = Snapshot::from_views(1, now_ms, 1, false, &views);
    match &snap.tunnels[0].status {
        WireStatus::Retrying { next_at_ms, .. } => {
            assert!(*next_at_ms > now_ms, "a scheduled retry must be in the future");
            assert!(*next_at_ms < now_ms + 60_000, "but within a minute");
        }
        other => panic!("expected Retrying, got {other:?}"),
    }
}

#[test]
fn an_empty_snapshot_encodes_its_tunnel_list_explicitly() {
    // An absent field would be indistinguishable from "the server forgot to
    // send any tunnels"; an empty array says "there are none".
    let snap = Snapshot::from_views(0, 0, 0, false, &[]);
    let text = toml::to_string(&snap).expect("an empty snapshot must encode");
    assert!(text.contains("tunnels = []"), "expected an explicit empty array, got: {text}");
}

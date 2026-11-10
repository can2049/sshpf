//! Unit tests for `tunnel.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::logging::{Config, Level, Logger};
use crate::model::FailureInfo;
use std::path::Path;

fn logger(dir: &Path) -> Logger {
    Logger::open(&Config { dir: dir.to_path_buf(), level: Level::Debug, ..Config::default() }).unwrap()
}

fn text(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(crate::logging::LOG_FILE_NAME)).unwrap_or_default()
}

fn sample() -> TunnelConfig {
    TunnelConfig {
        id: 7,
        name: "cloud-pc".into(),
        kind: crate::model::ForwardKind::Local,
        ssh_host: "cloud_pc".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 19711,
        remote_host: "10.0.0.1".into(),
        remote_port: 22,
        enabled: true,
        auto_restart: true,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    }
}

#[test]
fn every_tunnel_record_starts_with_the_identity_fields() {
    let dir = tempfile::tempdir().unwrap();
    let log = logger(dir.path());
    let cfg = sample();
    tunnel_event_on(&log, Level::Info, "tunnel.ready", &cfg, &[("elapsed_ms", "812".to_string())]);

    let line = text(dir.path());
    // A reader must be able to tell which tunnel a line is about without
    // reading the surrounding lines, so the identity fields come first and in
    // a fixed order.
    let expected = "tunnel=7 name=cloud-pc forward=local listen=127.0.0.1:19711 target=10.0.0.1:22 host=cloud_pc";
    assert!(line.contains(expected), "identity fields missing or reordered:\n{line}");
    assert!(line.contains("elapsed_ms=812"), "caller fields follow the identity ones:\n{line}");
    assert!(line.contains(" INFO "), "{line}");
}

#[test]
fn forward_kind_names_are_ascii_and_stable() {
    // The log is read by tools and humans; the Chinese UI label must not leak
    // into it.
    assert_eq!(kind_name(crate::model::ForwardKind::Local), "local");
    assert_eq!(kind_name(crate::model::ForwardKind::Remote), "remote");
    assert_eq!(kind_name(crate::model::ForwardKind::Dynamic), "dynamic");
    for kind in crate::model::ForwardKind::all() {
        assert!(kind_name(kind).is_ascii(), "{kind:?} leaked a non-ASCII name");
    }
}

#[test]
fn a_ready_record_reports_how_long_connecting_took() {
    let dir = tempfile::tempdir().unwrap();
    let log = logger(dir.path());
    tunnel_ready_on(&log, &sample(), 1499);
    assert!(text(dir.path()).contains("event=tunnel.ready tunnel=7"), "{}", text(dir.path()));
    assert!(text(dir.path()).contains("elapsed_ms=1499"), "{}", text(dir.path()));
}

#[test]
fn a_drop_after_up_is_a_warning_while_a_failed_start_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let log = logger(dir.path());
    let cfg = sample();
    let reason = FailureInfo::new(crate::model::FailureKind::Timeout, "连接超时");

    tunnel_exit_on(&log, &cfg, true, Some(1), Some(&reason));
    let after_up = text(dir.path());
    assert!(after_up.contains(" WARN "), "a tunnel that worked and then dropped is a warning:\n{after_up}");
    assert!(after_up.contains("was_up=true"), "{after_up}");

    tunnel_exit_on(&log, &cfg, false, Some(1), Some(&reason));
    let failure = text(dir.path());
    let last = failure.lines().last().unwrap();
    assert!(last.contains(" ERROR "), "a session that never came up is an error: {last}");
    assert!(last.contains("was_up=false"), "{last}");
    assert!(last.contains("kind=timeout"), "the classified cause travels with the record: {last}");
    assert!(last.contains("short=连接超时"), "and the user-facing phrase too: {last}");
}

#[test]
fn terminal_failure_says_whether_a_retry_could_help() {
    let dir = tempfile::tempdir().unwrap();
    let log = logger(dir.path());
    // A config-class failure and a network-class one differ in exactly this
    // field, which is what tells a reader whether waiting would help.
    tunnel_failed_on(&log, &sample(), &FailureInfo::new(crate::model::FailureKind::AuthFailed, "认证失败"));
    tunnel_failed_on(&log, &sample(), &FailureInfo::new(crate::model::FailureKind::Timeout, "连接超时"));
    let text = text(dir.path());
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].contains("retryable=false"), "auth failures are not retryable: {}", lines[0]);
    assert!(lines[1].contains("retryable=true"), "timeouts are retryable: {}", lines[1]);
}

#[test]
fn a_rejection_keeps_the_stage_that_refused_it() {
    let dir = tempfile::tempdir().unwrap();
    let log = logger(dir.path());
    let reason = FailureInfo::with_short(
        crate::model::FailureKind::PortInUse,
        "本地端口被占用",
        "127.0.0.1:8080 已被占用 (pid 42)",
    );
    tunnel_start_rejected_on(&log, &sample(), "preflight_bind", &reason);
    let text = text(dir.path());
    assert!(text.contains("event=tunnel.rejected"), "{text}");
    assert!(text.contains("stage=preflight_bind"), "which check refused it: {text}");
    assert!(text.contains("short=本地端口被占用"), "{text}");
    assert!(text.contains("msg=\"127.0.0.1:8080 已被占用 (pid 42)\""), "the detail is quoted: {text}");
}

#[test]
fn a_failed_health_check_names_the_tunnel_the_cause_and_the_interval() {
    let dir = tempfile::tempdir().unwrap();
    let log = logger(dir.path());
    tunnel_health_failed_on(&log, &sample(), "ping_timeout", "探活超时", 30_000);
    let text = text(dir.path());
    assert!(text.contains(" WARN "), "a healed-by-retry drop is a warning: {text}");
    assert!(text.contains("event=tunnel.health_failed"), "{text}");
    assert!(text.contains("tunnel=7 name=cloud-pc"), "identity fields come first: {text}");
    assert!(text.contains("cause=ping_timeout"), "the ASCII verdict is greppable: {text}");
    assert!(text.contains("interval_ms=30000"), "how often it was checked: {text}");
}

#[test]
fn progress_lines_are_debug_only_so_they_do_not_drown_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = Logger::open(&Config { dir: dir.path().to_path_buf(), level: Level::Info, ..Config::default() }).unwrap();
    tunnel_line_on(&log, &sample(), "转发到 10.0.0.9:80 失败: ConnectFailed");
    assert!(text(dir.path()).is_empty(), "at info level a progress line must not be written");
}

#[test]
fn an_abandoned_retry_says_which_stage_gave_up() {
    // The row stops flapping; this is the line that explains why, and which
    // start stage refused it.
    let dir = tempfile::tempdir().unwrap();
    let log = logger(dir.path());
    let reason = FailureInfo::new(crate::model::FailureKind::PortInUse, "端口被占用");
    tunnel_retry_abandoned_on(&log, &sample(), 4, "preflight_bind", &reason);
    let text = text(dir.path());
    assert!(text.contains(" WARN "), "{text}");
    assert!(text.contains("event=tunnel.retry_abandoned"), "{text}");
    assert!(text.contains("attempt=4"), "{text}");
    assert!(text.contains("stage=preflight_bind"), "{text}");
    assert!(text.contains("kind=port_in_use"), "{text}");
    assert!(text.contains("tunnel=7 name=cloud-pc"), "identity fields still come first: {text}");
}

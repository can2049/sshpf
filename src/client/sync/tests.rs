//! Unit tests for `sync.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::app::{App, Mirror};
use crate::config::{ConfigFile, Paths};
use crate::ipc::{Snapshot, WireStatus, WireTunnel};
use crate::model::{ForwardKind, TunnelConfig, TunnelStatus};

/// A mirror `App` over a temp config, plus the outbox the tests can read.
fn mirror_app() -> (tempfile::TempDir, App, std::sync::mpsc::Receiver<ipc::ClientMsg>) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = App::new(paths);
    app.attach_mirror(Mirror::new(tx));
    (dir, app, rx)
}

fn config(id: u64, name: &str) -> TunnelConfig {
    TunnelConfig {
        id,
        name: name.into(),
        kind: ForwardKind::Local,
        ssh_host: "host".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 8080,
        remote_host: "127.0.0.1".into(),
        remote_port: 80,
        enabled: false,
        auto_restart: true,
        ssh_options: vec![],
        note: String::new(),
        created_at: 0,
    }
}

fn wire(id: u64, name: &str) -> WireTunnel {
    WireTunnel { cfg: config(id, name), status: WireStatus::Stopped, retries: 0, log_tail: vec![] }
}

fn snapshot(revision: u64, tunnels: Vec<WireTunnel>) -> Snapshot {
    Snapshot { revision, server_now_ms: 1_700_000_000_000, client_count: 2, keep_server: false, tunnels }
}

#[test]
fn a_snapshot_lands_in_the_tunnel_table() {
    let (_d, mut app, _rx) = mirror_app();
    apply(&mut app, ServerMsg::Snapshot(snapshot(1, vec![wire(1, "one"), wire(2, "two")])));
    assert_eq!(app.tunnels.len(), 2);
    assert_eq!(app.tunnels[0].cfg.name, "one");
    assert_eq!(app.tunnels[1].cfg.name, "two");
    assert_eq!(app.client_count, 2, "the count drives the last-client warning");
}

#[test]
fn a_welcome_is_applied_like_a_snapshot() {
    let (_d, mut app, _rx) = mirror_app();
    apply(
        &mut app,
        ServerMsg::Welcome {
            proto_version: ipc::PROTO_VERSION,
            config_path: "/tmp/config.toml".into(),
            server_pid: 7,
            snapshot: snapshot(3, vec![wire(9, "from-server")]),
        },
    );
    assert_eq!(app.tunnels.len(), 1);
    assert_eq!(app.tunnels[0].cfg.name, "from-server");
}

#[test]
fn an_older_snapshot_never_rolls_the_table_back() {
    let (_d, mut app, _rx) = mirror_app();
    apply(&mut app, ServerMsg::Snapshot(snapshot(5, vec![wire(1, "current")])));
    apply(&mut app, ServerMsg::Snapshot(snapshot(2, vec![wire(1, "old")])));
    assert_eq!(app.tunnels[0].cfg.name, "current", "an out-of-order frame must not render");
}

#[test]
fn a_stale_reply_changes_no_state() {
    let (_d, mut app, _rx) = mirror_app();
    apply(&mut app, ServerMsg::Snapshot(snapshot(4, vec![wire(1, "kept")])));
    apply(&mut app, ServerMsg::Stale { req_id: 2, rev: 9 });
    assert_eq!(app.tunnels.len(), 1);
    assert_eq!(app.tunnels[0].cfg.name, "kept", "a stale refusal must not touch the table");
    let status = app.status.as_ref().expect("a stale reply must say something");
    assert!(status.text.contains("旧状态"), "{}", status.text);
}

#[test]
fn an_error_sets_a_status_message_and_keeps_the_app_alive() {
    let (_d, mut app, _rx) = mirror_app();
    apply(&mut app, ServerMsg::Snapshot(snapshot(1, vec![wire(1, "kept")])));
    apply(
        &mut app,
        ServerMsg::Error {
            code: "bad_frame".into(),
            error: ipc::WireError {
                code: "bad_frame".into(),
                zh: "收到无法解析的消息".into(),
                en: "a frame could not be decoded".into(),
            },
        },
    );
    assert_eq!(app.tunnels.len(), 1, "an error must not clear the table");
    let status = app.status.as_ref().expect("an error must reach the status line");
    assert_eq!(status.level, crate::model::LogLevel::Error);
    assert!(status.text.contains("无法解析"), "{}", status.text);
    assert!(status.text.contains("could not be decoded"), "both languages: {}", status.text);
    assert!(!app.should_quit, "a protocol error must never end the session");
}

#[test]
fn a_failed_reply_explains_itself_on_the_status_line() {
    let (_d, mut app, _rx) = mirror_app();
    apply(
        &mut app,
        ServerMsg::Reply {
            req_id: 3,
            ok: false,
            rev: 1,
            error: Some(ipc::WireError {
                code: "not_found".into(),
                zh: "该转发已不存在".into(),
                en: "that forward no longer exists".into(),
            }),
            remaining: None,
            ports: vec![],
        },
    );
    let status = app.status.as_ref().expect("a refusal must be visible");
    assert!(status.text.contains("已不存在"), "{}", status.text);
    assert_eq!(status.level, crate::model::LogLevel::Error);
}

#[test]
fn a_reply_without_an_error_still_says_something() {
    // A failure with no explanation must not produce an empty status line.
    let (_d, mut app, _rx) = mirror_app();
    apply(&mut app, ServerMsg::Reply { req_id: 4, ok: false, rev: 1, error: None, remaining: None, ports: vec![] });
    let status = app.status.as_ref().expect("a refusal is still a refusal");
    assert!(!status.text.is_empty());
}

#[test]
fn a_bye_answer_updates_the_client_count() {
    let (_d, mut app, _rx) = mirror_app();
    apply(&mut app, ServerMsg::Reply { req_id: 1, ok: true, rev: 2, error: None, remaining: Some(1), ports: vec![] });
    assert_eq!(app.client_count, 1, "the server's recount is authoritative");
}

#[test]
fn a_pong_and_unknown_log_ids_are_harmless() {
    let (_d, mut app, _rx) = mirror_app();
    apply(&mut app, ServerMsg::Pong { server_now_ms: 5 });
    apply(&mut app, ServerMsg::Log { tunnel_id: Some(999), dropped: 0, lines: vec![] });
    apply(&mut app, ServerMsg::Log { tunnel_id: None, dropped: 0, lines: vec![] });
    assert!(app.tunnels.is_empty());
    assert!(app.status.is_none(), "routine traffic must not overwrite the status line");
}

#[test]
fn streamed_lines_append_to_their_tunnel_log() {
    let (_d, mut app, _rx) = mirror_app();
    apply(&mut app, ServerMsg::Snapshot(snapshot(1, vec![wire(1, "one")])));
    apply(
        &mut app,
        ServerMsg::Log {
            tunnel_id: Some(1),
            dropped: 0,
            lines: vec![ipc::WireLogLine { time: "12:00:00".into(), level: "warn".into(), text: "connected".into() }],
        },
    );
    let last = app.tunnels[0].log.last().expect("the line must be appended");
    assert_eq!(last.text, "connected");
    assert_eq!(last.level, crate::model::LogLevel::Warn);
}

#[test]
fn drain_applies_frames_in_order_and_stops_when_empty() {
    let (_d, mut app, _rx) = mirror_app();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(ServerMsg::Snapshot(snapshot(1, vec![wire(1, "first")]))).unwrap();
    tx.send(ServerMsg::Snapshot(snapshot(2, vec![wire(1, "second")]))).unwrap();
    drop(tx);
    drain(&mut app, &rx);
    assert_eq!(app.tunnels[0].cfg.name, "second", "the newest frame must win");
    // A drained channel is not an error.
    drain(&mut app, &rx);
}

#[test]
fn drain_is_bounded_so_a_flood_cannot_starve_the_frame() {
    let (_d, mut app, _rx) = mirror_app();
    let (tx, rx) = std::sync::mpsc::channel();
    for i in 0..(MAX_FRAMES_PER_FRAME + 20) {
        tx.send(ServerMsg::Snapshot(snapshot(i as u64 + 1, vec![wire(1, "flood")]))).unwrap();
    }
    drain(&mut app, &rx);
    // Whatever was left stays queued for the next frame instead of blocking this
    // one; the point is that `drain` returned at all.
    assert!(rx.try_recv().is_ok(), "the surplus must remain queued, not be consumed in one frame");
}

#[test]
fn a_snapshot_with_a_running_tunnel_reaches_the_renderer_as_running() {
    // The status conversion is what makes the table say "up"; a dropped field
    // here would show every remote forward as stopped.
    let (_d, mut app, _rx) = mirror_app();
    let mut tunnel = wire(1, "live");
    tunnel.status = WireStatus::Up { since_ms: 1_699_999_999_000 };
    apply(&mut app, ServerMsg::Snapshot(snapshot(1, vec![tunnel])));
    assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }), "{:?}", app.tunnels[0].status);
}

#[test]
fn a_snapshot_with_no_mirror_attached_is_logged_not_rendered() {
    // The caller forgot `attach_mirror`: state must not silently appear (the
    // app would then disagree with the server about who owns the tunnels).
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
    let mut app = App::new(paths);
    apply(&mut app, ServerMsg::Snapshot(snapshot(1, vec![wire(1, "ignored")])));
    assert!(app.tunnels.is_empty());
}

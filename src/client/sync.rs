//! The read side: turning `ServerMsg`s into `App` state, on the UI thread.
//!
//! # Why this is a free function and not a method on the connection
//!
//! The socket lives on a connection thread; [`App`] lives on the UI thread and is
//! `!Send`-hostile by design (it holds a `Supervisor`, an overlay stack and a hit
//! map). So the reader thread only ever pushes [`ServerMsg`]s into a channel, and
//! the UI thread calls [`drain`] once per frame to apply them. That split is what
//! makes [`apply`] a **pure-ish** function over `(&mut App, ServerMsg)` and lets it
//! be tested with no socket, no thread and no timing — the three things that make
//! socket tests flaky.
//!
//! # The rules `apply` enforces
//!
//! 1. **Revisions never go backwards.** A duplicated or out-of-order snapshot is
//!    dropped rather than rendered, because a rolled-back table shows forwards that
//!    are no longer running.
//! 2. **A local view survives.** `apply_snapshot` replaces the *shared* half only:
//!    the cursor, the open form and the status line belong to this screen, so
//!    another client's keypress cannot close a dialog someone is typing into.
//! 3. **Nothing here panics.** Every branch either applies, or explains itself on
//!    the status line. This runs on the UI thread, where a panic is the difference
//!    between "the server dropped" and "the terminal is gone".
//!
//! Belongs here: `apply`, `drain`, and the per-message handling they share.
//! Does not belong: the socket ([`super::link`]), the handshake
//! ([`super::connect`]), or deciding to reconnect ([`super::reconnect`]).

use std::sync::mpsc::Receiver;

use crate::app::App;
use crate::eventlog;
use crate::help;
use crate::ipc::{self, ServerMsg, WirePort};
use crate::model::{StatusMsg, TunnelId};

/// How many frames one call to [`drain`] applies before yielding the frame.
///
/// The same bound the tunnel event pump uses, for the same reason: a server that
/// broadcasts faster than the terminal redraws must not starve the render loop. What
/// is left waits for the next frame (~200 ms later).
pub const MAX_FRAMES_PER_FRAME: usize = 64;

/// Apply every queued server message, bounded, then return to the renderer.
///
/// Called once per UI frame, next to `App::drain_events`.
pub fn drain(app: &mut App, rx: &Receiver<ServerMsg>) {
    for _ in 0..MAX_FRAMES_PER_FRAME {
        let Ok(msg) = rx.try_recv() else {
            break;
        };
        apply(app, msg);
    }
}

/// Apply one server message to the app.
///
/// Public and socket-free on purpose: this is the function the unit tests drive,
/// and every branch below is reachable from a hand-built message.
pub fn apply(app: &mut App, msg: ServerMsg) {
    match msg {
        // The greeting carries the first snapshot, so a freshly attached client
        // renders real content instead of an empty table. The connection itself is
        // already logged by [`super::connect`], which is where the pid is known —
        // logging it again here would duplicate the record with a guessed pid.
        ServerMsg::Welcome { snapshot, .. } => apply_snapshot(app, &snapshot),
        ServerMsg::Snapshot(snapshot) => apply_snapshot(app, &snapshot),
        ServerMsg::Reply { req_id, ok, rev, error, remaining, ports } => {
            apply_reply(app, req_id, ok, rev, error, remaining, &ports)
        }
        ServerMsg::Stale { req_id, rev } => apply_stale(app, req_id, rev),
        ServerMsg::Error { code, error } => {
            // A protocol-level problem. The TUI must survive it: the connection
            // thread will reconnect, and until then the status line explains why
            // the table stopped moving.
            eventlog::client_disconnected(&code);
            app.set_status(StatusMsg::error(help::bi(&error.zh, &error.en)));
        }
        // The answer to a keep-alive. Its arrival is itself the information: the
        // server is alive. Nothing to render.
        ServerMsg::Pong { .. } => {}
        ServerMsg::Log { tunnel_id, dropped, lines } => apply_log(app, tunnel_id, dropped, lines),
    }
}

/// Install one snapshot, reporting a rollback attempt instead of rendering it.
fn apply_snapshot(app: &mut App, snapshot: &ipc::Snapshot) {
    if app.apply_snapshot(snapshot) {
        return;
    }
    // `apply_snapshot` returns false for two very different reasons, and they must
    // not be confused: "no mirror attached" is a programming error (the caller
    // forgot to attach), while "not newer" is normal and silent. Telling them apart
    // is what keeps a real bug observable without a status line per duplicate frame.
    if !app.is_mirror() {
        crate::logging::warn("client.snapshot_ignored", &[("reason", &"no_mirror"), ("rev", &snapshot.revision)]);
    }
}

/// Apply the answer to one command.
fn apply_reply(
    app: &mut App,
    req_id: u64,
    ok: bool,
    rev: u64,
    error: Option<ipc::WireError>,
    remaining: Option<u32>,
    ports: &[WirePort],
) {
    // One record for every answer, whichever way it went: `ok` is the field that
    // tells them apart, so "did my keypress reach the server?" stays one grep.
    crate::logging::debug(
        "client.command",
        &[
            ("req_id", &req_id),
            ("ok", &ok),
            ("rev", &rev),
            ("code", &error.as_ref().map(|e| e.code.clone()).unwrap_or_default()),
        ],
    );
    if !ok {
        let text = match &error {
            Some(e) => help::bi(&e.zh, &e.en),
            // A failure with no explanation is still a failure; saying so beats a
            // status line that silently keeps the old text.
            None => help::bi("服务端拒绝了这次操作。", "The server refused this action."),
        };
        app.set_status(StatusMsg::error(text));
        return;
    }
    // The snapshot is authoritative for the table; a reply only carries the facts
    // that belong to this request, so nothing else is inferred from `rev`.
    let _ = rev;
    if let Some(left) = remaining {
        app.client_count = left;
    }
    if !ports.is_empty() {
        apply_ports(app, ports);
    }
}

/// A command was refused because the client's view was older than the server's.
///
/// Not an error: the next snapshot repairs the view and the user retries. The one
/// thing this must not do is *change any state* — a stale refusal that also edited
/// the table would be a silent rollback with extra steps.
fn apply_stale(app: &mut App, req_id: u64, rev: u64) {
    crate::logging::debug("client.command_stale", &[("req_id", &req_id), ("rev", &rev)]);
    app.set_status(StatusMsg::warn(help::bi(
        "这条操作基于旧状态，已忽略；列表刷新后请重试。",
        "That action was based on an out-of-date view and was ignored; retry once the list refreshes.",
    )));
}

/// Store scan results from a `PortScan` reply, when a picker is actually open.
///
/// The wire carries only `port` / `proto` / `process`, so two facts the local scan
/// had are missing here, and both are degraded **honestly** rather than invented:
///
/// * **no bind addresses** — so a row cannot suggest a target host, and the caller
///   keeps whatever address the user typed (`suggest_target_host` returns `None` for
///   an empty list). Guessing `127.0.0.1` would produce a tunnel that looks right and
///   forwards nowhere.
/// * **no `excluded` count** — the "N hidden because your ssh config already lists
///   them" hint is zero. Under-reporting a convenience count is better than a number
///   the client made up.
fn apply_ports(app: &mut App, ports: &[WirePort]) {
    if app.ports.is_none() {
        // No picker is open: the answer belongs to a form the user has since closed.
        return;
    }
    let rows: Vec<crate::model::RemotePort> = ports.iter().map(|p| crate::model::RemotePort::new(p.port, "")).collect();
    // The picker owns the generation counter, so the install goes through the same
    // entry point a locally-run scan uses; a reply that arrives after the user
    // switched hosts is dropped there rather than here.
    let seq = app.port_seq;
    app.apply_port_scan(seq, Ok((rows, 0)));
}

/// Append streamed log lines to their tunnel's ring.
///
/// The snapshot already carries the tail of every log, so this is a *supplement*
/// for lines that arrived between snapshots. A line for a tunnel that is gone is
/// dropped silently: that is a normal race with a delete, not a failure.
fn apply_log(app: &mut App, tunnel_id: Option<TunnelId>, dropped: u32, lines: Vec<ipc::WireLogLine>) {
    if dropped > 0 {
        crate::logging::debug("client.log_dropped", &[("dropped", &dropped)]);
    }
    let Some(id) = tunnel_id else {
        return;
    };
    let Some(view) = app.tunnels.iter_mut().find(|t| t.cfg.id == id) else {
        return;
    };
    for line in lines {
        let level = ipc::level_from_token(&line.level);
        view.log.push(crate::model::LogLine { time: line.time, level, text: line.text });
    }
}

#[cfg(test)]
mod tests {
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
        apply(
            &mut app,
            ServerMsg::Reply { req_id: 1, ok: true, rev: 2, error: None, remaining: Some(1), ports: vec![] },
        );
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
                lines: vec![ipc::WireLogLine {
                    time: "12:00:00".into(),
                    level: "warn".into(),
                    text: "connected".into(),
                }],
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
}

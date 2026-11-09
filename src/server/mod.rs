//! The authoritative process: one per configuration, owning every tunnel and
//! serving the state to any number of attached clients.
//!
//! # Why this process exists
//!
//! Before it, every `sshpf` was its own owner: it restored `enabled` tunnels and
//! bound the same local ports, so a second window failed with "port already in
//! use" — naming the *first* `sshpf` as the occupier. The requirement is the
//! opposite: a new window shows the existing state and can operate it.
//!
//! ```text
//! win 1 ─┐
//! win 2 ─┼─► this process ─► the only tunnels, the only config file
//! win 3 ─┘
//! ```
//!
//! # The tick
//!
//! Once every [`TICK`]:
//!
//! ```text
//! 1. app.drain_events()      tunnel events, retry deadlines, log lines
//! 2. accept + handshake      new clients become sessions (counted)
//! 3. drain session events    commands, departures
//! 4. apply commands          revalidated against the current state
//! 5. publish if changed      one revision, broadcast to every client
//! 6. lifecycle               grace countdown, or shutdown
//! ```
//!
//! Steps 4 and 5 are deliberately separate: a batch of commands from two clients
//! arriving in one tick produces **one** revision, so clients never see a
//! half-applied picture.
//!
//! # Shutdown
//!
//! The order is fixed and load-bearing: stop the tunnels, save the state, unlink
//! the socket, release the lock, then `return`. Never `process::exit` — that skips
//! `Drop`, and the whole reason `Supervisor` has one is that tunnels must not
//! outlive the process.
//!
//! Belongs here: the loop and the wiring.
//! Does not belong: the election ([`election`]), the frames ([`session`],
//! [`broadcast`]), what a command means ([`commands`]), when to stop
//! ([`lifecycle`]).

pub mod broadcast;
pub mod commands;
pub mod election;
pub mod lifecycle;
pub mod session;

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::{self, Paths};
use crate::eventlog;
use crate::ipc::{self, ServerMsg};
use crate::logging;
use crate::model::StatusMsg;
use crate::ssh::known_hosts::HostKeyPolicy;

use broadcast::{publish, send_to};
use commands::Outcome;
use election::{Election, elect, unlink_socket};
use lifecycle::{Grace, Trigger};
use session::{Session, SessionEvent, accept_pending, served_config};

/// How often the loop wakes to drain events and check the grace window.
///
/// Matches the TUI's own tick so a client's view is never more than one UI frame
/// behind the authority.
const TICK: Duration = Duration::from_millis(200);

/// Run the server until the last client leaves (or a quit signal arrives).
pub fn run(paths: Paths, keep_server: bool, no_restore: bool, host_key_policy: HostKeyPolicy) -> ExitCode {
    let endpoint = ipc::endpoint_for(&paths.config);
    match elect(&endpoint) {
        // Normal: another process serves this config. Silent, because the client
        // that spawned us is about to connect to that one instead.
        Ok(Election::Lost) => ExitCode::from(3),
        Ok(Election::Won { lock, listener }) => {
            eventlog::server_start(
                &config::display_path(&paths.config),
                &endpoint.socket_path.display().to_string(),
                keep_server,
            );
            let code = serve(paths, keep_server, no_restore, host_key_policy, &endpoint, listener);
            // Unlink before releasing the lock: the next start must find a clean
            // endpoint, not one that needs recovery.
            unlink_socket(&endpoint);
            drop(lock);
            code
        }
        Err(e) => {
            logging::error("server.bind_failed", &[("msg", &e.to_string())]);
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!("无法启动服务端 / Cannot start the server: {e}\n").as_bytes(),
            );
            ExitCode::FAILURE
        }
    }
}

/// The main loop, separated from [`run`] so every exit path is a plain `return`
/// and the socket cleanup above happens exactly once.
fn serve(
    paths: Paths,
    keep_server: bool,
    no_restore: bool,
    host_key_policy: HostKeyPolicy,
    endpoint: &ipc::Endpoint,
    listener: std::os::unix::net::UnixListener,
) -> ExitCode {
    let quit = install_signals();
    let mut app = start_app(paths, no_restore, host_key_policy);
    let mut state = LoopState::new(app.state_signature());

    loop {
        // 1. Tunnel events first: a retry deadline or a tunnel going ready is a
        //    change clients must see in the same tick it happened.
        app.drain_events();

        // 2. New connections, then 3. whatever the sessions reported.
        accept_pending(&listener, &app, &state.next_id, &state.events_tx, &mut state.sessions);
        drain_sessions(
            &state.events_rx,
            &mut app,
            &mut state.sessions,
            &mut state.revision,
            keep_server,
            &mut state.grace,
            &mut state.dirty,
        );

        // 4. Publish once, if anything a client could see changed.
        state.publish_if_changed(&app, keep_server);

        // 5. Stop when nobody is watching.
        if let Some(trigger) = state.should_stop(keep_server, quit.load(Ordering::Relaxed)) {
            eventlog::server_shutdown_begin(client_count(&state.sessions), active_tunnels(&app), trigger);
            break;
        }
        std::thread::sleep(TICK);
    }

    shut_down(&mut app, &mut state.sessions, endpoint);
    ExitCode::SUCCESS
}

/// Build the authoritative app and recover the user's intent.
///
/// The server is the **only** process that restores: a client doing it too would
/// bounce every tunnel once per attached window.
fn start_app(paths: Paths, no_restore: bool, host_key_policy: HostKeyPolicy) -> App {
    let mut app = App::with_host_key_policy(paths, host_key_policy);
    app.load_hosts();
    if no_restore {
        logging::info("app.restore_skipped", &[("reason", &"no-restore")]);
    } else {
        app.restore_enabled();
    }
    app
}

/// How many tunnels are live, for the shutdown record.
fn active_tunnels(app: &App) -> usize {
    app.tunnels.iter().filter(|t| t.status.is_active()).count()
}

/// Everything the loop carries between ticks.
///
/// Grouped rather than passed as nine arguments: the tick reads as five named
/// steps instead of a wall of parameters, and a future field lands in one place.
struct LoopState {
    events_tx: Sender<SessionEvent>,
    events_rx: Receiver<SessionEvent>,
    sessions: Vec<Session>,
    next_id: AtomicU64,
    revision: u64,
    grace: Grace,
    last_signature: u64,
    last_count: u32,
    /// Whether any session event arrived this tick.
    dirty: bool,
}

impl LoopState {
    fn new(signature: u64) -> LoopState {
        let (events_tx, events_rx) = channel::<SessionEvent>();
        LoopState {
            events_tx,
            events_rx,
            sessions: Vec::new(),
            next_id: AtomicU64::new(1),
            revision: 0,
            grace: Grace::default(),
            last_signature: signature,
            last_count: 0,
            dirty: false,
        }
    }

    /// Broadcast one revision if a client could observe a difference.
    ///
    /// Both conditions matter: `dirty` covers commands and arrivals/departures
    /// (which do not always change the table), `signature` covers tunnel events
    /// (a retry deadline firing, a tunnel reaching `Up`).
    fn publish_if_changed(&mut self, app: &App, keep_server: bool) {
        let signature = app.state_signature();
        let count = client_count(&self.sessions);
        if !self.dirty && signature == self.last_signature && count == self.last_count {
            return;
        }
        let doomed = publish(app, &mut self.sessions, &mut self.revision, count, keep_server);
        self.last_signature = signature;
        self.last_count = count;
        self.dirty = false;
        drop_sessions(&mut self.sessions, &doomed);
    }

    /// Whether this tick is the last one, and why.
    fn should_stop(&self, keep_server: bool, quit: bool) -> Option<&'static str> {
        // A signal is an explicit "stop now": skip the grace window, the user
        // asked for it.
        if quit {
            return Some("signal");
        }
        // `--keep-server` is documented as the opposite of the default: it keeps
        // the tunnels up with nobody attached.
        if keep_server {
            return None;
        }
        let count = client_count(&self.sessions);
        if count == 0 && self.grace.expired() {
            return Some(self.grace.trigger().map(Trigger::name).unwrap_or("no_clients"));
        }
        None
    }
}

/// Handle every queued session event, applying commands as they arrive.
#[allow(clippy::too_many_arguments)]
fn drain_sessions(
    events: &Receiver<SessionEvent>,
    app: &mut App,
    sessions: &mut Vec<Session>,
    revision: &mut u64,
    keep_server: bool,
    grace: &mut Grace,
    dirty: &mut bool,
) {
    while let Ok(event) = events.try_recv() {
        match event {
            SessionEvent::Opened { id } => {
                // The count already includes this session (it was pushed before
                // the event), so `arrived` is what cancels a countdown.
                if grace.arrived() {
                    eventlog::server_grace_cancelled(client_count(sessions));
                }
                let welcome = welcome_frame(app, *revision, client_count(sessions), keep_server);
                let doomed = send_to(sessions, id, welcome);
                drop_sessions(sessions, &doomed);
                eventlog::server_client_connected(client_count(sessions));
                *dirty = true;
            }
            SessionEvent::Command { id, req_id, expect_rev, op } => {
                let outcome = commands::apply(app, &op, expect_rev, *revision);
                let reply = reply_frame(req_id, outcome, *revision, client_count(sessions));
                let doomed = send_to(sessions, id, reply);
                drop_sessions(sessions, &doomed);
                *dirty = true;
            }
            SessionEvent::Closed { id, via_bye } => {
                if let Some(pos) = sessions.iter().position(|s| s.id == id) {
                    let mut session = sessions.remove(pos);
                    session.said_bye = via_bye;
                    session.shut();
                }
                let remaining = sessions.len();
                let trigger = if via_bye { Trigger::Confirmed } else { Trigger::Unexpected };
                if grace.departed(remaining, trigger) {
                    eventlog::server_grace_started(trigger.grace().as_millis() as u64, trigger.name());
                }
                eventlog::server_client_disconnected(remaining as u32, if via_bye { "bye" } else { "eof" });
                *dirty = true;
            }
        }
    }
}

/// The first frame a client receives: the protocol handshake plus the state.
fn welcome_frame(app: &App, revision: u64, count: u32, keep_server: bool) -> ServerMsg {
    ServerMsg::Welcome {
        proto_version: ipc::PROTO_VERSION,
        config_path: served_config(app),
        server_pid: std::process::id(),
        snapshot: ipc::Snapshot::from_views(revision, session::now_ms(), count, keep_server, &app.tunnels),
    }
}

/// Turn a command's verdict into its answer.
/// `remaining` is how many clients are still attached. It travels on every reply
/// (it is cheap) so a client can refresh its "am I the last one?" belief from the
/// answer to its own command rather than waiting for the next snapshot.
fn reply_frame(req_id: u64, outcome: Outcome, revision: u64, remaining: u32) -> ServerMsg {
    if outcome == Outcome::Stale {
        return ServerMsg::Stale { req_id, rev: revision };
    }
    let (ok, error) = outcome.into_reply();
    ServerMsg::Reply { req_id, ok, rev: revision, error, remaining: Some(remaining), ports: vec![] }
}

/// How many clients are attached right now.
///
/// A session exists only after its handshake, so its length *is* the count — and
/// a session whose writer died is dropped in the same tick, so a wedged window
/// cannot keep the server alive forever.
fn client_count(sessions: &[Session]) -> u32 {
    u32::try_from(sessions.len()).unwrap_or(u32::MAX)
}

/// Remove sessions the broadcaster could not reach.
fn drop_sessions(sessions: &mut Vec<Session>, doomed: &[u64]) {
    for id in doomed {
        if let Some(pos) = sessions.iter().position(|s| s.id == *id) {
            logging::warn("server.client_dropped", &[("session", id), ("reason", &"queue_full")]);
            let mut session = sessions.remove(pos);
            session.shut();
        }
    }
}

/// Stop everything in the order that leaves no broken state behind.
fn shut_down(app: &mut App, sessions: &mut Vec<Session>, endpoint: &ipc::Endpoint) {
    // Closing the outgoing frames first means each client sees a clean EOF and
    // can say "the server went away" instead of timing out.
    for session in sessions.iter_mut() {
        let _ = session.try_send(ServerMsg::Error {
            code: "server_shutdown".to_string(),
            error: ipc::WireError {
                code: "server_shutdown".to_string(),
                zh: "服务端正在退出".to_string(),
                en: "The server is shutting down".to_string(),
            },
        });
        session.shut();
    }
    sessions.clear();

    // Tunnels before files: `App::shutdown` stops the supervisor and writes
    // `state.toml`, which should describe the state *after* stopping.
    app.shutdown();
    app.set_status(StatusMsg::warn("服务端已停止"));
    eventlog::server_client_disconnected(0, "shutdown");
    // `unlink` is the caller's job (`run`), right before the lock is released.
    logging::info("server.stopped", &[("socket", &endpoint.socket_path.display().to_string())]);
}

/// Install the quit flags, ignoring `SIGHUP`.
///
/// A detached server must not die with the terminal that spawned it: otherwise
/// closing the *first* window would stop every forward in every other window,
/// which is exactly the power this design refuses to give to a terminal.
fn install_signals() -> Arc<AtomicBool> {
    let quit = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        if signal_hook::flag::register(sig, quit.clone()).is_err() {
            logging::warn("server.signal_unavailable", &[("signal", &crate::sys::signal_name(sig))]);
        }
    }
    // A throwaway flag installs a no-op handler, so SIGHUP's default "terminate"
    // action never runs.
    let ignored = Arc::new(AtomicBool::new(false));
    if signal_hook::flag::register(signal_hook::consts::SIGHUP, ignored).is_err() {
        logging::warn("server.signal_unavailable", &[("signal", &"SIGHUP")]);
    }
    quit
}

/// Wait until a server for `paths` is listening, for tests and for the client's
/// own spawn path; returns whether one answered inside `budget`.
pub fn wait_until_listening(paths: &Paths, budget: Duration) -> bool {
    let endpoint = ipc::endpoint_for(&paths.config);
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(&endpoint.socket_path).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The channel type the server publishes on, named so `run`'s signature reads.
pub type EventSender = Sender<SessionEvent>;
/// The receiving end of the same channel.
pub type EventReceiver = Receiver<SessionEvent>;

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: u64, cap: usize) -> (Session, std::sync::mpsc::Receiver<ServerMsg>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(cap);
        (Session::with_queue(id, tx), rx)
    }

    #[test]
    fn the_count_is_the_number_of_handshaked_sessions() {
        assert_eq!(client_count(&[]), 0);
        let mut sessions = vec![session(1, 2).0, session(2, 2).0];
        assert_eq!(client_count(&sessions), 2);
        sessions.clear();
        assert_eq!(client_count(&sessions), 0);
    }

    #[test]
    fn shutdown_waits_for_the_grace_window_before_exiting() {
        let (session, _rx) = session(1, 2);
        let mut state = LoopState::new(0);
        state.sessions.push(session);
        assert!(state.should_stop(false, false).is_none(), "a client is attached");

        state.sessions.clear();
        assert!(state.should_stop(false, false).is_none(), "zero clients but no countdown started yet");
        assert!(state.grace.departed(0, Trigger::Confirmed));
        assert!(state.should_stop(false, false).is_none(), "the countdown has not expired yet");
    }

    #[test]
    fn a_signal_skips_the_grace_window() {
        let (session, _rx) = session(1, 2);
        let mut state = LoopState::new(0);
        state.sessions.push(session);
        assert_eq!(
            state.should_stop(false, true),
            Some("signal"),
            "an explicit stop signal must not wait for clients to leave"
        );
    }

    #[test]
    fn keep_server_never_stops_on_its_own() {
        let mut state = LoopState::new(0);
        assert!(state.grace.departed(0, Trigger::Unexpected));
        assert!(
            state.should_stop(true, false).is_none(),
            "--keep-server is exactly the choice to outlive the last client"
        );
    }

    #[test]
    fn a_stale_reply_becomes_a_stale_message_not_a_failure() {
        match reply_frame(4, Outcome::Stale, 12, 1) {
            ServerMsg::Stale { req_id, rev } => {
                assert_eq!(req_id, 4);
                assert_eq!(rev, 12, "the client needs the current revision to refresh against");
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn a_refusal_becomes_a_failed_reply_carrying_its_reason() {
        let outcome = commands::apply(
            &mut {
                let dir = tempfile::tempdir().unwrap();
                App::new(crate::config::Paths {
                    config: dir.path().join("config.toml"),
                    state: dir.path().join("state.toml"),
                })
            },
            &crate::ipc::Op::Delete { tunnel_id: 99 },
            Some(0),
            0,
        );
        match reply_frame(1, outcome, 0, 1) {
            ServerMsg::Reply { ok, error, .. } => {
                assert!(!ok);
                assert_eq!(error.map(|e| e.code), Some("not_found".to_string()));
            }
            other => panic!("expected a Reply, got {other:?}"),
        }
    }

    #[test]
    fn the_welcome_frame_carries_the_protocol_version_and_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(crate::config::Paths {
            config: dir.path().join("config.toml"),
            state: dir.path().join("state.toml"),
        });
        match welcome_frame(&app, 3, 2, false) {
            ServerMsg::Welcome { proto_version, snapshot, server_pid, .. } => {
                assert_eq!(proto_version, ipc::PROTO_VERSION);
                assert_eq!(snapshot.revision, 3);
                assert_eq!(snapshot.client_count, 2);
                assert_eq!(server_pid, std::process::id());
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    #[test]
    fn a_departing_client_leaves_the_others_alone() {
        let (events_tx, events_rx) = channel();
        let mut sessions = vec![session(1, 2).0, session(2, 2).0];
        let mut app = {
            let dir = tempfile::tempdir().unwrap();
            App::new(crate::config::Paths {
                config: dir.path().join("config.toml"),
                state: dir.path().join("state.toml"),
            })
        };
        let mut rev = 0;
        let mut grace = Grace::default();
        let mut dirty = false;
        events_tx.send(SessionEvent::Closed { id: 1, via_bye: true }).unwrap();
        drain_sessions(&events_rx, &mut app, &mut sessions, &mut rev, false, &mut grace, &mut dirty);
        assert_eq!(sessions.len(), 1, "only the departed client is removed");
        assert!(!grace.waiting(), "one client is still attached, so nothing may count down");
    }
}

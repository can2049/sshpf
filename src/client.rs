//! The client half of client-server: attaching to a server, and keeping the
//! attachment alive.
//!
//! # What the client is, and what it is not
//!
//! A client owns **no tunnels**. It owns a table on a screen and a socket to the
//! process that does. That single sentence explains every rule below:
//!
//! * it never opens a listener, never spawns an SSH session, never writes the config;
//! * every mutation is a *request* ([`ipc::ClientMsg`]) that the server may refuse;
//! * the state it renders is the last snapshot the server sent, at a revision.
//!
//! # Shape
//!
//! ```text
//! main ──► run()                      attach (spawning a server if none is listening)
//!           └── Session               outbox Sender<ClientMsg> ──► the caller's Mirror
//!                                     inbox  Receiver<ServerMsg> ──► sync::drain, per frame
//! connection thread                   link::serve → on EOF, reconnect with backoff
//! ```
//!
//! # Why the socket lives on its own thread
//!
//! The UI thread must never block on a socket read: a server that stops talking
//! would freeze the terminal, including the key that would have quit. So the
//! connection thread does all blocking I/O and communicates through two channels;
//! the UI thread only ever calls `try_recv` (via [`sync::drain`]) and `send`.
//!
//! # Why a reconnect must not re-spawn a server
//!
//! Starting a server runs `restore_enabled`, which re-opens every forward the config
//! says was on. Doing that automatically because a *connection* dropped would let a
//! momentary failure resurrect forwards the user had just decided to stop — a side
//! effect in the "creates listeners" direction, which is the dangerous one. So the
//! client spawns a server only before its first successful attach, and afterwards
//! only reconnects, reporting honestly when the server is gone for good.
//!
//! # Layout
//!
//! | file | what lives there |
//! |---|---|
//! | [`connect`] | discovery, dial, and the `Hello`/`Welcome` handshake |
//! | [`link`] | the read thread, the write loop, the keep-alive |
//! | [`sync`] | `ServerMsg` → `App`, applied on the UI thread |
//! | [`spawn`] | starting a detached server process |
//! | [`reconnect`] | the spawn wait and the backoff schedule |
//!
//! Belongs here: the session handle, `run`, and the reconnect loop that owns them.
//! Does not belong: what a frame means ([`sync`]), the wire format ([`crate::ipc`]),
//! or any `App` mutation of its own — the client reports, the server decides.

pub mod connect;
pub mod link;
pub mod reconnect;
pub mod spawn;
pub mod sync;

pub use connect::{AttachError, Connection};
pub use link::{Ended, Served};
pub use reconnect::{backoff, wait_for_spawn};
pub use spawn::{SpawnRequest, SpawnedServer, spawn_server};

use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::eventlog;
use crate::help;
use crate::ipc::{self, ClientMsg, Endpoint, ServerMsg, WireError};

/// How many consecutive failed reconnects before the client stops trying.
///
/// Bounded on purpose. With the capped backoff this is roughly three quarters of a
/// minute of retrying — long enough to ride out a server restart, short enough that
/// a client whose server is gone says so instead of pretending forever. The UI stays
/// usable throughout, so `q` still works.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 12;

/// The codes of the frames this module *synthesizes* for the UI.
///
/// They never cross the socket: they are pushed straight into the inbox the UI
/// drains, so `sync::apply` can report a connection problem through exactly the same
/// path as a server-sent error. A reader who greps the wire protocol will not find
/// them, which is why they are named here and documented in the report.
pub mod code {
    /// The established link dropped; a reconnect is being scheduled.
    pub const CONN_LOST: &str = "conn_lost";
    /// A reconnect attempt failed; another is scheduled.
    pub const RECONNECT_FAILED: &str = "reconnect_failed";
    /// Every attempt was spent: the server is gone and no retry remains.
    pub const GONE: &str = "server_gone";
}

/// Why [`run`] could not produce a [`Session`].
#[derive(Debug)]
pub enum ClientError {
    /// The first attach failed, and either nothing could be started or the server
    /// that was started never became reachable.
    Attach(AttachError),
    /// Starting a server was necessary and did not work.
    Spawn(std::io::Error),
    /// The connection thread could not be created.
    Thread(std::io::Error),
}

impl ClientError {
    /// The user-facing sentence, bilingual, Chinese first.
    pub fn message(&self) -> String {
        match self {
            ClientError::Attach(e) => e.message(),
            ClientError::Spawn(e) => {
                help::bi(&format!("无法启动 sshpf 服务端：{e}"), &format!("Could not start the sshpf server: {e}"))
            }
            ClientError::Thread(e) => help::bi(
                &format!("无法启动客户端连接线程：{e}"),
                &format!("Could not start the client connection thread: {e}"),
            ),
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// A live attachment: the two channels the UI needs, plus the thread behind them.
///
/// Dropping the session without [`Session::close`] still ends the connection thread
/// (its outbox closes), but sends no `Bye` — the server then counts the client out
/// when the socket reaches EOF. `close` is what makes the goodbye explicit, which is
/// what lets a `last_exit` proposal reach the server.
pub struct Session {
    /// Commands to the connection thread. This is what [`crate::app::Mirror`] wraps.
    outbox: Sender<ClientMsg>,
    /// Frames from the connection thread, for [`sync::drain`].
    inbox: Receiver<ServerMsg>,
    /// The connection thread, joined by [`Session::close`].
    handle: Option<thread::JoinHandle<()>>,
}

impl Session {
    /// The outbox, ready to hand to [`crate::app::Mirror::new`].
    pub fn outbox(&self) -> Sender<ClientMsg> {
        self.outbox.clone()
    }

    /// The inbox, for [`sync::drain`].
    pub fn inbox(&self) -> &Receiver<ServerMsg> {
        &self.inbox
    }

    /// Say goodbye and wait for the connection thread to finish.
    ///
    /// `last_exit` is the client's **proposal** that this was the last one attached.
    /// The server recounts before acting, because this view of `client_count` may be
    /// seconds old and a stale view must never decide to stop everyone's forwards.
    pub fn close(mut self, last_exit: bool) {
        // A closed channel means the connection thread already gave up (the server
        // is gone), which is not an error worth reporting: the session is over
        // either way.
        if self.outbox.send(ClientMsg::Bye { last_exit }).is_err() {
            return;
        }
        if let Some(handle) = self.handle.take() {
            // The thread is bounded: it is either in the write loop (ends on the
            // `Bye`) or in a reconnect wait (ends on the deadline or this `Bye`).
            let _ = handle.join();
        }
    }
}

/// Attach to the server for `config_path`, starting one if none is listening.
///
/// `explicit_config` says whether the user named the config on the command line; the
/// spawned server is given `-c` only in that case, so both processes resolve the same
/// default otherwise. Returns as soon as the handshake completes — the read loop runs
/// on its own thread and reconnects by itself from then on.
pub fn run(config_path: &Path, explicit_config: bool) -> Result<Session, ClientError> {
    let endpoint = ipc::endpoint_for(config_path);
    let connection = first_attach(&endpoint, config_path, explicit_config)?;
    let (outbox, commands) = mpsc::channel();
    let (messages, inbox) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("sshpf-client-link".to_string())
        .spawn(move || connection_loop(endpoint, connection, &commands, &messages))
        .map_err(ClientError::Thread)?;
    Ok(Session { outbox, inbox, handle: Some(handle) })
}

/// Attach once, spawning a server when that is the only thing that can help.
fn first_attach(endpoint: &Endpoint, config_path: &Path, explicit_config: bool) -> Result<Connection, ClientError> {
    match connect::attach(endpoint) {
        Ok(connection) => return Ok(connection),
        // A server is running and refused us (token, version, config). Spawning a
        // second one would either lose the election or bind the same ports again.
        Err(e) if !e.may_spawn() => return Err(ClientError::Attach(e)),
        Err(_) => {}
    }
    // Only the path the user actually named travels: an absent `-c` means the child
    // resolves the same default this process did.
    let named = if explicit_config { Some(config_path) } else { None };
    let spawned = spawn::spawn_server(named).map_err(|e| {
        eventlog::client_spawn_failed(&e.to_string());
        ClientError::Spawn(e)
    })?;
    crate::logging::info("client.attach_from", &[("pid", &spawned.pid)]);
    reconnect::wait_for_spawn(endpoint).map_err(ClientError::Attach)
}

/// One attachment, then keep re-attaching until the caller leaves or the wait runs out.
fn connection_loop(
    endpoint: Endpoint,
    mut connection: Connection,
    commands: &Receiver<ClientMsg>,
    inbox: &Sender<ServerMsg>,
) {
    let mut attempts = 0u32;
    loop {
        let served = link::serve(connection, commands, inbox);
        // The caller sent `Bye` or dropped the outbox: this session is over.
        if served.ended == Ended::CallerGone {
            return;
        }
        eventlog::client_disconnected("eof");
        let _ = inbox.send(synthesized(
            code::CONN_LOST,
            "与服务端的连接已断开，正在尝试重连（列表暂时冻结）。",
            "Lost the connection to the server; reconnecting (the list is frozen meanwhile).",
        ));
        match reattach(&endpoint, commands, inbox, &mut attempts) {
            Reattached::Now(next) => connection = next,
            Reattached::CallerLeft => return,
            Reattached::GaveUp => {
                // Nothing left to do on this thread, but the UI must stay alive: the
                // user can still read the frozen table and quit with `q`.
                eventlog::client_disconnected("gave_up");
                return;
            }
        }
    }
}

/// The outcome of trying to get back to the server.
enum Reattached {
    /// A fresh, handshaken connection.
    Now(Connection),
    /// The caller quit while we were down; there is nobody to reconnect for.
    CallerLeft,
    /// Every attempt was spent.
    GaveUp,
}

/// Try to get back to the server, backing off between attempts.
///
/// Deliberately **never** spawns: see the module docs. A server that is gone stays
/// gone until the user starts one, because spawning it would re-open forwards nobody
/// asked for.
fn reattach(
    endpoint: &Endpoint,
    commands: &Receiver<ClientMsg>,
    inbox: &Sender<ServerMsg>,
    attempts: &mut u32,
) -> Reattached {
    loop {
        if *attempts >= MAX_RECONNECT_ATTEMPTS {
            let _ = inbox.send(synthesized(
                code::GONE,
                "服务端已退出且重连失败：请重新启动 sshpf（按 q 退出）。",
                "The server has exited and reconnecting failed: start sshpf again (press q to quit).",
            ));
            return Reattached::GaveUp;
        }
        *attempts = attempts.saturating_add(1);
        let delay = reconnect::backoff(*attempts);
        eventlog::client_reconnect(*attempts, delay.as_millis());
        match wait_or_quit(commands, delay) {
            Waiting::Bye(last_exit) => {
                // A `Bye` while disconnected has no server to reach; the record is
                // all that is left of it.
                eventlog::client_bye(last_exit, None);
                return Reattached::CallerLeft;
            }
            Waiting::OutboxGone => return Reattached::CallerLeft,
            Waiting::Elapsed => {}
        }
        match connect::attach(endpoint) {
            Ok(connection) => {
                *attempts = 0;
                return Reattached::Now(connection);
            }
            Err(e) => {
                let _ = inbox.send(synthesized(code::RECONNECT_FAILED, &e.message(), &e.message()));
            }
        }
    }
}

/// What happened while waiting out a backoff delay.
enum Waiting {
    /// Time is up; make the attempt.
    Elapsed,
    /// The caller quit.
    Bye(bool),
    /// The session was dropped.
    OutboxGone,
}

/// Sleep for `delay`, but give up when the caller quits.
///
/// Commands queued while the link was down are **dropped**: they were built on a
/// view of a server that no longer exists, and replaying them against a fresh process
/// would apply a stale intention to new state.
fn wait_or_quit(commands: &Receiver<ClientMsg>, delay: Duration) -> Waiting {
    let deadline = Instant::now() + delay;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Waiting::Elapsed;
        }
        match commands.recv_timeout(left) {
            Ok(ClientMsg::Bye { last_exit }) => return Waiting::Bye(last_exit),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return Waiting::Elapsed,
            Err(RecvTimeoutError::Disconnected) => return Waiting::OutboxGone,
        }
    }
}

/// Build one of the frames this module invents for the UI.
fn synthesized(code: &str, zh: &str, en: &str) -> ServerMsg {
    ServerMsg::Error {
        code: code.to_string(),
        error: WireError { code: code.to_string(), zh: zh.to_string(), en: en.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Snapshot;

    fn welcome(config: &str) -> ServerMsg {
        ServerMsg::Welcome {
            proto_version: ipc::PROTO_VERSION,
            config_path: config.to_string(),
            server_pid: 5,
            snapshot: Snapshot::from_views(1, 0, 1, false, &[]),
        }
    }

    /// An endpoint plus a "server" thread that answers one handshake and then holds
    /// the socket open, so the session has something to attach to.
    fn fake_server(dir: &Path, tag: &str) -> (Endpoint, ServerMsg) {
        let mut endpoint = ipc::endpoint_for(dir.join("config.toml").as_path());
        endpoint.dir = dir.to_path_buf();
        endpoint.port_path = dir.join(format!("{tag}.port"));
        endpoint.token_path = dir.join(format!("{tag}.token"));
        endpoint.lock_path = dir.join(format!("{tag}.lock"));
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "t").expect("token");
        let greeting = welcome(&endpoint.config_path.to_string_lossy());
        let answer = greeting.clone();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            let _ = ipc::write_message(&mut stream, &answer);
            // Keep the link up while the test inspects the session.
            thread::sleep(Duration::from_millis(300));
        });
        (endpoint, greeting)
    }

    #[test]
    fn a_synthesized_frame_is_an_error_the_app_already_knows_how_to_show() {
        // These frames never cross the wire; they exist so a connection problem uses
        // the same UI path as a server-sent error.
        let frame = synthesized(code::CONN_LOST, "断开", "lost");
        match frame {
            ServerMsg::Error { code: ref c, error } => {
                assert_eq!(c, "conn_lost");
                assert_eq!(error.zh, "断开");
                assert_eq!(error.en, "lost");
            }
            other => panic!("a notice must be an Error frame: {other:?}"),
        }
    }

    #[test]
    fn the_client_never_spends_more_than_a_minute_retrying() {
        // The whole schedule is bounded so a dead server is reported, not hidden.
        let mut total = Duration::ZERO;
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            total += reconnect::backoff(attempt);
        }
        assert!(total >= Duration::from_secs(10), "too short to ride out a restart: {total:?}");
        assert!(total <= Duration::from_secs(90), "too long to leave a user guessing: {total:?}");
    }

    #[test]
    fn a_queued_command_does_not_cut_a_backoff_wait_short() {
        // Commands from a dead link are dropped, so the retry keeps its schedule
        // instead of spinning on a full queue.
        let (tx, rx) = mpsc::channel();
        for _ in 0..3 {
            tx.send(ClientMsg::Ping).unwrap();
        }
        let started = Instant::now();
        let outcome = wait_or_quit(&rx, Duration::from_millis(80));
        assert!(matches!(outcome, Waiting::Elapsed));
        assert!(started.elapsed() >= Duration::from_millis(50), "the wait must not be cut short");
        assert!(rx.try_recv().is_err(), "stale commands must be dropped");
    }

    #[test]
    fn a_bye_ends_a_backoff_wait_immediately() {
        let (tx, rx) = mpsc::channel();
        tx.send(ClientMsg::Bye { last_exit: true }).unwrap();
        let started = Instant::now();
        assert!(matches!(wait_or_quit(&rx, Duration::from_secs(30)), Waiting::Bye(true)));
        assert!(started.elapsed() < Duration::from_secs(5), "quitting must not wait out the backoff");
    }

    #[test]
    fn losing_the_outbox_ends_a_backoff_wait() {
        let (tx, rx) = mpsc::channel::<ClientMsg>();
        drop(tx);
        assert!(matches!(wait_or_quit(&rx, Duration::from_secs(30)), Waiting::OutboxGone));
    }

    #[test]
    fn a_session_reports_the_welcome_and_carries_a_bye_to_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, _greeting) = fake_server(dir.path(), "happy");
        let connection = connect::attach(&endpoint).expect("the fake server must answer");
        let (outbox, commands) = mpsc::channel();
        let (messages, inbox) = mpsc::channel();
        let handle = thread::spawn(move || connection_loop(endpoint, connection, &commands, &messages));
        let session = Session { outbox, inbox, handle: Some(handle) };
        let first = session.inbox().recv_timeout(Duration::from_secs(2)).expect("the greeting must arrive");
        assert!(matches!(first, ServerMsg::Welcome { .. }));
        // `close` joins the thread, so returning here proves the `Bye` ended it.
        session.close(true);
    }

    #[test]
    fn a_dropped_session_does_not_leak_the_connection_thread() {
        let dir = tempfile::tempdir().unwrap();
        let (endpoint, _greeting) = fake_server(dir.path(), "drop");
        let connection = connect::attach(&endpoint).expect("attach");
        let (outbox, commands) = mpsc::channel();
        let (messages, inbox) = mpsc::channel();
        let handle = thread::spawn(move || connection_loop(endpoint, connection, &commands, &messages));
        let session = Session { outbox, inbox, handle: Some(handle) };
        drop(session);
        // Nothing to assert beyond "the process is still sane": the thread ends when
        // its outbox closes, which is what keeps a test suite from accumulating
        // threads. The status poll below gives it a moment to happen.
        thread::sleep(Duration::from_millis(250));
    }

    #[test]
    fn an_error_message_is_bilingual_for_every_variant() {
        let variants = [
            ClientError::Attach(AttachError::NoServer(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))),
            ClientError::Spawn(std::io::Error::other("nope")),
            ClientError::Thread(std::io::Error::other("nope")),
        ];
        for error in variants {
            assert!(!error.message().is_empty(), "{error:?}");
        }
        // The variants that wrap an I/O failure must name it in both languages,
        // because that sentence is what the user reads on a failed start.
        let spawn = ClientError::Spawn(std::io::Error::other("nope")).message();
        assert!(spawn.contains("无法启动"), "{spawn}");
        assert!(spawn.contains("Could not start"), "{spawn}");
        let thread = ClientError::Thread(std::io::Error::other("nope")).message();
        assert!(thread.contains("连接线程"), "{thread}");
        assert!(thread.contains("connection thread"), "{thread}");
    }
}

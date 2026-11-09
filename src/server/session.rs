//! Where a listening socket becomes a client: accept, handshake, and the two
//! threads that carry frames in each direction.
//!
//! # The shape, and why it is three pieces
//!
//! ```text
//! accept ─► handshake (on the server loop's own thread, bounded by a timeout)
//!            │
//!            ├─► reader thread: frames ──► mpsc ──► server main loop
//!            └─► writer thread: mpsc ──► frames
//! ```
//!
//! The handshake stays synchronous with the accept loop on purpose: it is bounded
//! (a read timeout plus a version check), and doing it inline means a half-open
//! connection that never says `Hello` cannot be counted as a client or leak two
//! threads. `client_count` is what decides whether the server may shut down, so
//! counting must happen only after a connection has identified itself.
//!
//! # Why the welcome frame is not sent here
//!
//! [`ServerMsg::Welcome`] carries a snapshot, and a snapshot carries the
//! `revision` and `client_count`. Neither is known until the main loop has added
//! this session to the count — so the handshake returns an empty queue and the
//! loop pushes `Welcome` as the first frame of the broadcast. That keeps one
//! place (`broadcast`) responsible for every frame a client receives.
//!
//! # Why each session gets a bounded send queue
//!
//! The broadcast loop must never block on one client's socket: a window that
//! stopped reading (suspended laptop, wedged terminal) would otherwise freeze
//! every other client's view. The writer thread pulls from a small
//! [`mpsc::SyncSender`], so "this client cannot keep up" surfaces at the *enqueue*
//! (`try_send` → `Full`) instead of inside a `write` that has nowhere to put the
//! bytes. A full queue is the signal to drop that session, which is honest: the
//! alternative is buffering without limit.
//!
//! Belongs here: accept + handshake, session identity, and the reader/writer
//! threads for one connection.
//! Does not belong: deciding what a command means ([`super::commands`]), what to
//! broadcast ([`super::broadcast`]), or when to shut down ([`super::lifecycle`]).

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::app::App;
use crate::ipc::{self, ClientMsg, ProtocolError, ServerMsg, check_config, check_version};

/// How long a connecting peer has to send its `Hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// How many snapshot frames may queue for one client before it is dropped.
///
/// Two is enough for "one being written, one superseding it": snapshots are full
/// replacements, so a third means the client is not reading at all.
pub const SNAPSHOT_QUEUE: usize = 2;

/// One event the server main loop must react to.
#[derive(Debug)]
pub enum SessionEvent {
    /// A peer completed the handshake and became a client.
    Opened { id: u64 },
    /// One command from a client.
    Command { id: u64, req_id: u64, expect_rev: Option<u64>, op: crate::ipc::Op },
    /// The client said goodbye (`via_bye: true`) or its socket ended (`false`).
    Closed { id: u64, via_bye: bool },
}

/// The server's handle on one connected client.
pub struct Session {
    /// Session identity, unique within the process.
    pub id: u64,
    /// Snapshot frames to this client; `None` once it can no longer be served.
    outbox: Option<SyncSender<ServerMsg>>,
    /// Set when this client said goodbye, so the shutdown trigger can tell a
    /// deliberate exit from a crash (they get different grace windows).
    pub said_bye: bool,
    /// The writer thread, joined on removal so the socket is really closed.
    writer: Option<JoinHandle<()>>,
}

impl Session {
    /// A session whose queue the caller owns, for exercising the fan-out rules
    /// without a socket.
    #[cfg(test)]
    pub fn with_queue(id: u64, outbox: SyncSender<ServerMsg>) -> Session {
        Session { id, outbox: Some(outbox), said_bye: false, writer: None }
    }

    /// Try to queue one message without ever blocking the broadcaster.
    ///
    /// Returns `false` when the queue is full or the writer is gone: either way
    /// this client can no longer be kept up to date, and the caller drops it.
    pub fn try_send(&mut self, msg: ServerMsg) -> bool {
        let Some(outbox) = self.outbox.as_ref() else {
            return false;
        };
        match outbox.try_send(msg) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.outbox = None;
                false
            }
        }
    }

    /// Close the queue and wait for the writer, so the peer sees a clean EOF.
    pub fn shut(&mut self) {
        self.outbox = None;
        if let Some(handle) = self.writer.take() {
            // Best effort: the writer observes the closed channel and returns.
            let _ = handle.join();
        }
    }
}

/// Accept every pending connection, handshaking each one.
///
/// Non-blocking: the server main loop polls this once per tick, so accepting
/// needs no thread of its own. Newly opened sessions are appended to `sessions`
/// and reported on `events` as [`SessionEvent::Opened`], in that order, so the
/// loop can send each one its own `Welcome` against the final count.
pub fn accept_pending(
    listener: &UnixListener,
    app: &App,
    next_id: &AtomicU64,
    events: &std::sync::mpsc::Sender<SessionEvent>,
    sessions: &mut Vec<Session>,
) {
    loop {
        let Ok((stream, _addr)) = listener.accept() else {
            // `WouldBlock` is the normal "nothing pending" answer for a
            // non-blocking listener; a real error will simply be retried on the
            // next tick, and the loop must not stall the UI either way.
            return;
        };
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        if let Some(session) = handshake(stream, id, app, events) {
            sessions.push(session);
        }
    }
}

/// Complete the handshake and start the two threads, or reject the peer.
///
/// A rejected peer is not a server failure: it is an old client, a different
/// config, or a random process poking the socket. It is told why — when the
/// protocol still allows a frame — and dropped.
fn handshake(
    mut stream: UnixStream,
    id: u64,
    app: &App,
    events: &std::sync::mpsc::Sender<SessionEvent>,
) -> Option<Session> {
    // The accepted socket inherits the listener's blocking mode on Linux, so set
    // the handshake bound explicitly rather than assuming.
    stream.set_read_timeout(Some(HELLO_TIMEOUT)).ok()?;
    let (proto_version, config_path) = match ipc::read_client_msg(&mut stream) {
        // A first frame that is not `Hello` cannot be answered meaningfully: the
        // peer is not speaking this handshake.
        Ok(Some(ClientMsg::Hello { proto_version, config_path, .. })) => (proto_version, config_path),
        Ok(_) => return None,
        Err(_) => return None,
    };
    if let Err(e) = check_version(proto_version).and_then(|()| check_config(&served_config(app), &config_path)) {
        let _ = reject(&mut stream, &e);
        return None;
    }
    // From here the peer is a client. Clear the handshake bound so the reader can
    // block normally on the next frame.
    stream.set_read_timeout(None).ok()?;
    let session = start_threads(stream, id, events)?;
    // Only now is it a client: the count drives the shutdown decision, so a
    // half-open connection that never said `Hello` must not appear in it.
    let _ = events.send(SessionEvent::Opened { id });
    Some(session)
}

/// Epoch milliseconds for a snapshot's `server_now_ms`.
///
/// `model::now_unix()` counts **seconds**; the wire wants milliseconds, and a
/// client derives every age from a difference against this one value, so the
/// multiplication has to happen exactly once, here.
pub fn now_ms() -> i64 {
    crate::model::now_unix().saturating_mul(1000)
}

/// The config path this server serves, in the same lexical form a client sends.
///
/// Both sides derive it from `ipc::endpoint_for(config)`, which normalises
/// lexically, so the strings compare equal without a `canonicalize` that would
/// fail before the config file exists.
pub fn served_config(app: &App) -> String {
    ipc::endpoint_for(&app.paths.config).config_path.display().to_string()
}

/// Send a refusal frame, best effort.
fn reject(stream: &mut UnixStream, err: &ProtocolError) -> std::io::Result<()> {
    let msg = ServerMsg::Error { code: err.code.to_string(), error: err.error.clone() };
    ipc::write_message(stream, &msg).and_then(|()| stream.flush())
}

/// Spawn the reader and writer threads for one accepted client.
fn start_threads(stream: UnixStream, id: u64, events: &std::sync::mpsc::Sender<SessionEvent>) -> Option<Session> {
    let read_half = stream.try_clone().ok()?;
    let (outbox, out_rx) = sync_channel::<ServerMsg>(SNAPSHOT_QUEUE);
    spawn_reader(read_half, id, events.clone())?;
    let writer = std::thread::Builder::new()
        .name(format!("sshpf-srv-tx{id}"))
        .spawn(move || writer_loop(stream, out_rx))
        .ok()?;
    Some(Session { id, outbox: Some(outbox), said_bye: false, writer: Some(writer) })
}

/// Read frames from one client until it goes away.
fn spawn_reader(mut stream: UnixStream, id: u64, events: std::sync::mpsc::Sender<SessionEvent>) -> Option<()> {
    std::thread::Builder::new()
        .name(format!("sshpf-srv-rx{id}"))
        .spawn(move || {
            loop {
                match ipc::read_client_msg(&mut stream) {
                    Ok(Some(ClientMsg::Command { req_id, expect_rev, op })) => {
                        if events.send(SessionEvent::Command { id, req_id, expect_rev, op }).is_err() {
                            return;
                        }
                    }
                    Ok(Some(ClientMsg::Bye { .. })) => {
                        let _ = events.send(SessionEvent::Closed { id, via_bye: true });
                        return;
                    }
                    // A repeated `Hello` or a log subscription is accepted and
                    // ignored: every snapshot already carries the log tail the
                    // client renders, and the handshake is over.
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            // EOF or a broken frame: the client vanished without a `Bye` (crash,
            // `SIGKILL`, closed terminal). The count must still drop, or the
            // server would never be allowed to exit.
            let _ = events.send(SessionEvent::Closed { id, via_bye: false });
        })
        .ok()
        .map(|_| ())
}

/// Drain the queue into the socket; stop when the queue closes or a write fails.
fn writer_loop(mut stream: UnixStream, out_rx: Receiver<ServerMsg>) {
    while let Ok(msg) = out_rx.recv() {
        if ipc::write_message(&mut stream, &msg).and_then(|()| stream.flush()).is_err() {
            return;
        }
    }
    // Shutting the write half is what tells the peer the session ended.
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

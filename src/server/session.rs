//! One client's session: handshake, the read loop, and the reply path.
//!
//! # Why this thread exists
//!
//! Accepting a connection and *serving* it are different latencies: a client may
//! sit idle for hours, so its reads must never block the accept loop; and a command
//! needs an answer, so its writes must never block on the state lock. One thread
//! per client (reader + writer on the same socket) is the simplest shape that
//! satisfies both, and ten clients is the documented ceiling.
//!
//! # The handshake is a gate, not a formality
//!
//! The first frame must be a [`ClientMsg::Hello`] carrying the config path and the
//! token. Only after both check out does the client receive
//! [`ServerMsg::Welcome`], and only then is it registered for broadcasts and
//! **counted**. Anything else — a wrong token, a wrong version, a different config,
//! or no `Hello` within [`ipc::HANDSHAKE_TIMEOUT`] — is refused with a frame that
//! says what to do, and the socket is closed. Counting only on welcome is what
//! keeps a half-open connection from pinning the server open forever.
//!
//! Belongs here: one connection's life.
//! Does not belong: counting clients ([`super::state`]), fan-out
//! ([`super::broadcast`]), or what a command does ([`super::commands`]).

use std::io::{BufWriter, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::eventlog;
use crate::ipc::{self, ClientMsg, ServerMsg, WireError};

use super::framing::{FrameReader, Incoming};
use super::state::Shared;

/// How long the loop waits for one frame before re-checking its own flags.
///
/// Short enough that a shutdown or a lagging client is noticed promptly, long
/// enough that an idle client costs almost nothing.
const READ_POLL: Duration = Duration::from_millis(200);

/// Spawn the session thread for one accepted connection.
///
/// A thread that cannot be created is reported and the connection dropped: the
/// client retries, and the alternative (accepting without ever serving) looks
/// exactly like a hang.
pub fn spawn(stream: TcpStream, peer: SocketAddr, shared: Shared) {
    let spawned = std::thread::Builder::new()
        .name(format!("sshpf-session-{}", peer.port()))
        .spawn(move || serve(stream, peer, shared));
    if let Err(e) = spawned {
        crate::logging::error("server.session_thread_failed", &[("msg", &e.to_string())]);
    }
}

/// The whole life of one connection.
fn serve(stream: TcpStream, peer: SocketAddr, shared: Shared) {
    if let Err(e) = negotiate_timeouts(&stream) {
        eventlog::server_connection_refused(&format!("{peer}: {e}"));
        return;
    }
    let Ok(read_half) = stream.try_clone() else {
        eventlog::server_connection_refused(&format!("{peer}: cannot clone the socket"));
        return;
    };
    let mut reader = std::io::BufReader::new(read_half);
    let mut writer = BufWriter::new(stream);

    let Some(session) = handshake(&mut reader, &mut writer, peer, &shared) else {
        return;
    };
    run_session(reader, &mut writer, session, shared, peer);
}

/// What a successful handshake produced.
struct Session {
    /// The registry id, used for removal when this session ends.
    id: u64,
    /// Encoded frames waiting to be written; bounded by the broadcaster.
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    /// Set by the broadcaster when this client fell too far behind.
    lagging: Arc<AtomicBool>,
}

/// Validate the opening `Hello`, returning the served config path on success.
///
/// Every rejection writes a frame that tells the client what to do and is recorded
/// once, so the caller only has to stop.
fn admit(
    reader: &mut std::io::BufReader<TcpStream>,
    writer: &mut BufWriter<TcpStream>,
    peer: SocketAddr,
    shared: &Shared,
) -> Option<String> {
    let msg = match ipc::read_client_msg(reader) {
        Ok(Some(msg)) => msg,
        Ok(None) => {
            eventlog::server_connection_refused(&format!("{peer}: closed before Hello"));
            return None;
        }
        Err(e) => {
            refuse(writer, "bad_frame", &e.to_string());
            eventlog::server_connection_refused(&format!("{peer}: {e}"));
            return None;
        }
    };
    let ClientMsg::Hello { proto_version, config_path, token } = msg else {
        refuse(writer, "expected_hello", "the first frame must be a hello");
        eventlog::server_connection_refused(&format!("{peer}: the first frame was not a hello"));
        return None;
    };
    if let Err(e) = ipc::check_version(proto_version) {
        refuse(writer, e.code, &e.error.en);
        eventlog::server_connection_refused(&format!("{peer}: {}", e.code));
        return None;
    }
    let served = shared.config_display();
    if let Err(e) = ipc::check_config(&served, &config_path) {
        refuse(writer, e.code, &e.error.en);
        eventlog::server_connection_refused(&format!("{peer}: {}", e.code));
        return None;
    }
    if let Err(e) = ipc::check_token(&shared.token(), &token) {
        refuse(writer, e.code, &e.error.en);
        eventlog::server_connection_refused(&format!("{peer}: {}", e.code));
        return None;
    }
    Some(served)
}

/// Read the `Hello`, validate it, and send the `Welcome`.
fn handshake(
    reader: &mut std::io::BufReader<TcpStream>,
    writer: &mut BufWriter<TcpStream>,
    peer: SocketAddr,
    shared: &Shared,
) -> Option<Session> {
    // Everything that can refuse the client — a malformed frame, a wrong version, a
    // different config, a wrong token — is one gate, so the welcome path below has
    // no refusals left in it.
    let served = admit(reader, writer, peer, shared)?;

    // Counting happens here, after the token check, and never before: a connection
    // refused for a bad secret must not have incremented `client_count`, or anyone
    // who can reach the port could keep the server alive by probing it.
    if !shared.admit_client(&peer) {
        refuse(writer, "server_full", "the server is at its client limit or shutting down");
        eventlog::server_connection_refused(&format!("{peer}: refused (full or stopping)"));
        return None;
    }

    let (tx, rx) = std::sync::mpsc::sync_channel(super::broadcast::CLIENT_QUEUE);
    let lagging = Arc::new(AtomicBool::new(false));
    let id = super::broadcast::register(tx, lagging.clone());
    let welcome = ServerMsg::Welcome {
        proto_version: ipc::PROTO_VERSION,
        config_path: served,
        server_pid: std::process::id(),
        snapshot: shared.snapshot(),
    };
    if let Err(e) = ipc::write_message(writer, &welcome) {
        // Vanished between connecting and being welcomed: give the slot back, or the
        // count would stay inflated and the server would never exit.
        super::broadcast::unregister(id);
        shared.release_client("welcome_failed");
        eventlog::server_connection_refused(&format!("{peer}: {e}"));
        return None;
    }
    Some(Session { id, rx, lagging })
}

/// Serve a welcomed client until it leaves.
fn run_session(
    reader: std::io::BufReader<TcpStream>,
    writer: &mut BufWriter<TcpStream>,
    session: Session,
    shared: Shared,
    peer: SocketAddr,
) {
    let mut reader = FrameReader::new(reader);
    let mut last_exit = false;
    let mut reason = "eof";
    loop {
        if shared.is_stopping() {
            reason = "shutdown";
            break;
        }
        match reader.read_incoming() {
            Incoming::Msg(msg) => match handle_msg(writer, msg, &shared) {
                Flow::Continue => {}
                Flow::Bye(exits) => {
                    last_exit = exits;
                    acknowledge_bye(writer, &shared);
                    break;
                }
                Flow::Dead(why) => {
                    reason = why;
                    break;
                }
            },
            Incoming::Closed => {
                reason = "eof";
                break;
            }
            // Nothing complete this poll: not an event, so the loop continues to
            // the lagging check and the outbound queue below.
            Incoming::Timeout => {}
            Incoming::Failed(e) => {
                // This session was already welcomed, so a broken frame or a reset
                // socket is not a *refusal* — that wording belongs to the handshake
                // gate. On Windows a killed window arrives here as
                // `WSAECONNRESET`, which is exactly the "the window died" fact
                // whoever reads the log is looking for.
                eventlog::server_session_ended(&format!("{peer}: {e}"));
                reason = "read_failed";
                break;
            }
        }
        if session.lagging.load(Ordering::Relaxed) {
            reason = "lagging";
            break;
        }
        drain_queue(writer, &session);
    }
    super::broadcast::unregister(session.id);
    // Flush first: the last client leaving should still receive the reply that
    // acknowledges its own `Bye`, and the count must not drop before that frame is
    // on the wire.
    drain_queue(writer, &session);
    let remaining = shared.release_client(reason);
    super::lifecycle::on_client_left(&shared, remaining, last_exit);
}

/// Answer a `Bye` so the client learns how many clients remain.
///
/// Best effort: a client that already closed its socket is not an error, and the
/// count it would have been told is only advisory anyway (the server recounted).
fn acknowledge_bye(writer: &mut BufWriter<TcpStream>, shared: &Shared) {
    let reply = ServerMsg::Reply {
        req_id: 0,
        ok: true,
        rev: shared.revision(),
        error: None,
        remaining: Some(shared.client_count()),
        ports: Vec::new(),
    };
    let _ = write_message(writer, &reply);
}

/// What handling one message means for the loop.
enum Flow {
    Continue,
    /// The client said goodbye; carries its `last_exit` proposal.
    Bye(bool),
    /// The connection is unusable.
    Dead(&'static str),
}

/// Apply one client message.
fn handle_msg(writer: &mut BufWriter<TcpStream>, msg: ClientMsg, shared: &Shared) -> Flow {
    match msg {
        ClientMsg::Command { req_id, expect_rev, op } => {
            let outcome = super::commands::run(shared, &op, expect_rev);
            let reply = ServerMsg::Reply {
                req_id,
                ok: outcome.ok,
                rev: shared.revision(),
                error: outcome.error,
                remaining: Some(shared.client_count()),
                ports: outcome.ports,
            };
            if write_message(writer, &reply) { Flow::Continue } else { Flow::Dead("write_failed") }
        }
        ClientMsg::Bye { last_exit } => {
            eventlog::client_bye(last_exit, Some(shared.client_count()));
            Flow::Bye(last_exit)
        }
        ClientMsg::Ping => {
            let pong = ServerMsg::Pong { server_now_ms: super::state::now_ms() };
            if write_message(writer, &pong) { Flow::Continue } else { Flow::Dead("write_failed") }
        }
        // A second `Hello` on an established session is protocol misuse, not a
        // reason to drop a working client. Log subscriptions are not implemented
        // yet; ignoring them keeps an older client working.
        ClientMsg::Hello { .. } | ClientMsg::SubscribeLogs { .. } => Flow::Continue,
    }
}

/// Write one message, reporting whether it landed.
fn write_message(writer: &mut BufWriter<TcpStream>, msg: &ServerMsg) -> bool {
    ipc::write_message(writer, msg).is_ok()
}

/// Write every frame the broadcaster queued for this client.
///
/// The queue holds complete, already-framed bytes (see [`super::broadcast`]), so
/// this writes them verbatim: no re-encoding and, crucially, no re-adding of a
/// length prefix.
fn drain_queue(writer: &mut BufWriter<TcpStream>, session: &Session) {
    while let Ok(frame) = session.rx.try_recv() {
        if writer.write_all(&frame).is_err() {
            return;
        }
        let _ = writer.flush();
    }
}

/// Write a protocol-level refusal, best effort.
fn refuse(writer: &mut BufWriter<TcpStream>, code: &str, detail: &str) {
    let msg = ServerMsg::Error {
        code: code.to_string(),
        error: WireError {
            code: code.to_string(), zh: format!("服务端拒绝连接：{detail}"), en: detail.to_string()
        },
    };
    let _ = ipc::write_message(writer, &msg);
}

/// Give the socket timeouts so the loop can poll its own flags between frames.
///
/// A read timeout is what makes the session observable: the loop can then check
/// whether the server is shutting down or the client fell behind, instead of
/// blocking forever on a client that will never speak again. The write timeout
/// bounds a wedged client so one unresponsive peer cannot hold a session thread.
fn negotiate_timeouts(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(READ_POLL))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    // Small frames, latency matters: a client's reply should not wait for Nagle.
    let _ = stream.set_nodelay(true);
    Ok(())
}

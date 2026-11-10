//! Moving frames for one attachment: the reader thread, the writer, the keep-alive.
//!
//! # Two threads, and why a read timeout is not the alternative
//!
//! Reading blocks until the peer sends something; writing must happen on a timer
//! (the keep-alive) and whenever the user presses a key. The obvious way to do both
//! on one thread — a read timeout — is **not** usable here: a timeout that lands
//! inside a frame leaves the stream positioned mid-message, and a partial frame
//! cannot be resumed, so the next read would decode garbage. The reader therefore
//! blocks with no timeout, and the writer shuts the socket down when it is done,
//! which is what unblocks the reader.
//!
//! # Why the reader decides nothing
//!
//! It hands every frame to the UI thread unmodified, including the `Welcome`. That
//! is what lets [`super::sync`] be a pure function over `(&mut App, ServerMsg)` and
//! be unit-tested without a socket; the only things the reader knows are "send this
//! on" and "the stream ended".
//!
//! Belongs here: the read loop, the write loop, and the keep-alive interval.
//! Does not belong: the handshake ([`super::connect`]), what a frame means
//! ([`super::sync`]), or who to reconnect to ([`super::reconnect`]).

use std::io::BufReader;
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::ipc::{self, ClientMsg, ServerMsg};

use super::connect::Connection;

/// How often a `Ping` is sent.
///
/// Without traffic, a loopback socket gives no signal that the peer is gone: a
/// client killed with `TerminateProcess` leaves a half-open connection that only a
/// write can discover. The server's `client_count` — which decides whether the whole
/// server exits — depends on noticing, hence the heartbeat.
pub const PING_INTERVAL: Duration = Duration::from_secs(2);

/// How often the writer wakes up to notice a dead reader or a due ping.
///
/// This is detection latency, not throughput: a reader that saw EOF is noticed
/// within one tick, so the UI learns the link died in a tick, not in two seconds.
const WRITER_TICK: Duration = Duration::from_millis(100);

/// Why one attachment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The peer closed the socket: server exit, crash, or a reset.
    PeerClosed,
    /// The caller left: it sent `Bye`, or its outbox was dropped.
    CallerGone,
}

/// What happened during one attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Served {
    /// How the socket ended.
    pub ended: Ended,
    /// The `last_exit` the caller proposed, when it sent a `Bye`.
    pub last_exit: Option<bool>,
}

/// Drive one attachment until the socket is finished with.
///
/// Returns only when the link is over; the caller decides what to do next. Nothing
/// here runs on the UI thread — [`super::run`] calls it on the connection thread.
pub fn serve(connection: Connection, commands: &Receiver<ClientMsg>, inbox: &Sender<ServerMsg>) -> Served {
    let Connection { stream, welcome, .. } = connection;
    // Hand the greeting over first, so the UI can render before any command is read.
    if inbox.send(welcome).is_err() {
        // The UI is gone; there is nobody left to serve.
        return Served { ended: Ended::CallerGone, last_exit: None };
    }
    let Ok(mut writer) = stream.try_clone() else {
        return Served { ended: Ended::PeerClosed, last_exit: None };
    };
    let peer_closed = Arc::new(AtomicBool::new(false));
    let Some(reader) = spawn_reader(stream, inbox, &peer_closed) else {
        return Served { ended: Ended::PeerClosed, last_exit: None };
    };
    let (ended, last_exit) = write_loop(&mut writer, commands, &peer_closed);
    // Shutting the socket down is what unblocks the reader; see the module docs.
    let _ = writer.shutdown(Shutdown::Both);
    let _ = reader.join();
    Served { ended, last_exit }
}

/// Start the read-side thread, or report that it could not be started.
fn spawn_reader(
    stream: TcpStream,
    inbox: &Sender<ServerMsg>,
    peer_closed: &Arc<AtomicBool>,
) -> Option<thread::JoinHandle<()>> {
    let inbox = inbox.clone();
    let peer_closed = peer_closed.clone();
    let spawned = thread::Builder::new()
        .name("sshpf-client-read".to_string())
        .spawn(move || read_loop(stream, &inbox, &peer_closed));
    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            crate::eventlog::client_disconnected("reader_spawn_failed");
            crate::logging::error("client.read_failed", &[("msg", &e.to_string())]);
            None
        }
    }
}

/// Read frames until EOF, an error, or the writer closing the socket.
///
/// Every exit path is the same: stop reading, tell the writer, and shut the socket
/// down. That is what makes "the server went away" and "the UI went away" the same
/// code path with the same effect on the peer.
fn read_loop(stream: TcpStream, inbox: &Sender<ServerMsg>, peer_closed: &AtomicBool) {
    let mut reader = BufReader::new(stream);
    // A closed channel means the UI is gone; either way the link is over.
    while let Ok(Some(msg)) = ipc::read_server_msg(&mut reader) {
        if inbox.send(msg).is_err() {
            break;
        }
    }
    peer_closed.store(true, Ordering::Relaxed);
    // Also stop the writer's next blocking write.
    let _ = reader.get_ref().shutdown(Shutdown::Both);
}

/// Write commands and keep-alives until the socket ends or the caller leaves.
fn write_loop(
    stream: &mut TcpStream,
    commands: &Receiver<ClientMsg>,
    peer_closed: &AtomicBool,
) -> (Ended, Option<bool>) {
    let mut last_ping = Instant::now();
    loop {
        if peer_closed.load(Ordering::Relaxed) {
            return (Ended::PeerClosed, None);
        }
        match commands.recv_timeout(WRITER_TICK) {
            Ok(ClientMsg::Bye { last_exit }) => {
                // Written explicitly rather than left to `Drop`: the server must
                // learn the user confirmed, and only a completed write proves it
                // arrived before the socket closes.
                let _ = ipc::write_message(stream, &ClientMsg::Bye { last_exit });
                return (Ended::CallerGone, Some(last_exit));
            }
            Ok(msg) => {
                if ipc::write_message(stream, &msg).is_err() {
                    return (Ended::PeerClosed, None);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if last_ping.elapsed() >= PING_INTERVAL {
                    if ipc::write_message(stream, &ClientMsg::Ping).is_err() {
                        return (Ended::PeerClosed, None);
                    }
                    last_ping = Instant::now();
                }
            }
            // The outbox was dropped, so nothing can be sent again. Flush whatever
            // is already queued — a `Bye` may be sitting there — then stop.
            Err(RecvTimeoutError::Disconnected) => {
                while let Ok(msg) = commands.try_recv() {
                    if ipc::write_message(stream, &msg).is_err() {
                        break;
                    }
                }
                return (Ended::CallerGone, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn welcome() -> ServerMsg {
        ServerMsg::Welcome {
            proto_version: ipc::PROTO_VERSION,
            config_path: "/tmp/cfg.toml".into(),
            server_pid: 4242,
            snapshot: ipc::Snapshot::from_views(1, 0, 1, false, &[]),
        }
    }

    /// A connected client end plus the listener the test drives as "server".
    ///
    /// The connect is not accepted here: a loopback backlog holds it, which is what
    /// lets the test decide *when* the server side starts reading.
    fn pair() -> (TcpListener, Connection) {
        let (listener, port) = ipc::bind_loopback().expect("loopback is bindable");
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let connection = Connection { stream, welcome: welcome() };
        (listener, connection)
    }

    #[test]
    fn the_keep_alive_is_frequent_enough_for_the_server_to_notice() {
        // The server's `client_count` decides whether it exits; a heartbeat slower
        // than the protocol's documented budget would let a dead client hold the
        // server up.
        assert!(PING_INTERVAL <= Duration::from_secs(2), "{PING_INTERVAL:?} is too slow");
        assert!(WRITER_TICK < PING_INTERVAL, "the tick must be finer than the heartbeat it schedules");
    }

    #[test]
    fn the_greeting_is_delivered_before_anything_else() {
        let (listener, connection) = pair();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("the client end must arrive");
            // Hold the connection open so the reader has something to read from.
            thread::sleep(Duration::from_millis(100));
        });
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        let (inbox_tx, inbox_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || serve(connection, &commands_rx, &inbox_tx));
        let first = inbox_rx.recv_timeout(Duration::from_secs(2)).expect("the welcome must arrive");
        assert!(matches!(first, ServerMsg::Welcome { server_pid: 4242, .. }), "{first:?}");
        drop(commands_tx);
        let _ = worker.join();
        let _ = server.join();
    }

    #[test]
    fn a_command_reaches_the_far_side_of_the_socket() {
        let (listener, connection) = pair();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            ipc::read_client_msg(&mut stream).expect("a frame").expect("something to read")
        });
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        let (inbox_tx, _inbox_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || serve(connection, &commands_rx, &inbox_tx));
        commands_tx.send(ClientMsg::Command { req_id: 7, expect_rev: Some(3), op: ipc::Op::StopAll }).expect("queue");
        let seen = server.join().expect("the server thread must finish");
        assert_eq!(seen, ClientMsg::Command { req_id: 7, expect_rev: Some(3), op: ipc::Op::StopAll });
        drop(commands_tx);
        let _ = worker.join();
    }

    #[test]
    fn a_bye_ends_the_attachment_and_carries_the_callers_proposal() {
        let (listener, connection) = pair();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            ipc::read_client_msg(&mut stream).expect("a frame").expect("something to read")
        });
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        let (inbox_tx, _inbox_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || serve(connection, &commands_rx, &inbox_tx));
        commands_tx.send(ClientMsg::Bye { last_exit: true }).expect("queue");
        let served = worker.join().expect("the serve thread must finish");
        assert_eq!(served.ended, Ended::CallerGone);
        assert_eq!(served.last_exit, Some(true), "the caller's proposal must reach the server");
        assert_eq!(server.join().expect("the server thread must finish"), ClientMsg::Bye { last_exit: true });
    }

    #[test]
    fn a_dropped_outbox_ends_the_attachment_without_a_bye() {
        let (listener, connection) = pair();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            let mut reader = BufReader::new(stream);
            // Whatever is left: EOF, or nothing.
            let _ = ipc::read_server_msg(&mut reader);
        });
        let (commands_tx, commands_rx) = std::sync::mpsc::channel::<ClientMsg>();
        let (inbox_tx, _inbox_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || serve(connection, &commands_rx, &inbox_tx));
        drop(commands_tx);
        let served = worker.join().expect("a closed outbox must end the loop");
        assert_eq!(served.ended, Ended::CallerGone);
        assert_eq!(served.last_exit, None, "no bye was sent, so nothing was proposed");
        let _ = server.join();
    }

    #[test]
    fn a_peer_that_goes_away_ends_the_attachment() {
        let (listener, connection) = pair();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            drop(stream);
        });
        let (_commands_tx, commands_rx) = std::sync::mpsc::channel::<ClientMsg>();
        let (inbox_tx, _inbox_rx) = std::sync::mpsc::channel();
        let served = serve(connection, &commands_rx, &inbox_tx);
        assert_eq!(served.ended, Ended::PeerClosed);
    }

    #[test]
    fn several_frames_arrive_in_order_after_the_greeting() {
        let (listener, connection) = pair();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            for ms in [1_i64, 2, 3] {
                ipc::write_message(&mut stream, &ServerMsg::Pong { server_now_ms: ms }).expect("write");
            }
            let _ = stream.flush();
            thread::sleep(Duration::from_millis(200));
        });
        let (_commands_tx, commands_rx) = std::sync::mpsc::channel::<ClientMsg>();
        let (inbox_tx, inbox_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || serve(connection, &commands_rx, &inbox_tx));
        assert!(matches!(inbox_rx.recv_timeout(Duration::from_secs(2)), Ok(ServerMsg::Welcome { .. })));
        for ms in [1_i64, 2, 3] {
            let got = inbox_rx.recv_timeout(Duration::from_secs(2)).expect("each pong must arrive");
            assert_eq!(got, ServerMsg::Pong { server_now_ms: ms }, "frames must keep their order");
        }
        let _ = worker.join();
        let _ = server.join();
    }
}

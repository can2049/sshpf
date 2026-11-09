//! The two threads that own a socket for the rest of the session: one forwarding
//! [`ServerMsg`]s in, one draining [`ClientMsg`]s out.
//!
//! # The shape, and why it is two threads and not one
//!
//! A `UnixStream` is one object; borrowing it from two directions needs a lock,
//! and a lock around a blocking read means the writer waits for the reader (or
//! vice versa). Two threads, each owning its half, needs no lock at all: reads
//! block for as long as the server is quiet, writes happen the instant the user
//! presses a key.
//!
//! ```text
//! reader: socket ──read_server_msg──▶ Sender<ServerMsg>  (into App)
//! writer: Receiver<ClientMsg> ──write_message──▶ socket
//! ```
//!
//! # Teardown is the hard part
//!
//! Both threads must end when *either* end of the session ends, and neither may
//! busy-wait:
//!
//! * **app quits** → drops `outbound` → the writer's `recv()` fails → the writer
//!   shuts the socket down for both directions, which is what wakes the reader,
//!   whose `read` is blocked in the kernel. Without that shutdown the reader
//!   would outlive the app forever.
//! * **server dies / EOF** → the reader's loop ends → it drops `inbound`, which
//!   the app sees as a closed channel and reports as `Lost`.
//!
//! # Memory is bounded by construction
//!
//! Neither channel is drained into a buffer of our own: a message crosses
//! straight from the socket into the app's channel and is consumed there every
//! frame. A `Snapshot` replaces state wholesale, so the only thing a backlog can
//! do is delay, never grow the process without limit.
//!
//! Belongs here: the two loops and the teardown contract between them.
//! Does not belong: the handshake ([`super::handshake`]), starting the server
//! ([`super::spawn`]), or what the messages mean ([`crate::app::remote`]).

use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

use crate::eventlog;
use crate::ipc::{self, ClientMsg, ServerMsg};

use super::AttachError;

/// Start the reader and writer threads and hand back the two channel ends.
///
/// `first` is delivered through the same `Sender` the reader uses, before the
/// reader starts, so the app's very first frame already has the server's state
/// (the `Welcome` snapshot) instead of rendering an empty table.
pub fn start(
    socket: UnixStream,
    first: Option<ServerMsg>,
) -> Result<(Sender<ClientMsg>, Receiver<ServerMsg>), AttachError> {
    // Small queues: a `Snapshot` is a whole table, and a backlog of them has no
    // value — the newest one supersedes every earlier one.
    let (out_tx, out_rx) = std::sync::mpsc::channel::<ClientMsg>();
    let (in_tx, in_rx) = std::sync::mpsc::channel::<ServerMsg>();
    if let Some(msg) = first {
        // The receiver exists and nothing can have dropped it yet.
        let _ = in_tx.send(msg);
    }
    // The writer gets its own handle: it needs `shutdown` to wake the blocked
    // reader, and that is the only reason the socket is shared at all.
    let reader_socket = socket.try_clone().map_err(|e| clone_error(&e))?;
    spawn_reader(reader_socket, in_tx);
    spawn_writer(socket, out_rx);
    Ok((out_tx, in_rx))
}

/// Forward every `ServerMsg` until the stream ends, then close the channel.
///
/// Dropping `in_tx` on the way out is the signal the app relies on: it is how a
/// mirror learns the transport died (see [`crate::app::remote::inbound`]).
fn spawn_reader(mut socket: UnixStream, in_tx: Sender<ServerMsg>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            match ipc::read_server_msg(&mut socket) {
                Ok(Some(msg)) => {
                    // A closed receiver means the app is gone; stop reading rather
                    // than decode frames nobody will look at.
                    if in_tx.send(msg).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    eventlog::client_disconnected("eof");
                    break;
                }
                Err(e) => {
                    // A read cut short by the writer's `shutdown` is the app quitting,
                    // already logged as a normal disconnect; anything else is a broken
                    // link worth a record.
                    eventlog::client_disconnected(&format!("read: {e}"));
                    break;
                }
            }
        }
    })
}

/// Write every `ClientMsg` until the outbound channel closes, then stop the
/// reader by shutting the socket down.
fn spawn_writer(mut socket: UnixStream, out_rx: Receiver<ClientMsg>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        // `recv()` blocks with no timeout: an idle client costs nothing, and the
        // thread wakes the moment there is intent to send.
        while let Ok(msg) = out_rx.recv() {
            if ipc::write_message(&mut socket, &msg).is_err() {
                // The link is already broken; the reader will report the same
                // failure on the next frame. Both halves are done.
                break;
            }
        }
        // The app has quit (or the connection failed). Unblock the reader, which
        // is parked in `read` and cannot notice a dropped Sender on its own.
        let _ = socket.shutdown(std::net::Shutdown::Both);
    })
}

/// The socket could not be duplicated for the reader thread.
fn clone_error(detail: &std::io::Error) -> AttachError {
    AttachError::new(
        format!("无法为读取线程复制套接字：{detail}。请重试。"),
        format!("Could not duplicate the socket for the reader thread: {detail}. Retry."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Op;
    use crate::ipc::snapshot::Snapshot;
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    /// How long a teardown assertion waits before declaring a hang. Wide enough
    /// that a loaded CI box does not flake, narrow enough that a leaked thread
    /// fails instead of hanging the suite.
    const PATIENCE: Duration = Duration::from_secs(5);

    fn snapshot(revision: u64) -> ServerMsg {
        ServerMsg::Snapshot(Snapshot::from_views(revision, 1_700_000_000_000, 1, false, &[]))
    }

    /// Receive one message, or `None` if the channel closed first.
    fn next_msg(rx: &Receiver<ServerMsg>, within: Duration) -> Option<ServerMsg> {
        rx.recv_timeout(within).ok()
    }

    #[test]
    fn the_welcome_frame_arrives_before_the_reader_starts() {
        // The app must be able to draw the server's state on its first frame; a
        // `Welcome` racing the reader would sometimes lose that race.
        let (peer, socket) = UnixStream::pair().expect("pair");
        let (_out, inbound) = start(socket, Some(snapshot(1))).expect("pump");
        match next_msg(&inbound, PATIENCE) {
            Some(ServerMsg::Snapshot(s)) => assert_eq!(s.revision, 1),
            other => panic!("expected the first frame, got {other:?}"),
        }
        drop(peer);
    }

    #[test]
    fn server_frames_reach_the_receiver_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = UnixListener::bind(dir.path().join("s.sock")).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().expect("accept");
            for rev in 1..=3u64 {
                ipc::write_message(&mut peer, &snapshot(rev)).expect("write");
            }
            // Closing the stream is how a server leaves; the client must see it.
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let (_out, inbound) = start(socket, None).expect("pump");
        let mut seen = Vec::new();
        while let Some(msg) = next_msg(&inbound, PATIENCE) {
            match msg {
                ServerMsg::Snapshot(s) => seen.push(s.revision),
                other => panic!("unexpected frame {other:?}"),
            }
        }
        assert_eq!(seen, [1, 2, 3], "frames must arrive in order and the channel must then close");
        server.join().expect("server thread");
    }

    #[test]
    fn the_reader_closes_the_channel_when_the_server_goes_away() {
        // This is the mirror's only "the link died" signal: a closed channel.
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = UnixListener::bind(dir.path().join("s.sock")).expect("bind");
        let server = std::thread::spawn(move || {
            let (peer, _) = listener.accept().expect("accept");
            drop(peer);
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let (_out, inbound) = start(socket, None).expect("pump");
        // EOF, not a timeout: the channel must be *disconnected*, and soon.
        let started = Instant::now();
        let deadline = started + PATIENCE;
        loop {
            match inbound.recv_timeout(Duration::from_millis(50)) {
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the reader never noticed EOF");
                }
                Ok(msg) => panic!("no frame was sent, got {msg:?}"),
            }
        }
        server.join().expect("server thread");
    }

    #[test]
    fn outbound_messages_reach_the_server_as_frames() {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = UnixListener::bind(dir.path().join("s.sock")).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().expect("accept");
            let mut ops = Vec::new();
            for _ in 0..2 {
                match ipc::read_client_msg(&mut peer).expect("read") {
                    Some(ClientMsg::Command { op, .. }) => ops.push(op),
                    other => panic!("expected a command, got {other:?}"),
                }
            }
            ops
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let (outbound, _inbound) = start(socket, None).expect("pump");
        outbound.send(ClientMsg::Command { req_id: 1, expect_rev: None, op: Op::StartAll }).expect("send");
        outbound.send(ClientMsg::Command { req_id: 2, expect_rev: None, op: Op::Stop { tunnel_id: 3 } }).expect("send");
        assert_eq!(server.join().expect("server thread"), [Op::StartAll, Op::Stop { tunnel_id: 3 }]);
    }

    #[test]
    fn closing_outbound_stops_the_writer_and_wakes_the_reader() {
        // The app quitting must not leave two threads parked on a dead session:
        // the writer exits, shuts the socket down, and the reader follows.
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = UnixListener::bind(dir.path().join("s.sock")).expect("bind");
        let server = std::thread::spawn(move || {
            let (peer, _) = listener.accept().expect("accept");
            // Hold the connection open and stay silent: if the client leaked its
            // reader thread, this test would hang rather than fail.
            std::thread::sleep(Duration::from_millis(300));
            drop(peer);
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let (outbound, inbound) = start(socket, None).expect("pump");
        drop(outbound);
        // The wake-up path ends the reader, which closes `inbound`.
        let started = Instant::now();
        loop {
            match inbound.recv_timeout(Duration::from_millis(50)) {
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() - started < PATIENCE, "the reader was not woken by the shutdown");
                }
                Ok(msg) => panic!("no frame was sent, got {msg:?}"),
            }
        }
        server.join().expect("server thread");
    }

    #[test]
    fn a_failing_write_ends_the_writer_without_a_panic() {
        // The server closing its end turns the next write into an error; that is
        // an ordinary disconnect, not a reason to take the process down.
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = UnixListener::bind(dir.path().join("s.sock")).expect("bind");
        let server = std::thread::spawn(move || {
            let (peer, _) = listener.accept().expect("accept");
            drop(peer);
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let (outbound, inbound) = start(socket, None).expect("pump");
        server.join().expect("server thread");
        // Give the reader a moment to notice the close first.
        let _ = inbound.recv_timeout(Duration::from_millis(100));
        // Sending into a dead link is allowed to fail (closed channel) or to be
        // accepted and dropped; neither may panic or block forever.
        let _ = outbound.send(ClientMsg::Bye { last_exit: false });
        drop(outbound);
        // The point of the test: nothing above panicked and the loop is over.
        let started = Instant::now();
        while started.elapsed() < PATIENCE {
            if matches!(
                inbound.recv_timeout(Duration::from_millis(50)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ) {
                return;
            }
        }
        panic!("the pump did not shut down");
    }

    #[test]
    fn an_empty_first_frame_still_starts_a_working_pump() {
        // `first: None` is the reconnect path; it must not be a special case in
        // the app (the reader is simply the only source of frames).
        let (_peer, socket) = UnixStream::pair().expect("pair");
        let (outbound, inbound) = start(socket, None).expect("pump");
        assert!(next_msg(&inbound, Duration::from_millis(100)).is_none(), "nothing was sent, so nothing may arrive");
        drop(outbound);
    }
}

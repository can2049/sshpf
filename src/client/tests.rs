//! Unit tests for `client.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

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

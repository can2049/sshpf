//! The synchronously-answered start of a session: `connect`, send `Hello`, and
//! turn the first frame into either a working socket or a precise reason.
//!
//! # Why the handshake is synchronous
//!
//! A TUI that attaches asynchronously has to render "connecting…" and then
//! explain a failure three frames later, with the window already open. Doing it
//! before the terminal is taken means every refusal — wrong protocol version,
//! wrong config, a server that never answered — reaches the user as one
//! sentence on stderr, and no half-open connection is ever handed to the app.
//!
//! # The timeout is load-bearing
//!
//! `set_read_timeout` is set for the handshake only, so a peer that accepts and
//! then says nothing cannot hang the client forever. It is **cleared again**
//! before the socket leaves this module: the reader thread must be able to block
//! in `read` for as long as the session lives.
//!
//! # Refusals are not retried by spawning
//!
//! If a server answered at all, it is the authority for this config. Starting a
//! rival because the two disagreed on protocol version or config path would hide
//! the disagreement and put two processes on one identity. Only "nothing is
//! listening" leads to a spawn (see [`super`]).
//!
//! Belongs here: `connect`, building `Hello`, reading and validating the first
//! frame, and the wording of each failure.
//! Does not belong: when to retry or spawn ([`super`]), the threads that own the
//! socket afterwards ([`super::pump`]), or what the snapshot means
//! ([`crate::app::remote`]).

use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::eventlog;
use crate::ipc::{self, ClientMsg, Endpoint, PROTO_VERSION, ServerMsg};
use crate::ssh::known_hosts::HostKeyPolicy;

use super::AttachError;

/// How long the client waits for the server's first frame.
///
/// Generous enough for a server that is still restoring tunnels, short enough
/// that a wedged one is reported instead of freezing the terminal.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// An accepted session: the socket, plus what `Welcome` said about its owner.
#[derive(Debug)]
pub struct Session {
    /// The connected socket, with no read timeout set.
    pub socket: UnixStream,
    /// The server process, for the header and for the log.
    pub server_pid: u32,
    /// The server's resolved config path — the real one, not the client's guess.
    pub config_path: String,
    /// The first frame, to be delivered to the app before the reader starts.
    pub welcome: ServerMsg,
}

/// Connect to the endpoint's socket.
///
/// A plain `io::Result` on purpose: this is called in the retry loop, where the
/// error is kept only to be quoted in the final message.
pub fn connect(endpoint: &Endpoint) -> io::Result<UnixStream> {
    UnixStream::connect(&endpoint.socket_path)
}

/// Perform the handshake on an established connection, with [`HANDSHAKE_TIMEOUT`].
pub fn handshake(
    socket: UnixStream,
    config_path: &str,
    host_key_policy: HostKeyPolicy,
) -> Result<Session, AttachError> {
    handshake_within(socket, config_path, host_key_policy, HANDSHAKE_TIMEOUT)
}

/// The handshake with an explicit budget, so a test need not wait two seconds.
pub fn handshake_within(
    mut socket: UnixStream,
    config_path: &str,
    host_key_policy: HostKeyPolicy,
    within: Duration,
) -> Result<Session, AttachError> {
    socket.set_read_timeout(Some(within)).map_err(|e| socket_error(config_path, &e))?;
    let hello = hello(config_path, host_key_policy);
    write_first(&mut socket, &hello, config_path)?;
    let first = read_first(&mut socket, config_path, within)?;
    accept(first, socket, config_path)
}

/// Send the `Hello`, treating an already-closed peer as the hang-up it is.
///
/// A server that accepts and exits at once fails the *write* (`Broken pipe`)
/// rather than the read — the same event, observed one syscall earlier. Reporting
/// it as "closed during the handshake" keeps the user's diagnosis identical
/// whichever side of the race the kernel reports, instead of leaking a bare
/// `os error 32` into an otherwise explanatory message.
fn write_first(socket: &mut UnixStream, hello: &ClientMsg, config_path: &str) -> Result<(), AttachError> {
    match ipc::write_message(socket, hello) {
        Ok(()) => Ok(()),
        Err(e) if peer_is_gone(&e) => Err(eof_error(config_path)),
        Err(e) => Err(write_error(config_path, &e)),
    }
}

/// Whether an I/O error means the peer's end of the stream no longer exists.
fn peer_is_gone(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
    )
}
/// The first frame the client sends, naming its version, its config and its
/// host-key policy.
fn hello(config_path: &str, host_key_policy: HostKeyPolicy) -> ClientMsg {
    ClientMsg::Hello {
        proto_version: PROTO_VERSION,
        config_path: config_path.to_string(),
        // Informational: the server dials with the policy it was started with, so
        // this lets it notice (and log) a client whose flag cannot be honoured,
        // rather than silently serving it under someone else's trust settings.
        host_key_policy: Some(eventlog::host_key_policy_name(host_key_policy).to_string()),
    }
}

/// Read exactly one frame, turning each way it can end into a sentence.
fn read_first(socket: &mut UnixStream, config_path: &str, within: Duration) -> Result<ServerMsg, AttachError> {
    match ipc::read_server_msg(socket) {
        Ok(Some(msg)) => Ok(msg),
        Ok(None) => Err(eof_error(config_path)),
        // A read timeout surfaces as `WouldBlock` (SO_RCVTIMEO) or `TimedOut`,
        // depending on the platform; both mean "the server said nothing".
        Err(e) if is_timeout(&e) => Err(timeout_error(config_path, within)),
        // The peer closed *while* we were reading. Depending on when the kernel
        // noticed, that is a clean EOF (handled above) or a reset — the same event,
        // and it must produce the same sentence, or the user sees a bare
        // `os error 104` for the most ordinary failure there is (a server that was
        // just killed).
        Err(e) if peer_is_gone(&e) => Err(eof_error(config_path)),
        Err(e) => Err(read_error(config_path, &e)),
    }
}

/// Whether an I/O error is a read timeout rather than a broken connection.
fn is_timeout(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Validate the first frame and hand back the session.
fn accept(first: ServerMsg, socket: UnixStream, asked: &str) -> Result<Session, AttachError> {
    match first {
        ServerMsg::Welcome { proto_version, config_path, server_pid, snapshot } => {
            // Same binary, same machine: a mismatch can only mean an instance
            // survived an upgrade. Refusing is safer than rendering a table that
            // may be silently wrong.
            ipc::check_version(proto_version).map_err(protocol_error)?;
            // The endpoint name is a hash of the config path, so a collision
            // would otherwise surface as "someone else's tunnels in my table".
            ipc::check_config(&config_path, asked).map_err(protocol_error)?;
            // From here on the socket belongs to the reader thread, which blocks.
            socket.set_read_timeout(None).map_err(|e| socket_error(asked, &e))?;
            Ok(Session {
                socket,
                server_pid,
                config_path: config_path.clone(),
                welcome: ServerMsg::Welcome { proto_version, config_path, server_pid, snapshot },
            })
        }
        // The server refused on purpose and already said why, in both languages.
        ServerMsg::Error { error, .. } => Err(AttachError::new(error.zh, error.en)),
        other => Err(unexpected_error(asked, message_kind(&other))),
    }
}

/// The wire error's own bilingual text, unchanged: it was written to be shown.
fn protocol_error(error: ipc::ProtocolError) -> AttachError {
    AttachError::new(error.error.zh, error.error.en)
}

/// A short name for a message, so a diagnostic never dumps a whole snapshot.
fn message_kind(msg: &ServerMsg) -> &'static str {
    match msg {
        ServerMsg::Welcome { .. } => "welcome",
        ServerMsg::Snapshot(_) => "snapshot",
        ServerMsg::Reply { .. } => "reply",
        ServerMsg::Stale { .. } => "stale",
        ServerMsg::Log { .. } => "log",
        ServerMsg::Error { .. } => "error",
    }
}

// ---------------------------------------------------------------------------
// wording
// ---------------------------------------------------------------------------

/// The peer accepted the connection and then closed it.
fn eof_error(config_path: &str) -> AttachError {
    AttachError::new(
        format!("服务端在握手阶段关闭了连接（配置 {config_path}）。请重试；若持续出现，请检查日志中服务端的退出原因。"),
        format!(
            "The server closed the connection during the handshake (config {config_path}). Retry; if it keeps happening, check the log for why the server exited."
        ),
    )
}

/// No first frame arrived within the budget.
fn timeout_error(config_path: &str, within: Duration) -> AttachError {
    let ms = within.as_millis();
    AttachError::new(
        format!(
            "服务端在 {ms} 毫秒内没有回应握手（配置 {config_path}）。请稍后重试；若持续超时，请运行 `sshpf --server` 查看它卡在哪一步。"
        ),
        format!(
            "The server did not answer the handshake within {ms} ms (config {config_path}). Retry in a moment; if it keeps timing out, run `sshpf --server` to see where it is stuck."
        ),
    )
}

/// The first frame was not a `Welcome`.
fn unexpected_error(config_path: &str, kind: &str) -> AttachError {
    AttachError::new(
        format!(
            "服务端的第一帧不是握手应答，而是 {kind}（配置 {config_path}）。双方版本可能不一致：请退出所有 sshpf 后重新启动。"
        ),
        format!(
            "The server's first frame was {kind}, not a handshake reply (config {config_path}). The two sides may be at different versions: quit every sshpf instance, then start again."
        ),
    )
}

/// The `Hello` could not be written.
fn write_error(config_path: &str, detail: &io::Error) -> AttachError {
    AttachError::new(
        format!("无法发送握手请求（配置 {config_path}）：{detail}。请确认这份配置的服务端仍在运行，然后重试。"),
        format!(
            "Could not send the handshake (config {config_path}): {detail}. Make sure the server for this config is still running, then retry."
        ),
    )
}

/// Reading the first frame failed for a reason that is not a timeout.
fn read_error(config_path: &str, detail: &io::Error) -> AttachError {
    AttachError::new(
        format!("读取握手应答失败（配置 {config_path}）：{detail}。请重试，并检查日志。"),
        format!("Could not read the handshake reply (config {config_path}): {detail}. Retry, and check the log."),
    )
}

/// The socket could not be configured for the handshake.
fn socket_error(config_path: &str, detail: &io::Error) -> AttachError {
    AttachError::new(
        format!("无法为连接设置超时（配置 {config_path}）：{detail}。请重试。"),
        format!("Could not set a timeout on the connection (config {config_path}): {detail}. Retry."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::snapshot::Snapshot;
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::thread::JoinHandle;

    /// A socket path plus a listener bound to it, in a throwaway directory.
    fn listening(dir: &Path) -> (Endpoint, UnixListener) {
        let endpoint = Endpoint {
            config_path: dir.join("config.toml"),
            socket_path: dir.join("s.sock"),
            lock_path: dir.join("s.lock"),
            dir: dir.to_path_buf(),
        };
        let listener = UnixListener::bind(&endpoint.socket_path).expect("bind");
        (endpoint, listener)
    }

    /// Answer one connection with `script`, on its own thread.
    fn serve(listener: UnixListener, script: impl FnOnce(&mut UnixStream) + Send + 'static) -> JoinHandle<()> {
        std::thread::spawn(move || match listener.accept() {
            Ok((mut peer, _)) => script(&mut peer),
            Err(e) => panic!("accept failed: {e}"),
        })
    }

    fn welcome(proto_version: u32, config_path: &str, server_pid: u32) -> ServerMsg {
        ServerMsg::Welcome {
            proto_version,
            config_path: config_path.to_string(),
            server_pid,
            snapshot: Snapshot::from_views(7, 1_700_000_000_000, 1, false, &[]),
        }
    }

    #[test]
    fn a_welcome_completes_the_handshake_and_honours_the_hello() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");
        let config_text = config.display().to_string();
        let (endpoint, listener) = listening(dir.path());
        let asked = config_text.clone();
        let handle = serve(listener, move |peer| {
            let hello = crate::ipc::read_client_msg(peer).expect("hello").expect("a frame");
            match hello {
                ClientMsg::Hello { proto_version, config_path, host_key_policy } => {
                    assert_eq!(proto_version, PROTO_VERSION);
                    assert_eq!(config_path, asked);
                    // The escape hatch must travel with the handshake.
                    assert_eq!(host_key_policy.as_deref(), Some("ignore"));
                }
                other => panic!("expected Hello, got {other:?}"),
            }
            crate::ipc::write_message(peer, &welcome(PROTO_VERSION, &asked, 4242)).expect("welcome");
        });
        let socket = connect(&endpoint).expect("connect");
        let session = handshake(socket, &config_text, HostKeyPolicy::Ignore).expect("handshake");
        assert_eq!(session.server_pid, 4242);
        assert_eq!(session.config_path, config_text);
        assert!(matches!(session.welcome, ServerMsg::Welcome { .. }));
        // The reader thread needs a blocking socket: the handshake timeout must
        // not survive into the session.
        assert_eq!(session.socket.read_timeout().expect("readable timeout"), None);
        handle.join().expect("server thread");
    }

    #[test]
    fn a_protocol_version_mismatch_is_refused_with_the_restart_instruction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_text = dir.path().join("config.toml").display().to_string();
        let (_endpoint, listener) = listening(dir.path());
        let asked = config_text.clone();
        let handle = serve(listener, move |peer| {
            let _ = crate::ipc::read_client_msg(peer);
            crate::ipc::write_message(peer, &welcome(PROTO_VERSION + 1, &asked, 1)).expect("welcome");
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let err = handshake(socket, &config_text, HostKeyPolicy::AcceptNew).expect_err("must be refused");
        assert!(err.zh.contains("重新启动"), "{}", err.zh);
        assert!(err.en.contains("start again"), "{}", err.en);
        handle.join().expect("server thread");
    }

    #[test]
    fn a_config_mismatch_names_both_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_text = dir.path().join("config.toml").display().to_string();
        let (_endpoint, listener) = listening(dir.path());
        let handle = serve(listener, move |peer| {
            let _ = crate::ipc::read_client_msg(peer);
            crate::ipc::write_message(peer, &welcome(PROTO_VERSION, "/somewhere/else.toml", 1)).expect("welcome");
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let err = handshake(socket, &config_text, HostKeyPolicy::AcceptNew).expect_err("must be refused");
        assert!(err.zh.contains("/somewhere/else.toml") && err.zh.contains(&config_text), "{}", err.zh);
        handle.join().expect("server thread");
    }

    #[test]
    fn a_server_error_is_surfaced_verbatim_and_bilingually() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_text = dir.path().join("config.toml").display().to_string();
        let (_endpoint, listener) = listening(dir.path());
        let handle = serve(listener, |peer| {
            let _ = crate::ipc::read_client_msg(peer);
            let refusal = ServerMsg::Error {
                code: "proto_version_mismatch".into(),
                error: crate::ipc::WireError {
                    code: "proto_version_mismatch".into(),
                    zh: "协议版本不一致。请退出所有 sshpf 后重新启动。".into(),
                    en: "Protocol version mismatch. Quit every sshpf instance, then start again.".into(),
                },
            };
            crate::ipc::write_message(peer, &refusal).expect("error frame");
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let err = handshake(socket, &config_text, HostKeyPolicy::AcceptNew).expect_err("must be refused");
        assert_eq!(err.zh, "协议版本不一致。请退出所有 sshpf 后重新启动。");
        assert_eq!(err.en, "Protocol version mismatch. Quit every sshpf instance, then start again.");
        handle.join().expect("server thread");
    }

    #[test]
    fn any_other_first_frame_is_reported_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_text = dir.path().join("config.toml").display().to_string();
        let (_endpoint, listener) = listening(dir.path());
        let handle = serve(listener, |peer| {
            let _ = crate::ipc::read_client_msg(peer);
            let snapshot = Snapshot::from_views(1, 0, 1, false, &[]);
            crate::ipc::write_message(peer, &ServerMsg::Snapshot(snapshot)).expect("snapshot frame");
        });
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let err = handshake(socket, &config_text, HostKeyPolicy::AcceptNew).expect_err("must be refused");
        assert!(err.zh.contains("snapshot"), "{}", err.zh);
        assert!(err.en.contains("snapshot"), "{}", err.en);
        handle.join().expect("server thread");
    }

    #[test]
    fn a_silent_server_times_out_instead_of_hanging_forever() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_text = dir.path().join("config.toml").display().to_string();
        let (_endpoint, listener) = listening(dir.path());
        // Accept and then say nothing at all: the half-open peer.
        let handle = serve(listener, |_peer| std::thread::sleep(Duration::from_millis(600)));
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let started = std::time::Instant::now();
        let err = handshake_within(socket, &config_text, HostKeyPolicy::AcceptNew, Duration::from_millis(120))
            .expect_err("a silent peer must time out");
        assert!(started.elapsed() < Duration::from_millis(500), "took {:?}", started.elapsed());
        // The message must say what happened and what to do, in both languages.
        assert!(err.zh.contains("没有回应"), "{}", err.zh);
        assert!(err.en.contains("did not answer"), "{}", err.en);
        assert!(err.zh.contains("--server") && err.en.contains("--server"), "{}", err.zh);
        handle.join().expect("server thread");
    }

    #[test]
    fn an_immediate_close_says_the_server_hung_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_text = dir.path().join("config.toml").display().to_string();
        let (_endpoint, listener) = listening(dir.path());
        let handle = serve(listener, |_peer| {});
        let socket = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
        let err = handshake(socket, &config_text, HostKeyPolicy::AcceptNew).expect_err("must be refused");
        assert!(err.zh.contains("关闭了连接"), "{}", err.zh);
        assert!(err.en.contains("closed the connection"), "{}", err.en);
        handle.join().expect("server thread");
    }

    #[test]
    fn a_listener_that_never_answers_leaves_the_error_for_the_retry_loop() {
        // `connect` itself must not consume the handshake budget: the election
        // loop decides how long to keep trying, so the error stays a plain io one.
        let dir = tempfile::tempdir().expect("tempdir");
        let err = connect(&Endpoint {
            config_path: dir.path().join("config.toml"),
            socket_path: dir.path().join("nothing.sock"),
            lock_path: dir.path().join("nothing.lock"),
            dir: dir.path().to_path_buf(),
        })
        .expect_err("no listener");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}

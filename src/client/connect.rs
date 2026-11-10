//! Attaching to a server: discovery, dial, and the `Hello`/`Welcome` handshake.
//!
//! # What an "attachment" is
//!
//! A client does not own tunnels; it holds a socket to the process that does. This
//! file turns an [`Endpoint`] into a [`Connection`] — a socket plus the `Welcome`
//! the server answered — and nothing more. Moving bytes is [`super::link`].
//!
//! # Why the handshake is strict
//!
//! [`ipc::check_version`] and [`ipc::check_config`] are refusals, not warnings, and
//! they run **before** the caller gets a `Connection`: a peer at another protocol
//! version would misread every later frame, and a collided config would put another
//! user's tunnels in your table. Both are fixable by the user (restart everything),
//! so the error carries their words rather than a code.
//!
//! # Why "no server" is its own error variant
//!
//! It is the only failure that starting a server can fix. Everything else means a
//! server *is* running, and spawning a second one would either lose an election or
//! bind a second copy of the same ports. [`AttachError::may_spawn`] is where that
//! decision is made once, instead of at every call site.
//!
//! Belongs here: reading the endpoint files, dialling, and the greeting.
//! Does not belong: moving frames ([`super::link`]), reconnecting
//! ([`super::reconnect`]), what a frame means to the UI ([`super::sync`]), or
//! starting a server ([`super::spawn`]).

use std::io::{self};
use std::net::TcpStream;
use std::time::Duration;

use crate::eventlog;
use crate::help;
use crate::ipc::{self, ClientMsg, Endpoint, PROTO_VERSION, ServerMsg};

/// How long the server may take to answer `Hello`.
///
/// The mirror image of the server's own handshake timeout: neither side may be
/// left hanging by a peer that connected and then said nothing.
pub const GREETING_TIMEOUT: Duration = ipc::HANDSHAKE_TIMEOUT;

/// How long one blocking write may take before the socket is treated as dead.
///
/// Loopback writes land immediately, so this only bounds the pathological case (a
/// peer that stopped reading). Short, because nobody is waiting on it.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// A socket that has already answered `Hello` with a `Welcome`.
///
/// The greeting is the only state kept: its `server_pid` and `config_path` are what
/// a caller would want to read, and holding copies beside it would be two sources of
/// truth for facts that arrive together.
#[derive(Debug)]
pub struct Connection {
    /// The socket, positioned immediately after the `Welcome` frame.
    pub(crate) stream: TcpStream,
    /// The `Welcome` frame itself.
    ///
    /// Kept rather than consumed so the link thread hands it to the UI through the
    /// same channel as every later frame: one path, one ordering, no special case
    /// for "the first snapshot".
    pub(crate) welcome: ServerMsg,
}

impl Connection {
    /// Which server process answered.
    pub fn server_pid(&self) -> Option<u32> {
        match &self.welcome {
            ServerMsg::Welcome { server_pid, .. } => Some(*server_pid),
            _ => None,
        }
    }

    /// The config the server says it serves (already compared with ours).
    pub fn config_path(&self) -> Option<&str> {
        match &self.welcome {
            ServerMsg::Welcome { config_path, .. } => Some(config_path.as_str()),
            _ => None,
        }
    }
}

/// Why an attach attempt did not produce a [`Connection`].
#[derive(Debug)]
pub enum AttachError {
    /// Nothing is listening: a missing discovery file, or a refused connection.
    NoServer(io::Error),
    /// A server answered and refused us, with text the user can act on.
    Rejected(String),
    /// Something is on the port but is not speaking this protocol.
    Protocol(String),
    /// A transport failure that starting a server would not fix.
    Io(io::Error),
}

impl AttachError {
    /// Whether starting a server could plausibly fix this.
    ///
    /// Deliberately narrow: only "nobody is home". A refused token or a version
    /// mismatch means a server *is* running.
    pub fn may_spawn(&self) -> bool {
        matches!(self, AttachError::NoServer(_))
    }

    /// Whether a server was reached at all.
    ///
    /// The reconnect loop uses this to decide whether retrying is pointless: a peer
    /// that answered and rejected us will answer the same way in 200 ms.
    pub fn reached_a_server(&self) -> bool {
        !matches!(self, AttachError::NoServer(_))
    }

    /// The user-facing sentence, bilingual, Chinese first.
    pub fn message(&self) -> String {
        match self {
            AttachError::NoServer(e) => {
                help::bi(&format!("没有正在运行的 sshpf 服务端（{e}）"), &format!("No sshpf server is running ({e})"))
            }
            AttachError::Rejected(text) | AttachError::Protocol(text) => text.clone(),
            AttachError::Io(e) => {
                help::bi(&format!("连接 sshpf 服务端失败：{e}"), &format!("Could not reach the sshpf server: {e}"))
            }
        }
    }

    /// A stable ASCII code, for the log and for the synthesized error frame.
    pub fn code(&self) -> &'static str {
        match self {
            AttachError::NoServer(_) => "no_server",
            AttachError::Rejected(_) => "refused",
            AttachError::Protocol(_) => "bad_protocol",
            AttachError::Io(_) => "io",
        }
    }
}

/// Connect to the server for `endpoint` and complete the handshake.
pub fn attach(endpoint: &Endpoint) -> Result<Connection, AttachError> {
    let asked = endpoint.config_path.to_string_lossy().into_owned();
    let (mut stream, token) = open(endpoint)?;
    let answer = greet(&mut stream, &asked, &token)?;
    let welcome = accept_welcome(answer, &asked)?;
    // The greeting timeout must go: it bounded the handshake, and leaving it on
    // would abort the stream mid-frame the first time the server went quiet for
    // five seconds. A partial frame cannot be resumed, so this is not cosmetic.
    stream.set_read_timeout(None).map_err(AttachError::Io)?;
    if let ServerMsg::Welcome { server_pid, config_path, .. } = &welcome {
        eventlog::client_connected(*server_pid, config_path);
    }
    Ok(Connection { stream, welcome })
}

/// Read the discovered token and port, then dial.
fn open(endpoint: &Endpoint) -> Result<(TcpStream, String), AttachError> {
    // The token is read first: a port file with no token beside it is a leftover
    // from a dead server, and dialling it would only produce a refusal that hides
    // the real cause.
    let token = ipc::read_token(endpoint).map_err(|e| {
        AttachError::NoServer(io::Error::new(e.kind(), format!("{}: {e}", endpoint.token_path.display())))
    })?;
    let stream = ipc::connect(endpoint).map_err(classify)?;
    Ok((stream, token))
}

/// Send `Hello` and read the server's first frame.
fn greet(stream: &mut TcpStream, asked: &str, token: &str) -> Result<ServerMsg, AttachError> {
    stream.set_read_timeout(Some(GREETING_TIMEOUT)).map_err(AttachError::Io)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).map_err(AttachError::Io)?;
    let hello =
        ClientMsg::Hello { proto_version: PROTO_VERSION, config_path: asked.to_string(), token: token.to_string() };
    ipc::write_message(stream, &hello).map_err(AttachError::Io)?;
    match ipc::read_server_msg(stream) {
        Ok(Some(msg)) => Ok(msg),
        Ok(None) => Err(AttachError::Protocol(help::bi(
            "服务端在握手时关闭了连接：请退出所有 sshpf 后重新启动。",
            "The server closed the connection during the handshake: quit every sshpf instance, then start again.",
        ))),
        Err(e) => Err(AttachError::Rejected(help::bi(
            &format!("与 sshpf 服务端握手失败：{e}"),
            &format!("The handshake with the sshpf server failed: {e}"),
        ))),
    }
}

/// Check the first frame is a compatible `Welcome`, or say why it is not.
fn accept_welcome(answer: ServerMsg, asked: &str) -> Result<ServerMsg, AttachError> {
    match answer {
        ServerMsg::Welcome { proto_version, ref config_path, .. } => {
            ipc::check_version(proto_version).map_err(|e| reject(&e))?;
            ipc::check_config(config_path, asked).map_err(|e| reject(&e))?;
            Ok(answer)
        }
        ServerMsg::Error { code, error } => {
            crate::logging::warn("client.server_error", &[("code", &code), ("msg", &error.en)]);
            Err(AttachError::Rejected(help::bi(&error.zh, &error.en)))
        }
        other => Err(AttachError::Protocol(help::bi(
            &format!("服务端的第一条消息不是欢迎帧：{other:?}"),
            "The server's first message was not a welcome frame",
        ))),
    }
}

/// Turn a protocol error into a user-facing refusal.
fn reject(e: &ipc::ProtocolError) -> AttachError {
    AttachError::Rejected(help::bi(&e.error.zh, &e.error.en))
}

/// Classify a discovery/connect failure into "nobody home" or a real failure.
fn classify(e: io::Error) -> AttachError {
    match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => AttachError::NoServer(e),
        _ => AttachError::Io(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Snapshot;
    use std::path::Path;
    use std::thread;

    /// An endpoint whose discovery files live in a temp dir, so a test never
    /// touches the real per-user directory.
    pub(crate) fn endpoint_in(dir: &Path, tag: &str) -> Endpoint {
        let mut endpoint = ipc::endpoint_for(dir.join("config.toml").as_path());
        endpoint.dir = dir.to_path_buf();
        endpoint.port_path = dir.join(format!("{tag}.port"));
        endpoint.token_path = dir.join(format!("{tag}.token"));
        endpoint.lock_path = dir.join(format!("{tag}.lock"));
        endpoint
    }

    fn welcome_for(endpoint: &Endpoint, proto: u32) -> ServerMsg {
        ServerMsg::Welcome {
            proto_version: proto,
            config_path: endpoint.config_path.to_string_lossy().into_owned(),
            server_pid: 4242,
            snapshot: Snapshot::from_views(1, 0, 1, false, &[]),
        }
    }

    #[test]
    fn attaching_without_an_endpoint_fails_cleanly_and_may_spawn() {
        // The first-run path: no server has ever been started for this config.
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "cold");
        let err = attach(&endpoint).expect_err("no endpoint files means no server");
        assert!(err.may_spawn(), "a missing server is exactly when spawning helps");
        assert!(!err.reached_a_server());
        assert_eq!(err.code(), "no_server");
        let text = err.message();
        assert!(text.contains("没有正在运行的 sshpf 服务端"), "{text}");
        assert!(text.contains("No sshpf server is running"), "{text}");
    }

    #[test]
    fn a_port_file_without_a_token_file_reads_as_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "half");
        std::fs::write(&endpoint.port_path, "12345").unwrap();
        let err = attach(&endpoint).expect_err("a token is required");
        assert!(err.may_spawn(), "a half-written endpoint is a stale one: spawning is right");
    }

    #[test]
    fn a_real_loopback_round_trip_delivers_the_welcome() {
        // The whole handshake against a hand-written server: no binary, no spawn.
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "live");
        let (listener, port) = ipc::bind_loopback().expect("loopback is bindable");
        ipc::write_port(&endpoint, port).expect("publish the port");
        ipc::write_token(&endpoint, "secret-token").expect("publish the token");
        let asked = endpoint.config_path.to_string_lossy().into_owned();
        let greet_welcome = welcome_for(&endpoint, PROTO_VERSION);

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("the client must arrive");
            let hello = ipc::read_client_msg(&mut stream).expect("a hello").expect("a frame");
            ipc::write_message(&mut stream, &greet_welcome).expect("write the welcome");
            hello
        });

        let connection = attach(&endpoint).expect("the handshake must succeed");
        assert_eq!(connection.server_pid(), Some(4242));
        assert_eq!(connection.config_path(), Some(asked.as_str()));
        assert!(matches!(connection.welcome, ServerMsg::Welcome { .. }));
        match server.join().expect("the server thread must finish") {
            ClientMsg::Hello { proto_version, config_path, token } => {
                assert_eq!(proto_version, PROTO_VERSION);
                assert_eq!(config_path, asked);
                assert_eq!(token, "secret-token", "the token from the discovery file must be presented");
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn a_version_mismatch_is_refused_with_actionable_text() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "version");
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "t").expect("token");
        let too_new = welcome_for(&endpoint, PROTO_VERSION + 1);
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            let _ = ipc::write_message(&mut stream, &too_new);
        });
        let err = attach(&endpoint).expect_err("a mismatched version must be refused");
        assert!(!err.may_spawn(), "a server is running; spawning another cannot help");
        let text = err.message();
        assert!(text.contains("重新启动"), "the user needs an instruction, not a code: {text}");
    }

    #[test]
    fn a_server_that_refuses_us_is_reported_not_answered_with_a_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "refused");
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "wrong").expect("token");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            let denial = ServerMsg::Error {
                code: "bad_token".into(),
                error: ipc::WireError {
                    code: "bad_token".into(),
                    zh: "服务端拒绝了这次连接：访问令牌不匹配。".into(),
                    en: "the access token did not match".into(),
                },
            };
            let _ = ipc::write_message(&mut stream, &denial);
        });
        let err = attach(&endpoint).expect_err("a refusal is an error");
        assert!(err.reached_a_server());
        assert!(!err.may_spawn());
        assert!(err.message().contains("访问令牌"), "{}", err.message());
    }

    #[test]
    fn a_first_frame_that_is_not_a_welcome_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "garbage");
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "t").expect("token");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            let _ = ipc::write_message(&mut stream, &ServerMsg::Pong { server_now_ms: 0 });
        });
        let err = attach(&endpoint).expect_err("a pong is not a greeting");
        assert!(matches!(err, AttachError::Protocol(_)), "{err:?}");
    }

    #[test]
    fn a_server_that_hangs_up_after_the_hello_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "hangup");
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "t").expect("token");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            // Close without answering.
        });
        let err = attach(&endpoint).expect_err("no welcome means no connection");
        assert!(!err.may_spawn());
        assert!(err.message().contains("握手"), "{}", err.message());
    }

    #[test]
    fn a_peer_that_answers_a_different_config_is_refused() {
        // The endpoint-name collision guard: one string comparison, and someone
        // else's tunnels never reach this table.
        let dir = tempfile::tempdir().unwrap();
        let endpoint = endpoint_in(dir.path(), "collision");
        let (listener, port) = ipc::bind_loopback().expect("bind");
        ipc::write_port(&endpoint, port).expect("port");
        ipc::write_token(&endpoint, "t").expect("token");
        let wrong = ServerMsg::Welcome {
            proto_version: PROTO_VERSION,
            config_path: "/somewhere/else/config.toml".into(),
            server_pid: 1,
            snapshot: Snapshot::default(),
        };
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = ipc::read_client_msg(&mut stream);
            let _ = ipc::write_message(&mut stream, &wrong);
        });
        let err = attach(&endpoint).expect_err("a config mismatch must be refused");
        assert!(err.message().contains("/somewhere/else/config.toml"), "{}", err.message());
    }
}

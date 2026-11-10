//! Encoding, decoding, and version negotiation.
//!
//! # Why TOML
//!
//! The crate already depends on `toml` and `serde`, and adding a JSON dependency
//! is forbidden. TOML round-trips these messages losslessly as long as a few
//! rules are respected (no nested `Option`, never hand-build text, no
//! `deny_unknown_fields`), which [`super::msg`] and [`super::snapshot`] are
//! written to satisfy.
//!
//! # Decoding is untrusted input
//!
//! Everything arriving on the socket comes from another process, so a malformed
//! payload must produce an error — never a panic. `toml::from_str` returns `Err`
//! for every malformed input tried (empty, bad bytes, wrong types, unknown
//! tags), which is what makes the "no `unwrap` in production code" rule
//! satisfiable here.
//!
//! Belongs here: bytes ⇄ message, and the errors a mismatch produces.
//! Does not belong: framing (`frame`), the message definitions (`msg`), or the
//! socket path (`endpoint`).

use std::io;

use super::frame::{read_frame, write_frame};
use super::msg::{ClientMsg, PROTO_VERSION, ServerMsg, WireError};

/// A protocol-level failure, already classified for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    /// Stable ASCII code, e.g. `proto_version_mismatch`.
    pub code: &'static str,
    /// The message to show the user, in both languages.
    pub error: WireError,
}

impl ProtocolError {
    /// Build a bilingual error.
    pub fn new(code: &'static str, zh: impl Into<String>, en: impl Into<String>) -> ProtocolError {
        ProtocolError { code, error: WireError { code: code.to_string(), zh: zh.into(), en: en.into() } }
    }

    /// Convert into an I/O error, so a caller deep in the framing layer can
    /// propagate it through `?` without a second error type.
    pub fn into_io(self) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, format!("{}: {}", self.code, self.error.en))
    }
}

/// Encode one message and write it as a single frame.
pub fn write_message<W: io::Write, M: serde::Serialize>(w: &mut W, msg: &M) -> io::Result<()> {
    let payload = toml::to_string(msg).map_err(|e| io_err("cannot encode message", &e))?;
    write_frame(w, &payload)
}

/// Read one frame and decode it.
///
/// `Ok(None)` is a clean end of stream. A malformed payload is an error carrying
/// a [`WireError`], so the caller can tell the peer *why* rather than dropping
/// the connection silently.
pub fn read_client_msg<R: io::Read>(r: &mut R) -> io::Result<Option<ClientMsg>> {
    decode(read_frame(r)?)
}

/// Read one frame and decode it as a server message.
pub fn read_server_msg<R: io::Read>(r: &mut R) -> io::Result<Option<ServerMsg>> {
    decode(read_frame(r)?)
}

/// Decode a payload, turning a TOML error into a reportable protocol error.
fn decode<T: serde::de::DeserializeOwned>(payload: Option<String>) -> io::Result<Option<T>> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    toml::from_str(&payload).map(Some).map_err(|e| {
        // The detail goes into the log; the user-facing half stays short and
        // points at the fix (a truncated or corrupt frame means a version or
        // transport problem, not something they can edit).
        ProtocolError::new(
            "bad_frame",
            "收到无法解析的消息：双方版本可能不一致，请退出所有 sshpf 后重新启动。",
            "Received a message that could not be decoded: the two sides may be \
             at different versions. Quit every sshpf instance, then start again.",
        )
        .into_io_with(&e)
    })
}

impl ProtocolError {
    /// Attach parser detail while keeping the stable code and the user-facing
    /// text intact.
    fn into_io_with(self, detail: &toml::de::Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, format!("{}: {detail}", self.code))
    }
}

/// Check a peer's protocol version, producing an actionable error on mismatch.
///
/// There is no compatibility mode on purpose: client and server are the same
/// binary on the same machine, so a mismatch only happens while an old process
/// is still running after an upgrade. Refusing with a clear instruction is safer
/// than half-working, which produces "some actions silently do nothing".
pub fn check_version(peer: u32) -> Result<(), ProtocolError> {
    if peer == PROTO_VERSION {
        return Ok(());
    }
    Err(ProtocolError::new(
        "proto_version_mismatch",
        format!("协议版本不一致（服务端 v{PROTO_VERSION}，客户端 v{peer}）。请退出所有 sshpf 后重新启动。"),
        format!(
            "Protocol version mismatch (server v{PROTO_VERSION}, client v{peer}). \
             Quit every sshpf instance, then start again."
        ),
    ))
}

/// Check that the client asked for the config this server actually serves.
///
/// The endpoint name is derived from the config path, so two different configs
/// could in principle collide on one endpoint. Comparing the real path turns that
/// from "someone else's tunnels appear in your table" into an error.
pub fn check_config(expected: &str, asked: &str) -> Result<(), ProtocolError> {
    if expected == asked {
        return Ok(());
    }
    Err(ProtocolError::new(
        "server_config_mismatch",
        format!("这个服务端提供的是配置 {expected}，而客户端请求的是 {asked}。"),
        format!("This server serves {expected}, but the client asked for {asked}."),
    ))
}

/// Check the token a client presented against the one this server publishes.
///
/// A loopback port is reachable by every local user, so this — not the socket —
/// is the permission boundary. A wrong token is refused before any state is sent,
/// and the message deliberately does not echo either token: the log is written to
/// disk and must not become a copy of the secret.
pub fn check_token(expected: &str, presented: &str) -> Result<(), ProtocolError> {
    // Constant-time comparison would matter if the tokens were short-lived
    // secrets facing a remote attacker; here both sides are local processes and
    // the token never leaves the machine, so a plain equality is the honest
    // choice: it does not claim a protection it does not need.
    if expected == presented && !expected.is_empty() {
        return Ok(());
    }
    Err(ProtocolError::new(
        "bad_token",
        "服务端拒绝了这次连接：访问令牌不匹配。请退出所有 sshpf 后重新启动。",
        "The server refused this connection: the access token did not match. \
         Quit every sshpf instance, then start again.",
    ))
}

/// Wrap a TOML error into an `io::Error` without losing the kind.
fn io_err(what: &str, e: &toml::ser::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::msg::Op;
    use std::io::Cursor;

    fn encode(msg: &ClientMsg) -> Vec<u8> {
        let mut buf = Vec::new();
        write_message(&mut buf, msg).expect("encoding into a Vec cannot fail");
        buf
    }

    #[test]
    fn a_message_survives_the_full_frame_path() {
        let msg = ClientMsg::Command { req_id: 11, expect_rev: Some(3), op: Op::Start { tunnel_id: 2 } };
        let buf = encode(&msg);
        let back = read_client_msg(&mut Cursor::new(buf)).expect("valid frame");
        assert_eq!(back, Some(msg));
    }

    #[test]
    fn a_clean_close_reads_as_none() {
        let mut empty: &[u8] = &[];
        assert_eq!(read_client_msg(&mut empty).expect("clean close"), None);
    }

    #[test]
    fn a_malformed_payload_is_an_error_not_a_panic() {
        // A length-prefixed payload that is valid UTF-8 but not valid TOML.
        let mut buf = Vec::new();
        let payload = "this is not toml {{{";
        buf.extend_from_slice(&u32::try_from(payload.len()).expect("fits").to_be_bytes());
        buf.extend_from_slice(payload.as_bytes());
        let err = read_client_msg(&mut Cursor::new(buf)).expect_err("must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("bad_frame"), "{err}");
    }

    #[test]
    fn an_empty_payload_is_rejected_by_the_frame_layer() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_be_bytes());
        assert!(read_client_msg(&mut Cursor::new(buf)).is_err());
    }

    #[test]
    fn a_matching_version_is_accepted() {
        assert_eq!(check_version(PROTO_VERSION), Ok(()));
    }

    #[test]
    fn a_version_mismatch_says_what_to_do() {
        let err = check_version(PROTO_VERSION + 1).expect_err("must be rejected");
        assert_eq!(err.code, "proto_version_mismatch");
        // Actionable: both halves must tell the user to restart, not just say
        // "invalid".
        assert!(err.error.zh.contains("重新启动"), "{}", err.error.zh);
        assert!(err.error.en.contains("start again"), "{}", err.error.en);
        // Bilingual means both are non-empty.
        assert!(!err.error.zh.is_empty() && !err.error.en.is_empty());
    }

    #[test]
    fn a_config_mismatch_is_reported_rather_than_served() {
        let err = check_config("/a/config.toml", "/b/config.toml").expect_err("must be rejected");
        assert_eq!(err.code, "server_config_mismatch");
        assert!(err.error.zh.contains("/a/config.toml") && err.error.zh.contains("/b/config.toml"));
    }

    #[test]
    fn a_matching_config_is_accepted() {
        assert_eq!(check_config("/a", "/a"), Ok(()));
    }

    #[test]
    fn a_matching_token_is_accepted() {
        assert_eq!(check_token("abc123", "abc123"), Ok(()));
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let err = check_token("abc123", "abc124").expect_err("a wrong token must be refused");
        assert_eq!(err.code, "bad_token");
    }

    #[test]
    fn an_empty_published_token_never_authorises_anyone() {
        // If the token file were somehow empty, `"" == ""` must not read as "the
        // client is welcome": that would silently disable the only local boundary.
        assert!(check_token("", "").is_err(), "an empty token must authorise nobody");
    }

    #[test]
    fn the_token_error_does_not_echo_the_secret() {
        let err = check_token("good-secret", "bad-secret").expect_err("must be refused");
        assert!(!err.error.zh.contains("good-secret"), "{}", err.error.zh);
        assert!(!err.error.en.contains("good-secret"), "{}", err.error.en);
        assert!(!err.error.en.contains("bad-secret"), "{}", err.error.en);
    }

    #[test]
    fn every_bad_payload_is_an_error_never_a_panic() {
        // The socket carries untrusted bytes; none of these may take the process
        // down. Each is a plausible kind of corruption or version skew.
        let bad: &[&[u8]] = &[
            b"",
            b"\x00",
            b"t = ",
            b"t = \"nonexistent_variant\"\n",
            b"t = 42\n",
            b"t = \"command\"\n",                    // missing req_id and op
            b"t = \"command\"\nreq_id = \"text\"\n", // wrong type
            b"[op]\nop = \"start\"\n",               // missing tag
            b"\xff\xfe\x00\x01",                     // not UTF-8
        ];
        for payload in bad {
            let mut frame = Vec::new();
            let len = u32::try_from(payload.len()).expect("test payload fits");
            frame.extend_from_slice(&len.to_be_bytes());
            frame.extend_from_slice(payload);
            // Either a decode error or a frame error: both are fine, a panic is not.
            let result = read_client_msg(&mut Cursor::new(frame));
            assert!(result.is_err(), "payload {payload:?} should not decode as a Command");
        }
    }

    #[test]
    fn a_server_message_survives_the_frame_path() {
        let msg = ServerMsg::Stale { req_id: 5, rev: 9 };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("must encode");
        let back = read_server_msg(&mut Cursor::new(buf)).expect("must decode");
        assert_eq!(back, Some(msg));
    }
}

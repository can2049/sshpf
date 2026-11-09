//! The protocol vocabulary: one enum per direction, plus the command/answer
//! pairs that make up a request.
//!
//! # Shape
//!
//! Both enums are internally tagged (`#[serde(tag = "t")]`) so a frame's payload
//! is one flat table that names itself. That was verified against the real
//! serializer: a tagged enum encodes directly at the document root, so no
//! wrapper level is needed.
//!
//! # Two rules that are easy to get wrong
//!
//! 1. **`Option` is used for at most one level.** TOML has no `null`, and a
//!    nested `Option<Option<_>>` is lossy (both `Some(None)` and `None` encode to
//!    nothing). A field is either present or absent.
//! 2. **No `deny_unknown_fields`.** An unknown field must be ignored, so that a
//!    newer peer adding one does not break an older peer.
//!
//! Belongs here: the message and command definitions, and the protocol version.
//! Does not belong: encoding/decoding (`codec`), framing (`frame`), or what a
//! snapshot contains (`snapshot`).

use super::snapshot::Snapshot;

/// Protocol version carried by the handshake.
///
/// Bumped whenever a change would make an older peer misinterpret a frame. Not a
/// compatibility matrix: the client and the server are the same binary on the
/// same machine, so a mismatch only happens mid-upgrade and is refused outright.
pub const PROTO_VERSION: u32 = 1;

/// Client → server.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t")]
pub enum ClientMsg {
    /// Must be the first frame on a connection.
    #[serde(rename = "hello")]
    Hello {
        /// The client's protocol version.
        proto_version: u32,
        /// Which config the client believes it is attaching to, so a hash
        /// collision between two configurations is detected rather than served.
        config_path: String,
        /// The host-key policy this client was started with
        /// (`--ignore-host-key-changes`), as a stable token.
        ///
        /// Optional and additive: an older client omits it and the server keeps
        /// its own policy. It exists so a client whose flag cannot be honoured
        /// can be *told* that, instead of silently getting the server's stricter
        /// or looser behaviour. It cannot change the policy of a running server —
        /// tunnels are dialled with one policy per server, and letting one client
        /// loosen trust for everyone else's connections would be a security
        /// defect, not a feature.
        #[serde(default)]
        host_key_policy: Option<String>,
    },
    /// A state-changing request.
    #[serde(rename = "command")]
    Command {
        /// Client-assigned, unique within this connection; echoes back on the
        /// answer so a caller can match the two.
        req_id: u64,
        /// Optimistic concurrency: the revision the user was looking at.
        /// A mismatch yields [`ServerMsg::Stale`] rather than a silent overwrite.
        #[serde(default)]
        expect_rev: Option<u64>,
        /// What to do.
        op: Op,
    },
    /// A polite goodbye.
    #[serde(rename = "bye")]
    Bye {
        /// The client showed the user "quitting stops every forward" and they
        /// agreed. This is a **proposal**: the server recounts before acting, so
        /// a stale view can never stop someone else's forwards.
        last_exit: bool,
    },
    /// Adjust how log lines are delivered for one tunnel.
    #[serde(rename = "subscribe_logs")]
    SubscribeLogs {
        /// Which tunnel, or `None` for process-level lines.
        #[serde(default)]
        tunnel_id: Option<u64>,
        /// Only send lines at or above this level (`debug`/`info`/`warn`/`error`).
        min_level: String,
    },
}

/// The single request carried by [`ClientMsg::Command`].
///
/// Field names are part of the wire format: renaming one is a breaking change,
/// so this enum may only grow.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    #[serde(rename = "start")]
    Start { tunnel_id: u64 },
    #[serde(rename = "stop")]
    Stop { tunnel_id: u64 },
    #[serde(rename = "restart")]
    Restart { tunnel_id: u64 },
    #[serde(rename = "delete")]
    Delete { tunnel_id: u64 },
    /// Start every stopped tunnel.
    #[serde(rename = "start_all")]
    StartAll,
    /// Stop every running tunnel — the "one client can stop everything" command.
    #[serde(rename = "stop_all")]
    StopAll,
    /// Create a tunnel. The server assigns the id; any id in `cfg` is ignored.
    #[serde(rename = "create")]
    Create { cfg: Box<crate::model::TunnelConfig> },
    /// Replace an existing tunnel's definition.
    #[serde(rename = "update")]
    Update { tunnel_id: u64, cfg: Box<crate::model::TunnelConfig> },
    /// Ask the peer's SSH session which ports are listening; answered separately
    /// because a scan can take seconds and must not block the command path.
    #[serde(rename = "port_scan")]
    PortScan { tunnel_id: u64 },
}

/// Server → client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t")]
pub enum ServerMsg {
    /// Accepted; carries the protocol version and the first snapshot, so the
    /// client can render immediately without another round trip.
    #[serde(rename = "welcome")]
    Welcome {
        proto_version: u32,
        /// The server's real, resolved config path.
        config_path: String,
        /// Which server process is answering, for diagnostics.
        server_pid: u32,
        snapshot: Snapshot,
    },
    /// The authoritative state, at `snapshot.revision`.
    #[serde(rename = "snapshot")]
    Snapshot(Snapshot),
    /// Answer to one command.
    #[serde(rename = "reply")]
    Reply {
        req_id: u64,
        ok: bool,
        /// The revision after applying, or the unchanged one on failure.
        rev: u64,
        /// Why it failed, when `ok` is false.
        #[serde(default)]
        error: Option<WireError>,
        /// Clients still attached after this answer; set when answering a `Bye`.
        /// Reusing `Reply` for that avoids inventing a message for one field.
        #[serde(default)]
        remaining: Option<u32>,
        /// Scan results, for `PortScan`.
        #[serde(default)]
        ports: Vec<WirePort>,
    },
    /// `expect_rev` did not match. Normal control flow, not an error: the client
    /// refreshes and lets the user decide whether to retry.
    #[serde(rename = "stale")]
    Stale { req_id: u64, rev: u64 },
    /// New log lines for one tunnel.
    #[serde(rename = "log")]
    Log {
        #[serde(default)]
        tunnel_id: Option<u64>,
        /// Lines dropped before these ones because a queue was full.
        #[serde(default)]
        dropped: u32,
        lines: Vec<super::snapshot::WireLogLine>,
    },
    /// A protocol-level problem. Usually followed by a disconnect.
    #[serde(rename = "error")]
    Error { code: String, error: WireError },
}

/// A failure a peer can act on: stable ASCII code plus text in both languages.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WireError {
    /// Stable ASCII code, e.g. `stale` / `not_found` / `invalid_config`.
    pub code: String,
    /// Chinese explanation, matching `en` in information content.
    pub zh: String,
    /// English explanation.
    pub en: String,
}

/// One listening port reported by a scan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WirePort {
    pub port: u16,
    /// `tcp` / `udp`.
    pub proto: String,
    /// The process name, when the peer could tell.
    #[serde(default)]
    pub process: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: &ClientMsg) -> ClientMsg {
        let text = toml::to_string(msg).expect("a client message must encode");
        toml::from_str(&text).expect("a client message must decode")
    }

    fn round_trip_server(msg: &ServerMsg) -> ServerMsg {
        let text = toml::to_string(msg).expect("a server message must encode");
        toml::from_str(&text).expect("a server message must decode")
    }

    fn sample_cfg() -> crate::model::TunnelConfig {
        crate::model::TunnelConfig {
            id: 1,
            name: "web".into(),
            kind: crate::model::ForwardKind::Local,
            ssh_host: "host".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "127.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: true,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        }
    }

    #[test]
    fn every_client_message_round_trips() {
        let messages = [
            ClientMsg::Hello {
                proto_version: PROTO_VERSION,
                config_path: "/tmp/config.toml".into(),
                host_key_policy: None,
            },
            ClientMsg::Hello {
                proto_version: PROTO_VERSION,
                config_path: "/tmp/config.toml".into(),
                host_key_policy: Some("ignore".into()),
            },
            ClientMsg::Bye { last_exit: true },
            ClientMsg::Bye { last_exit: false },
            ClientMsg::Command { req_id: 1, expect_rev: Some(42), op: Op::Start { tunnel_id: 7 } },
            // `None` must survive as "field absent", which is how TOML says None.
            ClientMsg::Command { req_id: 2, expect_rev: None, op: Op::StopAll },
            ClientMsg::SubscribeLogs { tunnel_id: Some(3), min_level: "warn".into() },
            ClientMsg::SubscribeLogs { tunnel_id: None, min_level: "info".into() },
        ];
        for msg in &messages {
            assert_eq!(&round_trip(msg), msg, "message did not survive the wire: {msg:?}");
        }
    }

    #[test]
    fn every_command_variant_round_trips() {
        let ops = [
            Op::Start { tunnel_id: 1 },
            Op::Stop { tunnel_id: 1 },
            Op::Restart { tunnel_id: 1 },
            Op::Delete { tunnel_id: 1 },
            Op::StartAll,
            Op::StopAll,
            Op::Create { cfg: Box::new(sample_cfg()) },
            Op::Update { tunnel_id: 1, cfg: Box::new(sample_cfg()) },
            Op::PortScan { tunnel_id: 1 },
        ];
        for op in ops {
            let msg = ClientMsg::Command { req_id: 9, expect_rev: None, op: op.clone() };
            assert_eq!(round_trip(&msg), msg, "command did not survive: {op:?}");
        }
    }

    #[test]
    fn a_server_message_round_trips() {
        let snap = crate::ipc::snapshot::Snapshot::from_views(3, 1_700_000_000_000, 1, false, &[]);
        let messages = [
            ServerMsg::Snapshot(snap.clone()),
            ServerMsg::Welcome {
                proto_version: PROTO_VERSION,
                config_path: "/tmp/config.toml".into(),
                server_pid: 4242,
                snapshot: snap,
            },
        ];
        for msg in &messages {
            assert_eq!(&round_trip_server(msg), msg, "server message did not survive: {msg:?}");
        }
    }

    #[test]
    fn a_reply_with_optional_fields_round_trips() {
        let replies = [
            ServerMsg::Reply { req_id: 1, ok: true, rev: 5, error: None, remaining: None, ports: vec![] },
            ServerMsg::Reply {
                req_id: 2,
                ok: false,
                rev: 5,
                error: Some(WireError { code: "not_found".into(), zh: "不存在".into(), en: "not found".into() }),
                remaining: None,
                ports: vec![],
            },
            // The Bye answer reuses Reply, carrying how many clients remain.
            ServerMsg::Reply { req_id: 3, ok: true, rev: 6, error: None, remaining: Some(2), ports: vec![] },
        ];
        for msg in &replies {
            assert_eq!(&round_trip_server(msg), msg);
        }
    }

    #[test]
    fn an_absent_expect_rev_decodes_as_none() {
        // This is the TOML rule the protocol leans on: a missing field is None,
        // because TOML has no null literal.
        let text = "t = \"command\"\nreq_id = 1\n[op]\nop = \"start\"\ntunnel_id = 4\n";
        let msg: ClientMsg = toml::from_str(text).expect("must decode");
        match msg {
            ClientMsg::Command { expect_rev, op, .. } => {
                assert_eq!(expect_rev, None);
                assert_eq!(op, Op::Start { tunnel_id: 4 });
            }
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_field_is_ignored_so_a_newer_peer_does_not_break_an_older_one() {
        // Forward compatibility: a future server adds a field nobody here knows.
        let text = "t = \"hello\"\nproto_version = 1\nconfig_path = \"/x\"\nbrand_new_field = 99\n";
        let msg: ClientMsg = toml::from_str(text).expect("an unknown field must be ignored");
        assert!(matches!(msg, ClientMsg::Hello { .. }));
    }

    #[test]
    fn the_wire_names_are_the_documented_lowercase_tokens() {
        // The `t`/`op`/`s` values are a contract: changing one silently breaks
        // every peer built against the documented vocabulary.
        let text = toml::to_string(&ClientMsg::Command { req_id: 1, expect_rev: None, op: Op::StartAll })
            .expect("must encode");
        assert!(text.contains("t = \"command\""), "{text}");
        assert!(text.contains("op = \"start_all\""), "{text}");
    }
}

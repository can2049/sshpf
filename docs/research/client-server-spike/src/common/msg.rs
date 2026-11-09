//! The message enums exchanged over the socket.
//!
//! What belongs here: the exact vocabulary of the protocol — one enum per
//! direction, internally tagged so a frame is self-describing. What does not:
//! how a message is framed (`frame`) or what a snapshot contains (`wire`).
//!
//! Both enums are internally tagged (`#[serde(tag = ...)]`). That was verified
//! to serialise directly at the TOML document root, which keeps a frame payload
//! to one flat table instead of an extra wrapper level.

use serde::{Deserialize, Serialize};

use super::wire::Snapshot;

/// Client → server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientMsg {
    /// Must be the first frame on a connection.
    #[serde(rename = "hello")]
    Hello { proto_version: u32, client_pid: u32 },
    /// A state-changing request.
    #[serde(rename = "cmd")]
    Command {
        req_id: u64,
        /// Optimistic concurrency: the revision the client believes is current.
        /// A mismatch yields `Stale` rather than a silent overwrite.
        #[serde(default)]
        expect_rev: Option<u64>,
        op: Op,
    },
    /// Polite disconnect.
    #[serde(rename = "bye")]
    Bye {
        /// The client told the user "quitting stops every forward" and they
        /// agreed. This is a **proposal**: the server recounts before acting.
        last_exit: bool,
    },
}

/// The single request in a `Command`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    #[serde(rename = "start")]
    Start { id: u64 },
    #[serde(rename = "stop")]
    Stop { id: u64 },
    #[serde(rename = "stop_all")]
    StopAll { all: bool },
    #[serde(rename = "rename")]
    Rename { id: u64, name: String },
}

/// Server → client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ServerMsg {
    #[serde(rename = "welcome")]
    Welcome { proto_version: u32, server_pid: u32, snapshot: Snapshot },
    #[serde(rename = "snapshot")]
    Snapshot(Snapshot),
    #[serde(rename = "reply")]
    Reply {
        req_id: u64,
        ok: bool,
        /// The revision after applying, so the client can tell whether it needs
        /// to wait for a snapshot.
        rev: u64,
        #[serde(default)]
        error: String,
    },
    /// `expect_rev` did not match. The client must refresh, not replay.
    #[serde(rename = "stale")]
    Stale { req_id: u64, rev: u64 },
    #[serde(rename = "error")]
    Error { code: String, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_client(m: &ClientMsg) {
        let text = toml::to_string(m).unwrap();
        let back: ClientMsg = toml::from_str(&text).unwrap();
        assert_eq!(m, &back, "payload was {text}");
    }

    #[test]
    fn every_client_message_round_trips() {
        round_trip_client(&ClientMsg::Hello { proto_version: 1, client_pid: 42 });
        round_trip_client(&ClientMsg::Bye { last_exit: true });
        round_trip_client(&ClientMsg::Bye { last_exit: false });
        round_trip_client(&ClientMsg::Command { req_id: 1, expect_rev: Some(3), op: Op::Start { id: 7 } });
        // `None` must survive as "field absent", which is how TOML encodes it.
        round_trip_client(&ClientMsg::Command { req_id: 2, expect_rev: None, op: Op::StopAll { all: true } });
    }

    #[test]
    fn a_snapshot_message_round_trips() {
        let m = ServerMsg::Snapshot(Snapshot::sample(9, 3));
        let text = toml::to_string(&m).unwrap();
        let back: ServerMsg = toml::from_str(&text).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn an_absent_expect_rev_decodes_as_none() {
        // This is the TOML rule the protocol depends on: a missing field is
        // `None`, because TOML has no null.
        let text = "t = \"cmd\"\nreq_id = 1\n[op]\nop = \"start\"\nid = 4\n";
        let msg: ClientMsg = toml::from_str(text).unwrap();
        match msg {
            ClientMsg::Command { expect_rev, op, .. } => {
                assert_eq!(expect_rev, None);
                assert_eq!(op, Op::Start { id: 4 });
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}

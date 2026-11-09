//! The wire protocol between a client and a server process: framing, messages,
//! the snapshot they carry, and the endpoint a connection is made to.
//!
//! # What belongs here
//!
//! Only the protocol. This layer knows how to turn bytes into messages and back,
//! where the socket lives for a given config, and how to reject an incompatible
//! peer with an actionable message. It does **not** know what a tunnel is, does
//! not open a `Supervisor`, and never writes a config file.
//!
//! # Why it is a separate layer
//!
//! Both `server` and `client` depend on this, in opposite directions. If it
//! imported either of them (or `app`, or `tunnel`) the dependency graph would
//! grow a cycle, and the protocol could no longer be tested on its own. The rules
//! it follows are therefore:
//!
//! * **no `crate::app`, `crate::ui`, `crate::tunnel`, `crate::server`,
//!   `crate::client`** — only `model`, `text`, `logging` and the leaves;
//! * **no I/O beyond the socket a caller hands it** — no threads, no config
//!   writes, no logging side effects;
//! * **no panics** — every payload is untrusted input.
//!
//! # Layout
//!
//! | file | what lives there |
//! |---|---|
//! | [`frame`] | `u32` length prefix, the size limit, and read/write of one frame |
//! | [`msg`] | `ClientMsg`/`ServerMsg`, `Op`, `WireError`, `PROTO_VERSION` |
//! | [`snapshot`] | `Snapshot`/`WireTunnel`/`WireStatus`, and `Instant` → millisecond stamps |
//! | [`codec`] | encode/decode, version and config negotiation, error wording |
//! | [`endpoint`] | `config_path` → socket/lock paths, directory permissions |
//!
//! # Where a new thing goes
//!
//! * another message or command variant → [`msg`], then the version rule in
//!   `docs/research/client-server/protocol.md` §8.2;
//! * another field in a snapshot → [`snapshot`] (add only; renaming is breaking);
//! * another transport detail → [`frame`] or [`endpoint`];
//! * another error the peer must act on → [`codec`] with both languages filled.

pub mod codec;
pub mod endpoint;
pub mod frame;
pub mod msg;
pub mod snapshot;

pub use codec::{ProtocolError, check_config, check_version, read_client_msg, read_server_msg, write_message};
pub use endpoint::{Endpoint, endpoint_for};
pub use frame::{MAX_FRAME, read_frame, write_frame};
pub use msg::{ClientMsg, Op, PROTO_VERSION, ServerMsg, WireError, WirePort};
pub use snapshot::{
    SNAPSHOT_LOG_TAIL, Snapshot, WireFailure, WireLogLine, WireStatus, WireTunnel, failure_kind_from_token,
    level_from_token, level_token,
};

//! Running this `App` as a **client**: state arrives from a server, and every
//! change leaves as a command instead of being applied here.
//!
//! # Why one `App` and not two
//!
//! There are two processes but only one description of the screen. A second
//! "client-only" state type would duplicate the ~1394-line `App` and force
//! `ui`/`keys` to be written twice, so instead `App` gains an optional
//! [`Remote`]: when it is `Some`, this instance is a *mirror*. Everything the
//! renderer reads (`tunnels`, `cfg`, `hosts`, `status`, …) stays exactly where it
//! was; only the **origin** of tunnel state and the **destination** of user
//! intent change:
//!
//! ```text
//! authority (server)                       mirror (client)
//!   Supervisor ──events──▶ tunnels           socket ──Snapshot──▶ tunnels
//!   user intent ──▶ tunnels (直接)            user intent ──Command──▶ socket
//! ```
//!
//! # The two rules a mirror must never break
//!
//! 1. **It never persists.** `config.toml` and `state.toml` belong to the
//!    server. A client that wrote them would turn a stale mirror into an
//!    overwrite of the authority's file — the exact "stale view overrides the
//!    present" defect the protocol exists to prevent.
//! 2. **It never spawns a tunnel.** `Supervisor` is present only as an inert
//!    field here; the transport must not be reached from a mirror's `start`.
//!
//! Both are enforced by routing *before* the side effect: see the `if
//! self.is_mirror()` early returns in `app::runtime`, `app::persist`,
//! `app::form_submit` and `app::select`.
//!
//! # What a mirror validates locally (and why that is safe)
//!
//! Field-level checks (`validate_tunnel`, duplicate detection, listen-port
//! collision) run here so the user gets an instant answer instead of a round
//! trip. They are **advisory**: the server re-runs all of them and is the only
//! authority. Because [`App::apply_snapshot`] rebuilds `cfg.tunnels` from the
//! wire, those local checks are always computed from the last known authoritative
//! definitions rather than from whatever the config file said at start-up.
//!
//! Belongs here: the mirror's connection state, applying a snapshot, and turning
//! an intent into a queued [`Op`].
//! Does not belong: the socket itself ([`crate::client`]), who is the serializer
//! ([`crate::ipc`]), or what a snapshot contains ([`crate::ipc::snapshot`]).

//! # Layout
//!
//! | file | what lives there |
//! |---|---|
//! | [`mod`] (this file) | [`Remote`], [`ConnState`], attaching, and intent → wire |
//! | [`inbound`] | what to do with each [`ServerMsg`] |
//! | [`wire_to_live`] | `Snapshot` → live `TunnelView`s (server clock → `Instant`) |
//! | [`signature`] | the change detector the server broadcasts on |

pub mod inbound;
pub mod signature;
pub mod wire_to_live;

use std::sync::mpsc::{Receiver, Sender};

use crate::app::App;
use crate::ipc::{ClientMsg, Op, ServerMsg};

/// Where a client stands relative to its server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    /// Handshake accepted; snapshots are flowing.
    Attached,
    /// The transport ended (server died, socket broke). A message says why.
    Lost(String),
}

/// The client half of a session: the two channels, plus what the header needs.
pub struct Remote {
    /// Intent (and `Bye`) to the transport writer. Never written to directly —
    /// go through [`App::send_op`] / [`App::send_bye`].
    pub outbound: Sender<ClientMsg>,
    /// Server messages, drained once per frame by [`App::drain_remote`].
    pub inbound: Receiver<ServerMsg>,
    /// Connection status for the header and the status bar.
    pub conn: ConnState,
    /// Newest revision applied; older or duplicate snapshots are ignored.
    pub revision: u64,
    /// Server's own count of attached clients, from the last snapshot.
    pub client_count: u32,
    /// Whether the server stays up after the last client leaves.
    pub keep_server: bool,
    /// The server's pid, for the header and for diagnostics.
    pub server_pid: u32,
    /// The server's resolved config path, shown instead of a local guess.
    pub config_path: String,
    /// Next request id; only needs to be unique within this connection.
    next_req: u64,
    /// A rejected command's reason, waiting to be shown on the status bar.
    pending_error: Option<String>,
}

impl Remote {
    /// Build the client half around a transport's two channel ends.
    pub fn new(outbound: Sender<ClientMsg>, inbound: Receiver<ServerMsg>) -> Remote {
        Remote {
            outbound,
            inbound,
            conn: ConnState::Attached,
            revision: 0,
            client_count: 1,
            keep_server: false,
            server_pid: 0,
            config_path: String::new(),
            next_req: 1,
            pending_error: None,
        }
    }

    /// Take the next request id.
    fn next_req(&mut self) -> u64 {
        let id = self.next_req;
        self.next_req = self.next_req.saturating_add(1);
        id
    }
}

impl App {
    // -- attaching ---------------------------------------------------------

    /// Become a mirror of the server reached through `outbound`/`inbound`.
    ///
    /// The local `tunnels` are **cleared** rather than kept: what this process
    /// read from `config.toml` is a guess at the server's state, and showing a
    /// guess for even one frame is how a client ends up "reporting" ports that are
    /// not actually there. The `Welcome` snapshot arrives on the first frame.
    pub fn attach_remote(&mut self, outbound: Sender<ClientMsg>, inbound: Receiver<ServerMsg>) {
        self.tunnels.clear();
        self.cfg.tunnels.clear();
        self.selected = 0;
        self.remote = Some(Remote::new(outbound, inbound));
    }

    /// Whether this instance mirrors a server instead of owning the tunnels.
    pub fn is_mirror(&self) -> bool {
        self.remote.is_some()
    }

    /// Whether quitting here would leave nobody attached to the server.
    ///
    /// Authoritative mode is always "the last client": its own exit stops every
    /// tunnel it owns. A mirror trusts the server's count, which may be one frame
    /// old — and that stale possibility is exactly why this is only a *prompt*
    /// decision: the server recounts before acting on `Bye`.
    pub fn is_last_client(&self) -> bool {
        match self.remote.as_ref() {
            Some(remote) => remote.client_count <= 1,
            None => true,
        }
    }

    /// The connection status, for the header; `None` when authoritative.
    pub fn conn_state(&self) -> Option<&ConnState> {
        self.remote.as_ref().map(|r| &r.conn)
    }

    /// How many clients the server reports; `0` when authoritative.
    pub fn remote_client_count(&self) -> Option<u32> {
        self.remote.as_ref().map(|r| r.client_count)
    }

    /// The config path the server is serving, when known.
    pub fn remote_config_path(&self) -> Option<&str> {
        self.remote.as_ref().map(|r| r.config_path.as_str()).filter(|p| !p.is_empty())
    }

    /// Whether the server survives the last client leaving.
    pub fn remote_keep_server(&self) -> bool {
        self.remote.as_ref().is_some_and(|r| r.keep_server)
    }

    // -- intent → wire -----------------------------------------------------

    /// Queue one command for the server.
    ///
    /// Returns `false` when this instance is authoritative, so call sites can be
    /// written as `if self.send_op(op) { return; }` and keep one code path for
    /// both modes. `expect_rev` is attached only for the two commands where a
    /// stale view must be refused rather than applied (`delete`/`update`).
    pub(crate) fn send_op(&mut self, op: Op) -> bool {
        let Some(remote) = self.remote.as_mut() else {
            return false;
        };
        let expect_rev = match op {
            Op::Delete { .. } | Op::Update { .. } => Some(remote.revision),
            _ => None,
        };
        let req_id = remote.next_req();
        let msg = ClientMsg::Command { req_id, expect_rev, op };
        // A closed channel means the transport is gone; the reader side will
        // report `Lost` on the same frame, so there is nothing useful to add.
        if remote.outbound.send(msg).is_err() {
            remote.conn = ConnState::Lost("客户端连接已关闭 / the client link is closed".to_string());
        }
        true
    }

    /// Announce an intentional exit, so the server can decide whether to shut
    /// down (it recounts; `last_exit` is only a proposal).
    pub fn send_bye(&mut self, last_exit: bool) {
        if let Some(remote) = self.remote.as_mut() {
            let _ = remote.outbound.send(ClientMsg::Bye { last_exit });
        }
    }
}

/// The bilingual helper, in one place for this module's sentences.
fn help_bi(zh: &str, en: &str) -> String {
    crate::help::bi(zh, en)
}

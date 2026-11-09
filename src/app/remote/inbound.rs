//! What a client does with each message the server sends.
//!
//! The rule this file exists to keep: **the server's word is final**. A snapshot
//! replaces the table wholesale at its own `revision`; a `Reply` only ever adds a
//! sentence to the status bar; a `Stale` never retries on the user's behalf. No
//! message here spawns a tunnel or writes a file.
//!
//! `Log` frames are deliberately ignored: every snapshot already carries the log
//! tail the detail panel renders, and the separate append stream exists for a
//! future long-history view. Dropping it keeps an old client and a new server
//! interoperable in both directions.
//!
//! Belongs here: `drain_remote`, message dispatch, applying a snapshot.
//! Does not belong: converting the snapshot's values ([`super::wire_to_live`]),
//! the socket ([`crate::client`]), sending intent ([`super`]).

use std::time::Instant;

use crate::app::App;
use crate::ipc::{PROTO_VERSION, ServerMsg, Snapshot, WireError};
use crate::model::{StatusMsg, TunnelView};

use super::wire_to_live::live_view;
use super::{ConnState, help_bi};

impl App {
    // -- snapshot → screen -------------------------------------------------

    /// Drain server messages. Called once per frame from [`App::drain_events`].
    pub fn drain_remote(&mut self) {
        let mut latest: Option<ServerMsg> = None;
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        loop {
            match remote.inbound.try_recv() {
                Ok(msg) => latest = Some(msg),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.mark_lost("与服务端的连接已断开 / the link to the server closed".to_string());
                    return;
                }
            }
        }
        if let Some(msg) = latest {
            self.handle_server_msg(msg);
        }
    }

    /// Record that the transport died, once — not once per frame.
    fn mark_lost(&mut self, reason: String) {
        let Some(remote) = self.remote.as_mut() else {
            return;
        };
        if matches!(remote.conn, ConnState::Lost(_)) {
            return;
        }
        remote.conn = ConnState::Lost(reason.clone());
        self.status = Some(StatusMsg::error(reason));
    }

    /// Apply one server message.
    fn handle_server_msg(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::Welcome { proto_version, config_path, server_pid, snapshot } => {
                if let Some(remote) = self.remote.as_mut() {
                    remote.server_pid = server_pid;
                    remote.config_path = config_path;
                    remote.conn = ConnState::Attached;
                    if proto_version != PROTO_VERSION {
                        // Both sides are the same binary; a mismatch means an old
                        // instance is still running. Say so instead of rendering a
                        // table that may be silently wrong.
                        let reason =
                            crate::ipc::check_version(proto_version).err().map(|e| e.error.zh).unwrap_or_default();
                        remote.conn = ConnState::Lost(reason.clone());
                        self.status = Some(StatusMsg::error(reason));
                        return;
                    }
                }
                self.apply_snapshot(snapshot);
            }
            ServerMsg::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            ServerMsg::Reply { ok, error, remaining, .. } => {
                if let Some(remaining) = remaining
                    && let Some(remote) = self.remote.as_mut()
                {
                    remote.client_count = remaining;
                }
                match (ok, error) {
                    (true, _) => {}
                    (false, Some(err)) => self.take_remote_error(err),
                    (false, None) => {
                        self.status = Some(StatusMsg::error(help_bi("服务端拒绝了该操作", "The server refused it")))
                    }
                }
            }
            ServerMsg::Stale { .. } => {
                self.status = Some(StatusMsg::warn(help_bi(
                    "状态已更新，请确认后重试",
                    "The state changed; review it and try again",
                )));
            }
            // Log lines are carried inside every snapshot (`log_tail`), which is
            // exactly what the detail panel renders, so the separate append
            // stream is not needed by this client. Ignoring it (rather than
            // erroring) keeps an older/newer peer interoperable.
            ServerMsg::Log { .. } => {}
            ServerMsg::Error { code, error } => {
                let text = help_bi(&error.zh, &error.en);
                self.status = Some(StatusMsg::error(text));
                self.mark_lost(format!("server error: {code}"));
            }
        }
    }

    /// Show a rejected command and remember that the next snapshot supersedes it.
    fn take_remote_error(&mut self, err: WireError) {
        if let Some(remote) = self.remote.as_mut() {
            remote.pending_error = Some(help_bi(&err.zh, &err.en));
        }
        self.status = Some(StatusMsg::error(help_bi(&err.zh, &err.en)));
    }

    /// Replace our state with the server's, at `snapshot.revision`.
    ///
    /// Authoritative by construction: `revision` is monotonic, so an older frame
    /// that arrives late (or twice) is dropped instead of rewinding the table.
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        let Some(remote) = self.remote.as_mut() else {
            return;
        };
        if snapshot.revision <= remote.revision {
            return;
        }
        remote.revision = snapshot.revision;
        remote.client_count = snapshot.client_count;
        remote.keep_server = snapshot.keep_server;
        remote.pending_error = None;
        // One clock reading for the whole snapshot: two rows must not disagree
        // about what "now" is when their ages are derived from the same frame.
        let local_now = Instant::now();
        let now_ms = snapshot.server_now_ms;
        let tunnels: Vec<TunnelView> =
            snapshot.tunnels.into_iter().map(|wire| live_view(wire, now_ms, local_now)).collect();
        // The definitions are the server's, so every local advisory check
        // (duplicate / name / id allocation / listen conflict) is computed from
        // this list rather than from the file we read at start-up.
        self.cfg.tunnels = tunnels.iter().map(|v| v.cfg.clone()).collect();
        self.tunnels = tunnels;
        let visible = self.visible_indices().len();
        self.selected = self.selected.min(visible.saturating_sub(1));
        // Entries deleted by another client must not stay marked here, or a batch
        // action would carry ids that no longer exist.
        let existing: Vec<u64> = self.tunnels.iter().map(|t| t.cfg.id).collect();
        self.select_mode.retain_existing(|id| existing.contains(&id));
        // An open edit form for a tunnel someone else just deleted has no target.
        if let Some(overlay_id) = self.editing_tunnel_id()
            && !existing.contains(&overlay_id)
        {
            self.form = None;
            self.picker = None;
            self.close_overlay();
            self.status = Some(StatusMsg::warn(help_bi(
                "正在编辑的转发已被其它客户端删除",
                "The forward being edited was deleted by another client",
            )));
        }
    }

    /// Which tunnel the open form is editing, if any.
    fn editing_tunnel_id(&self) -> Option<u64> {
        match self.form.as_ref().map(|f| f.mode) {
            Some(crate::app::FormMode::Edit(id)) => Some(id),
            _ => None,
        }
    }
}

//! Whole-set and removal operations: deleting a tunnel, and stopping everything
//! at once.
//!
//! These live apart from the main `app.rs` because they are the operations that
//! treat the table as a set rather than a selection, and because each one has a
//! mirror/authority split to get right: a client must send one command for the
//! set (`delete_all` has no such op — it is per-tunnel by design, so a stale row
//! cannot silently remove a neighbour) while the authority applies it directly.
//!
//! Belongs here: `delete_tunnel`, `delete_selected`, `start_all`, `stop_all`,
//! and the request wrappers that decide whether a gate is needed.
//! Does not belong: the per-tunnel runtime primitives (`runtime.rs`), the gate
//! text (`confirm.rs`), or persistence (`persist.rs`).

use crate::app::{App, ConfirmAction};
use crate::eventlog;
use crate::help;
use crate::model::{StatusMsg, TunnelId};

impl App {
    /// Remove one tunnel definition and its runtime state.
    ///
    /// Split out from [`App::delete_selected`] because the batch path deletes by
    /// id without moving the cursor between removals (a shuffled cursor would
    /// make `selected` mean a different tunnel on every iteration).
    pub fn delete_tunnel(&mut self, id: TunnelId) {
        if let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == id) {
            eventlog::tunnel_deleted(&view.cfg);
        }
        // Deletion is `expect_rev`-guarded on the wire: the definition is about to
        // be destroyed, so a stale mirror must be told to look again instead of
        // silently removing a row someone else just changed.
        if self.send_op(crate::ipc::Op::Delete { tunnel_id: id }) {
            return;
        }
        self.supervisor.stop(id);
        self.forget_runtime_state(id);
        self.cfg.tunnels.retain(|t| t.id != id);
        self.tunnels.retain(|t| t.cfg.id != id);
        let n = self.visible_indices().len();
        self.selected = self.selected.min(n.saturating_sub(1));
        self.persist_quietly();
    }

    pub fn delete_selected(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.delete_tunnel(id);
        self.set_status(StatusMsg::info(help::bi("已删除", "Deleted")));
    }

    /// `x`: stopping everything is a bulk destructive action.
    pub fn request_stop_all(&mut self) {
        if self.counts().off == self.tunnels.len() {
            self.set_status(StatusMsg::info(help::bi("没有运行中的转发", "No forward is running")));
            return;
        }
        self.ask_confirm(ConfirmAction::StopAll);
    }

    /// `a`: starting everything opens listeners; confirm once for the batch.
    pub fn request_start_all(&mut self) {
        if self.counts().off == 0 {
            self.set_status(StatusMsg::info(help::bi("没有未启用的转发", "No disabled forward")));
            return;
        }
        self.ask_confirm(ConfirmAction::StartAll);
    }

    pub fn start_all(&mut self) {
        // One command rather than N: the server applies the same rule (every
        // stopped tunnel) against its own authoritative list, so a mirror holding
        // a one-frame-old table cannot miss a row that was just added.
        if self.send_op(crate::ipc::Op::StartAll) {
            return;
        }
        let ids: Vec<TunnelId> =
            self.tunnels.iter().filter(|t| !t.status.is_running_or_pending()).map(|t| t.cfg.id).collect();
        for id in ids {
            self.start_tunnel_with(id, "start_all");
        }
    }

    pub fn stop_all(&mut self) {
        if self.send_op(crate::ipc::Op::StopAll) {
            return;
        }
        let ids: Vec<TunnelId> = self.tunnels.iter().map(|t| t.cfg.id).collect();
        for id in ids {
            self.stop_tunnel_with(id, "stop_all");
        }
    }
}

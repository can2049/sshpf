//! Server-side creation and replacement: the two mutations that are more than a
//! single call on an existing row.
//!
//! These exist because `Create`/`Update` arrive as *data* from another process
//! and must be applied with the same bookkeeping the interactive form uses —
//! push the definition, push the view, persist, and (for an edit) rebuild the
//! live connection. Duplicating that sequence in `src/server/` would let the two
//! drift, and the server's copy is the one that would rot silently because the UI
//! never exercises it.
//!
//! Belongs here: `adopt_created`, `adopt_updated`, and the conflict lookup the
//! server needs to refuse a colliding listen endpoint.
//! Does not belong: deciding *whether* the command is allowed ([`crate::server`]),
//! the wire (`crate::ipc`), or the form's field parsing (`form_submit`).

use crate::app::App;
use crate::config;
use crate::eventlog;
use crate::model::{TunnelConfig, TunnelId, TunnelView};

impl App {
    /// The name of a tunnel whose listen endpoint collides with `candidate`.
    ///
    /// Same rule as the form's own check (`config::listen_endpoint` plus the
    /// wildcard rule), exposed because the server must answer the same question
    /// about a definition it did not build. `None` when nothing collides.
    pub fn listen_conflict_name(&self, candidate: &TunnelConfig) -> Option<String> {
        let (addr, port) = config::listen_endpoint(candidate);
        for t in &self.tunnels {
            if t.cfg.id == candidate.id {
                continue;
            }
            let (a, p) = config::listen_endpoint(&t.cfg);
            let overlapping = p == port && (a == addr || a == "0.0.0.0" || addr == "0.0.0.0");
            if overlapping {
                return Some(t.cfg.name.clone());
            }
        }
        None
    }

    /// Add a definition the server just accepted, persist it, and start it.
    ///
    /// `next` is expected to carry an id the *server* allocated and a name it has
    /// already made unique. Returns the save error, if any: a failed write is
    /// reported to the client rather than swallowed, because the definition then
    /// exists only in memory and would vanish on the next restart.
    pub fn adopt_created(&mut self, next: TunnelConfig) -> Result<(), String> {
        eventlog::tunnel_created(&next, "client");
        self.cfg.tunnels.push(next.clone());
        self.tunnels.push(TunnelView::new(next.clone()));
        let id = next.id;
        self.start_tunnel_with(id, "created");
        self.persist()
    }

    /// Replace an existing definition, persist it, and rebuild its connection.
    ///
    /// The `enabled` flag is the OR of the old and new values: a definition being
    /// edited must not silently become "off" for the next start-up just because
    /// the client that edited it had an older view of the flag.
    pub fn adopt_updated(&mut self, id: TunnelId, next: TunnelConfig) -> Result<(), String> {
        if let Some(slot) = self.cfg.find_mut(id) {
            let keep_enabled = slot.enabled;
            let mut merged = next.clone();
            merged.id = id;
            merged.enabled = keep_enabled || next.enabled;
            *slot = merged;
        }
        if let Some(view) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            let enabled = view.cfg.enabled;
            view.cfg = next;
            view.cfg.id = id;
            view.cfg.enabled = enabled || view.cfg.enabled;
        }
        if let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == id) {
            eventlog::tunnel_updated(&view.cfg);
        }
        // A definition that changed shape must not keep the old connection: the
        // listener it holds may no longer be the one the definition describes.
        self.supervisor.stop(id);
        self.forget_runtime_state(id);
        self.start_tunnel_with(id, "edited");
        self.persist()
    }
}

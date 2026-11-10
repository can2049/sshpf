//! The handful of `App` operations the server needs that no key handler does.
//!
//! # Why these live apart from the key-driven paths
//!
//! A key press arrives with a *form* open and a user watching; a client command
//! arrives as a bare intention ("create this definition") with no form at all. The
//! ordinary create path (`commit_form_create`) is therefore the wrong shape for a
//! headless process: it closes an overlay, moves a cursor and re-plans ports, none
//! of which exist on the server.
//!
//! So the server gets its own small set of entry points that perform exactly the
//! state transition and nothing else. They are `pub` so the server/`app` contract
//! is visible in one file rather than spread over a dozen `pub(crate)` details.
//!
//! Belongs here: create/replace/stop-session, plus the read-only checks the server
//! re-runs before accepting a command.
//! Does not belong: the command wire format ([`crate::ipc`]) or key handling.

use crate::app::App;
use crate::eventlog;
use crate::model::{TunnelConfig, TunnelId, TunnelView};

impl App {
    /// The next free tunnel id, `None` when the space is exhausted.
    ///
    /// A thin alias over [`App::next_tunnel_id`] so the server does not have to
    /// reach for a `pub(crate)` item across module boundaries.
    pub fn next_free_id(&self) -> Option<TunnelId> {
        self.next_tunnel_id()
    }

    /// The name of a saved tunnel whose listen endpoint collides with `cfg`.
    ///
    /// `listen_conflict` is `pub(crate)`; the server needs the same answer to refuse
    /// a command built on a stale view.
    pub fn listen_conflict_for(&self, cfg: &TunnelConfig) -> Option<String> {
        self.listen_conflict(cfg)
    }

    /// Add a definition the server has already validated, and start it.
    pub fn add_tunnel(&mut self, cfg: TunnelConfig) {
        eventlog::tunnel_created(&cfg, "client");
        self.cfg.tunnels.push(cfg.clone());
        self.tunnels.push(TunnelView::new(cfg.clone()));
        self.start_tunnel_with(cfg.id, "created");
        self.persist_quietly();
    }

    /// Replace an existing definition and restart its session.
    pub fn replace_tunnel(&mut self, cfg: TunnelConfig) {
        let id = cfg.id;
        if let Some(slot) = self.cfg.find_mut(id) {
            let keep_enabled = slot.enabled;
            let mut next = cfg.clone();
            next.enabled = keep_enabled || next.enabled;
            *slot = next;
        }
        if let Some(view) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            let enabled = view.cfg.enabled;
            view.cfg = cfg;
            view.cfg.enabled = enabled || view.cfg.enabled;
            eventlog::tunnel_updated(&view.cfg);
        }
        self.supervisor_stop(id);
        self.forget_runtime_state(id);
        self.start_tunnel_with(id, "edited");
        self.persist_quietly();
    }

    /// Stop one supervised session without touching the status or persistence.
    ///
    /// Used by the restart paths, which immediately start a new session: the row
    /// must not flicker through `Stopped` in between, and the definition must not
    /// be rewritten.
    pub fn supervisor_stop(&mut self, id: TunnelId) {
        self.supervisor_stop_inner(id);
    }
}

//! Read-only queries over the app state: selection, filtering, counters.
//!
//! Every accessor here is a *pure read* — no supervisor calls, no persistence, no
//! mutation — which is why they are one file: they are the state machine's public
//! face for the renderer and the key handlers, and nothing in them can start or
//! stop anything.
//!
//! The one rule they all obey: **`App::selected` indexes the *filtered* view**, not
//! `tunnels`. Narrowing the filter can leave the cursor past the end, so anything
//! that reads the selection goes through [`App::visible_indices`] and the cursor is
//! re-clamped on every filter change or deletion.

use crate::model::{TunnelId, TunnelView, fuzzy_match};

use super::{App, Counts};

use crate::model::TunnelStatus;

impl App {
    /// Indices into `tunnels` for rows matching the current filter, in order.
    pub fn visible_indices(&self) -> Vec<usize> {
        self.tunnels
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                self.filter.is_empty()
                    || fuzzy_match(&self.filter, &t.cfg.name)
                    || fuzzy_match(&self.filter, &t.cfg.ssh_host)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Ids of the rows currently visible (filter applied), in display order.
    pub fn visible_ids(&self) -> Vec<TunnelId> {
        self.visible_indices().iter().filter_map(|i| self.tunnels.get(*i)).map(|t| t.cfg.id).collect()
    }

    /// Marked ids that still exist, in display order so prompts and bulk logs
    /// read in the same order as the table.
    pub fn marked_ids_in_view_order(&self) -> Vec<TunnelId> {
        self.visible_ids().into_iter().filter(|id| self.select_mode.is_marked(*id)).collect()
    }

    pub fn selected_tunnel(&self) -> Option<&TunnelView> {
        self.visible_indices().get(self.selected).and_then(|i| self.tunnels.get(*i))
    }

    pub fn selected_id(&self) -> Option<TunnelId> {
        self.selected_tunnel().map(|t| t.cfg.id)
    }

    pub fn tunnel(&self, id: TunnelId) -> Option<&TunnelView> {
        self.tunnels.iter().find(|t| t.cfg.id == id)
    }

    /// Clamp `selected` into the current (possibly filtered) view.
    ///
    /// `selected` indexes `visible_indices()`, so narrowing the filter can leave
    /// it pointing past the end. Every reader of `selected` goes through
    /// `visible_indices()`, so one clamp here keeps the cursor valid after any
    /// filter change or deletion.
    pub fn clamp_selection(&mut self) {
        let n = self.visible_indices().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    pub fn select_next(&mut self) {
        self.clamp_selection();
        let n = self.visible_indices().len();
        if n > 0 && self.selected + 1 < n {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.clamp_selection();
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.visible_indices().len().saturating_sub(1);
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for t in &self.tunnels {
            match &t.status {
                TunnelStatus::Up { .. } => c.up += 1,
                TunnelStatus::Starting { .. } => c.starting += 1,
                TunnelStatus::Retrying { .. } => c.retrying += 1,
                TunnelStatus::Failed { .. } => c.failed += 1,
                TunnelStatus::Stopped => c.off += 1,
            }
        }
        c.total = self.tunnels.len();
        c
    }

    pub fn running_count(&self) -> usize {
        self.supervisor.running_count()
    }

    pub fn set_status(&mut self, msg: crate::model::StatusMsg) {
        self.status = Some(msg);
    }

    /// Drop the transient message once it has aged out.
    pub fn expire_status(&mut self) {
        if self.status.as_ref().is_some_and(|s| s.expired()) {
            self.status = None;
        }
    }

    // -- host discovery ----------------------------------------------------

    pub fn load_hosts(&mut self) {
        match crate::ssh::list_hosts() {
            Ok(hosts) => {
                crate::eventlog::hosts_loaded(hosts.len());
                self.hosts = hosts;
                self.hosts_error = None;
            }
            Err(e) => {
                crate::eventlog::hosts_failed(&e.to_string());
                self.hosts.clear();
                self.hosts_error = Some(e.to_string());
            }
        }
    }
}

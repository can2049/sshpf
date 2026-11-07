//! The SSH-host dropdown: filtering, cursor movement, and accepting a host.
//!
//! The dropdown is a sub-state of the form rather than its own overlay, so it
//! lives beside the form code but keeps its own scroll/query logic here.

use crate::app::{App, HOST_DROPDOWN_ROWS, Overlay};
use crate::model::{HostPicker, fuzzy_match};

impl App {
    /// Expand the SSH HOST dropdown.
    ///
    /// The dropdown is part of the form (not a separate modal) so that picking
    /// a host from `~/.ssh/config` is discoverable. The overlay stays `Form`.
    pub fn open_host_picker(&mut self) {
        let mut picker = HostPicker { open: true, ..HostPicker::default() };
        if let Some(form) = &self.form {
            // Pre-seed the filter with what is already typed.
            let typed = form.ssh_host.value().trim().to_string();
            if !typed.is_empty() {
                picker.query = typed;
            }
        }
        self.picker = Some(picker);
        self.refresh_picker();
        self.overlay = Overlay::Form;
    }

    /// Collapse the dropdown without changing the field.
    pub fn close_host_picker(&mut self) {
        self.picker = None;
    }

    pub fn host_picker_open(&self) -> bool {
        self.picker.as_ref().is_some_and(|p| p.open)
    }

    /// Recompute dropdown matches against the current query.
    pub fn refresh_picker(&mut self) {
        let hosts = &self.hosts;
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let query = picker.query.clone();
        let needle = query.to_lowercase();

        // Filter by alias first. Matching `user@host:port` too would be handy
        // (find a host by IP), but showing a row whose alias looks unrelated
        // reads as a bug: typing "de" must not list "bastion-prod" just
        // because its endpoint is deploy@…  So endpoint matching is only a
        // fallback for when no alias matches at all.
        let mut matches: Vec<usize> = (0..hosts.len()).filter(|i| fuzzy_match(&query, &hosts[*i].alias)).collect();
        if matches.is_empty() && !query.is_empty() {
            matches = (0..hosts.len()).filter(|i| fuzzy_match(&query, &hosts[*i].endpoint())).collect();
        }
        // Alias prefix matches first, then alphabetical (hosts are pre-sorted).
        matches.sort_by_key(|i| !hosts[*i].alias.to_lowercase().starts_with(&needle));
        picker.matches = matches;
        picker.selected = picker.selected.min(picker.matches.len().saturating_sub(1));
        picker.ensure_visible(HOST_DROPDOWN_ROWS);
    }

    /// Move the dropdown cursor, `delta` rows.
    pub fn picker_move(&mut self, delta: i32) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        if delta > 0 {
            for _ in 0..delta {
                picker.down(HOST_DROPDOWN_ROWS);
            }
        } else {
            for _ in 0..-delta {
                picker.up(HOST_DROPDOWN_ROWS);
            }
        }
    }

    /// Type into the dropdown filter.
    pub fn picker_query_char(&mut self, c: char) {
        if let Some(picker) = self.picker.as_mut() {
            picker.query_char(c);
        }
        self.refresh_picker();
    }

    pub fn picker_query_backspace(&mut self) {
        if let Some(picker) = self.picker.as_mut() {
            picker.query_backspace();
        }
        self.refresh_picker();
    }

    /// Accept the highlighted host: fill SSH HOST (and NAME when still empty).
    pub fn picker_accept(&mut self) {
        let Some(host) = self.picker.as_ref().and_then(|p| p.current()).and_then(|i| self.hosts.get(i).cloned()) else {
            self.close_host_picker();
            return;
        };
        if let Some(form) = self.form.as_mut() {
            form.ssh_host.set(host.alias.clone());
            if form.name.value().trim().is_empty() {
                form.name.set(host.alias.clone());
            }
            form.revalidate();
        }
        self.close_host_picker();
    }
}

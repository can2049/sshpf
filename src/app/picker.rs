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
    ///
    /// Two hazards handled here, both from `delta` being a public `i32`:
    /// `-delta` overflows for `i32::MIN` (panic in a debug build), and
    /// `unsigned_abs()` of a huge delta is a loop that effectively never ends
    /// because `down`/`up` are already clamped to the list. So the step count is
    /// taken from `unsigned_abs` and the loop stops as soon as the cursor stops
    /// moving — i.e. at either end of the list.
    pub fn picker_move(&mut self, delta: i32) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        for _ in 0..delta.unsigned_abs() {
            let before = picker.selected;
            if delta > 0 {
                picker.down(HOST_DROPDOWN_ROWS);
            } else {
                picker.up(HOST_DROPDOWN_ROWS);
            }
            if picker.selected == before {
                break;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::ssh::SshHost;

    fn host(alias: &str) -> SshHost {
        SshHost {
            alias: alias.into(),
            hostname: "1.1.1.1".into(),
            user: None,
            port: 22,
            proxy_jump: Vec::new(),
            identity_file: None,
        }
    }

    fn app_with_picker(hosts: Vec<SshHost>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        let mut app = App::new(paths);
        app.hosts = hosts;
        (dir, app)
    }

    #[test]
    fn a_huge_delta_does_not_overflow_or_hang() {
        // `picker_move` takes an `i32`; `-delta` overflows for `i32::MIN`
        // (panic in debug builds), and a huge positive delta used to walk the
        // cursor `delta` times even though `down` is already clamped.
        let (_d, mut app) = app_with_picker((0..10).map(|i| host(&format!("h{i}"))).collect());
        app.open_form_create();
        app.open_host_picker();

        app.picker_move(i32::MIN);
        assert_eq!(app.picker.as_ref().unwrap().selected, 0, "moving up past the top clamps");

        app.picker_move(i32::MAX);
        assert_eq!(app.picker.as_ref().unwrap().selected, 9, "moving down past the end clamps");
    }

    #[test]
    fn moving_the_picker_keeps_the_selection_inside_the_window() {
        let (_d, mut app) = app_with_picker((0..40).map(|i| host(&format!("h{i:02}"))).collect());
        app.open_form_create();
        app.open_host_picker();
        for _ in 0..30 {
            app.picker_move(1);
        }
        let p = app.picker.as_ref().unwrap();
        assert_eq!(p.selected, 30);
        assert!(p.selected < p.scroll as usize + HOST_DROPDOWN_ROWS, "the row must be visible");
        assert!(p.scroll as usize <= p.selected);
    }

    #[test]
    fn a_filter_that_empties_the_list_leaves_a_usable_picker() {
        // Accepting with no matches must close the dropdown, not index `hosts`
        // with a stale match index.
        let (_d, mut app) = app_with_picker(vec![host("alpha")]);
        app.open_form_create();
        app.open_host_picker();
        for c in "zzzz-no-match".chars() {
            app.picker_query_char(c);
        }
        assert!(app.picker.as_ref().unwrap().matches.is_empty());
        app.picker_move(1);
        app.picker_move(-1);
        app.picker_accept();
        assert!(!app.host_picker_open());
        // The form must be untouched (no host was picked).
        assert_eq!(app.form.as_ref().unwrap().ssh_host.value(), "");
    }

    #[test]
    fn a_stale_match_index_cannot_reach_past_the_host_list() {
        // `matches` holds indices into `hosts`. If `hosts` is reloaded smaller
        // while a dropdown is open, the old indices must not index out of
        // bounds; `picker_accept` must simply close.
        let (_d, mut app) = app_with_picker((0..5).map(|i| host(&format!("h{i}"))).collect());
        app.open_form_create();
        app.open_host_picker();
        // Reload a shorter list without refreshing the picker's matches.
        app.hosts.truncate(1);
        app.picker_accept();
        assert!(!app.host_picker_open());
    }

    #[test]
    fn opening_the_picker_with_no_hosts_is_harmless() {
        let (_d, mut app) = app_with_picker(vec![]);
        app.open_form_create();
        app.open_host_picker();
        assert!(app.picker.as_ref().unwrap().matches.is_empty());
        assert_eq!(app.picker.as_ref().unwrap().selected, 0);
        app.picker_accept();
        assert!(!app.host_picker_open());
    }

    #[test]
    fn refresh_picker_without_a_form_is_a_no_op() {
        // `refresh_picker` reads `self.hosts` and `self.picker`; neither implies
        // the other exists.
        let (_d, mut app) = app_with_picker(vec![host("a")]);
        app.refresh_picker(); // no picker at all
        assert!(app.picker.is_none());
    }
}

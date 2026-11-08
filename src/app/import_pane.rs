//! The import pane: discovering a host's forwards and driving the pane's widget.
//!
//! This is the *interface* half of importing — everything the user does to the
//! table (open/collapse the pane, filter columns, move the cursor, tick rows).
//! The *commit* half (turning ticked rows into tunnels) lives in
//! [`super::import`]; keeping them apart means the pane can be tested without any
//! persistence and the batch can be tested without any UI state.
//!
//! Two design points worth keeping:
//!
//! * The pane **opens itself** as soon as a host with importable forwards is
//!   committed. "Default all ticked" is only a real default if the rows are on
//!   screen without a second key press — an offer the user has to discover is not
//!   a default. It only auto-opens when there is at least one *importable* row.
//! * Discovery is **not repeated** for the same host: re-running it would throw
//!   away the user's tick marks and filters on every keystroke.

use crate::app::{App, FormMode};
use crate::eventlog;
use crate::help;
use crate::model::{ImportColumn, ImportTable, SshForward, StatusMsg};
use crate::{keys, ssh};

impl App {
    /// Read the forwards declared for `host_alias` and load them into the open
    /// form's import table.
    ///
    /// Called when the SSH HOST field changes (and when the picker accepts a
    /// host), so the table always describes the host currently in the field.
    /// Re-running for the same host is a no-op, which keeps typing cheap.
    pub fn refresh_import_table(&mut self, host_alias: &str) {
        let alias = host_alias.trim().to_string();
        let Some(form) = self.form.as_ref() else {
            return;
        };
        // Editing an existing tunnel must not silently rewrite it into a batch:
        // the import table only exists for new tunnels.
        if matches!(form.mode, FormMode::Edit(_)) {
            return;
        }
        // Same host: keep the table *and* the user's tick marks / collapsed
        // state. Re-discovery here would throw away choices they already made.
        if form.import_host_name() == alias {
            return;
        }
        // Empty field means "no host chosen yet". Do not scan for it: `Host *`
        // matches the empty string, which would list the global forwards under
        // an alias the user never picked.
        if alias.is_empty() {
            self.install_import_rows(String::new(), Vec::new());
            self.import_error = None;
            return;
        }

        let (forwards, error) = discover_forwards(&alias);
        self.apply_import_discovery(alias, forwards, error);
    }

    /// Install what discovery found for `host` and record any error to show.
    ///
    /// Split from [`Self::refresh_import_table`] so the "rows arrived, now what"
    /// half can be tested without touching the filesystem or the process
    /// environment (which would need `unsafe` under Rust 2024).
    pub(crate) fn apply_import_discovery(&mut self, host: String, forwards: Vec<SshForward>, error: Option<String>) {
        self.install_import_rows(host, forwards);
        self.import_error = error;
    }

    /// Install the rows discovered for `host` and decide whether the pane shows.
    ///
    /// This is the whole "default, not a hidden offer" rule, kept separate from
    /// discovery so it can be unit-tested without a real `~/.ssh/config`:
    ///
    /// * at least one **importable** row → open, every importable row already
    ///   ticked (`ImportTable::new` does the ticking) and the user unticks what
    ///   they do not want;
    /// * no importable row (either nothing at all, or only rows that cannot be
    ///   mapped onto a `TunnelConfig`) → stay closed, so the form is not covered
    ///   by a pane with nothing to tick. `space`/`i` still opens it, which is how
    ///   the un-importable rows explain themselves.
    ///
    /// Only a *changed* host reaches this (see `refresh_import_table`), so
    /// re-committing the same alias cannot undo a deliberate collapse.
    pub(crate) fn install_import_rows(&mut self, host: impl Into<String>, forwards: Vec<SshForward>) {
        let table = ImportTable::new(forwards);
        let importable = table.importable_count();
        if let Some(form) = self.form.as_mut() {
            form.set_import_table(host, table);
            form.import_open = importable > 0;
        }
    }

    /// Re-run discovery for whatever is in the SSH HOST field.
    pub fn refresh_import_for_current_host(&mut self) {
        let alias = match self.form.as_ref() {
            Some(f) => f.ssh_host.value().to_string(),
            None => return,
        };
        self.refresh_import_table(&alias);
    }

    /// `space`/`i` on the SSH HOST field: show or hide the import pane.
    ///
    /// Loads the table first if it is stale, so the pane can never display rows
    /// belonging to a different host.
    pub fn toggle_import_pane(&mut self) {
        self.refresh_import_for_current_host();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.import.is_empty() {
            self.import_error = self.import_error.clone().or_else(|| {
                Some(help::bi(
                    &format!("{} 在 ~/.ssh/config 中没有端口转发配置", form.ssh_host.value()),
                    &format!("{} has no port forwards in ~/.ssh/config", form.ssh_host.value()),
                ))
            });
            form.import_open = false;
            let msg = self.import_error.clone().unwrap_or_default();
            self.set_status(StatusMsg::warn(msg));
            return;
        }
        form.import_open = !form.import_open;
        form.import.end_filter();
    }

    pub fn close_import_pane(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import_open = false;
            form.import.end_filter();
        }
    }

    pub fn import_pane_open(&self) -> bool {
        self.form.as_ref().is_some_and(|f| f.import_open && !f.import.is_empty())
    }

    // -- filter editing ----------------------------------------------------

    /// Start typing into one column's filter (bound to the header click /
    /// `f` cycling, and to mouse clicks on a header).
    ///
    /// Leaves the text field's editing stage if it was active. Two cursors cannot
    /// both own the keyboard, and after this change the loser is decided by
    /// `is_editing_field()` rather than by draw order — so a mouse click on a
    /// header while a field was being edited would otherwise show the filter
    /// cursor in the pane while every keystroke still went into the field.
    pub fn begin_import_filter(&mut self, col: ImportColumn) {
        if let Some(form) = self.form.as_mut() {
            form.end_edit();
            form.import.begin_filter(col);
            form.import_open = true;
        }
    }

    pub fn end_import_filter(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.end_filter();
        }
    }

    /// Route a character into the filter of the column being edited.
    pub fn import_filter_char(&mut self, c: char) {
        if let Some(form) = self.form.as_mut()
            && let Some(col) = form.import.editing
        {
            form.import.filter_char(col, c);
        }
    }

    pub fn import_filter_backspace(&mut self) {
        if let Some(form) = self.form.as_mut()
            && let Some(col) = form.import.editing
        {
            form.import.filter_backspace(col);
        }
    }

    /// Delete the filter on `col` (chip `×`, or `F` on a column).
    pub fn clear_import_filter(&mut self, col: ImportColumn) {
        if let Some(form) = self.form.as_mut() {
            form.import.clear_filter(col);
        }
    }

    pub fn clear_all_import_filters(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.clear_all_filters();
        }
    }

    /// Undo the last filter change. Reports when there is nothing to undo rather
    /// than appearing to do nothing.
    pub fn undo_import_filter(&mut self) {
        let undone = self.form.as_mut().is_some_and(|f| f.import.undo_filter());
        if undone {
            self.set_status(StatusMsg::info(help::bi("已撤销筛选", "Filter change undone")));
        } else {
            self.set_status(StatusMsg::info(help::bi("没有可撤销的筛选", "No filter change to undo")));
        }
    }

    // -- row selection -----------------------------------------------------

    pub fn import_move(&mut self, delta: i32) {
        let rows = keys::IMPORT_ROWS_VISIBLE;
        if let Some(form) = self.form.as_mut() {
            form.import.move_cursor(delta);
            form.import.ensure_visible(rows);
        }
    }

    pub fn import_home(&mut self) {
        let rows = keys::IMPORT_ROWS_VISIBLE;
        if let Some(form) = self.form.as_mut() {
            form.import.home();
            form.import.ensure_visible(rows);
        }
    }

    pub fn import_end(&mut self) {
        let rows = keys::IMPORT_ROWS_VISIBLE;
        if let Some(form) = self.form.as_mut() {
            form.import.end();
            form.import.ensure_visible(rows);
        }
    }

    pub fn import_toggle_cursor(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.toggle_cursor();
        }
    }

    /// Toggle the row at a raw index (mouse click on a checkbox).
    ///
    /// The index comes from the frame's hit map, which can be one frame stale:
    /// a filter change or a re-discovery between drawing and clicking leaves the
    /// click pointing at a row that is no longer visible. That must be a no-op —
    /// moving the cursor to "wherever it happens to be" and ticking *that* row
    /// would tick a row the user did not click.
    pub fn import_toggle_at(&mut self, row: usize) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(pos) = form.import.visible().iter().position(|i| *i == row) else {
            eventlog::import_click_stale(row);
            return;
        };
        // Move the cursor there too: the click both selects and toggles, so the
        // highlighted row always matches what the user just acted on.
        form.import.cursor = pos;
        form.import.toggle_cursor();
    }

    /// Put the cursor on a raw row index (mouse click elsewhere on a row).
    pub fn import_select_at(&mut self, row: usize) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        match form.import.visible().iter().position(|i| *i == row) {
            Some(pos) => form.import.cursor = pos,
            None => eventlog::import_click_stale(row),
        }
    }

    pub fn import_check_all(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.check_all_visible();
        }
    }

    pub fn import_uncheck_all(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.uncheck_all_visible();
        }
    }

    pub fn import_invert(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.invert_visible();
        }
    }
}

/// Read the forwards `alias` declares in `~/.ssh/config`, turning a read failure
/// into the bilingual sentence the pane shows.
///
/// Returns an empty row set *and* the message on failure: a discovery error is a
/// degraded-but-usable state (the pane can still be opened to explain itself),
/// not a reason to abort the form.
fn discover_forwards(alias: &str) -> (Vec<SshForward>, Option<String>) {
    match ssh::forwards::forwards_for_host(alias) {
        Ok(rows) => (rows, None),
        Err(e) => (
            Vec::new(),
            Some(help::bi(
                &format!("读取 ~/.ssh/config 转发配置失败: {e}"),
                &format!("Cannot read the forward rules in ~/.ssh/config: {e}"),
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{ForwardKind, ImportColumn, ImportTable, SshForward, TunnelConfig, TunnelView};

    fn app_fresh() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        (dir, App::new(paths))
    }

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn show(port: u16) -> SshForward {
        SshForward {
            kind: crate::model::ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port: port,
            target_host: "10.0.0.1".into(),
            target_port: 80,
            host_alias: "dev-box".into(),
            line: 1,
            raw: format!("LocalForward {port} 10.0.0.1:80"),
        }
    }

    fn app_with_import(rows: Vec<SshForward>) -> (tempfile::TempDir, App) {
        let (dir, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        let table = ImportTable::new(rows);
        app.form.as_mut().unwrap().set_import_table("dev-box", table);
        app.form.as_mut().unwrap().import_open = true;
        (dir, app)
    }

    #[test]
    fn refresh_loads_the_table_for_the_typed_host_and_opens_the_pane() {
        // End-to-end through the discovery result: typing an alias and committing
        // it must be enough to get the rows on screen, with the pane opening by
        // itself. `apply_import_discovery` is the seam that takes rows from
        // discovery, so this asserts the whole "default offer" rule without
        // touching the process environment (which Rust 2024 makes `unsafe`).
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        let port = free_port();
        app.apply_import_discovery("dev-box".to_string(), vec![show(port)], None);

        let form = app.form.as_ref().unwrap();
        assert_eq!(form.import.len(), 1);
        assert_eq!(form.import.rows[0].forward.listen_port, port);
        assert!(form.import.rows[0].checked, "rows start ticked");
        assert!(app.import_pane_open(), "a host with importable forwards opens the pane by itself");
        assert_eq!(app.import_error, None);
    }

    #[test]
    fn a_discovery_error_is_recorded_without_dropping_the_rows() {
        // A failed read is degraded-but-usable: the message is kept for the pane,
        // and the (empty) table still replaces the previous host's, so the pane
        // cannot show stale rows for a host we could not read.
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.apply_import_discovery("dev-box".to_string(), vec![show(free_port())], None);
        assert!(app.import_pane_open());

        app.apply_import_discovery("broken".to_string(), Vec::new(), Some("读取失败".to_string()));
        assert_eq!(app.import_error.as_deref(), Some("读取失败"));
        assert!(app.form.as_ref().unwrap().import.is_empty(), "stale rows must not survive a failed read");
        assert!(!app.import_pane_open());
    }

    #[test]
    fn discovery_is_not_repeated_for_the_same_host() {
        // Cheap guard: typing one character at a time would otherwise re-parse
        // the config and discard the user's tick marks on every keystroke.
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        let table = ImportTable::new(vec![show(free_port())]);
        app.form.as_mut().unwrap().set_import_table("dev-box", table);
        // Untick it, then refresh for the *same* host: the mark must survive.
        app.form.as_mut().unwrap().import.rows[0].checked = false;
        app.refresh_import_for_current_host();
        assert!(!app.form.as_ref().unwrap().import.rows[0].checked, "tick marks must survive a no-op refresh");
    }

    #[test]
    fn a_host_with_importable_forwards_opens_the_pane_already_ticked() {
        // The headline behaviour: "the forwards this host declares" is the
        // *default* offer, not something behind a key press. Every importable
        // row arrives ticked, so the user's job is to untick the exceptions.
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.install_import_rows("dev-box", vec![show(p1), show(p2)]);
        let form = app.form.as_ref().unwrap();
        assert!(form.import_open, "the pane must open on its own");
        assert!(app.import_pane_open(), "and be actually drawable");
        assert_eq!(form.import.checked_count(), 2, "all importable rows start ticked");
        assert_eq!(form.import.checked_forwards().len(), 2);
    }

    #[test]
    fn a_host_with_no_forwards_does_not_open_an_empty_pane() {
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.install_import_rows("no-forwards", Vec::new());
        assert!(!app.form.as_ref().unwrap().import_open, "the flag itself must stay down");
        assert!(!app.import_pane_open(), "an empty pane would only cover the form");
        assert!(app.form.as_ref().unwrap().import.is_empty());
    }

    #[test]
    fn a_host_whose_forwards_are_all_unimportable_does_not_auto_open() {
        // Unix-socket / target-less directives are listed (silently hiding them
        // would look like the tool missed the config) but there is nothing to
        // tick, so auto-opening would be a pane full of `[–]`. `space`/`i` still
        // opens it to read the reasons.
        let mut broken = show(0);
        broken.target_host = String::new();
        broken.target_port = 0;
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.install_import_rows("dev-box", vec![broken]);
        {
            let form = app.form.as_ref().unwrap();
            assert_eq!(form.import.len(), 1, "the row must still be listed");
            assert_eq!(form.import.importable_count(), 0);
            assert!(!form.import_open, "nothing can be ticked, so the pane stays closed");
            assert!(!app.import_pane_open());
        }

        // ... and the manual escape hatch still works: this is the only way the
        // user ever sees *why* the row cannot be imported, so it must not have
        // been closed off along with the auto-open.
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.toggle_import_pane();
        assert!(app.import_pane_open(), "space/i must still open a pane holding only `[–]` rows");
        assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 0, "and nothing became tickable");
    }

    #[test]
    fn switching_hosts_reopens_the_pane_for_a_host_that_has_forwards() {
        // Host A has forwards (pane auto-open), the user collapses it, then picks
        // host B which also has forwards: B's rows are the new default, so the
        // pane comes back. Collapsing is per-host, not a permanent preference.
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.install_import_rows("host-a", vec![show(p1)]);
        assert!(app.import_pane_open());
        app.close_import_pane();
        assert!(!app.import_pane_open());

        app.install_import_rows("host-b", vec![show(p2)]);
        assert!(app.import_pane_open(), "a different host's forwards are a fresh default");
        assert_eq!(app.form.as_ref().unwrap().import.rows[0].forward.listen_port, p2, "and B's rows replace A's");
    }

    #[test]
    fn collapsing_the_pane_survives_a_refresh_of_the_same_host() {
        // `enter`/`esc` on the host field refreshes discovery. For an unchanged
        // alias that must be a no-op, or a deliberate collapse would pop back
        // open every time the user leaves the field.
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.install_import_rows("dev-box", vec![show(free_port())]);
        app.close_import_pane();
        app.refresh_import_for_current_host();
        assert!(!app.import_pane_open(), "refreshing the same host must not undo the collapse");
        assert_eq!(app.form.as_ref().unwrap().import.len(), 1, "and must not drop the rows either");
    }

    #[test]
    fn an_empty_host_field_installs_nothing_instead_of_the_global_forwards() {
        // `Host *` matches the empty string, so scanning "" would list every
        // global forward under an alias the user never picked — and `ctrl+s`
        // would then create them.
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.install_import_rows("dev-box", vec![show(free_port())]);
        app.form.as_mut().unwrap().ssh_host.set("   ");
        app.refresh_import_for_current_host();
        let form = app.form.as_ref().unwrap();
        assert!(form.import.is_empty(), "no host means no rows");
        assert!(!form.import_open);
        assert_eq!(app.import_error, None, "not scanning is not a failure to report");
    }

    #[test]
    fn editing_an_existing_tunnel_never_imports() {
        // Importing is a create-time convenience; silently turning an edit into a
        // batch create would be a data-loss bug.
        let (_d, mut app) = app_fresh();
        app.cfg.tunnels.push(TunnelConfig {
            id: 1,
            name: "existing".into(),
            kind: ForwardKind::Local,
            ssh_host: "dev-box".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 18000,
            remote_host: "10.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        });
        app.tunnels.push(TunnelView::new(app.cfg.tunnels[0].clone()));
        app.open_form_edit();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.refresh_import_for_current_host();
        assert!(app.form.as_ref().unwrap().import.is_empty());
    }

    #[test]
    fn toggling_the_pane_on_a_host_without_forwards_says_so() {
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("no-such-host-xyzzy");
        app.toggle_import_pane();
        assert!(!app.import_pane_open(), "an empty pane must not open");
        let msg = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        assert!(msg.contains("没有端口转发"), "the user must be told why nothing opened: {msg}");
    }

    #[test]
    fn pane_visibility_needs_rows_as_well_as_the_flag() {
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().import_open = true;
        assert!(!app.import_pane_open(), "the flag alone must not show an empty pane");
        let table = ImportTable::new(vec![show(free_port())]);
        app.form.as_mut().unwrap().set_import_table("dev-box", table);
        assert!(app.import_pane_open());
    }

    #[test]
    fn starting_a_column_filter_takes_the_keyboard_away_from_the_field() {
        // With the pane open by default, both the form field and a column filter
        // can plausibly "have the cursor". `handle_form_key` gives the field
        // priority while `is_editing_field()`, so a mouse click on a column
        // header while a field was being edited would draw the filter cursor in
        // the pane (it renders last) while every keystroke still went into the
        // field. The click has to end field editing to stay honest.
        let (_d, mut app) = app_with_import(vec![show(free_port())]);
        app.form.as_mut().unwrap().field = crate::model::FormField::Name;
        app.form.as_mut().unwrap().begin_edit();
        assert!(app.form.as_ref().unwrap().is_editing_field());

        app.begin_import_filter(ImportColumn::Listen);
        assert!(!app.form.as_ref().unwrap().is_editing_field(), "the filter owns the keyboard now");
        assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Listen));
    }

    #[test]
    fn filter_keys_reach_the_table() {
        let (_d, mut app) = app_with_import(vec![show(free_port()), show(free_port())]);
        let first = app.form.as_ref().unwrap().import.rows[0].forward.listen_port.to_string();
        app.begin_import_filter(ImportColumn::Listen);
        for c in first.chars() {
            app.import_filter_char(c);
        }
        assert_eq!(app.form.as_ref().unwrap().import.visible(), vec![0], "only the matching row survives");
        app.import_filter_backspace();
        assert_eq!(app.form.as_ref().unwrap().import.filter(ImportColumn::Listen), &first[..first.len() - 1]);
        app.clear_import_filter(ImportColumn::Listen);
        assert_eq!(app.form.as_ref().unwrap().import.visible().len(), 2);
    }

    #[test]
    fn undo_reports_when_there_is_nothing_to_undo() {
        let (_d, mut app) = app_with_import(vec![show(free_port())]);
        app.undo_import_filter();
        let msg = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        assert!(msg.contains("没有可撤销"), "silence would look broken: {msg}");
        app.begin_import_filter(ImportColumn::Kind);
        app.import_filter_char('L');
        app.undo_import_filter();
        assert!(!app.form.as_ref().unwrap().import.has_filters());
    }

    #[test]
    fn a_new_host_does_not_inherit_the_previous_hosts_filters() {
        // Filters narrow *this* host's rows; carrying them over would silently
        // hide rows of a different host for no visible reason.
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_import(vec![show(p1)]);
        app.form.as_mut().unwrap().import.set_filter(ImportColumn::Kind, "R");
        app.form.as_mut().unwrap().set_import_table("other-host", ImportTable::new(vec![show(p2)]));
        assert!(!app.form.as_ref().unwrap().import.has_filters());
    }

    #[test]
    fn import_interactions_do_not_open_the_pane_for_an_empty_host() {
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("nope");
        app.begin_import_filter(ImportColumn::Kind);
        // `begin_import_filter` flips the open flag; the visibility predicate
        // still refuses because there are no rows.
        assert!(!app.import_pane_open());
    }

    #[test]
    fn closing_the_pane_also_ends_filter_editing() {
        let (_d, mut app) = app_with_import(vec![show(free_port())]);
        app.begin_import_filter(ImportColumn::Listen);
        assert!(app.form.as_ref().unwrap().import.editing.is_some());
        app.close_import_pane();
        assert!(!app.import_pane_open());
        assert!(app.form.as_ref().unwrap().import.editing.is_none(), "a hidden pane must not keep the keyboard");
    }
}

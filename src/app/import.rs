//! Importing the forwards a host already declares in `~/.ssh/config`.
//!
//! Creating a tunnel used to be a blank form: the user retyped ports that the
//! ssh config next to it already described. Now picking a host discovers its
//! `LocalForward` / `RemoteForward` / `DynamicForward` directives, pre-ticks
//! them all, and lets the user untick what they do not want.
//!
//! The pane **opens itself** as soon as a host with importable forwards is
//! committed (`enter`/`esc` in the field, or accepting a host in the dropdown).
//! "Default all ticked" is only a real default if the rows are on screen without
//! a second key press — an offer the user has to discover is not a default. The
//! pane therefore only auto-opens when there is at least one *importable* row:
//! a host whose only directives are un-importable gets no pane, but `space`/`i`
//! still opens it to explain why.
//!
//! Two design points worth keeping:
//!
//! * The ticked rows are a **default, not a running listener**. The tunnels are
//!   only created when the user submits the form, so "default select all" cannot
//!   open ports behind the user's back.
//! * One `TunnelConfig` is one forward, so N ticked rows create N tunnels. The
//!   submit path therefore validates and inserts *in a batch*: a partial write
//!   (three tunnels created, then a validation error on the fourth) would leave
//!   a state the user never asked for, so everything is validated up front and
//!   nothing is written unless all of it is valid.

use crate::app::{App, FormMode};
use crate::config;
use crate::model::{
    self, ForwardKind, ImportColumn, ImportTable, SshForward, StatusMsg, TunnelConfig, TunnelView, now_unix,
};
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

        let (forwards, error) = match ssh::forwards::forwards_for_host(&alias) {
            Ok(rows) => (rows, None),
            Err(e) => (Vec::new(), Some(format!("读取 ~/.ssh/config 转发配置失败: {e}"))),
        };
        self.install_import_rows(alias, forwards);
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
            self.import_error = self
                .import_error
                .clone()
                .or_else(|| Some(format!("{} 在 ~/.ssh/config 中没有端口转发配置", form.ssh_host.value())));
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
            self.set_status(StatusMsg::info("已撤销筛选"));
        } else {
            self.set_status(StatusMsg::info("没有可撤销的筛选"));
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
        if let Some(form) = self.form.as_mut() {
            form.import.home();
        }
    }

    pub fn import_end(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.end();
        }
    }

    pub fn import_toggle_cursor(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.toggle_cursor();
        }
    }

    /// Toggle the row at a raw index (mouse click on a checkbox).
    pub fn import_toggle_at(&mut self, row: usize) {
        if let Some(form) = self.form.as_mut()
            && form.import.cursor_row().is_some()
        {
            // Move the cursor there too: the click both selects and toggles, so
            // the highlighted row always matches what the user just acted on.
            if let Some(pos) = form.import.visible().iter().position(|i| *i == row) {
                form.import.cursor = pos;
            }
            form.import.toggle_cursor();
        }
    }

    /// Put the cursor on a raw row index (mouse click elsewhere on a row).
    pub fn import_select_at(&mut self, row: usize) {
        if let Some(form) = self.form.as_mut()
            && let Some(pos) = form.import.visible().iter().position(|i| *i == row)
        {
            form.import.cursor = pos;
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

// ---------------------------------------------------------------------------
// Creating one tunnel per ticked row
// ---------------------------------------------------------------------------

impl App {
    /// Build a `TunnelConfig` from an imported ssh-config forward.
    ///
    /// `id` is assigned by the caller. Local ports can collide with other
    /// managed tunnels (the ssh config knows nothing about them), so `taken`
    /// drives the same upward scan the manual form uses — without it, enabling a
    /// batch would fail with "port in use" on a port `sshpf` itself holds.
    pub fn config_from_forward(&self, id: crate::model::TunnelId, forward: &SshForward, taken: &[u16]) -> TunnelConfig {
        let bind = self.cfg.defaults.bind.clone();
        let (local_addr, local_port, remote_host, remote_port) = match forward.kind {
            ForwardKind::Local => (
                if forward.listen_addr.is_empty() { bind.clone() } else { forward.listen_addr.clone() },
                forward.listen_port,
                forward.target_host.clone(),
                forward.target_port,
            ),
            // `-R` listens on the server and lands locally, so the config's
            // local side is the target and the remote side is the listener.
            ForwardKind::Remote => (
                forward.target_host.clone(),
                forward.target_port,
                if forward.listen_addr.is_empty() { bind.clone() } else { forward.listen_addr.clone() },
                forward.listen_port,
            ),
            ForwardKind::Dynamic => (
                if forward.listen_addr.is_empty() { bind.clone() } else { forward.listen_addr.clone() },
                forward.listen_port,
                String::new(),
                0,
            ),
        };

        // `-L`/`-D` bind locally, so only those need a collision-free port. The
        // rules live in `plan_local_port` (shared with the form) — most notably,
        // a privileged port is never scanned upwards from itself, which would
        // spend the whole budget inside the range that needs root and land on
        // another privileged port.
        let local_port = if forward.kind == ForwardKind::Remote {
            local_port
        } else {
            match model::plan_local_port(local_port, taken, |c| ssh::port_available(&local_addr, c)) {
                Some((port, _)) => port,
                // Nothing usable found: keep the requested port so validation
                // reports the real problem instead of silently using port 0.
                None => local_port,
            }
        };

        TunnelConfig {
            id,
            name: self.cfg.unique_name(&forward.suggested_name(), None),
            kind: forward.kind,
            ssh_host: forward.host_alias.clone(),
            local_addr,
            local_port,
            remote_host,
            remote_port,
            enabled: true,
            auto_restart: self.cfg.defaults.auto_restart,
            ssh_options: Vec::new(),
            note: format!("来自 ~/.ssh/config 第 {} 行", forward.line),
            created_at: now_unix(),
        }
    }

    /// Validate the form and, for a create, the imported rows as a batch.
    ///
    /// Returns the configs to create, or reports the problems into the form and
    /// creates nothing.
    pub fn validated_import_batch(&mut self) -> Option<Vec<TunnelConfig>> {
        let form = self.form.clone()?;
        if matches!(form.mode, FormMode::Edit(_)) {
            return None;
        }
        let forwards = form.import.checked_forwards();
        if forwards.is_empty() {
            return None;
        }
        let ssh_host = form.ssh_host.value().trim().to_string();
        if ssh_host.is_empty() {
            if let Some(f) = self.form.as_mut() {
                f.errors = vec!["SSH 主机不能为空".to_string()];
            }
            return None;
        }

        // Ports already claimed by other managed tunnels, so the batch does not
        // fight over one port with the tunnels it is replacing. Grows as we
        // assign, so two rows cannot be handed the same local port.
        let mut taken: Vec<u16> = self.taken_local_ports();
        let mut out: Vec<TunnelConfig> = Vec::with_capacity(forwards.len());
        let mut errors: Vec<String> = Vec::new();

        for (next_id, forward) in (self.cfg.next_id()..).zip(forwards.iter()) {
            let mut cfg = self.config_from_forward(next_id, forward, &taken);
            cfg.ssh_host = ssh_host.clone();
            // Keep names unique within the batch itself, not just against the
            // already-saved tunnels.
            cfg.name = unique_within(&cfg.name, out.iter().map(|c| c.name.clone()).collect());
            if let Err(e) = config::validate_tunnel(&cfg) {
                errors.push(format!("{}: {e}", cfg.name));
            }
            // Collision against already-saved tunnels, then against the rest of
            // this batch: two imported rows can describe the same listen port
            // (e.g. a `Host *` default plus a specific block), and creating both
            // would make the second one fail at bind time instead of here.
            let conflict =
                self.listen_conflict(&cfg).or_else(|| out.iter().find_map(|other| listen_collision(&cfg, other)));
            if let Some(conflict) = conflict {
                errors.push(format!("{}: 监听地址与「{conflict}」冲突", cfg.name));
            }
            if cfg.kind != ForwardKind::Remote {
                taken.push(cfg.local_port);
            }
            out.push(cfg);
        }

        if !errors.is_empty() {
            crate::app::log::import_rejected(&errors);
            if let Some(f) = self.form.as_mut() {
                f.errors = errors;
            }
            return None;
        }
        Some(out)
    }

    /// Create every validated tunnel, persist once, and start them.
    pub(crate) fn commit_import_batch(&mut self, configs: Vec<TunnelConfig>) {
        let n = configs.len();
        let first_id = configs.first().map(|c| c.id);
        let host = configs.first().map(|c| c.ssh_host.clone()).unwrap_or_default();
        crate::app::log::import_committed(&host, n);
        for cfg in &configs {
            crate::app::log::tunnel_created(cfg, "import");
            self.cfg.tunnels.push(cfg.clone());
            self.tunnels.push(TunnelView::new(cfg.clone()));
        }
        self.close_overlay();
        self.persist_quietly();
        for cfg in &configs {
            self.start_tunnel_with(cfg.id, "created");
        }
        // Put the cursor on the first created tunnel so the user lands where the
        // work happened instead of on a row that may be far away.
        if let Some(id) = first_id {
            let vis = self.visible_indices();
            if let Some(pos) = vis.iter().position(|i| self.tunnels.get(*i).is_some_and(|t| t.cfg.id == id)) {
                self.selected = pos;
            }
        }
        self.clamp_selection();
        if n == 1 {
            let name = configs.first().map(|c| c.name.clone()).unwrap_or_default();
            self.set_status(StatusMsg::ok(format!("已创建 1 条转发：{name}")));
        } else {
            self.set_status(StatusMsg::ok(format!("已创建 {n} 条转发（来自 ~/.ssh/config）")));
        }
    }

    /// Create-form submit when the import pane is in charge.
    ///
    /// Validates the whole batch first so a rejected row cannot leave the user
    /// with "four of five tunnels created and one error". Every "nothing to do"
    /// path reports the same phrase, because from the user's side they are one
    /// situation: they pressed save with no row ticked.
    pub(crate) fn commit_form_create_from_import(&mut self) {
        let configs = self.validated_import_batch();
        match configs {
            Some(configs) if !configs.is_empty() => self.commit_import_batch(configs),
            Some(_) | None => {
                // Validation reported its own problems into `form.errors`; only
                // speak up when it did not (no rows ticked, no host).
                if self.form.as_ref().is_some_and(|f| f.errors.is_empty())
                    && let Some(f) = self.form.as_mut()
                {
                    f.errors = vec!["没有勾选任何转发".to_string()];
                }
            }
        }
    }
}

/// A name not already used by `siblings` (the rest of the batch).
fn unique_within(base: &str, siblings: Vec<String>) -> String {
    if !siblings.iter().any(|s| s == base) {
        return base.to_string();
    }
    for i in 2..1000 {
        let candidate = format!("{base}-{i}");
        if !siblings.iter().any(|s| s == &candidate) {
            return candidate;
        }
    }
    base.to_string()
}

/// Whether two configs would listen on the same endpoint.
fn listen_collision(a: &TunnelConfig, b: &TunnelConfig) -> Option<String> {
    let (aa, ap) = config::listen_endpoint(a);
    let (ba, bp) = config::listen_endpoint(b);
    if ap == bp && (aa == ba || aa == "0.0.0.0" || ba == "0.0.0.0") { Some(b.name.clone()) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::TunnelStatus;

    fn app_fresh() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        (dir, App::new(paths))
    }

    /// A port that is free right now.
    ///
    /// Tests run in parallel and several of them bind ports, so asserting on a
    /// hard-coded port would be flaky — and worse, it would collide with the
    /// "busy port gets bumped" behaviour the importer deliberately implements,
    /// turning a correct bump into a test failure.
    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn show(port: u16) -> SshForward {
        SshForward {
            kind: ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port: port,
            target_host: "10.0.0.1".into(),
            target_port: 80,
            host_alias: "dev-box".into(),
            line: 1,
            raw: format!("LocalForward {port} 10.0.0.1:80"),
        }
    }

    /// Load a table into the open create form without touching the filesystem.
    fn app_with_import(rows: Vec<SshForward>) -> (tempfile::TempDir, App) {
        let (dir, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        let table = ImportTable::new(rows);
        app.form.as_mut().unwrap().set_import_table("dev-box", table);
        app.form.as_mut().unwrap().import_open = true;
        (dir, app)
    }

    // -- discovery plumbing ------------------------------------------------

    #[test]
    fn refresh_loads_the_table_for_the_typed_host_and_opens_the_pane() {
        // End-to-end through real discovery (a scratch config, so the test does
        // not depend on the developer's own `~/.ssh/config`): typing an alias
        // and committing it is enough to get the rows on screen.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("ssh_config");
        std::fs::write(&cfg_path, "Host dev-box\n  LocalForward 18001 10.0.0.1:80\n").unwrap();
        // SAFETY: this is the only test that touches `SSHPF_SSH_CONFIG`, and
        // discovery only reads it; the variable is restored before asserting.
        unsafe { std::env::set_var("SSHPF_SSH_CONFIG", &cfg_path) };
        let (_d, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.refresh_import_for_current_host();
        unsafe { std::env::remove_var("SSHPF_SSH_CONFIG") };

        let form = app.form.as_ref().unwrap();
        assert_eq!(form.import.len(), 1);
        assert_eq!(form.import.rows[0].forward.listen_port, 18001);
        assert!(form.import.rows[0].checked, "rows start ticked");
        assert!(app.import_pane_open(), "a host with importable forwards opens the pane by itself");
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

    // -- the default: auto-open, everything ticked -------------------------

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

    // -- creating the batch ------------------------------------------------

    #[test]
    fn every_ticked_row_becomes_its_own_tunnel() {
        let (p1, p2, p3) = (free_port(), free_port(), free_port());
        let (_d, mut app) = app_with_import(vec![show(p1), show(p2), show(p3)]);
        let configs = app.validated_import_batch().expect("batch should validate");
        assert_eq!(configs.len(), 3, "one TunnelConfig per ticked row");
        let ports: Vec<u16> = configs.iter().map(|c| c.local_port).collect();
        assert_eq!(ports, vec![p1, p2, p3], "free ports must be imported verbatim");
        assert!(configs.iter().all(|c| c.ssh_host == "dev-box"));
        assert!(configs.iter().all(|c| c.name.contains("dev-box")), "names should identify the source");
    }

    #[test]
    fn unticked_rows_are_not_created() {
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_import(vec![show(p1), show(p2)]);
        // The table starts fully ticked; untick the first row.
        app.form.as_mut().unwrap().import.rows[0].checked = false;
        let configs = app.validated_import_batch().unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].local_port, p2);
    }

    #[test]
    fn unchecking_everything_yields_no_batch() {
        let (_d, mut app) = app_with_import(vec![show(free_port())]);
        app.form.as_mut().unwrap().import.uncheck_all_visible();
        assert!(app.validated_import_batch().is_none());
        app.commit_form_create_from_import();
        assert!(app.tunnels.is_empty(), "nothing ticked means nothing created");
        assert!(
            app.form.as_ref().unwrap().errors.iter().any(|e| e.contains("没有")),
            "the form should say why: {:?}",
            app.form.as_ref().unwrap().errors
        );
    }

    #[test]
    fn a_collapsed_pane_hands_the_submit_back_to_the_single_form() {
        // The pane opens by default now, so "there are rows" is no longer the
        // same thing as "the user is working with them". Someone who collapses
        // the pane on purpose and types one forward by hand must get that one
        // tunnel — not "没有勾选任何转发", and not the batch.
        let (_d, mut app) = app_with_import(vec![show(free_port())]);
        app.close_import_pane();
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("manual");
            f.remote_host.set("10.0.0.9");
            f.remote_port.set("8080");
            f.local_port.set(free_port().to_string());
        }
        app.submit_form();
        assert_eq!(app.tunnels.len(), 1, "the single form creates exactly one tunnel");
        assert_eq!(app.tunnels[0].cfg.name, "manual", "and it is the one the user typed");
        assert_ne!(app.tunnels[0].cfg.note, "来自 ~/.ssh/config 第 1 行", "not an imported row");
    }

    #[test]
    fn an_open_pane_still_wins_over_a_perfectly_valid_single_form() {
        // The counterpart: while the pane is on screen, its ticks are the intent.
        // Otherwise filling the fields too would silently create one tunnel
        // instead of the batch the user is looking at.
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_import(vec![show(p1), show(p2)]);
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("manual");
            f.remote_host.set("10.0.0.9");
            f.remote_port.set("8080");
            f.local_port.set(free_port().to_string());
        }
        app.submit_form();
        assert_eq!(app.tunnels.len(), 2, "the visible batch decides");
        assert!(app.tunnels.iter().all(|t| t.cfg.name != "manual"));
    }

    #[test]
    fn submitting_with_imports_creates_the_whole_batch_and_persists() {
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_import(vec![show(p1), show(p2)]);
        // Fill the single-tunnel fields so the form itself is valid too: the
        // import path must win over them, not fight with them.
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("ignored");
            f.local_port.set("19000");
            f.remote_port.set("80");
        }
        app.submit_form();
        assert_eq!(app.tunnels.len(), 2, "one tunnel per ticked row, form fields ignored");
        assert!(app.form.is_none(), "the form closes on success");
        let reloaded = crate::config::load(&app.paths.config).unwrap();
        assert_eq!(reloaded.tunnels.len(), 2, "the batch must be persisted, not just in memory");
        let ports: Vec<u16> = reloaded.tunnels.iter().map(|t| t.local_port).collect();
        assert_eq!(ports, vec![p1, p2]);
        assert!(reloaded.tunnels.iter().all(|t| t.enabled), "created tunnels carry user intent to run");
    }

    #[test]
    fn the_cursor_lands_on_the_first_created_tunnel() {
        let (_d, mut app) = app_with_import(vec![show(free_port()), show(free_port())]);
        app.submit_form();
        let first_id = app.cfg.tunnels.iter().map(|t| t.id).min().unwrap();
        assert_eq!(app.selected_id(), Some(first_id), "the user should land where the work happened");
    }

    #[test]
    fn a_remote_forward_imports_with_the_ends_the_right_way_round() {
        // `-R` listens on the server and lands locally, so the config's local
        // side is the target and the remote side is the listener. Getting this
        // backwards would silently forward the wrong port.
        let mut fwd = show(7000);
        fwd.kind = ForwardKind::Remote;
        fwd.listen_addr = "127.0.0.1".into();
        fwd.listen_port = 7000;
        fwd.target_host = "127.0.0.1".into();
        fwd.target_port = 22;
        let (_d, mut app) = app_with_import(vec![fwd]);
        let cfg = app.validated_import_batch().unwrap().remove(0);
        assert_eq!(cfg.kind, ForwardKind::Remote);
        assert_eq!(cfg.remote_port, 7000, "the remote side is the listener");
        assert_eq!(cfg.local_port, 22, "the local side is the target");
    }

    #[test]
    fn a_dynamic_import_needs_no_remote_target() {
        let mut fwd = show(1080);
        fwd.kind = ForwardKind::Dynamic;
        fwd.target_host = String::new();
        fwd.target_port = 0;
        let (_d, mut app) = app_with_import(vec![fwd]);
        let cfg = app.validated_import_batch().unwrap().remove(0);
        assert_eq!(cfg.kind, ForwardKind::Dynamic);
        assert_eq!(cfg.local_port, 1080);
        assert!(config::validate_tunnel(&cfg).is_ok(), "a SOCKS forward has no remote side");
    }

    #[test]
    fn imported_names_do_not_collide_with_each_other_or_with_existing_ones() {
        // Two rows suggesting the same name (two host blocks, one port) plus a
        // saved tunnel already using it.
        let mut a = show(18001);
        a.host_alias = "dev-box".into();
        let mut b = show(18001);
        b.kind = ForwardKind::Local;
        let (_d, mut app) = app_with_import(vec![a, b]);
        app.cfg.tunnels.push(TunnelConfig {
            id: 99,
            name: "dev-box-L-18001".into(),
            kind: ForwardKind::Local,
            ssh_host: "other".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 18500,
            remote_host: "10.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        });
        let configs = app.validated_import_batch().unwrap();
        let names: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "names must be unique: {names:?}");
        assert!(!names.contains(&"dev-box-L-18001"), "must not reuse a saved tunnel's name");
    }

    #[test]
    fn the_overlap_guard_sees_wildcard_binds() {
        // 0.0.0.0 covers every interface, so it collides with a loopback bind on
        // the same port — the same rule the config-level validator applies.
        let (_d, app) = app_fresh();
        let p = free_port();
        let mut a = app.config_from_forward(1, &show(p), &[]);
        a.local_addr = "0.0.0.0".into();
        let mut b = app.config_from_forward(2, &show(p), &[]);
        b.local_port = a.local_port;
        b.local_addr = "127.0.0.1".into();
        assert!(listen_collision(&a, &b).is_some(), "the overlap guard must catch a wildcard bind");

        // Different ports never collide.
        let mut c = app.config_from_forward(3, &show(p), &[]);
        c.local_port = a.local_port.wrapping_add(1);
        assert!(listen_collision(&a, &c).is_none());
    }

    #[test]
    fn occupied_local_ports_are_steered_away_from() {
        // The ssh config knows nothing about ports sshpf already holds; importing
        // must not create a tunnel that cannot bind.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = listener.local_addr().unwrap().port();
        let (_d, mut app) = app_with_import(vec![show(busy)]);
        let configs = app.validated_import_batch().unwrap();
        assert_ne!(configs[0].local_port, busy, "the imported port is taken and must move");
        assert!(configs[0].local_port > busy, "the scan walks upwards: {}", configs[0].local_port);
        drop(listener);
    }

    #[test]
    fn ports_claimed_by_other_tunnels_are_also_avoided() {
        // A stopped tunnel is not listening, but starting it later would collide.
        let (_d, mut app) = app_fresh();
        app.cfg.tunnels.push(TunnelConfig {
            id: 1,
            name: "other".into(),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 18042,
            remote_host: "10.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        });
        app.tunnels.push(TunnelView::new(app.cfg.tunnels[0].clone()));
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        let table = ImportTable::new(vec![show(18042)]);
        app.form.as_mut().unwrap().set_import_table("dev-box", table);
        let cfg = app.validated_import_batch().unwrap().remove(0);
        assert_ne!(cfg.local_port, 18042, "must not fight over a port another tunnel owns");
    }

    #[test]
    fn two_rows_in_one_batch_never_get_the_same_local_port() {
        // Both rows ask for the same listen port (possible when `Host *` repeats
        // a specific block's forward). The second must be steered elsewhere
        // instead of handing two tunnels the same port.
        let p = free_port();
        let (_d, mut app) = app_with_import(vec![show(p), show(p)]);
        let configs = app.validated_import_batch().unwrap();
        assert_eq!(configs.len(), 2);
        assert_ne!(configs[0].local_port, configs[1].local_port, "the scan must account for the batch itself");
    }

    #[test]
    fn an_empty_host_field_is_reported_not_created() {
        let (_d, mut app) = app_with_import(vec![show(free_port())]);
        app.form.as_mut().unwrap().ssh_host.set("   ");
        assert!(app.validated_import_batch().is_none());
        assert!(
            app.form.as_ref().unwrap().errors.iter().any(|e| e.contains("SSH 主机")),
            "{:?}",
            app.form.as_ref().unwrap().errors
        );
    }

    #[test]
    fn the_import_note_records_where_the_row_came_from() {
        let (_d, mut app) = app_with_import(vec![show(free_port())]);
        let cfg = app.validated_import_batch().unwrap().remove(0);
        assert!(cfg.note.contains("~/.ssh/config"), "note should be traceable: {}", cfg.note);
        assert!(cfg.note.contains('1'), "note should cite the line: {}", cfg.note);
    }

    // -- filter plumbing through the App -----------------------------------

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

    #[test]
    fn created_tunnels_are_not_immediately_failed_by_a_stale_status() {
        // `commit_import_batch` starts each tunnel; the status must at least move
        // out of Stopped, proving the start path ran.
        let (_d, mut app) = app_with_import(vec![show(free_port())]);
        app.submit_form();
        assert!(
            !matches!(app.tunnels[0].status, TunnelStatus::Stopped),
            "creating should attempt to run the tunnel: {:?}",
            app.tunnels[0].status
        );
    }
}

#[cfg(test)]
mod port_planning_tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};

    /// An importer config for `forward`, given a set of ports that are "taken".
    fn plan(forward: &SshForward, taken: &[u16]) -> TunnelConfig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("c.toml"), state: dir.path().join("s.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        let app = App::new(paths);
        app.config_from_forward(1, forward, taken)
    }

    fn local(port: u16) -> SshForward {
        SshForward {
            kind: ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port: port,
            target_host: "10.0.0.1".into(),
            target_port: 8080,
            host_alias: "web".into(),
            line: 1,
            raw: String::new(),
        }
    }

    #[test]
    fn an_imported_privileged_port_gets_an_unprivileged_local_port() {
        // Regression: the importer used to scan upwards from the privileged port
        // and land on *another* privileged one (80 -> 280), which then failed at
        // bind time with a permission error the user could not explain.
        let cfg = plan(&local(80), &[]);
        assert!(!cfg.uses_privileged_port(), "imported local port {} needs root", cfg.local_port);
        assert!(cfg.local_port >= 1024);
    }

    #[test]
    fn every_privileged_import_can_actually_bind() {
        // Exhaustive across the privileged range: no result may need root.
        for port in [1u16, 22, 80, 443, 587, 993, 1023] {
            let cfg = plan(&local(port), &[]);
            assert!(!cfg.uses_privileged_port(), "remote {port} imported privileged local {}", cfg.local_port);
        }
    }

    #[test]
    fn a_taken_import_port_is_steered_upwards() {
        let cfg = plan(&local(18001), &[18001]);
        assert_ne!(cfg.local_port, 18001);
        assert!(cfg.local_port > 18001, "must bump up, got {}", cfg.local_port);
    }
}

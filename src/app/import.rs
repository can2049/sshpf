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
use crate::help;
use crate::model::{self, ForwardKind, SshForward, StatusMsg, TunnelConfig, TunnelView, now_unix};
use crate::ssh;

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
        let ends = forward_ends(&self.cfg.defaults.bind, forward, taken);
        TunnelConfig {
            id,
            name: self.cfg.unique_name(&forward.suggested_name(), None),
            kind: forward.kind,
            ssh_host: forward.host_alias.clone(),
            local_addr: ends.local_addr,
            local_port: ends.local_port,
            remote_host: ends.remote_host,
            remote_port: ends.remote_port,
            enabled: true,
            auto_restart: self.cfg.defaults.auto_restart,
            ssh_options: Vec::new(),
            note: import_note(forward.line),
            created_at: now_unix(),
        }
    }

    /// Validate the form and, for a create, the imported rows as a batch.
    ///
    /// Returns the configs to create, or reports the problems into the form and
    /// creates nothing. Three steps, each its own function: [`Self::import_intent`]
    /// decides *whether* there is a batch at all, [`Self::allocate_import_ids`]
    /// reserves the id range, and [`Self::build_import_batch`] turns each ticked
    /// row into a config and collects every problem.
    pub fn validated_import_batch(&mut self) -> Option<Vec<TunnelConfig>> {
        let (ssh_host, forwards) = self.import_intent()?;
        let (base_id, last_id) = self.allocate_import_ids(forwards.len())?;
        let configs = self.build_import_batch(&ssh_host, &forwards, base_id, last_id);
        match configs {
            Ok(configs) => Some(configs),
            Err(errors) => {
                crate::eventlog::import_rejected(&errors);
                self.set_form_errors(errors);
                None
            }
        }
    }

    /// The user's intent, read off the open form: which host, and which rows are
    /// ticked. `None` means "there is nothing to do", which is not an error.
    fn import_intent(&mut self) -> Option<(String, Vec<SshForward>)> {
        let form = self.form.clone()?;
        // Importing is a create-time convenience: silently turning an edit into a
        // batch create would be a data-loss bug.
        if matches!(form.mode, FormMode::Edit(_)) {
            return None;
        }
        let forwards = form.import.checked_forwards();
        if forwards.is_empty() {
            return None;
        }
        let ssh_host = form.ssh_host.value().trim().to_string();
        if ssh_host.is_empty() {
            self.set_form_errors(vec![help::bi("SSH 主机不能为空", "SSH host must not be empty")]);
            return None;
        }
        Some((ssh_host, forwards))
    }

    /// Reserve the contiguous id range for `rows` ticked forwards.
    ///
    /// Ids are allocated up front and checked, not taken from an unbounded
    /// `(next_id..)` range: `max_id + 1` overflows for a hand-edited
    /// `id = u64::MAX`, and wrapping the range would reuse existing ids. Returns
    /// `(first, last)` inclusive.
    fn allocate_import_ids(&mut self, rows: usize) -> Option<(crate::model::TunnelId, crate::model::TunnelId)> {
        let base_id = match self.next_tunnel_id() {
            Some(id) => id,
            None => {
                self.set_form_errors(vec!["无法分配新的转发编号，请检查配置中的 id".to_string()]);
                return None;
            }
        };
        let span = (rows as u64).saturating_sub(1);
        let Some(last_id) = base_id.checked_add(span) else {
            crate::eventlog::id_space_exhausted(self.cfg.max_id());
            self.set_form_errors(vec!["无法为全部转发分配编号，请减少一次导入的条目数".to_string()]);
            return None;
        };
        Some((base_id, last_id))
    }

    /// Turn every ticked row into a validated config, or collect *all* the
    /// problems and create nothing.
    ///
    /// All-or-nothing on purpose: a partial write (three tunnels created, then a
    /// validation error on the fourth) would leave a state the user never asked
    /// for.
    fn build_import_batch(
        &mut self,
        ssh_host: &str,
        forwards: &[SshForward],
        base_id: crate::model::TunnelId,
        last_id: crate::model::TunnelId,
    ) -> Result<Vec<TunnelConfig>, Vec<String>> {
        // Ports already claimed by other managed tunnels, so the batch does not
        // fight over one port with the tunnels it is replacing. Grows as we
        // assign, so two rows cannot be handed the same local port.
        let mut taken: Vec<u16> = self.taken_local_ports();
        let mut out: Vec<TunnelConfig> = Vec::with_capacity(forwards.len());
        let mut errors: Vec<String> = Vec::new();

        for (next_id, forward) in (base_id..=last_id).zip(forwards.iter()) {
            let cfg = self.import_row_config(next_id, forward, ssh_host, &taken, &out, &mut errors);
            if cfg.kind != ForwardKind::Remote {
                taken.push(cfg.local_port);
            }
            out.push(cfg);
        }

        if errors.is_empty() { Ok(out) } else { Err(errors) }
    }

    /// Build one row of the batch and append anything wrong with it to `errors`.
    ///
    /// `out` is the batch built so far — needed because two imported rows can
    /// describe the same listen port (a `Host *` default plus a specific block),
    /// and creating both would fail at bind time instead of here.
    fn import_row_config(
        &self,
        id: crate::model::TunnelId,
        forward: &SshForward,
        ssh_host: &str,
        taken: &[u16],
        out: &[TunnelConfig],
        errors: &mut Vec<String>,
    ) -> TunnelConfig {
        let mut cfg = self.config_from_forward(id, forward, taken);
        cfg.ssh_host = ssh_host.to_string();
        // Keep names unique within the batch itself, not just against the
        // already-saved tunnels.
        cfg.name = unique_within(&cfg.name, out.iter().map(|c| c.name.clone()).collect());
        if let Err(e) = config::validate_tunnel(&cfg) {
            // `e` is the Chinese validator message (`src/config.rs`, not our
            // scope); append the English category so the row is bilingual.
            errors.push(format!("{}: {e} / Invalid definition", cfg.name));
        }
        // Collision against already-saved tunnels, then against the rest of this
        // batch.
        let conflict =
            self.listen_conflict(&cfg).or_else(|| out.iter().find_map(|other| listen_collision(&cfg, other)));
        if let Some(conflict) = conflict {
            errors.push(help::bi(
                &format!("{}: 监听地址与「{conflict}」冲突", cfg.name),
                &format!("{}: listen endpoint collides with \"{conflict}\"", cfg.name),
            ));
        }
        cfg
    }

    /// Report validation problems into the open form (the batch creates nothing).
    fn set_form_errors(&mut self, errors: Vec<String>) {
        if let Some(f) = self.form.as_mut() {
            f.errors = errors;
        }
    }

    /// Create every validated tunnel, persist once, and start them.
    pub(crate) fn commit_import_batch(&mut self, configs: Vec<TunnelConfig>) {
        let n = configs.len();
        let first_id = configs.first().map(|c| c.id);
        let host = configs.first().map(|c| c.ssh_host.clone()).unwrap_or_default();
        crate::eventlog::import_committed(&host, n);
        for cfg in &configs {
            crate::eventlog::tunnel_created(cfg, "import");
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
            self.set_status(StatusMsg::ok(help::bi(
                &format!("已创建 1 条转发：{name}"),
                &format!("Created 1 forward: {name}"),
            )));
        } else {
            self.set_status(StatusMsg::ok(help::bi(
                &format!("已创建 {n} 条转发（来自 ~/.ssh/config）"),
                &format!("Created {n} forwards (from ~/.ssh/config)"),
            )));
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
                    f.errors = vec![help::bi("没有勾选任何转发", "No forward is ticked")];
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

/// Which side of an imported forward becomes which side of the config.
struct ForwardEnds {
    local_addr: String,
    local_port: u16,
    remote_host: String,
    remote_port: u16,
}

/// Map an imported ssh-config forward onto a config's local/remote sides.
///
/// The mapping is not symmetric, which is why it is one function: `-L`/`-D`
/// listen locally, but `-R` listens *on the server* and lands locally, so the ssh
/// config's listen side becomes the config's remote side. Getting this backwards
/// would silently forward the wrong port.
///
/// `-L`/`-D` bind locally, so only those need a collision-free port; that scan
/// uses `plan_local_port` (shared with the form) and `taken` so two rows cannot
/// be handed the same port.
fn forward_ends(bind: &str, forward: &SshForward, taken: &[u16]) -> ForwardEnds {
    let listen_addr = if forward.listen_addr.is_empty() { bind.to_string() } else { forward.listen_addr.clone() };
    let mut ends = match forward.kind {
        ForwardKind::Local => ForwardEnds {
            local_addr: listen_addr,
            local_port: forward.listen_port,
            remote_host: forward.target_host.clone(),
            remote_port: forward.target_port,
        },
        ForwardKind::Remote => ForwardEnds {
            local_addr: forward.target_host.clone(),
            local_port: forward.target_port,
            remote_host: listen_addr,
            remote_port: forward.listen_port,
        },
        ForwardKind::Dynamic => ForwardEnds {
            local_addr: listen_addr,
            local_port: forward.listen_port,
            remote_host: String::new(),
            remote_port: 0,
        },
    };
    if forward.kind != ForwardKind::Remote {
        // A privileged port is never scanned upwards from itself, which would
        // spend the whole budget inside the range that needs root.
        let addr = ends.local_addr.clone();
        if let Some((port, _)) = model::plan_local_port(ends.local_port, taken, |c| ssh::port_available(&addr, c)) {
            ends.local_port = port;
        }
        // Nothing usable found: keep the requested port so validation reports the
        // real problem instead of silently using port 0.
    }
    ends
}

/// The `note` that makes an imported tunnel traceable back to its source line.
fn import_note(line: usize) -> String {
    help::bi(&format!("来自 ~/.ssh/config 第 {line} 行"), &format!("From ~/.ssh/config line {line}"))
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
    use crate::model::ImportTable;
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

    // -- the default: auto-open, everything ticked -------------------------

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

#[cfg(test)]
mod hardening_tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::TunnelStatus;
    use crate::model::{ImportColumn, ImportTable};

    fn app_fresh() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        (dir, App::new(paths))
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

    fn app_with_import(rows: Vec<SshForward>) -> (tempfile::TempDir, App) {
        let (dir, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        let table = ImportTable::new(rows);
        app.form.as_mut().unwrap().set_import_table("dev-box", table);
        app.form.as_mut().unwrap().import_open = true;
        (dir, app)
    }

    /// Give the app a saved tunnel with `id`, so `next_id()` is deterministic.
    fn app_with_max_id(id: crate::model::TunnelId) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let cfg = ConfigFile {
            tunnels: vec![TunnelConfig {
                id,
                name: "existing".into(),
                kind: ForwardKind::Local,
                ssh_host: "h".into(),
                local_addr: "127.0.0.1".into(),
                local_port: 18001,
                remote_host: "127.0.0.1".into(),
                remote_port: 22,
                enabled: false,
                auto_restart: false,
                ssh_options: vec![],
                note: String::new(),
                created_at: 1,
            }],
            ..ConfigFile::default()
        };
        crate::config::save(&paths.config, &cfg).unwrap();
        (dir, App::new(paths))
    }

    // -- port planning at the edges of the port space ----------------------

    #[test]
    fn a_port_at_the_top_of_the_range_does_not_wrap_to_zero() {
        // `plan_local_port` scans `remote.max(1024)..=u16::MAX` (bounded by
        // PORT_SCAN_LIMIT). At the very top there is no room above, so the
        // planner must give up rather than wrap to 0 — a port-0 listener is not
        // a "next free port", it silently asks the OS to choose one.
        let (dir, mut app) = app_with_import(vec![show(u16::MAX)]);
        let configs = app.validated_import_batch().expect("validation should still produce a batch");
        assert_eq!(configs.len(), 1);
        assert_ne!(configs[0].local_port, 0, "a wrapped port must never reach the config");
        drop(app);

        // And the pure planner agrees: nothing free at the top means None.
        let planned = crate::model::plan_local_port(u16::MAX, &[], |_| false);
        assert!(planned.is_none(), "exhausted port space must report None, not wrap");
        drop(dir);
    }

    #[test]
    fn an_already_taken_top_port_is_not_reused() {
        // Every candidate above the requested port is exhausted, so the planner
        // must decline; the config then falls back to the requested port so
        // validation can report the real problem instead of silently using 0.
        let (dir, mut app) = app_with_import(vec![show(u16::MAX)]);
        let configs = app.validated_import_batch().expect("still a batch");
        assert_ne!(configs[0].local_port, 0, "giving up must not mean port 0");
        drop(app);
        drop(dir);
    }

    #[test]
    fn imported_ids_do_not_overflow_and_stay_unique() {
        // The batch used to iterate an unbounded `(next_id..)` range, and the
        // ids came from `max_id + 1` — an overflow for `u64::MAX`.
        let (_d, mut app) = app_with_max_id(100);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.form.as_mut().unwrap().set_import_table("dev-box", ImportTable::new(vec![show(18101), show(18102)]));
        let configs = app.validated_import_batch().expect("batch");
        assert_eq!(configs.len(), 2);
        let ids: Vec<_> = configs.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![101, 102]);
        assert_ne!(ids[0], ids[1], "two rows must not share an id");
    }

    #[test]
    fn an_exhausted_id_space_fails_the_import_batch_without_panicking() {
        let (_d, mut app) = app_with_max_id(u64::MAX);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.form.as_mut().unwrap().set_import_table("dev-box", ImportTable::new(vec![show(18101)]));
        assert!(app.validated_import_batch().is_none(), "no ids left means no batch");
        let errors = app.form.as_ref().unwrap().errors.clone();
        assert!(errors.iter().any(|e| e.contains("编号")), "the user must be told why: {errors:?}");
    }

    #[test]
    fn a_batch_that_would_run_past_the_id_space_is_refused_whole() {
        // u64::MAX - 1 leaves room for exactly one id; two rows must not be
        // silently truncated into a partial import.
        let (_d, mut app) = app_with_max_id(u64::MAX - 1);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.form.as_mut().unwrap().set_import_table("dev-box", ImportTable::new(vec![show(18101), show(18102)]));
        assert!(app.validated_import_batch().is_none(), "a partial batch is worse than none");
        let errors = app.form.as_ref().unwrap().errors.clone();
        assert!(errors.iter().any(|e| e.contains("编号")), "{errors:?}");
    }

    #[test]
    fn one_row_fits_into_the_last_free_id() {
        let (_d, mut app) = app_with_max_id(u64::MAX - 1);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.form.as_mut().unwrap().set_import_table("dev-box", ImportTable::new(vec![show(18101)]));
        let configs = app.validated_import_batch().expect("the last id is usable");
        assert_eq!(configs[0].id, u64::MAX);
    }

    #[test]
    fn a_forward_with_no_listen_port_is_never_imported() {
        // A `DynamicForward` / unix socket with no numeric port: listing it is
        // fine, but it must not reach `plan_local_port(0, ..)` as a request.
        let mut broken = show(0);
        broken.listen_port = 0;
        let (_d, mut app) = app_with_import(vec![broken, show(18103)]);
        let configs = app.validated_import_batch().expect("the good row is still importable");
        assert_eq!(configs.len(), 1, "the port-less row must be skipped, not created with port 0");
        // Not asserted to be exactly 18103: the planner probes real sockets and
        // may bump a busy port, which would make this flaky on a loaded machine.
        assert_ne!(configs[0].local_port, 0, "an imported config may never listen on port 0");
        assert!(config::validate_tunnel(&configs[0]).is_ok());
    }

    #[test]
    fn every_imported_config_passes_validation() {
        // The batch's whole promise: validate up front so a partial write cannot
        // happen. A config that fails validation here would contradict it.
        let mut remote = show(7000);
        remote.kind = ForwardKind::Remote;
        remote.listen_addr = "127.0.0.1".into();
        remote.listen_port = 7000;
        remote.target_host = "127.0.0.1".into();
        remote.target_port = 22;
        let mut dynamic = show(1080);
        dynamic.kind = ForwardKind::Dynamic;
        dynamic.target_host = String::new();
        dynamic.target_port = 0;

        let (_d, mut app) = app_with_import(vec![show(18104), remote, dynamic]);
        let configs = app.validated_import_batch().expect("batch");
        assert_eq!(configs.len(), 3);
        for cfg in &configs {
            assert!(config::validate_tunnel(cfg).is_ok(), "{cfg:?} must validate before anything is written");
        }
    }

    #[test]
    fn commit_import_batch_with_an_empty_vec_is_a_no_op() {
        // `configs.first()` would be `None`; the cursor bookkeeping must cope.
        let (_d, mut app) = app_fresh();
        app.commit_import_batch(Vec::new());
        assert!(app.tunnels.is_empty());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn toggling_a_row_that_is_not_visible_is_a_no_op() {
        // Mouse clicks carry a raw row index from the hit map, which can be
        // stale after a filter change. It must not index out of bounds.
        let (_d, mut app) = app_with_import(vec![show(18105), show(18106)]);
        app.begin_import_filter(ImportColumn::Listen);
        app.import_filter_char('1');
        app.import_filter_char('8');
        app.import_filter_char('1');
        app.import_filter_char('0');
        app.import_filter_char('5');
        let before = app.form.as_ref().unwrap().import.checked_count();
        app.import_toggle_at(999); // raw index that cannot exist
        app.import_select_at(999);
        assert_eq!(app.form.as_ref().unwrap().import.checked_count(), before, "a stale click must not tick anything");
        assert!(app.form.as_ref().unwrap().import.cursor_row().is_some() || before == 0);
    }

    #[test]
    fn the_cursor_lands_on_the_first_imported_tunnel_even_with_a_filter() {
        let (_d, mut app) = app_with_import(vec![show(18107), show(18108)]);
        app.submit_form();
        assert_eq!(app.tunnels.len(), 2);
        assert!(app.selected_tunnel().is_some(), "the cursor must land on something");
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Starting { .. } | TunnelStatus::Failed { .. }));
    }
}

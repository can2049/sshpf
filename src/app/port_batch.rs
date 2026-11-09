//! Turning ticked remote ports into tunnels.
//!
//! The picker (see [`super::port_scan`]) is a *suggestion* surface: one `enter`
//! fills the field with the port under the cursor, and that is the whole
//! interaction for "make one tunnel". Ticking several rows is a different
//! gesture — the user is saying "these services, each on its own forward" — and
//! this file is where that becomes N `TunnelConfig`s.
//!
//! Kept apart from [`super::import`] even though both produce a batch, because
//! the two batches differ in what they trust: an import batch copies ports the
//! ssh config already fixes, while a port batch *chooses* the remote ports from
//! what the peer reports and must therefore re-derive the local side with
//! `plan_local_port`, exactly like the manual form does.
//!
//! Two invariants worth keeping:
//!
//! * **No duplicate forwards.** A ticked port whose forward is already saved is
//!   skipped and reported, never created twice: the local port is planned from
//!   what is free, so the same intent can otherwise be produced a second time
//!   under a bumped local port.
//! * **All or nothing.** A partial write (three tunnels created, then a
//!   validation error on the fourth) leaves a state the user never asked for, so
//!   everything is validated before anything is written.
//! * **Legible derivation.** Every tunnel is named after the form's name plus
//!   its own remote port, and carries a note saying where the port came from.
//!   A batch of forwards called "tunnel-2, tunnel-3, tunnel-4" cannot be
//!   matched to a service afterwards.
//!
//! Belongs here: validation, naming, local-port planning, and the commit.
//! Does not belong: the scan itself, row state, rendering, key dispatch.

use crate::app::{App, FormMode};
use crate::config;
use crate::eventlog;
use crate::help;
use crate::model::{self, ForwardKind, StatusMsg, TunnelConfig, TunnelView, now_unix};
use crate::ssh;

impl App {
    /// Whether saving the form should create the ticked ports as a batch.
    ///
    /// Three conditions, and the third is the important one: the pane must be
    /// **visible**. A user who collapsed it to type by hand must get the single
    /// tunnel they are looking at, not a batch they cannot see — the same rule
    /// the import pane follows.
    pub(crate) fn port_batch_intent(&self) -> bool {
        let Some(form) = self.form.as_ref() else {
            return false;
        };
        let Some(pick) = self.ports.as_ref() else {
            return false;
        };
        matches!(form.mode, FormMode::Create) && pick.open && pick.multi && pick.checked_count() > 0
    }

    /// Validate every ticked port into a config, or report all problems and
    /// create nothing.
    ///
    /// The ticked ports whose forward is already saved are left out of the
    /// returned configs and remembered in [`App::pending_port_skipped`], because
    /// the submit path calls `validated_port_batch()` and then
    /// `commit_port_batch(configs)` — keeping that signature is what lets the
    /// commit's status line still say how many rows were skipped.
    pub(crate) fn validated_port_batch(&mut self) -> Option<Vec<TunnelConfig>> {
        let batch = self.validated_port_batch_outcome()?;
        if batch.configs.is_empty() {
            // Every ticked port is already saved. That is success with a no-op
            // body, not "no port is ticked": say so and close, instead of
            // reporting a field the user never left empty.
            self.set_status(StatusMsg::ok(help::bi(
                &format!("{} 条转发已存在，未重复创建", batch.skipped.len()),
                &format!("{} forward(s) already exist; nothing was created", batch.skipped.len()),
            )));
            self.close_overlay();
            return None;
        }
        self.pending_port_skipped = batch.skipped;
        Some(batch.configs)
    }

    /// Like [`Self::validated_port_batch`], but keeps the ticked ports that were
    /// skipped because the forward already exists.
    pub(crate) fn validated_port_batch_outcome(&mut self) -> Option<PortBatch> {
        let form = self.form.clone()?;
        let pick = self.ports.as_ref()?;
        let ports = pick.checked_ports();
        if ports.is_empty() {
            return None;
        }
        let (base_id, last_id) = self.allocate_port_ids(ports.len())?;
        match self.build_port_batch(&form, &ports, base_id, last_id) {
            Ok(batch) => Some(batch),
            Err(errors) => {
                eventlog::port_batch_rejected(&errors);
                if let Some(f) = self.form.as_mut() {
                    f.errors = errors;
                }
                None
            }
        }
    }

    /// Reserve the contiguous id range for `rows` tunnels.
    ///
    /// Same reasoning as the import batch: ids are checked up front instead of
    /// taken from an unbounded range, because `max_id + 1` overflows for a
    /// hand-edited `id = u64::MAX`.
    fn allocate_port_ids(&mut self, rows: usize) -> Option<(crate::model::TunnelId, crate::model::TunnelId)> {
        let base_id = match self.next_tunnel_id() {
            Some(id) => id,
            None => {
                let msg = "无法分配新的转发编号，请检查配置中的 id".to_string();
                if let Some(f) = self.form.as_mut() {
                    f.errors = vec![msg];
                }
                return None;
            }
        };
        match base_id.checked_add((rows as u64).saturating_sub(1)) {
            Some(last_id) => Some((base_id, last_id)),
            None => {
                eventlog::id_space_exhausted(self.cfg.max_id());
                let msg = "无法为全部端口分配编号，请减少一次创建的端口数".to_string();
                if let Some(f) = self.form.as_mut() {
                    f.errors = vec![msg];
                }
                None
            }
        }
    }

    /// Turn every ticked port into a validated config, or collect *all* the
    /// problems.
    ///
    /// A port whose forward already exists is skipped instead of created: the
    /// local port is planned from what is free, so re-scanning a peer can propose
    /// the same intent under a different local port. The first ticked port of a
    /// same-identity pair wins; every skipped one is reported back.
    fn build_port_batch(
        &mut self,
        form: &crate::app::FormState,
        ports: &[u16],
        base_id: crate::model::TunnelId,
        last_id: crate::model::TunnelId,
    ) -> Result<PortBatch, Vec<String>> {
        let ssh_host = form.ssh_host.value().trim().to_string();
        if ssh_host.is_empty() {
            return Err(vec![help::bi("SSH 主机不能为空", "SSH host must not be empty")]);
        }
        let remote_host = form.remote_host.value().trim().to_string();
        if remote_host.is_empty() {
            return Err(vec![help::bi("远端地址不能为空", "Remote address must not be empty")]);
        }
        // Grow as ports are assigned, so two rows can never be handed one local
        // port: a batch that collides with itself fails at bind time with an error
        // that points at the wrong row.
        let mut taken = self.taken_local_ports();
        let mut out: Vec<TunnelConfig> = Vec::with_capacity(ports.len());
        let mut skipped: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for (id, port) in (base_id..=last_id).zip(ports.iter().copied()) {
            let mut cfg = port_config(form, id, &ssh_host, &remote_host, port, &taken);
            cfg.name = unique_within(&cfg.name, out.iter().map(|c| c.name.clone()).collect());
            cfg.local_port = plan_local(&cfg, &taken).unwrap_or(cfg.local_port);
            if let Some(existing) = self.duplicate_in_batch(&cfg, &out) {
                eventlog::action_duplicate_skipped("port-scan", &cfg.name, &existing);
                skipped.push(cfg.name);
                continue;
            }
            taken.push(cfg.local_port);
            if let Err(e) = config::validate_tunnel(&cfg) {
                errors.push(format!("{}: {e} / Invalid definition", cfg.name));
            }
            out.push(cfg);
        }
        if errors.is_empty() { Ok(PortBatch { configs: out, skipped }) } else { Err(errors) }
    }

    /// Create every validated tunnel, persist once, and start them.
    ///
    /// Takes the skipped rows recorded by [`Self::validated_port_batch`] so the
    /// status line can explain a batch smaller than the tick selection.
    pub(crate) fn commit_port_batch(&mut self, configs: Vec<TunnelConfig>) {
        let skipped = std::mem::take(&mut self.pending_port_skipped);
        self.commit_port_batch_with_skipped(configs, skipped);
    }

    /// Create the batch, reporting how many ticked ports were skipped as
    /// duplicates of a saved forward.
    pub(crate) fn commit_port_batch_with_skipped(&mut self, configs: Vec<TunnelConfig>, skipped: Vec<String>) {
        let n = configs.len();
        let first_id = configs.first().map(|c| c.id);
        for cfg in &configs {
            eventlog::tunnel_created(cfg, "port-scan");
            self.cfg.tunnels.push(cfg.clone());
            self.tunnels.push(TunnelView::new(cfg.clone()));
        }
        self.close_overlay();
        self.persist_quietly();
        for cfg in &configs {
            self.start_tunnel_with(cfg.id, "created");
        }
        self.focus_first_created(first_id);
        let ports: Vec<String> = configs.iter().map(|c| c.remote_port.to_string()).collect();
        let created = help::bi(
            &format!("已按扫描到的端口创建 {n} 条转发：{}", ports.join(" / ")),
            &format!("Created {n} forwards from the scanned ports: {}", ports.join(" / ")),
        );
        let text = if skipped.is_empty() {
            created
        } else {
            help::bi(
                &format!("{created}；跳过 {} 条重复转发", skipped.len()),
                &format!("{created}; skipped {} duplicate forward(s)", skipped.len()),
            )
        };
        self.set_status(StatusMsg::ok(text));
    }

    /// Put the cursor on the first created tunnel so the user lands on the work.
    fn focus_first_created(&mut self, first_id: Option<crate::model::TunnelId>) {
        let Some(id) = first_id else {
            return;
        };
        let vis = self.visible_indices();
        if let Some(pos) = vis.iter().position(|i| self.tunnels.get(*i).is_some_and(|t| t.cfg.id == id)) {
            self.selected = pos;
        }
        self.clamp_selection();
    }
}

/// The configs a ticked port selection produced, plus the rows dropped as
/// duplicates of an already-saved forward.
///
/// `skipped` is kept so "nothing ticked" and "everything ticked is already
/// saved" stay distinguishable: the first needs a form error, the second is a
/// successful no-op.
pub(crate) struct PortBatch {
    pub configs: Vec<TunnelConfig>,
    pub skipped: Vec<String>,
}

/// One config for a ticked port.
fn port_config(
    form: &crate::app::FormState,
    id: crate::model::TunnelId,
    ssh_host: &str,
    remote_host: &str,
    port: u16,
    _taken: &[u16],
) -> TunnelConfig {
    let base_name = form.name.value().trim();
    let name = if base_name.is_empty() { format!("{ssh_host}-L-{port}") } else { format!("{base_name}-{port}") };
    TunnelConfig {
        id,
        name,
        // A port scan only offers the `-L` direction: it lists what the peer can
        // reach, which is exactly what a local forward lands on.
        kind: ForwardKind::Local,
        ssh_host: ssh_host.to_string(),
        local_addr: form.local_addr.value().trim().to_string(),
        // Replaced by `plan_local` below; this is only the seed.
        local_port: port,
        remote_host: remote_host.to_string(),
        remote_port: port,
        enabled: true,
        auto_restart: form.auto_restart,
        ssh_options: Vec::new(),
        note: help::bi(
            &format!("远端端口 {port} 来自对端扫描"),
            &format!("Remote port {port} chosen from the peer scan"),
        ),
        created_at: now_unix(),
    }
}

/// The local port for one row of the batch: the same number when it is free,
/// otherwise the next one up.
fn plan_local(cfg: &TunnelConfig, taken: &[u16]) -> Option<u16> {
    let addr = if cfg.local_addr.is_empty() { "127.0.0.1" } else { cfg.local_addr.as_str() };
    model::plan_local_port(cfg.remote_port, taken, |candidate| ssh::port_available(addr, candidate))
        .map(|(port, _)| port)
}

/// A name not already used by `siblings` (the rest of the batch).
///
/// The saved config is checked separately by `ConfigFile::unique_name` on the
/// single-form path; a batch has no such call, so two rows with the same base
/// name would otherwise be created with identical names.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{PortPick, RemotePort, TunnelStatus};

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

    /// A create form with a host and a name, plus a picker holding `ports`.
    fn app_with_ports(ports: &[u16]) -> (tempfile::TempDir, App) {
        let (dir, mut app) = app_fresh();
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("dev-box");
            f.name.set("svc");
            f.remote_host.set("10.0.0.5");
        }
        let mut pick = PortPick::scanning("dev-box", true, 1);
        let rows = ports.iter().map(|p| RemotePort::new(*p, "127.0.0.1")).collect::<Vec<_>>();
        assert!(pick.install(1, rows, 0));
        app.ports = Some(pick);
        (dir, app)
    }

    #[test]
    fn one_tunnel_per_ticked_port_with_unique_names_and_ports() {
        let (p1, p2, p3) = (free_port(), free_port(), free_port());
        let (_d, mut app) = app_with_ports(&[p1, p2, p3]);
        app.ports.as_mut().unwrap().check_all();
        let configs = app.validated_port_batch().expect("the batch should validate");
        assert_eq!(configs.len(), 3);
        // `checked_ports` sorts, so the batch order is deterministic rather than
        // "whatever order the scan happened to report".
        let mut expected = vec![p1, p2, p3];
        expected.sort_unstable();
        assert_eq!(configs.iter().map(|c| c.remote_port).collect::<Vec<_>>(), expected);
        let names: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "names must be unique: {names:?}");
        assert!(names.iter().all(|n| n.contains("svc")), "each name keeps the typed base: {names:?}");
        assert!(names.iter().all(|n| *n != "svc"), "a batch disambiguates with the port: {names:?}");
    }

    #[test]
    fn a_batch_lands_on_the_ports_the_peer_reports() {
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_ports(&[p1, p2]);
        app.ports.as_mut().unwrap().check_all();
        let configs = app.validated_port_batch().unwrap();
        // A free remote port is reused locally: "same number on both ends" is what
        // people expect, and it is what the manual form does.
        for cfg in &configs {
            assert_eq!(cfg.local_port, cfg.remote_port, "a free port must be reused verbatim");
        }
        assert!(configs.iter().all(|c| c.kind == ForwardKind::Local), "a scan is the -L direction");
        assert!(configs.iter().all(|c| c.remote_host == "10.0.0.5"), "the typed target address is kept");
    }

    #[test]
    fn two_rows_never_share_a_local_port() {
        // Both ticked ports are the same number at the remote end is impossible
        // (they are unique), but the *local* ports can collide with a managed
        // tunnel — the second row must move up instead of failing at bind time.
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = busy.local_addr().unwrap().port();
        let (_d, mut app) = app_with_ports(&[taken]);
        app.ports.as_mut().unwrap().check_all();
        let configs = app.validated_port_batch().unwrap();
        assert_ne!(configs[0].local_port, taken, "a busy local port must be steered away from");
        assert!(configs[0].local_port > taken);
        drop(busy);
    }

    #[test]
    fn the_batch_is_exposed_only_while_the_pane_is_visible() {
        // Collapsing the list to type by hand must hand the submit back to the
        // single form: a batch nobody can see is a batch nobody asked for.
        let (_d, mut app) = app_with_ports(&[free_port()]);
        app.ports.as_mut().unwrap().check_all();
        assert!(app.port_batch_intent());
        app.ports.as_mut().unwrap().open = false;
        assert!(!app.port_batch_intent());
    }

    #[test]
    fn an_edit_never_batches() {
        // Editing one tunnel must stay one tunnel; silently creating N would be a
        // data-loss bug.
        let (_d, mut app) = app_fresh();
        {
            let cfg = TunnelConfig {
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
            };
            app.cfg.tunnels.push(cfg.clone());
            app.tunnels.push(TunnelView::new(cfg));
        }
        app.open_form_edit();
        let mut pick = PortPick::scanning("dev-box", false, 1);
        assert!(pick.install(1, vec![RemotePort::new(free_port(), "127.0.0.1")], 0));
        app.ports = Some(pick);
        assert!(!app.port_batch_intent(), "editing is one tunnel by definition");
        assert!(app.validated_port_batch().is_none());
    }

    #[test]
    fn a_read_only_picker_produces_no_batch() {
        let (_d, mut app) = app_with_ports(&[free_port()]);
        app.ports.as_mut().unwrap().multi = false;
        app.ports.as_mut().unwrap().check_all();
        assert_eq!(app.ports.as_ref().unwrap().checked_count(), 0, "a read-only list cannot be ticked");
        assert!(!app.port_batch_intent());
    }

    #[test]
    fn an_empty_tick_selection_produces_no_batch() {
        let (_d, mut app) = app_with_ports(&[free_port()]);
        assert!(!app.port_batch_intent(), "a fresh scan ticks nothing");
        assert!(app.validated_port_batch().is_none());
    }

    #[test]
    fn committing_creates_persists_and_starts_every_tunnel() {
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_ports(&[p1, p2]);
        app.ports.as_mut().unwrap().check_all();
        let configs = app.validated_port_batch().unwrap();
        app.commit_port_batch(configs);
        assert_eq!(app.tunnels.len(), 2);
        assert!(app.form.is_none(), "the form closes on success");
        assert!(app.ports.is_none(), "and the picker with it");
        let reloaded = crate::config::load(&app.paths.config).unwrap();
        assert_eq!(reloaded.tunnels.len(), 2, "the batch must be persisted, not just in memory");
        assert!(reloaded.tunnels.iter().all(|t| t.enabled));
        assert_eq!(app.selected_id(), reloaded.tunnels.iter().map(|t| t.id).min(), "cursor lands on the first");
        assert!(!matches!(app.tunnels[0].status, TunnelStatus::Stopped), "creating should attempt to run the tunnels");
    }

    #[test]
    fn a_missing_host_is_reported_not_created() {
        let (_d, mut app) = app_with_ports(&[free_port()]);
        app.form.as_mut().unwrap().ssh_host.set("   ");
        app.ports.as_mut().unwrap().check_all();
        assert!(app.validated_port_batch().is_none());
        let errors = app.form.as_ref().unwrap().errors.clone();
        assert!(errors.iter().any(|e| e.contains("SSH 主机")), "{errors:?}");
    }

    #[test]
    fn the_note_records_that_the_port_came_from_a_scan() {
        let (_d, mut app) = app_with_ports(&[free_port()]);
        app.ports.as_mut().unwrap().check_all();
        let cfg = app.validated_port_batch().unwrap().remove(0);
        assert!(cfg.note.contains("扫描"), "the note should say where the port came from: {}", cfg.note);
    }

    #[test]
    fn a_name_is_derived_when_the_form_left_it_empty() {
        let port = free_port();
        let (_d, mut app) = app_with_ports(&[port]);
        app.form.as_mut().unwrap().name.set("");
        app.ports.as_mut().unwrap().check_all();
        let cfg = app.validated_port_batch().unwrap().remove(0);
        assert!(cfg.name.contains("dev-box"), "the host stands in for a missing name: {}", cfg.name);
        assert!(cfg.name.contains(&port.to_string()), "and the port makes it unique: {}", cfg.name);
    }

    #[test]
    fn committing_an_empty_vec_is_a_no_op() {
        let (_d, mut app) = app_fresh();
        app.commit_port_batch(Vec::new());
        assert!(app.tunnels.is_empty());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn unique_within_avoids_repeats() {
        assert_eq!(unique_within("a", vec![]), "a");
        assert_eq!(unique_within("a", vec!["a".into()]), "a-2");
        assert_eq!(unique_within("a", vec!["a".into(), "a-2".into()]), "a-3");
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{PortPick, RemotePort, TunnelStatus};

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// A saved `-L` tunnel from `local_port` to `remote_host:remote_port`.
    fn saved(
        id: crate::model::TunnelId,
        name: &str,
        local_port: u16,
        remote_host: &str,
        remote_port: u16,
    ) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "dev-box".into(),
            local_addr: "127.0.0.1".into(),
            local_port,
            remote_host: remote_host.into(),
            remote_port,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    /// A create form plus a multi-row picker holding `ports`, with `tunnels`
    /// already saved.
    fn app_with(tunnels: Vec<TunnelConfig>, ports: &[u16]) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile { tunnels, ..ConfigFile::default() }).unwrap();
        let mut app = App::new(paths);
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("dev-box");
            f.name.set("svc");
            f.remote_host.set("10.0.0.5");
        }
        let mut pick = PortPick::scanning("dev-box", true, 1);
        let rows = ports.iter().map(|p| RemotePort::new(*p, "127.0.0.1")).collect::<Vec<_>>();
        assert!(pick.install(1, rows, 0));
        pick.check_all();
        app.ports = Some(pick);
        (dir, app)
    }

    #[test]
    fn a_ticked_port_that_is_already_saved_is_skipped() {
        // The scan proposes 8080 -> 10.0.0.5:8080, which is exactly what is
        // already saved. Re-creating it would duplicate the forward (possibly
        // with a bumped local port), so it must be skipped, not created.
        let port = free_port();
        let (_d, mut app) = app_with(vec![saved(1, "svc-8080", port, "10.0.0.5", port)], &[port]);

        let batch = app.validated_port_batch_outcome().expect("a batch exists");
        assert!(batch.configs.is_empty(), "the duplicate must not be planned for creation");
        assert_eq!(batch.skipped.len(), 1, "and it must be reported: {:?}", batch.skipped);
    }

    #[test]
    fn one_duplicate_port_and_one_new_port_create_only_the_new_one() {
        let dup = free_port();
        let new = free_port();
        let (_d, mut app) = app_with(vec![saved(1, "svc-old", dup, "10.0.0.5", dup)], &[dup, new]);

        let configs = app.validated_port_batch().expect("the new port still validates");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].remote_port, new);
        assert_eq!(app.pending_port_skipped.len(), 1);

        app.commit_port_batch(configs);
        assert_eq!(app.tunnels.len(), 2, "the saved tunnel plus the one new forward");
        let status = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        assert!(status.contains("跳过") || status.contains("skip"), "the status must explain the gap: {status:?}");
    }

    #[test]
    fn a_persisted_bumped_local_port_is_still_the_same_forward() {
        // The saved tunnel's local port was bumped to `bumped` on an earlier run
        // (that bumped value is what got persisted); the scan proposes `port`
        // again. The local port is a derived value, so this is one forward, not
        // two — the regression anchor for the real-world duplicate.
        let port = free_port();
        let bumped = port.wrapping_add(1).max(1024);
        let (_d, mut app) = app_with(vec![saved(1, "svc-old", bumped, "10.0.0.5", port)], &[port]);
        let batch = app.validated_port_batch_outcome().expect("batch");
        assert!(batch.configs.is_empty(), "the scanned forward is already saved under a bumped port");
        assert_eq!(batch.skipped.len(), 1);
    }

    #[test]
    fn a_different_target_host_is_a_different_forward() {
        let port = free_port();
        let (_d, mut app) = app_with(vec![saved(1, "svc-other", port, "10.0.0.9", port)], &[port]);
        let batch = app.validated_port_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1, "the scanned target is another address");
        assert!(batch.skipped.is_empty());
    }

    #[test]
    fn a_different_ssh_host_is_a_different_forward() {
        let port = free_port();
        let mut other = saved(1, "other-host", port, "10.0.0.5", port);
        other.ssh_host = "another-box".into();
        let (_d, mut app) = app_with(vec![other], &[port]);
        let batch = app.validated_port_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1);
        assert!(batch.skipped.is_empty());
    }

    #[test]
    fn an_all_duplicate_selection_closes_the_form_instead_of_complaining() {
        // "No port is ticked" describes a field the user did not leave empty; the
        // honest answer is "these already exist".
        let port = free_port();
        let (_d, mut app) = app_with(vec![saved(1, "svc-8080", port, "10.0.0.5", port)], &[port]);
        assert!(app.port_batch_intent(), "the pane is visible with one tick");
        assert!(app.validated_port_batch().is_none());
        assert!(app.form.is_none(), "the form closes: there was nothing wrong to fix");
        let status = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        assert!(status.contains("已存在") || status.contains("already exist"), "{status:?}");
        assert!(app.tunnels.len() == 1, "and nothing was created");
    }

    #[test]
    fn committing_a_fresh_batch_still_works_unchanged() {
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with(vec![], &[p1, p2]);
        let configs = app.validated_port_batch().expect("batch");
        assert_eq!(configs.len(), 2);
        assert!(app.pending_port_skipped.is_empty(), "nothing to report when nothing was skipped");
        app.commit_port_batch(configs);
        assert_eq!(app.tunnels.len(), 2);
        assert!(!matches!(app.tunnels[0].status, TunnelStatus::Stopped));
    }
}

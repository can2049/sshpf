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
//! * One `TunnelConfig` is one forward, so N ticked rows create N tunnels — minus
//!   the rows that duplicate a forward that is already saved (or another row of
//!   the same batch). The local-port planner bumps a busy listen port, which used
//!   to make a re-import of the same `LocalForward` look like a new forward; the
//!   duplicate check is what stops that second definition from being created. The
//!   submit path therefore validates and inserts *in a batch*: a partial write
//!   (three tunnels created, then a validation error on the fourth) would leave
//!   a state the user never asked for, so everything is validated up front and
//!   nothing is written unless all of it is valid.

use crate::app::{App, FormMode};
use crate::config;
use crate::help;
use crate::model::{self, ForwardEnds, ForwardKind, SshForward, StatusMsg, TunnelConfig, TunnelView, now_unix};
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
        self.validated_import_batch_outcome().map(|batch| batch.configs)
    }

    /// Like [`Self::validated_import_batch`], but keeps the rows that were
    /// skipped because the forward is already saved.
    ///
    /// The submit path needs to distinguish "nothing was ticked" from "everything
    /// ticked is already saved": only the first is a user error.
    pub(crate) fn validated_import_batch_outcome(&mut self) -> Option<ImportBatch> {
        let (ssh_host, forwards) = self.import_intent()?;
        let (base_id, last_id) = self.allocate_import_ids(forwards.len())?;
        match self.build_import_batch(&ssh_host, &forwards, base_id, last_id) {
            Ok(batch) => Some(batch),
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
    /// Rows whose forward is already saved are skipped, not created: the local
    /// listen port is a derived value the planner may bump (and a bump from an
    /// earlier run is persisted), so re-importing the same `LocalForward` would
    /// otherwise create a second entry for one intent. The first row of a
    /// same-identity pair wins, and every skipped row is reported back to the
    /// caller (see [`ImportBatch`]).
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
    ) -> Result<ImportBatch, Vec<String>> {
        // Ports already claimed by other managed tunnels, so the batch does not
        // fight over one port with the tunnels it is replacing. Grows as we
        // assign, so two rows cannot be handed the same local port.
        let mut taken: Vec<u16> = self.taken_local_ports();
        let mut out: Vec<TunnelConfig> = Vec::with_capacity(forwards.len());
        let mut skipped: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        for (next_id, forward) in (base_id..=last_id).zip(forwards.iter()) {
            let cfg = self.prepare_import_row(next_id, forward, ssh_host, &taken, &out);
            if let Some(existing) = self.duplicate_in_batch(&cfg, &out) {
                crate::eventlog::action_duplicate_skipped("import", &cfg.name, &existing);
                skipped.push(cfg.name);
                continue;
            }
            self.check_import_row(&cfg, &out, &mut errors);
            if cfg.kind != ForwardKind::Remote {
                taken.push(cfg.local_port);
            }
            out.push(cfg);
        }

        if errors.is_empty() { Ok(ImportBatch { configs: out, skipped }) } else { Err(errors) }
    }

    /// Build one row of the batch, before anything about it is checked.
    ///
    /// `out` is the batch accepted so far, which keeps names unique within the
    /// batch itself (not just against the already-saved tunnels).
    fn prepare_import_row(
        &self,
        id: crate::model::TunnelId,
        forward: &SshForward,
        ssh_host: &str,
        taken: &[u16],
        out: &[TunnelConfig],
    ) -> TunnelConfig {
        let mut cfg = self.config_from_forward(id, forward, taken);
        cfg.ssh_host = ssh_host.to_string();
        cfg.name = unique_within(&cfg.name, out.iter().map(|c| c.name.clone()).collect());
        cfg
    }

    /// Append anything wrong with a built row to `errors`.
    ///
    /// `out` is the batch built so far — needed because two imported rows can
    /// describe the same listen port (a `Host *` default plus a specific block),
    /// and creating both would fail at bind time instead of here.
    fn check_import_row(&self, cfg: &TunnelConfig, out: &[TunnelConfig], errors: &mut Vec<String>) {
        if let Err(e) = config::validate_tunnel(cfg) {
            // `e` is the Chinese validator message (`src/config.rs`, not our
            // scope); append the English category so the row is bilingual.
            errors.push(format!("{}: {e} / Invalid definition", cfg.name));
        }
        // Collision against already-saved tunnels, then against the rest of this
        // batch.
        let conflict = self.listen_conflict(cfg).or_else(|| out.iter().find_map(|other| listen_collision(cfg, other)));
        if let Some(conflict) = conflict {
            errors.push(help::bi(
                &format!("{}: 监听地址与「{conflict}」冲突", cfg.name),
                &format!("{}: listen endpoint collides with \"{conflict}\"", cfg.name),
            ));
        }
    }

    /// Report validation problems into the open form (the batch creates nothing).
    fn set_form_errors(&mut self, errors: Vec<String>) {
        if let Some(f) = self.form.as_mut() {
            f.errors = errors;
        }
    }

    /// Create the batch, reporting how many duplicate rows were skipped.
    ///
    /// `skipped` names the ticked rows that were dropped because the same forward
    /// is already saved. They are mentioned in the status so a batch that created
    /// fewer tunnels than rows are ticked is explained rather than mysterious.
    pub(crate) fn commit_import_batch_with_skipped(&mut self, configs: Vec<TunnelConfig>, skipped: Vec<String>) {
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
        self.set_status(StatusMsg::ok(import_batch_status(&configs, skipped.len())));
    }

    /// Create-form submit when the import pane is in charge.
    ///
    /// Validates the whole batch first so a rejected row cannot leave the user
    /// with "four of five tunnels created and one error". A batch whose every
    /// ticked row duplicates an already-saved forward creates nothing *and is not
    /// an error*: the user asked for something that is already true, so the form
    /// closes with a status saying so. Only "no row ticked at all" is reported
    /// into the form.
    pub(crate) fn commit_form_create_from_import(&mut self) {
        let batch = self.validated_import_batch_outcome();
        match batch {
            Some(batch) if !batch.configs.is_empty() => {
                self.commit_import_batch_with_skipped(batch.configs, batch.skipped);
            }
            Some(batch) if !batch.skipped.is_empty() => {
                // Every ticked forward already exists: nothing to create, and
                // nothing is wrong either.
                self.set_status(StatusMsg::ok(help::bi(
                    &format!("{} 条转发已存在，未重复创建", batch.skipped.len()),
                    &format!("{} forward(s) already exist; nothing was created", batch.skipped.len()),
                )));
                self.close_overlay();
            }
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

/// The configs a ticked import produced, plus the rows dropped as duplicates.
///
/// `skipped` keeps "nothing ticked" (a user error) distinguishable from
/// "everything ticked is already saved" (success with a no-op body).
pub(crate) struct ImportBatch {
    pub configs: Vec<TunnelConfig>,
    pub skipped: Vec<String>,
}

/// The import-commit status line: created count, plus how many were skipped.
fn import_batch_status(configs: &[TunnelConfig], skipped: usize) -> String {
    let n = configs.len();
    let created = if n == 1 {
        let name = configs.first().map(|c| c.name.clone()).unwrap_or_default();
        help::bi(&format!("已创建 1 条转发：{name}"), &format!("Created 1 forward: {name}"))
    } else {
        help::bi(
            &format!("已创建 {n} 条转发（来自 ~/.ssh/config）"),
            &format!("Created {n} forwards (from ~/.ssh/config)"),
        )
    };
    if skipped == 0 {
        return created;
    }
    help::bi(
        &format!("{created}；跳过 {skipped} 条重复转发"),
        &format!("{created}; skipped {skipped} duplicate forward(s)"),
    )
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

/// Map an imported ssh-config forward onto a config's local/remote sides, then
/// pick a bindable local port where one is needed.
///
/// The direction mapping itself is [`SshForward::ends`] (in `model`), because the
/// duplicate detector asks the same question without planning a port. What is
/// added here is the *planning*: `-L`/`-D` bind locally, so only those need a
/// collision-free port, and `taken` keeps two rows of one batch from being handed
/// the same one.
fn forward_ends(bind: &str, forward: &SshForward, taken: &[u16]) -> ForwardEnds {
    let mut ends = forward.ends(bind);
    // `-R` does not bind locally, so its port is passed through untouched.
    if forward.kind == ForwardKind::Remote {
        return ends;
    }
    // A privileged port is never scanned upwards from itself, which would spend
    // the whole budget inside the range that needs root.
    let addr = ends.local_addr.clone();
    if let Some((port, _)) = model::plan_local_port(ends.local_port, taken, |c| ssh::port_available(&addr, c)) {
        ends.local_port = port;
    }
    // Nothing usable found: keep the requested port so validation reports the real
    // problem instead of silently using port 0.
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
mod tests;

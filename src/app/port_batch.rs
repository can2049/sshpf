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
mod tests;

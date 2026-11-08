//! The create/edit form: a plain state object plus its validation rules.
//!
//! Kept apart from `App` because it is a *pure* state machine — it never
//! spawns, never persists, and can be unit-tested on its own. `App` owns one
//! and drives it from key events.

use crate::app::log as eventlog;
use crate::app::{App, ConfirmAction};
use crate::model::{
    self, FormField, FormMode, ForwardKind, ImportTable, InputBuf, TunnelConfig, TunnelId, TunnelView, now_unix,
};
use crate::ssh;

/// How the form is currently being driven.
///
/// Two-stage editing: arrow keys only *move the highlight* between fields, and
/// `enter` is what commits to editing the highlighted one. This keeps
/// accidental edits away from the arrows a user presses constantly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    /// Arrows move the highlight; `enter` starts editing the field.
    Navigating,
    /// Keystrokes edit the highlighted field; `enter`/`esc` leave editing.
    Editing,
}

/// Create/edit form state.
#[derive(Debug, Clone)]
pub struct FormState {
    pub mode: FormMode,
    pub edit_mode: EditMode,
    pub field: FormField,
    pub kind_idx: usize,
    pub name: InputBuf,
    pub ssh_host: InputBuf,
    pub local_addr: InputBuf,
    pub local_port: InputBuf,
    pub remote_host: InputBuf,
    pub remote_port: InputBuf,
    pub auto_restart: bool,
    /// Validation problems, refreshed on every edit.
    pub errors: Vec<String>,
    /// Port availability hint for the currently typed local port.
    pub port_hint: Option<String>,
    /// True while `local_port` holds an auto-suggestion derived from
    /// `remote_port`. Cleared as soon as the user edits the local port by hand,
    /// so we never overwrite an explicit choice.
    pub local_port_auto: bool,
    /// Explanation of the current suggestion, shown under the fields.
    pub local_port_note: Option<String>,
    /// Forwards discovered in `~/.ssh/config` for the selected host, with the
    /// user's tick marks. Only used for a *new* tunnel: the rows the user keeps
    /// become one tunnel each, because a `TunnelConfig` describes a single
    /// forward. Empty means "no host picked yet".
    pub import: ImportTable,
    /// Show the import table (space-toggled from the SSH HOST field) even when
    /// the user is not currently on that field.
    pub import_open: bool,
    /// The host whose forwards are in `import`; changing SSH HOST re-discovers.
    import_host: String,
}

impl FormState {
    pub fn create(defaults_bind: &str) -> Self {
        FormState {
            mode: FormMode::Create,
            edit_mode: EditMode::Navigating,
            field: FormField::SshHost,
            kind_idx: 0,
            name: InputBuf::new(""),
            ssh_host: InputBuf::new(""),
            local_addr: InputBuf::new(defaults_bind),
            local_port: InputBuf::new(""),
            remote_host: InputBuf::new("127.0.0.1"),
            remote_port: InputBuf::new(""),
            auto_restart: true,
            errors: Vec::new(),
            port_hint: None,
            local_port_auto: true,
            local_port_note: None,
            import: ImportTable::empty(),
            import_open: false,
            import_host: String::new(),
        }
    }

    pub fn edit(cfg: &TunnelConfig) -> Self {
        let kind_idx = ForwardKind::all().iter().position(|k| *k == cfg.kind).unwrap_or(0);
        FormState {
            mode: FormMode::Edit(cfg.id),
            edit_mode: EditMode::Navigating,
            field: FormField::SshHost,
            kind_idx,
            name: InputBuf::new(&cfg.name),
            ssh_host: InputBuf::new(&cfg.ssh_host),
            local_addr: InputBuf::new(&cfg.local_addr),
            local_port: InputBuf::new(cfg.local_port.to_string()),
            remote_host: InputBuf::new(&cfg.remote_host),
            remote_port: InputBuf::new(cfg.remote_port.to_string()),
            auto_restart: cfg.auto_restart,
            errors: Vec::new(),
            port_hint: None,
            local_port_auto: false, // editing an existing tunnel: keep its port
            local_port_note: None,
            import: ImportTable::empty(),
            import_open: false,
            import_host: String::new(),
        }
    }

    /// Host whose forwards are currently in `import`.
    pub fn import_host_name(&self) -> &str {
        &self.import_host
    }

    /// Install a freshly discovered import table for `host`.
    pub fn set_import_table(&mut self, host: impl Into<String>, table: ImportTable) {
        self.import_host = host.into();
        self.import = table;
        // A new host's rows must not inherit the previous host's filter, but the
        // pane-open state is a UI preference the user just expressed.
        self.import.filters = std::array::from_fn(|_| String::new());
        self.import.end_filter();
    }

    pub fn kind(&self) -> ForwardKind {
        ForwardKind::all()[self.kind_idx.min(2)]
    }

    pub fn cycle_kind(&mut self, forward: bool) {
        let n = ForwardKind::all().len();
        self.kind_idx = if forward { (self.kind_idx + 1) % n } else { (self.kind_idx + n - 1) % n };
    }

    /// True when this form is *saving changes to* an existing tunnel.
    pub fn is_modifying(&self) -> bool {
        matches!(self.mode, FormMode::Edit(_))
    }

    /// True when the highlighted field currently accepts text input.
    pub fn is_editing_field(&self) -> bool {
        self.edit_mode == EditMode::Editing
    }

    /// Start editing the highlighted field (only meaningful for text fields).
    pub fn begin_edit(&mut self) {
        if self.field.is_text() {
            self.edit_mode = EditMode::Editing;
        }
    }

    pub fn end_edit(&mut self) {
        self.edit_mode = EditMode::Navigating;
    }

    /// The input buffer for a field, for key routing.
    pub fn buf_mut(&mut self, field: FormField) -> Option<&mut InputBuf> {
        match field {
            FormField::Name => Some(&mut self.name),
            FormField::SshHost => Some(&mut self.ssh_host),
            FormField::LocalAddr => Some(&mut self.local_addr),
            FormField::LocalPort => Some(&mut self.local_port),
            FormField::RemoteHost => Some(&mut self.remote_host),
            FormField::RemotePort => Some(&mut self.remote_port),
            FormField::Kind | FormField::AutoRestart => None,
        }
    }

    /// Derive the local port from the remote port.
    ///
    /// Only fires while the local port is still an untouched suggestion
    /// (`local_port_auto`), so a value the user typed is never overwritten.
    /// When the natural choice is taken — by another tunnel in the config or by
    /// something else on the machine — we walk upwards to the next free port.
    /// The result is always a *suggestion*: the user can edit it freely.
    pub fn suggest_local_port(&mut self, taken: &[u16]) {
        if !self.local_port_auto || self.kind() == ForwardKind::Dynamic {
            return;
        }
        let Ok(remote) = self.remote_port.value().trim().parse::<u16>() else {
            return;
        };
        if remote == 0 {
            return;
        }
        let addr = self.local_addr.value().trim().to_string();
        let probe_addr = if addr.is_empty() { "127.0.0.1" } else { addr.as_str() };

        // The rules live in `model::plan_local_port` because the ssh-config
        // importer needs exactly the same ones, and two copies of "which port
        // should this land on" had already started to diverge.
        match model::plan_local_port(remote, taken, |c| ssh::port_available(probe_addr, c)) {
            Some((port, reason)) => {
                self.local_port.set(port.to_string());
                self.local_port_note = Some(match reason {
                    model::LocalPortReason::SameAsRemote => format!("已自动设为 {port}（与远端端口相同）"),
                    model::LocalPortReason::PrivilegedAlias => {
                        format!("远端端口 {remote} 需特权，本地建议 {port}")
                    }
                    model::LocalPortReason::Bumped => format!("{remote} 已被占用，已自动顺延到 {port}"),
                });
            }
            None => {
                self.local_port_note = Some("找不到可用本地端口，请手动填写".to_string());
            }
        }
    }

    /// Mark the local port as hand-edited so auto-suggestion stops touching it.
    pub fn mark_local_port_manual(&mut self) {
        self.local_port_auto = false;
        self.local_port_note = None;
    }

    /// Recompute `errors` / `port_hint` after an edit.
    pub fn revalidate(&mut self) {
        self.errors.clear();
        if self.ssh_host.value().trim().is_empty() {
            self.errors.push("SSH 主机不能为空".into());
        }
        if let (Ok(port), false) =
            (self.local_port.value().trim().parse::<u16>(), self.local_port.value().trim().is_empty())
            && port != 0
        {
            let addr = self.local_addr.value().trim();
            if !addr.is_empty() && !ssh::port_available(addr, port) {
                self.port_hint = Some(format!("{addr}:{port} 已被占用"));
            } else {
                self.port_hint = Some(format!("{addr}:{port} 可用"));
            }
        } else {
            self.port_hint = None;
        }
    }

    /// Build a config from the form; `Err` carries field-level problems.
    pub fn to_config(&self, id: TunnelId, existing: Option<&TunnelConfig>) -> Result<TunnelConfig, Vec<String>> {
        let mut errs = Vec::new();
        let name = self.name.value().trim().to_string();
        let ssh_host = self.ssh_host.value().trim().to_string();
        let local_addr = self.local_addr.value().trim().to_string();
        let kind = self.kind();

        if name.is_empty() {
            errs.push("名称不能为空".to_string());
        }
        if ssh_host.is_empty() {
            errs.push("SSH 主机不能为空".to_string());
        }
        if ssh_host.chars().any(char::is_whitespace) {
            errs.push("SSH 主机不能包含空格".to_string());
        }
        if kind != ForwardKind::Remote && local_addr.is_empty() {
            errs.push("本地地址不能为空".to_string());
        }

        let mut local_port = 0u16;
        match self.local_port.value().trim().parse::<u16>() {
            Ok(0) | Err(_) => errs.push("本地端口必须是 1-65535 的数字".to_string()),
            Ok(p) => local_port = p,
        }

        let mut remote_port = 0u16;
        let mut remote_host = String::new();
        if kind != ForwardKind::Dynamic {
            match self.remote_port.value().trim().parse::<u16>() {
                Ok(0) | Err(_) => errs.push("远端端口必须是 1-65535 的数字".to_string()),
                Ok(p) => remote_port = p,
            }
            remote_host = self.remote_host.value().trim().to_string();
            if remote_host.is_empty() {
                errs.push("远端地址不能为空".to_string());
            }
        }

        if !errs.is_empty() {
            return Err(errs);
        }

        Ok(TunnelConfig {
            id,
            name,
            kind,
            ssh_host,
            local_addr,
            local_port,
            remote_host,
            remote_port,
            enabled: existing.map(|e| e.enabled).unwrap_or(true),
            auto_restart: self.auto_restart,
            ssh_options: existing.map(|e| e.ssh_options.clone()).unwrap_or_default(),
            note: existing.map(|e| e.note.clone()).unwrap_or_default(),
            created_at: existing.map(|e| e.created_at).filter(|v| *v != 0).unwrap_or_else(now_unix),
        })
    }
}
// ---------------------------------------------------------------------------
// Form-driven App operations
// ---------------------------------------------------------------------------
//
// Saving the form touches three collaborators (validation, persistence, the
// process supervisor), so it needs `App`; but it is entirely about the form,
// hence it lives next to the form state rather than in the orchestration file.

impl App {
    /// Validate the open form and either save immediately (new tunnel) or ask
    /// for confirmation (editing an existing one, which replaces a running
    /// process and therefore drops live connections).
    pub fn submit_form(&mut self) {
        // The import pane, once it has loaded rows, owns the submit: it lists what
        // the host already declares, so those rows are the intent and the port
        // fields are only a scratch pad. Validating the scratch fields would
        // reject a perfectly good batch (they are typically empty once the pane is
        // in use) — and worse, unticking every row would fall through to the
        // single-field path and report "名称不能为空", which describes a field the
        // user is not even looking at.
        if let Some(form) = self.form.as_ref()
            && !form.is_modifying()
            && !form.import.is_empty()
        {
            self.commit_form_create_from_import();
            return;
        }

        let Some(cfg) = self.validated_form_config() else {
            return;
        };
        match cfg.clone() {
            // Creating a tunnel is additive and easily undone by deleting it,
            // so it saves straight away and connects immediately.
            _ if matches!(self.form.as_ref().map(|f| f.mode), Some(FormMode::Create)) => {
                self.commit_form_create(cfg);
            }
            // Editing rewrites persisted config and bounces a live process.
            _ => {
                let id = cfg.id;
                self.pending_form = Some(cfg);
                self.ask_confirm(ConfirmAction::ApplyEdit(id));
            }
        }
    }

    /// Turn the current form into a validated `TunnelConfig`, reporting field
    /// errors into the form instead of acting when anything is wrong.
    fn validated_form_config(&mut self) -> Option<TunnelConfig> {
        let form = self.form.clone()?;
        let existing = match form.mode {
            FormMode::Edit(id) => self.cfg.find(id).cloned(),
            FormMode::Create => None,
        };
        let id = match form.mode {
            FormMode::Edit(id) => id,
            FormMode::Create => self.cfg.next_id(),
        };
        let mut cfg = match form.to_config(id, existing.as_ref()) {
            Ok(c) => c,
            Err(errs) => {
                if let Some(f) = self.form.as_mut() {
                    f.errors = errs;
                }
                return None;
            }
        };
        // Cross-entry conflict: same listen endpoint as another enabled tunnel.
        if let Some(conflict) = self.listen_conflict(&cfg) {
            if let Some(f) = self.form.as_mut() {
                f.errors = vec![format!("监听地址与「{conflict}」冲突")];
            }
            return None;
        }
        cfg.name = self.cfg.unique_name(
            &cfg.name,
            match form.mode {
                FormMode::Edit(id) => Some(id),
                FormMode::Create => None,
            },
        );
        Some(cfg)
    }

    fn commit_form_create(&mut self, mut cfg: TunnelConfig) {
        cfg.enabled = true;
        eventlog::tunnel_created(&cfg, "form");
        self.cfg.tunnels.push(cfg.clone());
        self.tunnels.push(TunnelView::new(cfg.clone()));
        let pos = self.tunnels.len() - 1;
        let vis = self.visible_indices();
        self.selected = vis.iter().position(|i| *i == pos).unwrap_or(vis.len().saturating_sub(1));
        self.start_tunnel_with(cfg.id, "created");
        self.close_overlay();
        self.persist_quietly();
    }

    /// Apply the pending edit that the user confirmed.
    pub(crate) fn commit_form_edit(&mut self, eid: TunnelId) {
        let Some(cfg) = self.pending_form.take() else {
            return;
        };
        if let Some(slot) = self.cfg.find_mut(eid) {
            let keep_enabled = slot.enabled;
            let mut next = cfg.clone();
            next.id = eid;
            next.enabled = keep_enabled || next.enabled;
            *slot = next;
        }
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == eid) {
            let enabled = v.cfg.enabled;
            v.cfg = cfg.clone();
            v.cfg.id = eid;
            v.cfg.enabled = enabled || v.cfg.enabled;
        }
        if let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == eid) {
            eventlog::tunnel_updated(&view.cfg);
        }
        self.supervisor.stop(eid);
        self.forget_runtime_state(eid);
        self.start_tunnel_with(eid, "edited");
        self.close_overlay();
        self.persist_quietly();
    }

    /// Whether the open form has changes worth warning about when discarding.
    pub fn form_has_unsaved_edits(&self) -> bool {
        let Some(form) = self.form.as_ref() else {
            return false;
        };
        match form.mode {
            FormMode::Create => {
                !form.ssh_host.value().trim().is_empty()
                    || !form.name.value().trim().is_empty()
                    || !form.remote_port.value().trim().is_empty()
                    || !form.local_port.value().trim().is_empty()
            }
            // Editing: compare against the stored definition.
            FormMode::Edit(id) => match self.cfg.find(id) {
                Some(orig) => {
                    form.name.value() != orig.name
                        || form.ssh_host.value() != orig.ssh_host
                        || form.local_addr.value() != orig.local_addr
                        || form.local_port.value() != orig.local_port.to_string()
                        || form.remote_host.value() != orig.remote_host
                        || form.remote_port.value() != orig.remote_port.to_string()
                        || form.kind() != orig.kind
                        || form.auto_restart != orig.auto_restart
                }
                None => true,
            },
        }
    }

    /// Request closing the form, asking first if there is unsaved input.
    pub fn request_close_form(&mut self) {
        if self.form_has_unsaved_edits() {
            self.ask_confirm(ConfirmAction::DiscardForm);
        } else {
            self.close_overlay();
        }
    }
}

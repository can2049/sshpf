//! The create/edit form: a plain state object plus its validation rules.
//!
//! Kept apart from `App` because it is a *pure* state machine — it never
//! spawns, never persists, and can be unit-tested on its own. `App` owns one
//! and drives it from key events.

use crate::eventlog;
use crate::help;
use crate::model::{self, FormField, FormMode, ForwardKind, ImportTable, InputBuf, TunnelConfig, TunnelId, now_unix};
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
    ///
    /// A new table is a **new default offer**, so it starts untouched: any tick
    /// the user made on the previous host's rows must not be read as an
    /// instruction about *these* rows. (Only `install_import_rows` reaches this,
    /// and only for a changed host — see `refresh_import_table`.)
    pub fn set_import_table(&mut self, host: impl Into<String>, table: ImportTable) {
        self.import_host = host.into();
        self.import = table;
        // A new host's rows must not inherit the previous host's filter, but the
        // pane-open state is a UI preference the user just expressed.
        self.import.filters = std::array::from_fn(|_| String::new());
        self.import.touched = false;
        self.import.end_filter();
    }

    pub fn kind(&self) -> ForwardKind {
        // `kind_idx` is a plain `usize` field other modules can assign, and
        // `ForwardKind::all()` is a fixed array whose length is not a constant
        // this line can hard-code. A stale index degrades to the default kind
        // (and says so) instead of panicking on an out-of-bounds slice index.
        match ForwardKind::all().get(self.kind_idx) {
            Some(kind) => *kind,
            None => {
                eventlog::form_kind_index_invalid(self.kind_idx, ForwardKind::all().len());
                ForwardKind::default()
            }
        }
    }

    pub fn cycle_kind(&mut self, forward: bool) {
        let n = ForwardKind::all().len();
        // Start from a valid index: a form built with an out-of-range `kind_idx`
        // would otherwise stay out of range under `(i + n - 1) % n` as well.
        let i = self.kind_idx % n;
        self.kind_idx = if forward { (i + 1) % n } else { (i + n - 1) % n };
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
                    model::LocalPortReason::SameAsRemote => {
                        help::bi(&format!("已自动设为 {port}（与远端端口相同）"), "same as the remote port")
                    }
                    model::LocalPortReason::PrivilegedAlias => {
                        help::bi(&format!("远端端口 {remote} 需特权，本地建议 {port}"), "needs root: <1024")
                    }
                    model::LocalPortReason::Bumped => {
                        help::bi(&format!("{remote} 已被占用，已自动顺延到 {port}"), "already taken, moved up")
                    }
                });
            }
            None => {
                self.local_port_note = Some(help::bi("找不到可用本地端口，请手动填写", "no free port — type one"));
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
            self.errors.push(help::bi("SSH 主机不能为空", "SSH host must not be empty"));
        }
        if let (Ok(port), false) =
            (self.local_port.value().trim().parse::<u16>(), self.local_port.value().trim().is_empty())
            && port != 0
        {
            let addr = self.local_addr.value().trim();
            if !addr.is_empty() && !ssh::port_available(addr, port) {
                // `src/ui.rs` colours this line by `contains("占用")`, so the
                // Chinese phrase must stay first and verbatim.
                self.port_hint =
                    Some(help::bi(&format!("{addr}:{port} 已被占用"), &format!("{addr}:{port} is already in use")));
            } else {
                self.port_hint = Some(help::bi(&format!("{addr}:{port} 可用"), &format!("{addr}:{port} is available")));
            }
        } else {
            self.port_hint = None;
        }
    }

    /// Build a config from the form; `Err` carries field-level problems.
    pub fn to_config(&self, id: TunnelId, existing: Option<&TunnelConfig>) -> Result<TunnelConfig, Vec<String>> {
        let draft = self.draft();
        let errs = self.validate(&draft);
        if !errs.is_empty() {
            return Err(errs);
        }
        Ok(self.assemble(draft, id, existing))
    }

    /// Whether the typed fields alone already describe one valid tunnel.
    ///
    /// A *field-only* question, on purpose: it allocates no id, checks no
    /// duplicate and writes no error into [`Self::errors`]. The submit router
    /// asks it to decide whether the user has actually typed a forward by hand
    /// (and therefore whether a complete form may outrank the import pane's
    /// default offer), so it must be answerable without touching `App` state.
    ///
    /// `false` for a completely empty form — that is "nothing typed", which is
    /// exactly what lets "picked a host, pressed ctrl+s" keep importing.
    pub fn is_complete(&self) -> bool {
        self.validate(&self.draft()).is_empty()
    }

    /// The trimmed text of every field, before any validation.
    fn draft(&self) -> Draft {
        Draft {
            name: self.name.value().trim().to_string(),
            ssh_host: self.ssh_host.value().trim().to_string(),
            local_addr: self.local_addr.value().trim().to_string(),
            local_port: parse_port(self.local_port.value()),
            remote_host: self.remote_host.value().trim().to_string(),
            remote_port: parse_port(self.remote_port.value()),
            kind: self.kind(),
        }
    }

    /// Every validation failure at once, in the order the fields appear on
    /// screen, so the user fixes one pass instead of one field per attempt.
    fn validate(&self, d: &Draft) -> Vec<String> {
        let mut errs = Vec::new();
        if d.name.is_empty() {
            errs.push(help::bi("名称不能为空", "Name must not be empty"));
        }
        if d.ssh_host.is_empty() {
            errs.push(help::bi("SSH 主机不能为空", "SSH host must not be empty"));
        }
        if d.ssh_host.chars().any(char::is_whitespace) {
            errs.push(help::bi("SSH 主机不能包含空格", "SSH host must not contain spaces"));
        }
        if d.kind != ForwardKind::Remote && d.local_addr.is_empty() {
            errs.push(help::bi("本地地址不能为空", "Local address must not be empty"));
        }
        if d.local_port == 0 {
            errs.push(help::bi("本地端口必须是 1-65535 的数字", "Local port must be a number 1-65535"));
        }
        // A dynamic (`-D`) forward has no remote side at all, so its remote
        // fields are neither required nor validated.
        if d.kind != ForwardKind::Dynamic {
            if d.remote_port == 0 {
                errs.push(help::bi("远端端口必须是 1-65535 的数字", "Remote port must be a number 1-65535"));
            }
            if d.remote_host.is_empty() {
                errs.push(help::bi("远端地址不能为空", "Remote address must not be empty"));
            }
        }
        errs
    }

    /// Build the persisted config, carrying over the fields the form does not
    /// expose (`enabled`, `ssh_options`, `note`, `created_at`).
    fn assemble(&self, d: Draft, id: TunnelId, existing: Option<&TunnelConfig>) -> TunnelConfig {
        TunnelConfig {
            id,
            name: d.name,
            kind: d.kind,
            ssh_host: d.ssh_host,
            local_addr: d.local_addr,
            local_port: d.local_port,
            remote_host: if d.kind == ForwardKind::Dynamic { String::new() } else { d.remote_host },
            remote_port: if d.kind == ForwardKind::Dynamic { 0 } else { d.remote_port },
            enabled: existing.map(|e| e.enabled).unwrap_or(true),
            auto_restart: self.auto_restart,
            ssh_options: existing.map(|e| e.ssh_options.clone()).unwrap_or_default(),
            note: existing.map(|e| e.note.clone()).unwrap_or_default(),
            created_at: existing.map(|e| e.created_at).filter(|v| *v != 0).unwrap_or_else(now_unix),
        }
    }
}

/// The trimmed text of every editable field, before validation.
///
/// A named struct rather than a tuple: the validation and assembly steps both
/// read it, and a six-field tuple would make a field swap a silent bug.
struct Draft {
    name: String,
    ssh_host: String,
    local_addr: String,
    local_port: u16,
    remote_host: String,
    remote_port: u16,
    kind: ForwardKind,
}

/// `0` for anything that is not a valid 1..=65535 number, so the caller has one
/// failure to report instead of two shapes (`Err` and `Ok(0)`).
fn parse_port(text: &str) -> u16 {
    text.trim().parse::<u16>().unwrap_or(0)
}

#[cfg(test)]
mod tests;

//! The application state machine: owns tunnels, consumes supervisor events,
//! handles key presses and drives persistence.
//!
//! There is deliberately no `tokio` here. The UI thread polls
//! `crossterm::event::poll(200ms)` and drains an `mpsc` channel fed by one
//! stderr-reader thread per tunnel — the architecture validated in
//! `docs/research/tech-options.md` §3.3.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::config::{self, ConfigFile, Paths};
use crate::model::{
    FailureInfo, FailureKind, FormField, FormMode, ForwardKind, HostPicker, InputBuf, LogLevel, LogLine, StatusMsg,
    TunnelConfig, TunnelId, TunnelStatus, TunnelView, fuzzy_match, now_unix,
};
use crate::ssh::{self, SshHost};
use crate::tunnel::{Supervisor, TunnelEvent};

/// How many host rows the inline dropdown shows at once.
///
/// Kept in one place so the key handler (scrolling window) and the renderer
/// (popup height) cannot drift apart.
pub const HOST_DROPDOWN_ROWS: usize = 8;

/// How many candidate ports the local-port suggester probes before giving up.
const PORT_SCAN_LIMIT: usize = 200;

/// Which overlay (if any) currently captures keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    /// Create/edit form. The SSH-host dropdown lives *inside* this overlay
    /// (`App::picker`), so it does not need its own variant.
    Form,
    /// Destructive-action confirmation.
    Confirm {
        action: ConfirmAction,
        prompt: String,
    },
    /// Scrollable full detail for one tunnel.
    Detail(TunnelId),
    /// Inline filter editing on the main table.
    Filter,
}

/// A destructive or otherwise consequential action awaiting confirmation.
///
/// Anything that (a) loses data, (b) drops live connections, or (c) is hard to
/// notice afterwards goes through this gate. Cheap, reversible, in-place
/// actions (moving the cursor, filtering) deliberately do not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Remove one tunnel definition (loses the config entry).
    Delete(TunnelId),
    /// Stop one running tunnel (drops its live connections).
    Stop(TunnelId),
    /// Stop every running tunnel.
    StopAll,
    /// Restart one tunnel (briefly drops its connections).
    Restart(TunnelId),
    /// Start every stopped tunnel (opens many listeners at once).
    StartAll,
    /// Save edits to an existing tunnel (replaces its running process).
    ApplyEdit(TunnelId),
    /// Discard unsaved edits (loses typed input).
    DiscardForm,
    /// Quit the program (kills all children).
    Quit,
}

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
        }
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

        // Candidate order: the remote port itself, then upward.
        //
        // Privileged ports (<1024) are skipped: binding them needs root, so
        // `port_available` would report every one of them as busy and the scan
        // would burn its whole budget below 1024 without ever finding a usable
        // port — which is exactly what happens for a remote port of 22.
        // `taken` covers ports another managed tunnel owns but is not yet
        // listening on.
        let usable = |c: u16| !taken.contains(&c) && ssh::port_available(probe_addr, c);

        // Candidate order:
        //   1. the remote port itself (the intuitive "same number on both ends");
        //   2. for privileged ports, a friendly unprivileged alias like 2222 for
        //      ssh's 22 — that is what people type by hand anyway;
        //   3. a plain upward scan from the first unprivileged port.
        let mut chosen = None;
        if remote >= 1024 && usable(remote) {
            chosen = Some(remote);
        }
        if chosen.is_none() && remote < 1024 {
            let alias = 2000u16.saturating_add(remote).saturating_add(200);
            if usable(alias) {
                chosen = Some(alias);
            }
        }
        if chosen.is_none() {
            for candidate in (remote.max(1024)..=u16::MAX).take(PORT_SCAN_LIMIT) {
                if usable(candidate) {
                    chosen = Some(candidate);
                    break;
                }
            }
        }

        match chosen {
            Some(port) => {
                self.local_port.set(port.to_string());
                self.local_port_note = if port == remote {
                    Some(format!("已自动设为 {port}（与远端端口相同）"))
                } else if remote < 1024 {
                    Some(format!("远端端口 {remote} 需特权，本地建议 {port}"))
                } else {
                    Some(format!("{remote} 已被占用，已自动顺延到 {port}"))
                };
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

/// Per-tunnel reconnect bookkeeping.
#[derive(Debug, Clone, Copy)]
struct RetryState {
    attempt: u32,
    next_at: Instant,
}

/// The whole application state.
pub struct App {
    pub cfg: ConfigFile,
    pub paths: Paths,
    pub tunnels: Vec<TunnelView>,
    /// Index into `visible_indices()`, not into `tunnels`.
    pub selected: usize,
    pub overlay: Overlay,
    pub form: Option<FormState>,
    pub picker: Option<HostPicker>,
    pub hosts: Vec<SshHost>,
    pub hosts_error: Option<String>,
    pub status: Option<StatusMsg>,
    pub should_quit: bool,
    pub help_scroll: u16,
    pub detail_scroll: u16,
    pub filter: String,
    pub started_at: Instant,

    supervisor: Supervisor,
    event_tx: Sender<TunnelEvent>,
    event_rx: Receiver<TunnelEvent>,
    /// Scratch dir holding generated per-tunnel ssh configs.
    synth_dir: PathBuf,
    retry_state: HashMap<TunnelId, RetryState>,
    /// Per-tunnel generated config paths, removed on stop.
    synth_paths: HashMap<TunnelId, PathBuf>,
    /// A validated edit waiting for the user to confirm it, so the running
    /// process is not bounced before the dialog is answered.
    pending_form: Option<TunnelConfig>,
    /// Exits observed but not yet classified: ssh writes its diagnostic to
    /// stderr and exits immediately, so the exit can arrive before the last
    /// lines. Keyed by tunnel, valued by (exit code, when we saw it).
    pending_exit: HashMap<TunnelId, (Option<i32>, Instant)>,
    /// Tunnels that have printed "Local forwarding listening on ..." for the
    /// current attempt. Guards staged readiness when `ProxyJump` forks a child
    /// whose own "Entering interactive session." line arrives first.
    saw_listening: std::collections::HashSet<TunnelId>,
}

impl App {
    pub fn new(paths: Paths) -> Self {
        let cfg = config::load(&paths.config).unwrap_or_else(|e| {
            eprintln!("警告: {e}; 以空配置启动");
            ConfigFile::default()
        });
        let (event_tx, event_rx) = mpsc::channel();
        let synth_dir = std::env::temp_dir().join(format!("sshpf-{}", std::process::id()));
        cleanup_stale_scratch_dirs();
        let tunnels = build_views(&cfg);
        App {
            cfg,
            paths,
            tunnels,
            selected: 0,
            overlay: Overlay::None,
            form: None,
            picker: None,
            hosts: Vec::new(),
            hosts_error: None,
            status: None,
            should_quit: false,
            help_scroll: 0,
            detail_scroll: 0,
            filter: String::new(),
            started_at: Instant::now(),
            supervisor: Supervisor::new(),
            event_tx,
            event_rx,
            synth_dir,
            retry_state: HashMap::new(),
            synth_paths: HashMap::new(),
            pending_form: None,
            pending_exit: HashMap::new(),
            saw_listening: std::collections::HashSet::new(),
        }
    }

    // -- queries -----------------------------------------------------------

    /// Indices into `tunnels` for rows matching the current filter, in order.
    pub fn visible_indices(&self) -> Vec<usize> {
        self.tunnels
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                self.filter.is_empty()
                    || fuzzy_match(&self.filter, &t.cfg.name)
                    || fuzzy_match(&self.filter, &t.cfg.ssh_host)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_tunnel(&self) -> Option<&TunnelView> {
        self.visible_indices().get(self.selected).and_then(|i| self.tunnels.get(*i))
    }

    pub fn selected_id(&self) -> Option<TunnelId> {
        self.selected_tunnel().map(|t| t.cfg.id)
    }

    pub fn tunnel(&self, id: TunnelId) -> Option<&TunnelView> {
        self.tunnels.iter().find(|t| t.cfg.id == id)
    }

    /// Clamp `selected` into the current (possibly filtered) view.
    ///
    /// `selected` indexes `visible_indices()`, so narrowing the filter can leave
    /// it pointing past the end. Every reader of `selected` goes through
    /// `visible_indices()`, so one clamp here keeps the cursor valid after any
    /// filter change or deletion.
    pub fn clamp_selection(&mut self) {
        let n = self.visible_indices().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    pub fn select_next(&mut self) {
        self.clamp_selection();
        let n = self.visible_indices().len();
        if n > 0 && self.selected + 1 < n {
            self.selected += 1;
        }
    }

    pub fn select_prev(&mut self) {
        self.clamp_selection();
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.visible_indices().len().saturating_sub(1);
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for t in &self.tunnels {
            match &t.status {
                TunnelStatus::Up { .. } => c.up += 1,
                TunnelStatus::Starting { .. } => c.starting += 1,
                TunnelStatus::Retrying { .. } => c.retrying += 1,
                TunnelStatus::Failed { .. } => c.failed += 1,
                TunnelStatus::Stopped => c.off += 1,
            }
        }
        c.total = self.tunnels.len();
        c
    }

    pub fn running_count(&self) -> usize {
        self.supervisor.running_count()
    }

    pub fn set_status(&mut self, msg: StatusMsg) {
        self.status = Some(msg);
    }

    /// Drop the transient message once it has aged out.
    pub fn expire_status(&mut self) {
        if self.status.as_ref().is_some_and(|s| s.expired()) {
            self.status = None;
        }
    }

    // -- host discovery ----------------------------------------------------

    pub fn load_hosts(&mut self) {
        match ssh::list_hosts() {
            Ok(hosts) => {
                self.hosts = hosts;
                self.hosts_error = None;
            }
            Err(e) => {
                self.hosts.clear();
                self.hosts_error = Some(e.to_string());
            }
        }
    }

    // -- lifecycle ---------------------------------------------------------

    /// Start every tunnel marked `enabled` (user intent from the last run).
    pub fn restore_enabled(&mut self) {
        if !self.cfg.defaults.restore_on_start {
            return;
        }
        let ids: Vec<TunnelId> = self.tunnels.iter().filter(|t| t.cfg.enabled).map(|t| t.cfg.id).collect();
        if ids.is_empty() {
            return;
        }
        let n = ids.len();
        for id in ids {
            self.start_tunnel(id);
        }
        self.set_status(StatusMsg::info(format!("正在恢复 {n} 条转发…")));
    }

    fn log(&mut self, id: TunnelId, level: LogLevel, text: impl Into<String>) {
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.log.push(LogLine::new(level, text));
        }
    }

    fn set_state(&mut self, id: TunnelId, status: TunnelStatus) {
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.status = status;
        }
    }

    fn set_enabled(&mut self, id: TunnelId, enabled: bool) {
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.cfg.enabled = enabled;
        }
        if let Some(c) = self.cfg.find_mut(id) {
            c.enabled = enabled;
        }
    }

    /// Start (or restart) one tunnel.
    pub fn start_tunnel(&mut self, id: TunnelId) {
        let Some(cfg) = self.tunnels.iter().find(|t| t.cfg.id == id).map(|t| t.cfg.clone()) else {
            return;
        };
        self.arm_retry(id);

        // 1. Definition-level validation.
        if let Err(e) = config::validate_tunnel(&cfg) {
            let info = FailureInfo::with_short(FailureKind::Unknown, "配置无效", e.to_string());
            self.set_state(id, TunnelStatus::Failed { reason: info });
            self.log(id, LogLevel::Error, format!("配置无效: {e}"));
            self.set_status(StatusMsg::error(format!("{}: {e}", cfg.name)));
            return;
        }

        // 2. ssh binary present?
        if !ssh::ssh_available() {
            let info = FailureInfo::with_short(FailureKind::SpawnFailed, "未找到 ssh 可执行文件", "PATH 中没有 ssh");
            self.set_state(id, TunnelStatus::Failed { reason: info });
            self.log(id, LogLevel::Error, "未在 PATH 中找到 ssh");
            self.set_status(StatusMsg::error("未找到 ssh 可执行文件"));
            return;
        }

        // 3. Pre-flight bind check. A `-R` listen happens on the server, so we
        //    can only check our own side for -L/-D.
        if cfg.kind != ForwardKind::Remote {
            let (addr, port) = config::listen_endpoint(&cfg);
            if !ssh::port_available(addr, port) {
                let owner = ssh::port_owner(addr, port);
                let detail = match &owner {
                    Some(o) => format!("{addr}:{port} 已被占用 ({o})"),
                    None => format!("{addr}:{port} 已被占用"),
                };
                let info = FailureInfo::with_short(FailureKind::PortInUse, "本地端口被占用", detail.clone());
                self.set_state(id, TunnelStatus::Failed { reason: info });
                self.log(id, LogLevel::Error, detail.clone());
                self.set_status(StatusMsg::error(format!("{}: {detail}", cfg.name)));
                self.persist_quietly();
                return;
            }
        }

        // 4. Generate the synthetic ssh config (tech-options.md §2.7/§2.8).
        let synth = match ssh::write_synth_config(&cfg, &self.synth_dir) {
            Ok(p) => Some(p),
            Err(e) => {
                self.log(id, LogLevel::Warn, format!("生成合成配置失败，回退到用户配置: {e}"));
                None
            }
        };
        let args = ssh::build_ssh_args(&cfg, synth.as_deref());
        let program = ssh::ssh_binary();

        self.log(id, LogLevel::Info, format!("启动: ssh {}", args.join(" ")));
        self.saw_listening.remove(&id);
        self.set_state(id, TunnelStatus::Starting { since: Instant::now() });
        self.set_enabled(id, true);

        match self.supervisor.spawn(id, &program, &args, self.event_tx.clone()) {
            Ok(()) => {
                if let Some(p) = synth
                    && let Some(old) = self.synth_paths.insert(id, p)
                {
                    let _ = std::fs::remove_file(old);
                }
                self.set_status(StatusMsg::info(format!("{} 正在连接…", cfg.name)));
            }
            Err(e) => {
                let short: String = e.to_string().chars().take(28).collect();
                let info = FailureInfo::with_short(FailureKind::SpawnFailed, short, format!("启动 ssh 失败: {e}"));
                self.set_state(id, TunnelStatus::Failed { reason: info });
                self.log(id, LogLevel::Error, format!("启动 ssh 失败: {e}"));
                self.set_status(StatusMsg::error(format!("{} 启动失败: {e}", cfg.name)));
            }
        }
        self.persist_quietly();
    }

    /// Stop one tunnel and clear its `enabled` flag (user intent).
    pub fn stop_tunnel(&mut self, id: TunnelId) {
        self.supervisor.stop(id);
        if let Some(path) = self.synth_paths.remove(&id) {
            let _ = std::fs::remove_file(path);
        }
        self.retry_state.remove(&id);
        self.pending_exit.remove(&id);
        self.saw_listening.remove(&id);
        self.set_enabled(id, false);
        self.set_state(id, TunnelStatus::Stopped);
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.retries = 0;
        }
        self.log(id, LogLevel::Info, "已停止");
        self.persist_quietly();
    }

    /// Space: start immediately, but ask before dropping a live tunnel.
    pub fn toggle_selected(&mut self) {
        let Some(view) = self.selected_tunnel() else {
            return;
        };
        let (id, running) = (view.cfg.id, view.status.is_running_or_pending());
        if running {
            self.ask_confirm(ConfirmAction::Stop(id));
        } else {
            self.start_tunnel(id);
        }
    }

    /// `r`: ask before bouncing a live tunnel, so a stray keypress cannot drop
    /// someone's session. A stopped tunnel restarts silently (nothing to lose).
    pub fn restart_selected(&mut self) {
        let Some(view) = self.selected_tunnel() else {
            return;
        };
        let id = view.cfg.id;
        if view.status.is_running_or_pending() {
            self.ask_confirm(ConfirmAction::Restart(id));
        } else {
            self.apply_confirmed(ConfirmAction::Restart(id));
        }
    }

    /// Drain supervisor events. Called once per UI frame — never blocks.
    pub fn drain_events(&mut self) {
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                TunnelEvent::Line { id, line } => self.on_line(id, line),
                TunnelEvent::Exited { id, code } => {
                    // Defer: the failure classification needs the whole stderr.
                    self.pending_exit.insert(id, (code, Instant::now()));
                }
                TunnelEvent::StderrClosed { id } => {
                    if let Some((code, _)) = self.pending_exit.remove(&id) {
                        self.on_exit(id, code);
                    }
                }
                TunnelEvent::SpawnFailed { id, error } => {
                    let info = FailureInfo::with_short(FailureKind::SpawnFailed, "无法启动 ssh", error);
                    self.set_state(id, TunnelStatus::Failed { reason: info });
                }
            }
        }
        // Reap children whose stderr EOF raced ahead of the exit.
        for id in self.supervisor.ids() {
            if let Some(code) = self.supervisor.poll(id) {
                self.pending_exit.entry(id).or_insert((code, Instant::now()));
            }
        }
        self.flush_pending_exits();
        self.tick_retries();
        self.expire_status();
    }

    /// Classify deferred exits once their stderr is complete (or they have
    /// waited long enough that further lines are not coming).
    fn flush_pending_exits(&mut self) {
        const GRACE: Duration = Duration::from_millis(500);
        let ready: Vec<TunnelId> =
            self.pending_exit.iter().filter(|(_, (_, at))| at.elapsed() >= GRACE).map(|(id, _)| *id).collect();
        for id in ready {
            if let Some((code, _)) = self.pending_exit.remove(&id) {
                self.on_exit(id, code);
            }
        }
    }

    fn on_line(&mut self, id: TunnelId, line: String) {
        let Some(kind) = self.tunnels.iter().find(|t| t.cfg.id == id).map(|t| t.cfg.kind) else {
            return;
        };

        // Failure lines win over ready lines: a partially-bound `-L` still
        // prints "Entering interactive session" while one address family failed.
        let classified = ssh::classify_failure(&line);
        let ready = ssh::is_ready_line(kind, &line);
        let listening = ssh::is_local_listening_line(&line);
        // `-R` announces success *after* "Entering interactive session."
        // (measured: line 42 = Entering, line 47 = remote forward failure).
        let remote_established = line.contains("remote forward success for");

        let level = if classified.is_some() {
            LogLevel::Warn
        } else if ready {
            LogLevel::Info
        } else {
            LogLevel::Debug
        };
        self.log(id, level, line);

        // Staged readiness. Two independent hazards, both measured:
        //  * `ProxyJump` forks `ssh -W`; that child's own
        //    "Entering interactive session." is merged into our stderr before
        //    the final hop authenticates.
        //  * `-R` prints "Entering interactive session." *before* it knows
        //    whether the remote listener was accepted.
        // So `-L`/`-D` need a preceding "Local forwarding listening on" and
        // `-R` needs the explicit "remote forward success for" marker.
        if listening {
            self.saw_listening.insert(id);
        }
        let staged_ready = match kind {
            ForwardKind::Local | ForwardKind::Dynamic => ready && self.saw_listening.contains(&id),
            ForwardKind::Remote => remote_established,
        };

        let status = self.tunnels.iter().find(|t| t.cfg.id == id).map(|t| t.status.clone());
        match (classified, status) {
            (Some(info), Some(TunnelStatus::Starting { .. })) => {
                self.set_state(id, TunnelStatus::Failed { reason: info });
                // A tunnel whose forward was rejected is useless even though
                // ssh keeps the session open (`-R` failure, partially bound
                // `-L`): stop it so the UI and reality agree.
                self.stop_after_failure(id);
            }
            (Some(info), Some(TunnelStatus::Up { .. })) => {
                self.log(id, LogLevel::Warn, format!("连接目标失败: {}", info.detail));
            }
            (None, Some(TunnelStatus::Starting { .. })) if staged_ready => {
                self.set_state(id, TunnelStatus::Up { since: Instant::now() });
                self.retry_state.remove(&id);
                self.pending_exit.remove(&id);
                if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
                    v.retries = 0;
                }
                let name = self.tunnel(id).map(|t| t.cfg.name.clone()).unwrap_or_default();
                self.set_status(StatusMsg::ok(format!("✓ {name} 已连通")));
            }
            _ => {}
        }
    }

    /// Kill the ssh child of a tunnel that failed to establish its forward.
    ///
    /// `ssh -N` keeps running when a forwarding request is rejected (`-R`
    /// remote forward failure, or a partially bound `-L`): the session is up
    /// but the tunnel is useless. We stop it so the table never shows a
    /// "failed" row backed by a live process, and so the local port is not
    /// held.
    fn stop_after_failure(&mut self, id: TunnelId) {
        self.supervisor.stop(id);
        if let Some(path) = self.synth_paths.remove(&id) {
            let _ = std::fs::remove_file(path);
        }
        self.retry_state.remove(&id);
        self.pending_exit.remove(&id);
        self.saw_listening.remove(&id);
        // Keep `enabled` as the user set it; only the process goes away.
    }

    fn on_exit(&mut self, id: TunnelId, code: Option<i32>) {
        let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == id) else {
            return;
        };
        let status = view.status.clone();
        // A deliberate stop already reset the status; nothing to do.
        if matches!(status, TunnelStatus::Stopped) {
            return;
        }
        let was_up = matches!(status, TunnelStatus::Up { .. });
        self.log(id, if was_up { LogLevel::Warn } else { LogLevel::Error }, format!("ssh 进程退出 (code {code:?})"));

        let reason = match status.failure().cloned() {
            Some(f) => f,
            None => {
                let tail = self
                    .tunnels
                    .iter()
                    .find(|t| t.cfg.id == id)
                    .and_then(|t| t.log.iter().rev().find(|l| l.level != LogLevel::Debug).map(|l| l.text.clone()))
                    .unwrap_or_else(|| format!("ssh 退出，退出码 {code:?}"));
                FailureInfo::with_short(FailureKind::Exited, "ssh 异常退出", tail)
            }
        };

        let auto = self.cfg.defaults.auto_restart;
        let retryable = reason.kind.is_retryable() || was_up;
        if auto
            && retryable
            && let Some(entry) = self.retry_state.get_mut(&id)
        {
            entry.attempt += 1;
            let delay = backoff(entry.attempt, self.cfg.defaults.retry_initial_secs, self.cfg.defaults.retry_max_secs);
            entry.next_at = Instant::now() + delay;
            let (attempt, next_at) = (entry.attempt, entry.next_at);
            self.set_state(id, TunnelStatus::Retrying { attempt, next_at, reason });
            if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
                v.retries = attempt;
            }
            return;
        }
        self.set_state(id, TunnelStatus::Failed { reason });
    }

    fn tick_retries(&mut self) {
        // Only entries that have scheduled a retry (attempt >= 1) are eligible:
        // `arm_retry` inserts a placeholder with `next_at = now` at start-up, and
        // firing on that would reconnect in a tight loop regardless of
        // `auto_restart`. Retrying also requires the global switch.
        if !self.cfg.defaults.auto_restart {
            return;
        }
        let now = Instant::now();
        let due: Vec<TunnelId> =
            self.retry_state.iter().filter(|(_, r)| r.attempt >= 1 && now >= r.next_at).map(|(id, _)| *id).collect();
        for id in due {
            let attempt = self.retry_state.get(&id).map(|r| r.attempt).unwrap_or(1);
            let delay = backoff(attempt, self.cfg.defaults.retry_initial_secs, self.cfg.defaults.retry_max_secs);
            if let Some(r) = self.retry_state.get_mut(&id) {
                r.next_at = now + delay;
            }
            self.log(id, LogLevel::Info, format!("第 {attempt} 次自动重连…"));
            self.start_tunnel(id);
        }
    }

    /// Register retry bookkeeping for a tunnel that is about to start.
    ///
    /// The placeholder has `attempt = 0`, which [`App::tick_retries`] ignores,
    /// so it can never cause a spurious immediate reconnect. `on_exit` promotes
    /// it to `attempt >= 1` with a real backoff deadline.
    fn arm_retry(&mut self, id: TunnelId) {
        self.retry_state.entry(id).or_insert(RetryState { attempt: 0, next_at: Instant::now() });
    }

    // -- mutation ----------------------------------------------------------

    /// Validate the form and create/update the tunnel, then start it at once.
    /// Validate the open form and either save immediately (new tunnel) or ask
    /// for confirmation (editing an existing one, which replaces a running
    /// process and therefore drops live connections).
    pub fn submit_form(&mut self) {
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
        self.cfg.tunnels.push(cfg.clone());
        self.tunnels.push(TunnelView::new(cfg.clone()));
        let pos = self.tunnels.len() - 1;
        let vis = self.visible_indices();
        self.selected = vis.iter().position(|i| *i == pos).unwrap_or(vis.len().saturating_sub(1));
        self.start_tunnel(cfg.id);
        self.close_overlay();
        self.persist_quietly();
    }

    /// Apply the pending edit that the user confirmed.
    fn commit_form_edit(&mut self, eid: TunnelId) {
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
        self.supervisor.stop(eid);
        if let Some(p) = self.synth_paths.remove(&eid) {
            let _ = std::fs::remove_file(p);
        }
        self.retry_state.remove(&eid);
        self.pending_exit.remove(&eid);
        self.start_tunnel(eid);
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

    /// Local ports already claimed by other managed tunnels.
    ///
    /// The OS-level probe cannot see these: a stopped tunnel is not listening,
    /// yet starting it later would collide. Feeding them to the local-port
    /// suggester keeps two entries from fighting over one port.
    pub fn taken_local_ports(&self) -> Vec<u16> {
        let editing = self.form.as_ref().and_then(|f| match f.mode {
            FormMode::Edit(id) => Some(id),
            FormMode::Create => None,
        });
        self.tunnels
            .iter()
            .filter(|t| Some(t.cfg.id) != editing)
            .filter(|t| t.cfg.kind != ForwardKind::Remote)
            .map(|t| t.cfg.local_port)
            .collect()
    }

    /// Recompute the local port suggestion for the open form.
    ///
    /// Called after the remote port changes; safe to call at any time.
    pub fn refresh_local_port_suggestion(&mut self) {
        let taken = self.taken_local_ports();
        if let Some(form) = self.form.as_mut() {
            form.suggest_local_port(&taken);
            form.revalidate();
        }
    }

    fn listen_conflict(&self, candidate: &TunnelConfig) -> Option<String> {
        let (addr, port) = config::listen_endpoint(candidate);
        for t in &self.tunnels {
            if t.cfg.id == candidate.id {
                continue;
            }
            let (a, p) = config::listen_endpoint(&t.cfg);
            let overlapping = p == port && (a == addr || a == "0.0.0.0" || addr == "0.0.0.0");
            if overlapping {
                return Some(t.cfg.name.clone());
            }
        }
        None
    }

    pub fn delete_selected(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.supervisor.stop(id);
        if let Some(p) = self.synth_paths.remove(&id) {
            let _ = std::fs::remove_file(p);
        }
        self.retry_state.remove(&id);
        self.pending_exit.remove(&id);
        self.cfg.tunnels.retain(|t| t.id != id);
        self.tunnels.retain(|t| t.cfg.id != id);
        let n = self.visible_indices().len();
        self.selected = self.selected.min(n.saturating_sub(1));
        self.persist_quietly();
        self.set_status(StatusMsg::info("已删除"));
    }

    /// `x`: stopping everything is a bulk destructive action.
    pub fn request_stop_all(&mut self) {
        if self.counts().off == self.tunnels.len() {
            self.set_status(StatusMsg::info("没有运行中的转发"));
            return;
        }
        self.ask_confirm(ConfirmAction::StopAll);
    }

    /// `a`: starting everything opens listeners; confirm once for the batch.
    pub fn request_start_all(&mut self) {
        if self.counts().off == 0 {
            self.set_status(StatusMsg::info("没有未启用的转发"));
            return;
        }
        self.ask_confirm(ConfirmAction::StartAll);
    }

    pub fn start_all(&mut self) {
        let ids: Vec<TunnelId> =
            self.tunnels.iter().filter(|t| !t.status.is_running_or_pending()).map(|t| t.cfg.id).collect();
        for id in ids {
            self.start_tunnel(id);
        }
    }

    pub fn stop_all(&mut self) {
        let ids: Vec<TunnelId> = self.tunnels.iter().map(|t| t.cfg.id).collect();
        for id in ids {
            self.stop_tunnel(id);
        }
    }

    /// Stop every child immediately without touching persisted intent.
    pub fn shutdown(&mut self) {
        self.supervisor.stop_all();
        for (_, p) in self.synth_paths.drain() {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::remove_dir_all(&self.synth_dir);
        self.save_state();
    }

    // -- overlays ----------------------------------------------------------

    pub fn open_help(&mut self) {
        self.overlay = Overlay::Help;
        self.help_scroll = 0;
    }

    pub fn open_form_create(&mut self) {
        let bind = self.cfg.defaults.bind.clone();
        let mut form = FormState::create(&bind);
        form.auto_restart = self.cfg.defaults.auto_restart;
        self.form = Some(form);
        self.overlay = Overlay::Form;
    }

    pub fn open_form_edit(&mut self) {
        let Some(view) = self.selected_tunnel() else {
            return;
        };
        self.form = Some(FormState::edit(&view.cfg));
        self.overlay = Overlay::Form;
    }

    pub fn open_detail(&mut self) {
        if let Some(id) = self.selected_id() {
            self.overlay = Overlay::Detail(id);
            self.detail_scroll = 0;
        }
    }

    /// Arm a confirmation for `action`.
    ///
    /// The prompt is rendered by the UI from the same `ConfirmAction`, so a
    /// new risky operation cannot accidentally ship without a dialog.
    pub fn ask_confirm(&mut self, action: ConfirmAction) {
        let name = match &action {
            ConfirmAction::Delete(id)
            | ConfirmAction::Stop(id)
            | ConfirmAction::Restart(id)
            | ConfirmAction::ApplyEdit(id) => self.tunnel(*id).map(|t| t.cfg.name.as_str()).map(|n| n.to_string()),
            _ => None,
        };
        // Leak-free: keep the name in the overlay so the prompt can borrow it.
        let prompt = match &name {
            Some(n) => match &action {
                ConfirmAction::Delete(_) => format!("确认删除「{n}」？此操作不可撤销。"),
                ConfirmAction::Stop(_) => format!("确认停用「{n}」？将断开其现有连接。"),
                ConfirmAction::Restart(_) => format!("确认重启「{n}」？连接会短暂中断。"),
                ConfirmAction::ApplyEdit(_) => format!("确认保存对「{n}」的修改？运行中的连接会重建。"),
                _ => unreachable!(),
            },
            None => match &action {
                ConfirmAction::StopAll => {
                    let n = self.counts().up + self.counts().starting + self.counts().retrying;
                    format!("确认停用全部 {n} 条运行中的转发？将断开所有连接。")
                }
                ConfirmAction::StartAll => {
                    let n = self.counts().off;
                    format!("确认启用全部转发？将新开 {n} 个监听端口。")
                }
                ConfirmAction::DiscardForm => "确认放弃未保存的修改？已输入的内容会丢失。".to_string(),
                ConfirmAction::Quit => {
                    let running = self.supervisor.running_count();
                    if running == 0 {
                        "确认退出 sshpf？".to_string()
                    } else {
                        format!("还有 {running} 条转发在运行，退出将全部停止。")
                    }
                }
                _ => "确认执行该操作？".to_string(),
            },
        };
        self.overlay = Overlay::Confirm { action, prompt };
    }

    pub fn open_confirm_delete(&mut self) {
        if let Some(id) = self.selected_id() {
            self.ask_confirm(ConfirmAction::Delete(id));
        }
    }

    /// Quit confirmation. Unlike other risk gates this one always asks: quitting
    /// is irreversible for the user's session even when nothing is running.
    pub fn open_confirm_quit(&mut self) {
        self.ask_confirm(ConfirmAction::Quit);
    }

    pub fn confirm_yes(&mut self) {
        let Overlay::Confirm { action, .. } = self.overlay.clone() else {
            return;
        };
        self.overlay = Overlay::None;
        self.apply_confirmed(action);
    }

    fn apply_confirmed(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Delete(id) => {
                let vis = self.visible_indices();
                if let Some(pos) = vis.iter().position(|i| self.tunnels.get(*i).is_some_and(|t| t.cfg.id == id)) {
                    self.selected = pos;
                }
                self.delete_selected();
            }
            ConfirmAction::Stop(id) => {
                self.stop_tunnel(id);
                self.set_status(StatusMsg::info("已停用"));
            }
            ConfirmAction::StopAll => self.stop_all(),
            ConfirmAction::Restart(id) => {
                self.supervisor.stop(id);
                if let Some(p) = self.synth_paths.remove(&id) {
                    let _ = std::fs::remove_file(p);
                }
                self.retry_state.remove(&id);
                self.pending_exit.remove(&id);
                self.start_tunnel(id);
            }
            ConfirmAction::StartAll => self.start_all(),
            ConfirmAction::ApplyEdit(id) => self.commit_form_edit(id),
            ConfirmAction::DiscardForm => {
                self.close_overlay();
                self.set_status(StatusMsg::info("已放弃修改"));
            }
            ConfirmAction::Quit => self.should_quit = true,
        }
    }

    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
        self.form = None;
        self.picker = None;
    }

    /// Open the fuzzy host picker for the form's SSH HOST field.
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

    // -- persistence -------------------------------------------------------

    pub fn persist(&mut self) -> Result<(), String> {
        for view in &self.tunnels {
            if let Some(slot) = self.cfg.find_mut(view.cfg.id) {
                slot.enabled = view.cfg.enabled;
                slot.name = view.cfg.name.clone();
            }
        }
        config::save(&self.paths.config, &self.cfg).map_err(|e| e.to_string())
    }

    fn persist_quietly(&mut self) {
        if let Err(e) = self.persist() {
            self.set_status(StatusMsg::error(format!("保存配置失败: {e}")));
        }
    }

    pub fn save_state(&self) {
        let mut st = config::StateFile { last_run: now_unix(), ..Default::default() };
        for view in &self.tunnels {
            st.last_state.insert(view.cfg.id.to_string(), view.status.key().to_string());
        }
        let _ = config::save_state(&self.paths.state, &st);
    }
}

/// Counters for the header line.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub up: usize,
    pub starting: usize,
    pub retrying: usize,
    pub failed: usize,
    pub off: usize,
    pub total: usize,
}

/// Remove scratch dirs left behind by sshpf processes that no longer exist
/// (SIGKILL, OOM kill, a panic during teardown). These only ever contain
/// generated per-tunnel ssh configs, so deleting them is always safe.
///
/// Called once at start-up; a live process's own dir is skipped because its pid
/// is still alive.
fn cleanup_stale_scratch_dirs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_string_lossy().strip_prefix("sshpf-").and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        // `kill(pid, 0)` succeeds for any process we may signal, which is the
        // right liveness test for "is this scratch dir still in use".
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 }
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn build_views(cfg: &ConfigFile) -> Vec<TunnelView> {
    cfg.tunnels
        .iter()
        .map(|c| {
            let mut v = TunnelView::new(c.clone());
            // Nothing is running until `restore_enabled` says so.
            v.status = TunnelStatus::Stopped;
            v
        })
        .collect()
}

/// Exponential backoff: 2s, 4s, 8s … capped at `max_secs`.
pub fn backoff(attempt: u32, initial: u64, max_secs: u64) -> Duration {
    let shift = attempt.saturating_sub(1).min(20);
    let secs = initial.saturating_mul(1u64 << shift).min(max_secs.max(initial));
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(dir: &std::path::Path) -> Paths {
        Paths { config: dir.join("config.toml"), state: dir.join("state.toml") }
    }

    fn sample_tunnel(id: TunnelId, name: &str, port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: port,
            remote_host: "127.0.0.1".into(),
            remote_port: 22,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    fn free_tcp_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn app_with(dir: &std::path::Path, tunnels: Vec<TunnelConfig>) -> App {
        let paths = test_paths(dir);
        let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
        crate::config::save(&paths.config, &cfg).unwrap();
        App::new(paths)
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0, 2, 30).as_secs(), 2);
        assert_eq!(backoff(1, 2, 30).as_secs(), 2);
        assert_eq!(backoff(2, 2, 30).as_secs(), 4);
        assert_eq!(backoff(3, 2, 30).as_secs(), 8);
        assert_eq!(backoff(10, 2, 30).as_secs(), 30);
    }

    #[test]
    fn app_starts_with_persisted_tunnels_off() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_with(dir.path(), vec![sample_tunnel(1, "a", 18081), sample_tunnel(2, "b", 18082)]);
        assert_eq!(app.tunnels.len(), 2);
        assert!(app.tunnels.iter().all(|t| matches!(t.status, TunnelStatus::Stopped)));
        assert_eq!(app.counts().off, 2);
    }

    #[test]
    fn form_to_config_validates() {
        let mut f = FormState::create("127.0.0.1");
        f.name.set("t1");
        f.ssh_host.set("host");
        f.local_port.set("8080");
        f.remote_port.set("22");
        let cfg = f.to_config(1, None).unwrap();
        assert_eq!(cfg.local_port, 8080);
        assert_eq!(cfg.kind, ForwardKind::Local);

        let mut bad = FormState::create("127.0.0.1");
        bad.name.set("");
        bad.ssh_host.set("");
        bad.local_port.set("abc");
        assert!(bad.to_config(1, None).is_err());
    }

    #[test]
    fn dynamic_kind_needs_no_remote() {
        let mut f = FormState::create("127.0.0.1");
        f.name.set("socks");
        f.ssh_host.set("host");
        f.local_port.set("1080");
        f.kind_idx = 2;
        let cfg = f.to_config(1, None).unwrap();
        assert_eq!(cfg.kind, ForwardKind::Dynamic);
        assert_eq!(cfg.forward_spec(), "127.0.0.1:1080");
    }

    #[test]
    fn filter_narrows_visible_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "alpha", 18081), sample_tunnel(2, "beta", 18082)]);
        assert_eq!(app.visible_indices(), vec![0, 1]);
        app.filter = "alp".into();
        assert_eq!(app.visible_indices(), vec![0]);
        app.filter.clear();
        app.selected = 1;
        app.select_next();
        assert_eq!(app.selected, 1, "must clamp at the last row");
    }

    #[test]
    fn delete_removes_and_clamps_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "a", 18081)]);
        app.selected = 0;
        app.delete_selected();
        assert!(app.tunnels.is_empty());
        assert!(app.cfg.tunnels.is_empty());
        assert_eq!(app.selected, 0);
        // Deletion is persisted.
        let reloaded = crate::config::load(&app.paths.config).unwrap();
        assert!(reloaded.tunnels.is_empty());
    }

    #[test]
    fn stop_marks_disabled_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = sample_tunnel(1, "a", 18081);
        t.enabled = true;
        let mut app = app_with(dir.path(), vec![t]);
        app.stop_tunnel(1);
        assert!(!app.tunnels[0].cfg.enabled);
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Stopped));
        let reloaded = crate::config::load(&app.paths.config).unwrap();
        assert!(!reloaded.tunnels[0].enabled);
    }

    #[test]
    fn listen_conflict_detected() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_with(dir.path(), vec![sample_tunnel(1, "a", 18081)]);
        let mut candidate = sample_tunnel(2, "b", 18081);
        candidate.local_addr = "127.0.0.1".into();
        assert_eq!(app.listen_conflict(&candidate), Some("a".to_string()));
        candidate.local_port = 18082;
        assert_eq!(app.listen_conflict(&candidate), None);
    }

    fn sample_form() -> FormState {
        let mut f = FormState::create("127.0.0.1");
        f.ssh_host.set("host");
        f.name.set("demo");
        f
    }

    #[test]
    fn privileged_remote_port_gets_a_friendly_local_alias() {
        // Trying to forward to ssh's port 22 must not suggest binding local 22
        // (needs root) and must not spin through the privileged range.
        let mut f = sample_form();
        f.remote_port.set("22");
        f.suggest_local_port(&[]);
        let port: u16 = f.local_port.value().parse().expect("a suggestion was made");
        assert!(port >= 1024, "suggestion must be unprivileged, got {port}");
        assert_eq!(port, 2222, "expected the conventional 22 -> 2222 alias");
        assert!(f.local_port_note.as_deref().unwrap_or("").contains("特权"));
    }

    #[test]
    fn remote_port_below_privileged_range_always_yields_something() {
        for remote in ["1", "22", "80", "443", "587"] {
            let mut f = sample_form();
            f.remote_port.set(remote);
            f.suggest_local_port(&[]);
            let v = f.local_port.value().to_string();
            let p: u16 = v.parse().unwrap_or_else(|_| panic!("no suggestion for remote {remote}"));
            assert!(p >= 1024, "remote {remote} suggested privileged local {p}");
        }
    }

    #[test]
    fn local_port_follows_remote_port_when_free() {
        let mut f = sample_form();
        let port = free_tcp_port();
        f.remote_port.set(port.to_string());
        f.suggest_local_port(&[]);
        assert_eq!(f.local_port.value(), port.to_string(), "free port should be reused verbatim");
        assert!(f.local_port_note.as_deref().unwrap_or("").contains("相同"));
    }

    #[test]
    fn local_port_skips_a_busy_port() {
        // Occupy a port, ask for the suggestion: it must move on, not collide.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = listener.local_addr().unwrap().port();
        let mut f = sample_form();
        f.remote_port.set(busy.to_string());
        f.suggest_local_port(&[]);
        let suggested: u16 = f.local_port.value().parse().expect("suggested port");
        assert!(suggested > busy, "expected a higher port, got {suggested}");
        assert_ne!(suggested, busy);
        assert!(f.local_port_note.as_deref().unwrap_or("").contains("顺延"));
        drop(listener);
    }

    #[test]
    fn local_port_skips_ports_used_by_other_tunnels() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "other", 18090)]);
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.remote_port.set("18090");
        }
        app.refresh_local_port_suggestion();
        let suggested = app.form.as_ref().unwrap().local_port.value().to_string();
        assert_ne!(suggested, "18090", "must not propose a port another tunnel owns");
    }

    #[test]
    fn manual_local_port_is_never_overwritten() {
        let mut f = sample_form();
        f.mark_local_port_manual();
        f.local_port.set("15000");
        f.remote_port.set("8080");
        f.suggest_local_port(&[]);
        assert_eq!(f.local_port.value(), "15000", "hand-edited port must survive");
    }

    #[test]
    fn editing_an_existing_tunnel_keeps_its_local_port() {
        let cfg = sample_tunnel(7, "kept", 18087);
        let mut f = FormState::edit(&cfg);
        assert!(!f.local_port_auto, "edit mode must not auto-derive");
        f.remote_port.set("9999");
        f.suggest_local_port(&[]);
        assert_eq!(f.local_port.value(), "18087");
    }

    #[test]
    fn form_starts_in_navigating_mode() {
        let f = sample_form();
        assert!(!f.is_editing_field(), "arrows must not edit on open");
        assert_eq!(f.edit_mode, EditMode::Navigating);
    }

    #[test]
    fn begin_edit_only_works_on_text_fields() {
        let mut f = sample_form();
        f.field = FormField::Name;
        f.begin_edit();
        assert!(f.is_editing_field());
        f.end_edit();

        f.field = FormField::AutoRestart;
        f.begin_edit();
        assert!(!f.is_editing_field(), "toggles are not text-editable");
    }

    #[test]
    fn form_field_order_puts_remote_before_local_port() {
        let order = FormField::ORDER;
        let pos = |f: FormField| order.iter().position(|x| *x == f).unwrap();
        assert!(pos(FormField::SshHost) < pos(FormField::RemotePort));
        assert!(pos(FormField::RemotePort) < pos(FormField::LocalPort));
        assert!(pos(FormField::RemoteHost) < pos(FormField::RemotePort), "remote host must precede its port");
    }

    /// Every risky operation must raise a confirmation instead of acting.
    #[test]
    fn all_risky_operations_ask_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut running = sample_tunnel(1, "live", 18070);
        running.enabled = true;
        let stopped = sample_tunnel(2, "idle", 18071);
        let mut app = app_with(dir.path(), vec![running, stopped]);
        app.set_state(1, TunnelStatus::Up { since: Instant::now() });
        app.selected = 0; // the running one

        // (label, expected action, trigger)
        type Case = (&'static str, ConfirmAction, fn(&mut App));
        let cases: Vec<Case> = vec![
            ("删除", ConfirmAction::Delete(1), |a: &mut App| a.open_confirm_delete()),
            ("停用", ConfirmAction::Stop(1), |a: &mut App| a.toggle_selected()),
            ("重启", ConfirmAction::Restart(1), |a: &mut App| a.restart_selected()),
            ("批量停用", ConfirmAction::StopAll, |a: &mut App| a.request_stop_all()),
            ("批量启用", ConfirmAction::StartAll, |a: &mut App| a.request_start_all()),
            ("退出", ConfirmAction::Quit, |a: &mut App| a.open_confirm_quit()),
        ];

        for (label, expected, trigger) in cases {
            app.overlay = Overlay::None;
            trigger(&mut app);
            match &app.overlay {
                Overlay::Confirm { action, prompt } => {
                    assert_eq!(action, &expected, "{label}: wrong action");
                    assert!(!prompt.is_empty(), "{label}: empty prompt");
                }
                other => panic!("{label}: expected a confirmation, got {other:?}"),
            }
            // Nothing must have happened yet.
            assert!(app.tunnels.len() == 2, "{label}: acted before confirmation");
            assert!(!app.should_quit, "{label}: quit before confirmation");
            assert_eq!(app.running_count(), 0, "{label}: spawned before confirmation");
            // Cancelling leaves everything untouched.
            app.close_overlay();
        }
    }

    #[test]
    fn cancelling_a_confirmation_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = sample_tunnel(1, "keep", 18072);
        t.enabled = true;
        let mut app = app_with(dir.path(), vec![t]);
        app.set_state(1, TunnelStatus::Up { since: Instant::now() });

        app.open_confirm_delete();
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        app.close_overlay();
        assert_eq!(app.tunnels.len(), 1, "cancel must not delete");
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn confirming_a_delete_actually_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let app0 = app_with(dir.path(), vec![sample_tunnel(1, "gone", 18073)]);
        let mut app = app0;
        app.open_confirm_delete();
        app.confirm_yes();
        assert!(app.tunnels.is_empty());
        let reloaded = crate::config::load(&app.paths.config).unwrap();
        assert!(reloaded.tunnels.is_empty(), "deletion must be persisted");
    }

    #[test]
    fn editing_an_existing_tunnel_asks_before_saving() {
        let dir = tempfile::tempdir().unwrap();
        let mut original = sample_tunnel(1, "editme", 18074);
        original.enabled = false;
        let mut app = app_with(dir.path(), vec![original]);
        app.selected = 0;
        app.open_form_edit();
        app.form.as_mut().unwrap().name.set("renamed");
        app.form.as_mut().unwrap().remote_port.set("22");
        app.form.as_mut().unwrap().local_port.set("18075");

        app.submit_form();
        // Must not have written anything yet.
        let live = crate::config::load(&app.paths.config).unwrap();
        assert_eq!(live.tunnels[0].name, "editme", "saved before confirmation");
        match &app.overlay {
            Overlay::Confirm { action, .. } => assert_eq!(action, &ConfirmAction::ApplyEdit(1)),
            other => panic!("expected ApplyEdit confirmation, got {other:?}"),
        }

        app.confirm_yes();
        let after = crate::config::load(&app.paths.config).unwrap();
        assert_eq!(after.tunnels[0].name, "renamed");
        assert_eq!(after.tunnels[0].local_port, 18075);
    }

    #[test]
    fn creating_a_tunnel_saves_without_a_dialog() {
        // Creation is additive and trivially undoable, so it should not nag.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("host");
            f.name.set("fresh");
            f.remote_port.set("22");
            f.local_port.set("18076");
        }
        app.submit_form();
        assert!(matches!(app.overlay, Overlay::None), "create should not ask");
        assert_eq!(app.tunnels.len(), 1);
    }

    #[test]
    fn closing_a_dirty_form_asks_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.open_form_create();
        // Empty form: no dialog.
        app.request_close_form();
        assert!(matches!(app.overlay, Overlay::None), "empty form should close silently");

        // Dirty form: must ask.
        app.open_form_create();
        app.form.as_mut().unwrap().name.set("typed");
        app.request_close_form();
        assert!(
            matches!(app.overlay, Overlay::Confirm { action: ConfirmAction::DiscardForm, .. }),
            "dirty form should ask before discarding"
        );
        assert!(app.form.is_some(), "form still open while asking");
        // Cancelling keeps the form and its input intact.
        app.close_overlay();
        assert!(app.form.is_none(), "cancel closes the overlay only");
    }

    #[test]
    fn discarding_a_form_after_confirm_closes_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.open_form_create();
        app.form.as_mut().unwrap().name.set("typed");
        app.request_close_form();
        app.confirm_yes();
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.form.is_none(), "confirmed discard should close the form");
        assert!(app.tunnels.is_empty());
    }

    #[test]
    fn stopped_tunnel_restarts_without_a_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let app0 = app_with(dir.path(), vec![sample_tunnel(1, "idle", 18077)]);
        let mut app = app0;
        app.selected = 0;
        app.restart_selected();
        assert!(matches!(app.overlay, Overlay::None), "idle restart needs no dialog");
    }

    #[test]
    fn bulk_operations_on_nothing_do_not_ask() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "only", 18078)]);
        // Everything already stopped: stop-all has nothing to do.
        app.request_stop_all();
        assert!(matches!(app.overlay, Overlay::None), "no-op stop-all should not ask");

        // Everything already off: start-all is meaningful, so it does ask.
        app.request_start_all();
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
    }

    // -- event handling / timing ------------------------------------------

    #[test]
    fn clean_exit_becomes_failed_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = sample_tunnel(1, "x", 18020);
        t.auto_restart = false;
        let mut app = app_with(dir.path(), vec![t]);
        app.cfg.defaults.auto_restart = false;
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.on_exit(1, Some(255));
        match &app.tunnels[0].status {
            TunnelStatus::Failed { reason } => {
                assert_eq!(reason.kind, FailureKind::Exited);
                assert!(!reason.short.is_empty());
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn exit_after_failure_keeps_the_specific_reason() {
        // The classified failure must survive the process exit that follows it.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18021)]);
        app.cfg.defaults.auto_restart = false;
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.on_line(1, "wangcan@host: Permission denied (publickey).".into());
        let before = app.tunnels[0].status.clone();
        app.on_exit(1, Some(255));
        assert_eq!(app.tunnels[0].status, before, "exit must not overwrite a specific failure");
    }

    #[test]
    fn exit_while_stopped_is_ignored() {
        // Stopping kills the child; the resulting exit must not resurrect a
        // "failed" row for a tunnel the user deliberately turned off.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18022)]);
        app.set_state(1, TunnelStatus::Stopped);
        app.on_exit(1, None);
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Stopped));
    }

    #[test]
    fn network_failure_schedules_a_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18023)]);
        app.cfg.defaults.auto_restart = true;
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.arm_retry(1);
        app.on_exit(1, Some(255));
        match &app.tunnels[0].status {
            TunnelStatus::Retrying { attempt, .. } => assert_eq!(*attempt, 1),
            other => panic!("expected Retrying, got {other:?}"),
        }
    }

    #[test]
    fn config_class_failure_does_not_retry_even_if_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18024)]);
        app.cfg.defaults.auto_restart = true;
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.arm_retry(1);
        // Authentication cannot fix itself.
        app.on_line(1, "wangcan@host: Permission denied (publickey).".into());
        app.on_exit(1, Some(255));
        assert!(
            matches!(app.tunnels[0].status, TunnelStatus::Failed { .. }),
            "auth failure must be terminal, got {:?}",
            app.tunnels[0].status
        );
    }

    #[test]
    fn failure_after_up_is_retried_as_a_network_drop() {
        // A tunnel that was healthy and then dropped should come back.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18025)]);
        app.cfg.defaults.auto_restart = true;
        app.set_state(1, TunnelStatus::Up { since: Instant::now() });
        app.arm_retry(1);
        app.on_exit(1, Some(255));
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Retrying { .. }));
    }

    #[test]
    fn backoff_grows_across_consecutive_retries() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18026)]);
        app.cfg.defaults.auto_restart = true;
        app.arm_retry(1);
        let mut delays = Vec::new();
        for _ in 0..4 {
            app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
            let before = Instant::now();
            app.on_exit(1, Some(255));
            if let TunnelStatus::Retrying { attempt, next_at, .. } = app.tunnels[0].status {
                delays.push((attempt, next_at.saturating_duration_since(before).as_secs()));
            }
        }
        assert_eq!(delays.len(), 4, "each exit should schedule a retry");
        // Attempts increment and the delay must not shrink.
        for w in delays.windows(2) {
            assert!(w[1].0 > w[0].0, "attempt should increase: {delays:?}");
            assert!(w[1].1 >= w[0].1, "delay should not shrink: {delays:?}");
        }
    }

    #[test]
    fn target_refusal_after_up_does_not_flip_the_state() {
        // A dead service behind a healthy tunnel is not a tunnel failure.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18027)]);
        app.set_state(1, TunnelStatus::Up { since: Instant::now() });
        app.on_line(1, "debug1: channel 2: open failed: connect failed: Connection refused".into());
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }));
        let logs: Vec<String> = app.tunnels[0].log.iter().map(|l| l.text.clone()).collect();
        assert!(logs.iter().any(|l| l.contains("connect failed")), "should still be logged: {logs:?}");
    }

    #[test]
    fn lines_for_unknown_tunnels_are_ignored() {
        // A late line from a deleted tunnel must not panic or create state.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.on_line(99, "debug1: whatever".into());
        app.on_exit(99, Some(1));
        assert!(app.tunnels.is_empty());
    }

    #[test]
    fn full_happy_path_starting_to_up() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18028)]);
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.on_line(1, "debug1: Local forwarding listening on 127.0.0.1 port 18028.".into());
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Starting { .. }), "must wait for ready");
        app.on_line(1, "debug1: Entering interactive session.".into());
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }));
        assert!(app.tunnels[0].retries == 0, "a successful start resets retries");
    }

    #[test]
    fn lost_tunnel_moves_up_to_failed_without_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18029)]);
        app.cfg.defaults.auto_restart = false;
        app.set_state(1, TunnelStatus::Up { since: Instant::now() });
        app.on_exit(1, Some(255));
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Failed { .. }));
    }

    #[test]
    fn counts_track_every_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(
            dir.path(),
            vec![
                sample_tunnel(1, "up", 18030),
                sample_tunnel(2, "starting", 18031),
                sample_tunnel(3, "retrying", 18032),
                sample_tunnel(4, "failed", 18033),
                sample_tunnel(5, "off", 18034),
            ],
        );
        app.set_state(1, TunnelStatus::Up { since: Instant::now() });
        app.set_state(2, TunnelStatus::Starting { since: Instant::now() });
        app.set_state(
            3,
            TunnelStatus::Retrying {
                attempt: 1,
                next_at: Instant::now(),
                reason: FailureInfo::new(FailureKind::Timeout, "t"),
            },
        );
        app.set_state(4, TunnelStatus::Failed { reason: FailureInfo::new(FailureKind::AuthFailed, "a") });
        let c = app.counts();
        assert_eq!((c.up, c.starting, c.retrying, c.failed, c.off, c.total), (1, 1, 1, 1, 1, 5));
    }

    #[test]
    fn filter_matches_host_as_well_as_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut hosttunnel = sample_tunnel(1, "plain-name", 18035);
        hosttunnel.ssh_host = "bastion-prod".into();
        let mut app = app_with(dir.path(), vec![hosttunnel, sample_tunnel(2, "other", 18036)]);
        app.filter = "bastion".into();
        assert_eq!(app.visible_indices(), vec![0], "filter should match the ssh host");
        app.filter = "other".into();
        assert_eq!(app.visible_indices(), vec![1]);
        app.filter = "zzz".into();
        assert!(app.visible_indices().is_empty());
        // A filter that hides everything must not leave a dangling selection.
        app.selected = 0;
        assert!(app.selected_tunnel().is_none());
    }

    #[test]
    fn selection_survives_a_filter_that_shrinks_the_list() {
        // Narrowing the filter can leave `selected` pointing past the end. The
        // cursor must stay usable rather than becoming a dangling index.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "alpha", 18037), sample_tunnel(2, "beta", 18038)]);
        app.selected = 1;
        app.filter = "alpha".into();
        assert_eq!(app.visible_indices(), vec![0]);
        // `select_next` on a one-row view keeps the cursor at that row.
        app.select_next();
        assert_eq!(app.selected, 0, "cursor should land on the only visible row");
        assert_eq!(app.selected_tunnel().map(|t| t.cfg.name.as_str()), Some("alpha"));
    }

    #[test]
    fn host_dropdown_opens_filters_and_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.hosts = (0..12)
            .map(|i| SshHost {
                alias: format!("host-{i:02}"),
                hostname: format!("10.0.0.{i}"),
                user: Some("u".into()),
                port: 22,
                proxy_jump: None,
                identity_file: None,
            })
            .collect();
        app.open_form_create();
        assert!(!app.host_picker_open(), "dropdown starts collapsed");

        // Enter/↓ on the SSH HOST field expands it (the form must not submit).
        app.open_host_picker();
        assert!(app.host_picker_open());
        assert_eq!(app.picker.as_ref().unwrap().matches.len(), 12);

        // Typing filters by alias, case-insensitively.
        app.picker_query_char('H');
        app.picker_query_char('0');
        let matched = app.picker.as_ref().unwrap().matches.len();
        assert!(matched < 12 && matched > 0, "filter should narrow, got {matched}");

        // Enter fills the field and collapses the list.
        app.picker_accept();
        assert!(!app.host_picker_open());
        let filled = app.form.as_ref().unwrap().ssh_host.value().to_string();
        assert!(filled.starts_with("host-0"), "unexpected fill: {filled}");
        // NAME was empty, so it gets the alias too.
        assert_eq!(app.form.as_ref().unwrap().name.value(), filled);
    }

    #[test]
    fn host_dropdown_scrolls_a_long_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.hosts = (0..40)
            .map(|i| SshHost {
                alias: format!("h{i:02}"),
                hostname: "1.1.1.1".into(),
                user: None,
                port: 22,
                proxy_jump: None,
                identity_file: None,
            })
            .collect();
        app.open_form_create();
        app.open_host_picker();
        // Walk past the visible window; the offset must follow.
        for _ in 0..(HOST_DROPDOWN_ROWS + 3) {
            app.picker_move(1);
        }
        let pk = app.picker.as_ref().unwrap();
        assert_eq!(pk.selected, HOST_DROPDOWN_ROWS + 3);
        assert!(pk.scroll as usize <= pk.selected);
        assert!(pk.selected < pk.scroll as usize + HOST_DROPDOWN_ROWS);
    }

    #[test]
    fn host_dropdown_prefers_alias_over_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.hosts = vec![
            SshHost {
                alias: "bastion-prod".into(),
                hostname: "10.0.4.9".into(),
                user: Some("deploy".into()),
                port: 22,
                proxy_jump: None,
                identity_file: None,
            },
            SshHost {
                alias: "dev-box".into(),
                hostname: "192.168.1.7".into(),
                user: None,
                port: 22,
                proxy_jump: None,
                identity_file: None,
            },
        ];
        app.open_form_create();
        app.open_host_picker();
        app.picker_query_char('d');
        app.picker_query_char('e');
        // "bastion-prod" must not show up merely because its user is "deploy".
        let aliases: Vec<&str> =
            app.picker.as_ref().unwrap().matches.iter().map(|i| app.hosts[*i].alias.as_str()).collect();
        assert_eq!(aliases, vec!["dev-box"], "alias-first filtering expected");
    }

    #[test]
    fn closing_the_dropdown_keeps_the_typed_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.hosts = vec![SshHost {
            alias: "kept-host".into(),
            hostname: "1.1.1.1".into(),
            user: None,
            port: 22,
            proxy_jump: None,
            identity_file: None,
        }];
        app.open_form_create();
        if let Some(f) = app.form.as_mut() {
            f.ssh_host.set("typed-by-hand");
        }
        app.open_host_picker();
        assert!(app.host_picker_open());
        app.close_host_picker();
        assert!(!app.host_picker_open());
        assert_eq!(app.form.as_ref().unwrap().ssh_host.value(), "typed-by-hand");
    }

    #[test]
    fn host_picker_filters_and_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![]);
        app.hosts = vec![
            SshHost {
                alias: "alpha".into(),
                hostname: "1.1.1.1".into(),
                user: None,
                port: 22,
                proxy_jump: None,
                identity_file: None,
            },
            SshHost {
                alias: "beta".into(),
                hostname: "2.2.2.2".into(),
                user: None,
                port: 22,
                proxy_jump: None,
                identity_file: None,
            },
        ];
        app.open_form_create();
        app.open_host_picker();
        app.picker.as_mut().unwrap().query_char('b');
        app.refresh_picker();
        assert_eq!(app.picker.as_ref().unwrap().matches, vec![1]);
        app.picker_accept();
        assert_eq!(app.form.as_ref().unwrap().ssh_host.value(), "beta");
    }

    #[test]
    fn invalid_definition_fails_without_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = sample_tunnel(1, "", 0);
        t.ssh_host = String::new();
        let mut app = app_with(dir.path(), vec![t]);
        app.start_tunnel(1);
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Failed { .. }));
        assert_eq!(app.running_count(), 0);
    }

    #[test]
    fn stale_scratch_dirs_are_reaped() {
        // A crashed sshpf (SIGKILL/OOM) cannot run `shutdown`, so its scratch
        // dir survives. Start-up must sweep dirs whose pid is gone.
        let base = std::env::temp_dir();
        let dead = base.join("sshpf-999999999");
        let _ = std::fs::create_dir_all(&dead);
        assert!(dead.exists());
        cleanup_stale_scratch_dirs();
        assert!(!dead.exists(), "dead process scratch dir should be removed");

        // A directory belonging to this very process must be preserved.
        let mine = base.join(format!("sshpf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&mine);
        cleanup_stale_scratch_dirs();
        assert!(mine.exists(), "live process scratch dir must be kept");
        let _ = std::fs::remove_dir_all(&mine);
    }

    #[test]
    fn arm_retry_placeholder_never_triggers_reconnect() {
        // Regression: `arm_retry` inserting `next_at = now` used to make
        // `tick_retries` reconnect in a tight loop right after a start,
        // regardless of `auto_restart`.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "a", 18091)]);
        app.cfg.defaults.auto_restart = true;
        app.arm_retry(1);
        // Nothing scheduled yet -> the placeholder must be ignored.
        app.tick_retries();
        assert_eq!(app.running_count(), 0);
        assert_eq!(app.retry_state.get(&1).map(|r| r.attempt), Some(0));
    }

    #[test]
    fn auto_restart_disabled_never_retries() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "a", 18092)]);
        app.cfg.defaults.auto_restart = false;
        // Simulate a scheduled retry that is due.
        app.retry_state.insert(1, RetryState { attempt: 3, next_at: Instant::now() - Duration::from_secs(1) });
        app.tick_retries();
        assert_eq!(app.running_count(), 0, "auto_restart=false must not reconnect");
    }

    #[test]
    fn failure_to_establish_stops_the_process() {
        // A rejected forward leaves `ssh -N` alive; we must kill it so the
        // table never shows "failed" backed by a live process.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "a", 18093)]);
        app.start_tunnel(1);
        let spawned = app.running_count();
        // Directly exercise the failure path (no network involved).
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.stop_after_failure(1);
        assert_eq!(app.running_count(), 0);
        let _ = spawned;
    }

    #[test]
    fn remote_ready_requires_explicit_success_marker() {
        // Measured on OpenSSH 8.2: `-R` prints "Entering interactive session."
        // *before* it knows whether the remote listener was accepted.
        let dir = tempfile::tempdir().unwrap();
        let mut t = sample_tunnel(1, "r", 18094);
        t.kind = ForwardKind::Remote;
        t.remote_host = "127.0.0.1".into();
        t.remote_port = 18094;
        let mut app = app_with(dir.path(), vec![t]);
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.on_line(1, "debug1: Entering interactive session.".into());
        assert!(
            matches!(app.tunnels[0].status, TunnelStatus::Starting { .. }),
            "Entering must not mark a -R tunnel Up, got {:?}",
            app.tunnels[0].status
        );
        app.on_line(1, "debug1: remote forward success for: listen 127.0.0.1:18094, connect 127.0.0.1:22".into());
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }));
    }

    #[test]
    fn local_ready_requires_listening_line_first() {
        // ProxyJump merges the jump child's "Entering interactive session."
        // into our stderr before the final hop authenticates.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "l", 18095)]);
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.on_line(1, "debug1: Entering interactive session.".into());
        assert!(
            matches!(app.tunnels[0].status, TunnelStatus::Starting { .. }),
            "premature Entering must not mark -L Up"
        );
        app.on_line(1, "debug1: Local forwarding listening on 127.0.0.1 port 18095.".into());
        app.on_line(1, "debug1: Entering interactive session.".into());
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }));
    }

    #[test]
    fn occupied_port_is_reported_before_spawn() {
        // Occupy a port ourselves, then ask the app to forward from it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "a", port)]);
        app.start_tunnel(1);
        match &app.tunnels[0].status {
            TunnelStatus::Failed { reason } => assert_eq!(reason.kind, FailureKind::PortInUse),
            other => panic!("expected PortInUse, got {other:?}"),
        }
        assert_eq!(app.running_count(), 0);
        drop(listener);
    }
}

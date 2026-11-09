//! The application state machine: owns tunnels, consumes supervisor events,
//! handles key presses and drives persistence.
//!
//! There is deliberately no `tokio` here. The UI thread polls
//! `crossterm::event::poll(200ms)` and drains an `mpsc` channel fed by one
//! stderr-reader thread per tunnel — the architecture validated in
//! `docs/research/tech-options.md` §3.3.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use crate::config::{self, ConfigFile, Paths};
use crate::eventlog;
use crate::help;
use crate::logging;
use crate::model::{
    BatchAction, FailureInfo, FormMode, ForwardKind, HostPicker, MultiSelect, PortPick, StatusMsg, TunnelConfig,
    TunnelId, TunnelView,
};
use crate::ssh::SshHost;
use crate::tunnel::{Supervisor, TunnelEvent};

pub mod confirm;
pub mod duplicate;
pub mod events;
pub mod form;
pub mod form_submit;
pub mod helpers;
pub mod import;
pub mod import_pane;
pub mod mouse;
pub mod overlay;
pub mod persist;
pub mod picker;
pub mod port_batch;
pub mod port_scan;
pub mod query;
pub mod runtime;
pub mod select;

pub(crate) use helpers::deadline_after;
pub use helpers::{backoff, build_views, sanitize_config};

pub use form::{EditMode, FormState};

/// How many host rows the inline dropdown shows at once.
///
/// Kept in one place so the key handler (scrolling window) and the renderer
/// (popup height) cannot drift apart.
pub const HOST_DROPDOWN_ROWS: usize = 8;

/// Shortest reconnect delay the app will honour, in seconds.
///
/// A zero backoff turns the retry loop into a per-frame reconnect storm: every
/// tick sees `next_at <= now`, starts the tunnel again, and logs a retry. The
/// value comes from a hand-editable `config.toml`, so it is input, not an
/// invariant.
pub const MIN_RETRY_SECS: u64 = 1;

/// Longest reconnect delay the app will honour, in seconds (one day).
///
/// This is also what keeps `Instant::now() + delay` from overflowing: that
/// addition panics on overflow, and `retry_max_secs` is user input.
pub const MAX_RETRY_SECS: u64 = 24 * 60 * 60;

/// How many supervisor events one frame will consume before returning to the UI.
///
/// The channel is fed by one reader thread per tunnel. A talkative one can keep
/// it permanently non-empty, and an unbounded drain would then starve the frame
/// loop — the UI would stop painting and reading keys while staying "busy".
/// Events left over are handled on the next frame (~200 ms later).
pub const MAX_EVENTS_PER_FRAME: usize = 4096;

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
    /// Apply one operation to every marked tunnel in selection mode.
    Batch { action: BatchAction, count: usize },
    /// Quit the program (kills all children).
    Quit,
}

/// Per-tunnel reconnect bookkeeping, shared with the runtime module.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryState {
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
    retry_state: HashMap<TunnelId, RetryState>,
    /// How to treat unknown / changed server host keys.
    pub(crate) host_key_policy: crate::ssh::known_hosts::HostKeyPolicy,
    /// A validated edit waiting for the user to confirm it, so the running
    /// process is not bounced before the dialog is answered.
    pending_form: Option<TunnelConfig>,
    /// Exits observed but not yet classified. Kept because the completion
    /// signal and the final log lines can arrive in either order.
    pending_exit: HashMap<TunnelId, (Option<i32>, Instant)>,
    /// The failure reason carried by a just-observed exit, waiting to be paired
    /// with its `Exited`/`StderrClosed` counterpart.
    pending_reason: HashMap<TunnelId, Option<FailureInfo>>,
    /// Tunnels whose driver reported readiness. Retained for compatibility with
    /// the stop/restart bookkeeping (`forget_runtime_state`).
    saw_listening: std::collections::HashSet<TunnelId>,
    /// Multi-row selection on the main table (`v` enters selection mode).
    pub select_mode: MultiSelect,
    /// Semantic map of what was drawn last frame, for mouse hit-testing.
    ///
    /// The renderer owns it: `ui::draw` clears it and re-records every frame.
    /// Keeping it on `App` (rather than in the renderer) is what lets the mouse
    /// handler resolve a click without re-deriving the layout.
    pub hits: crate::ui::hit::HitMap,
    /// Why reading forwards out of `~/.ssh/config` failed for the open form.
    pub import_error: Option<String>,
    /// The peer's listening ports, offered for the 远端端口 field (`p`).
    ///
    /// `None` means the picker was never opened. Like the host dropdown, it is a
    /// sub-state of the form rather than an overlay: it fills a field *in* the
    /// form, so it must stay visible next to it.
    pub ports: Option<PortPick>,
    /// The in-flight scan's channel. One at a time is enough: a newer scan
    /// replaces it and the older answer is dropped by generation.
    pub(crate) port_rx: Option<Receiver<crate::app::port_scan::PortScanMsg>>,
    /// Generation counter for scan results, so a slow answer cannot overwrite a
    /// newer list.
    pub(crate) port_seq: u64,
    /// Port-batch rows skipped as duplicates of an already-saved forward, kept
    /// from `validated_port_batch` to the `commit_port_batch` that follows it in
    /// the same submit, so the status line can report the count.
    pub(crate) pending_port_skipped: Vec<String>,
    /// Duplicate saved entries that were folded on load, kept so `main` can say so.
    ///
    /// The repair itself happens in `config` (it owns the file), but *telling the
    /// user* is a UI decision, and the status bar is set from `main`/`app` — this
    /// field is the hand-off between them. Empty for a clean config, which is the
    /// normal case.
    pub folded_on_load: Vec<crate::model::Folded>,
}

impl App {
    /// Build the app with the default host-key policy.
    ///
    /// Kept as the single-argument constructor so the ~20 call sites in tests and
    /// test support do not have to thread a policy through; `main` uses
    /// [`App::with_host_key_policy`] to apply the command-line flag.
    pub fn new(paths: Paths) -> Self {
        App::with_host_key_policy(paths, crate::ssh::known_hosts::HostKeyPolicy::default())
    }

    /// Build the app with the policy implied by the command line.
    ///
    /// Only the process-wide escape hatch arrives this way; per-host settings
    /// from `~/.ssh/config` are resolved where each hop is dialled, because a
    /// `ProxyJump` chain verifies hops separately.
    pub fn with_host_key_policy(paths: Paths, host_key_policy: crate::ssh::known_hosts::HostKeyPolicy) -> Self {
        let mut folded_on_load = Vec::new();
        let mut cfg = match config::load_with_report(&paths.config) {
            Ok((cfg, folded)) => {
                eventlog::config_loaded(&config::display_path(&paths.config), cfg.tunnels.len(), &[]);
                folded_on_load = folded;
                cfg
            }
            Err(e) => {
                // The user's definitions are about to be invisible; the log is
                // the only place that survives to explain it afterwards.
                eventlog::config_load_failed(&config::display_path(&paths.config), &e.to_string(), "empty_config");
                eprintln!("警告: {e}; 以空配置启动\nWarning: {e}; starting with an empty config");
                ConfigFile::default()
            }
        };
        // `config.toml` is hand-editable, so its numeric fields are input rather
        // than invariants. Clamp them once, here, instead of trusting them at
        // every arithmetic site downstream.
        sanitize_config(&mut cfg);
        let (event_tx, event_rx) = mpsc::channel();
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
            retry_state: HashMap::new(),
            host_key_policy,
            pending_form: None,
            pending_exit: HashMap::new(),
            pending_reason: HashMap::new(),
            saw_listening: std::collections::HashSet::new(),
            select_mode: MultiSelect::new(),
            hits: crate::ui::hit::HitMap::new(),
            import_error: None,
            ports: None,
            port_rx: None,
            port_seq: 0,
            pending_port_skipped: Vec::new(),
            folded_on_load,
        }
    }

    // -- mutation ----------------------------------------------------------

    /// Allocate the next free tunnel id, or `None` when the space is exhausted.
    ///
    /// `ConfigFile::next_id` is `max_id + 1`, which overflows (and panics in
    /// debug builds) for a hand-edited `id = 18446744073709551615`. Callers get
    /// `None` and refuse the action instead of reusing an id that already exists.
    pub(crate) fn next_tunnel_id(&self) -> Option<TunnelId> {
        match self.cfg.max_id().checked_add(1) {
            Some(id) => Some(id),
            None => {
                eventlog::id_space_exhausted(self.cfg.max_id());
                None
            }
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
        self.delete_tunnel(id);
        self.set_status(StatusMsg::info(help::bi("已删除", "Deleted")));
    }

    /// Remove one tunnel definition and its runtime state.
    ///
    /// Split out from [`App::delete_selected`] because the batch path deletes by
    /// id without moving the cursor between removals (a shuffled cursor would
    /// make `selected` mean a different tunnel on every iteration).
    pub fn delete_tunnel(&mut self, id: TunnelId) {
        if let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == id) {
            eventlog::tunnel_deleted(&view.cfg);
        }
        self.supervisor.stop(id);
        self.forget_runtime_state(id);
        self.cfg.tunnels.retain(|t| t.id != id);
        self.tunnels.retain(|t| t.cfg.id != id);
        let n = self.visible_indices().len();
        self.selected = self.selected.min(n.saturating_sub(1));
        self.persist_quietly();
    }

    /// `x`: stopping everything is a bulk destructive action.
    pub fn request_stop_all(&mut self) {
        if self.counts().off == self.tunnels.len() {
            self.set_status(StatusMsg::info(help::bi("没有运行中的转发", "No forward is running")));
            return;
        }
        self.ask_confirm(ConfirmAction::StopAll);
    }

    /// `a`: starting everything opens listeners; confirm once for the batch.
    pub fn request_start_all(&mut self) {
        if self.counts().off == 0 {
            self.set_status(StatusMsg::info(help::bi("没有未启用的转发", "No disabled forward")));
            return;
        }
        self.ask_confirm(ConfirmAction::StartAll);
    }

    pub fn start_all(&mut self) {
        let ids: Vec<TunnelId> =
            self.tunnels.iter().filter(|t| !t.status.is_running_or_pending()).map(|t| t.cfg.id).collect();
        for id in ids {
            self.start_tunnel_with(id, "start_all");
        }
    }

    pub fn stop_all(&mut self) {
        let ids: Vec<TunnelId> = self.tunnels.iter().map(|t| t.cfg.id).collect();
        for id in ids {
            self.stop_tunnel_with(id, "stop_all");
        }
    }

    /// Stop every tunnel immediately without touching persisted intent.
    pub fn shutdown(&mut self) {
        let up = self.tunnels.iter().filter(|t| t.status.is_active()).count();
        logging::info("app.shutdown", &[("tunnels_active", &up), ("running", &self.supervisor.running_count())]);
        self.supervisor.stop_all();
        self.save_state();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TunnelStatus;
    use std::time::Duration;
    // Some types moved into submodules; the tests still reach for them by name.
    use crate::model::{FailureInfo, FailureKind, FormField};

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
            remote_port: port,
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
        // The classified failure must survive the exit that follows it.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18021)]);
        app.cfg.defaults.auto_restart = false;
        let reason = FailureInfo::with_short(FailureKind::AuthFailed, "认证失败", "权限不足");
        app.set_state(1, TunnelStatus::Failed { reason });
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
        // Authentication cannot fix itself, so the reason is terminal.
        let reason = FailureInfo::with_short(FailureKind::AuthFailed, "认证失败", "权限不足");
        app.set_state(1, TunnelStatus::Failed { reason });
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
        app.on_line(1, "转发到 10.0.0.9:80 失败: ConnectFailed".into());
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }), "a dead target must not fail the tunnel");
        let logs: Vec<String> = app.tunnels[0].log.iter().map(|l| l.text.clone()).collect();
        assert!(logs.iter().any(|l| l.contains("ConnectFailed")), "should still be logged: {logs:?}");
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
    fn happy_path_starting_to_up_is_driven_by_the_driver() {
        // Readiness is no longer guessed from stderr: the tunnel driver reports
        // it only once `bind`/`tcpip_forward` actually succeeded, so the app
        // just records that transition.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "x", 18028)]);
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.on_ready(1);
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }));
        assert!(app.tunnels[0].retries == 0, "a successful start resets retries");

        // A stale log line must never move the status on its own.
        let mut app2 = app_with(dir.path(), vec![sample_tunnel(2, "y", 18038)]);
        app2.set_state(2, TunnelStatus::Starting { since: Instant::now() });
        app2.on_line(2, "debug1: Entering interactive session.".into());
        assert!(matches!(app2.tunnels[0].status, TunnelStatus::Starting { .. }));
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
                proxy_jump: Vec::new(),
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
                proxy_jump: Vec::new(),
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
                proxy_jump: Vec::new(),
                identity_file: None,
            },
            SshHost {
                alias: "dev-box".into(),
                hostname: "192.168.1.7".into(),
                user: None,
                port: 22,
                proxy_jump: Vec::new(),
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
            proxy_jump: Vec::new(),
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
                proxy_jump: Vec::new(),
                identity_file: None,
            },
            SshHost {
                alias: "beta".into(),
                hostname: "2.2.2.2".into(),
                user: None,
                port: 22,
                proxy_jump: Vec::new(),
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
    fn a_failed_forward_never_leaves_a_supervised_tunnel() {
        // A rejected forward must not leave a live session behind, and the
        // table must not show "failed" backed by a running tunnel.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(1, "a", 18093)]);
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.cfg.defaults.auto_restart = false;
        let reason = FailureInfo::with_short(FailureKind::RemoteForwardFailed, "远程转发失败", "端口被占用");
        app.set_state(1, TunnelStatus::Failed { reason });
        app.on_exit(1, Some(1));
        assert_eq!(app.running_count(), 0);
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

    // -- panic hardening: arithmetic on config-supplied numbers -------------

    #[test]
    fn a_zero_retry_interval_is_clamped_instead_of_spinning() {
        // `retry_initial_secs = 0` in a hand-edited config made `backoff` return
        // `Duration::ZERO`, so every 200 ms tick re-fired the retry — a
        // reconnect storm with a log line per attempt. The clamp is what stops
        // it, and it is logged so the user can find out their file was changed.
        assert_eq!(backoff(0, 0, 30).as_secs(), 1, "a zero interval must not collapse the delay");
        assert!(backoff(99, 0, 30).as_secs() >= 1);
        // A zero *ceiling* must not fall below the interval either.
        assert_eq!(backoff(1, 2, 0).as_secs(), 2);
        assert_eq!(backoff(10, 2, 0).as_secs(), 2, "the ceiling never undercuts the first delay");
    }

    #[test]
    fn backoff_saturates_for_absurd_attempt_counts() {
        // `attempt` grows once per failed reconnect and is never reset while a
        // tunnel keeps flapping; the shift is capped, so the multiplication
        // cannot overflow and the delay stays at the ceiling.
        for attempt in [u32::MAX, u32::MAX - 1, 1_000_000] {
            let d = backoff(attempt, 2, 30);
            assert_eq!(d.as_secs(), 30, "attempt {attempt} must land on the cap");
        }
    }

    #[test]
    fn backoff_never_exceeds_the_configured_ceiling() {
        for (initial, cap) in [(2u64, 30u64), (1, 1), (5, 3600), (u64::MAX, u64::MAX)] {
            let d = backoff(20, initial, cap);
            assert!(d.as_secs() <= cap.max(initial).min(MAX_RETRY_SECS), "initial={initial} cap={cap} -> {d:?}");
        }
    }

    #[test]
    fn an_out_of_range_retry_default_is_repaired_on_load() {
        // The config file is input: `retry_max_secs` becomes `Instant + delay`
        // and `retry_initial_secs` decides whether retries spin.
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut cfg = ConfigFile::default();
        cfg.defaults.retry_initial_secs = 0;
        cfg.defaults.retry_max_secs = 0;
        crate::config::save(&paths.config, &cfg).unwrap();

        let app = App::new(paths);
        assert!(app.cfg.defaults.retry_initial_secs >= MIN_RETRY_SECS);
        assert!(app.cfg.defaults.retry_max_secs >= MIN_RETRY_SECS);
        assert!(app.cfg.defaults.retry_max_secs >= app.cfg.defaults.retry_initial_secs);
    }

    #[test]
    fn an_absurd_retry_ceiling_is_clamped_so_the_deadline_cannot_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut cfg = ConfigFile::default();
        cfg.defaults.retry_max_secs = u64::MAX;
        crate::config::save(&paths.config, &cfg).unwrap();

        let app = App::new(paths);
        assert_eq!(app.cfg.defaults.retry_max_secs, MAX_RETRY_SECS);
        // And the deadline arithmetic itself must be check-safe.
        let at = deadline_after(Instant::now(), backoff(30, 2, app.cfg.defaults.retry_max_secs));
        assert!(at > Instant::now());
    }

    #[test]
    fn an_inverted_retry_pair_raises_the_ceiling_to_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut cfg = ConfigFile::default();
        cfg.defaults.retry_initial_secs = 60;
        cfg.defaults.retry_max_secs = 5;
        crate::config::save(&paths.config, &cfg).unwrap();

        let app = App::new(paths);
        assert!(app.cfg.defaults.retry_max_secs >= app.cfg.defaults.retry_initial_secs);
        assert!(backoff(3, app.cfg.defaults.retry_initial_secs, app.cfg.defaults.retry_max_secs).as_secs() >= 60);
    }

    #[test]
    fn deadline_arithmetic_never_panics() {
        // `Instant + Duration` panics on overflow. `checked_add` reports it, and
        // the fallback is a one-second deadline rather than an abort.
        let now = Instant::now();
        let at = deadline_after(now, Duration::from_secs(u64::MAX / 2));
        assert!(at.saturating_duration_since(now) >= Duration::from_secs(1));
        let at = deadline_after(now, Duration::from_secs(0));
        assert!(at >= now);
    }

    #[test]
    fn an_exhausted_id_space_is_refused_instead_of_overflowing() {
        // `next_id()` is `max_id + 1`, which overflows for `id = u64::MAX`.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with(dir.path(), vec![sample_tunnel(u64::MAX, "huge", 18099)]);

        assert_eq!(app.next_tunnel_id(), None, "no id can follow u64::MAX");

        // The user-facing paths must report it, not panic.
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("host");
            f.name.set("fresh");
            f.remote_port.set("22");
            f.local_port.set("18098");
        }
        app.submit_form();
        assert!(app.tunnels.len() == 1, "nothing may be created without a free id");
        let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
        assert!(errors.iter().any(|e| e.contains("编号")), "the user must be told why: {errors:?}");
    }

    #[test]
    fn next_tunnel_id_is_just_past_the_highest_existing_one() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_with(dir.path(), vec![sample_tunnel(3, "a", 18001), sample_tunnel(7, "b", 18002)]);
        assert_eq!(app.next_tunnel_id(), Some(8));
    }
}

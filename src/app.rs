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
    FailureInfo, FormMode, ForwardKind, HostPicker, MultiSelect, PortPick, StatusMsg, TunnelConfig, TunnelId,
    TunnelView,
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
pub mod mirror;
pub mod mouse;
pub mod overlay;
pub mod persist;
pub mod picker;
pub mod port_batch;
pub mod port_scan;
pub mod query;
pub mod runtime;
pub mod select;
pub mod server_api;

pub(crate) use helpers::deadline_after;
pub use helpers::{backoff, build_views, sanitize_config};
pub use mirror::Mirror;

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
///
/// Defined in [`overlay`] alongside the code that opens and closes overlays; it is
/// re-exported here because `App` holds one and every handler names it.
pub use overlay::{ConfirmAction, Overlay};

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
    /// The client-server outbox, when this process is a **mirror** rather than the
    /// owner of the tunnels.
    ///
    /// `None` is the single-process owner: every mutation acts on this process's
    /// own `Supervisor`. `Some` means the truth lives in a server and every
    /// mutating method must forward instead of acting — see [`app::mirror`].
    pub(crate) mirror: Option<Mirror>,
    /// How many clients the server says are attached (from the latest snapshot).
    ///
    /// Drives the "you are the last client" warning on quit. Always `1` for an
    /// owner, which is exactly right: a single-process sshpf has one client.
    pub client_count: u32,
    /// Whether the server outlives its clients (mirrors the snapshot's flag).
    pub keep_server: bool,
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
        // Loading the file and building the shell are two separate concerns (and
        // together they used to be one function too long to review), so the load —
        // including its degraded-but-usable failure path — is its own step.
        let (cfg, folded_on_load) = Self::load_config_for(&paths);
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
            mirror: None,
            // A process that owns its tunnels has exactly one client: itself.
            client_count: 1,
            keep_server: false,
        }
    }

    /// Load the config file, repairing it, and report what the repair did.
    ///
    /// A malformed file is **not** fatal: the user's definitions are invisible but
    /// the program still starts, because a TUI that refuses to run because it cannot
    /// parse its own config gives the user no way to fix it. The failure is recorded
    /// and printed once.
    ///
    /// `config.toml` is hand-editable, so its numeric fields are input rather than
    /// invariants. They are clamped here, once, instead of trusting them at every
    /// arithmetic site downstream.
    fn load_config_for(paths: &Paths) -> (ConfigFile, Vec<crate::model::Folded>) {
        let (mut cfg, folded_on_load) = match config::load_with_report(&paths.config) {
            Ok((cfg, folded)) => {
                eventlog::config_loaded(&config::display_path(&paths.config), cfg.tunnels.len(), &[]);
                (cfg, folded)
            }
            Err(e) => {
                // The user's definitions are about to be invisible; the log is
                // the only place that survives to explain it afterwards.
                eventlog::config_load_failed(&config::display_path(&paths.config), &e.to_string(), "empty_config");
                eprintln!("警告: {e}; 以空配置启动\nWarning: {e}; starting with an empty config");
                (ConfigFile::default(), Vec::new())
            }
        };
        sanitize_config(&mut cfg);
        (cfg, folded_on_load)
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
        // A mirror asks the owner to delete: the config file is the server's to
        // write, and removing the row locally would make the table disagree with
        // the file until the next snapshot put it back.
        if self.forward(crate::ipc::Op::Delete { tunnel_id: id }) {
            return;
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
        if self.forward(crate::ipc::Op::StartAll) {
            return;
        }
        let ids: Vec<TunnelId> =
            self.tunnels.iter().filter(|t| !t.status.is_running_or_pending()).map(|t| t.cfg.id).collect();
        for id in ids {
            self.start_tunnel_with(id, "start_all");
        }
    }

    pub fn stop_all(&mut self) {
        if self.forward(crate::ipc::Op::StopAll) {
            return;
        }
        let ids: Vec<TunnelId> = self.tunnels.iter().map(|t| t.cfg.id).collect();
        for id in ids {
            self.stop_tunnel_with(id, "stop_all");
        }
    }

    /// Stop every tunnel immediately without touching persisted intent.
    pub fn shutdown(&mut self) {
        // A mirror owns no `Supervisor`, so there is nothing here to stop — the
        // tunnels belong to the server and outlive this process by design. It says
        // goodbye (`Bye`) through the connection thread instead; the server
        // decides, by recounting, whether that goodbye ends the server.
        // Inspecting the supervisor here would be the bug the split exists to
        // prevent, so this branch is deliberately first.
        if self.is_mirror() {
            return;
        }
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
mod tests;

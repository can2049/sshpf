//! The event vocabulary: every `event=` value sshpf writes, in one place.
//!
//! This is the layer between the state machine and [`crate::logging`]. Call sites
//! call a **named function** here instead of formatting a record inline, which is
//! what keeps the `event=` strings, their levels and their field order stable —
//! `docs/logging.md` documents them and `tests/logging.rs` /
//! `tests/panic_hardening.rs` assert on them.
//!
//! It sits at the bottom of the dependency graph (next to `logging`, `model`,
//! `text`, `sys`) so the `ssh` layer can log without reaching up into `app`. That
//! is why this module exists at all: `ssh::client` used to import
//! `crate::app::log`, a downward-only layering violation.
//!
//! # Where do I put a new event?
//!
//! | file | contents | examples |
//! |---|---|---|
//! | [`process`] | the program itself: start-up, exit, config load/save/repair, CLI modes | `app.start`, `config.save_failed` |
//! | [`tunnel`] | one tunnel's lifecycle, plus per-connection forwarding | `tunnel.ready`, `tunnel.retry_scheduled`, `forward.target_failed` |
//! | [`ssh`] | `~/.ssh/config` discovery, endpoint resolution, `known_hosts` trust decisions | `sshconfig.loaded`, `knownhosts.rejected` |
//! | [`user`] | user intent (confirmations, imports) and degraded-but-alive UI paths | `action.applied`, `ui.slow_frame`, `form.kind_index_invalid` |
//!
//! Add a function to the matching file and `pub use` it below; never inline a
//! `logging::info("some.event", …)` at a call site.
//!
//! # Severity policy
//!
//! * `error` — the user asked for something and it did not happen, or a tunnel
//!   died and will not retry on its own. These are the lines to read first.
//! * `warn` — degraded but self-healing: retry scheduled, host key learned,
//!   config repaired, permissions tightened, gate refused.
//! * `info` — the expected lifecycle: start-up, tunnel transitions, user
//!   actions. Enough to reconstruct a session without the UI.
//! * `debug` — high-volume detail (per-connection forwards, frame timing).

pub mod process;
pub mod ssh;
pub mod tunnel;
pub mod user;

pub use process::{
    app_exit, app_start, cli_error, cli_mode, client_attach_failed, client_attached, client_disconnected,
    client_server_spawned, config_deduped, config_defaults_clamped, config_defaults_clamped_on, config_load_failed,
    config_loaded, config_perms_tightened, config_repaired, config_save_failed, id_space_exhausted,
    id_space_exhausted_on, orphan_event, server_client_connected, server_client_disconnected, server_election_lost,
    server_grace_cancelled, server_grace_started, server_shutdown_begin, server_stale_socket_removed, server_start,
    state_save_failed,
};
pub use ssh::{
    host_key_ignored_changed, host_key_learned, host_key_policy_name, host_key_rejected, host_resolve_failed,
    hosts_failed, hosts_loaded,
};
pub use tunnel::{
    forward_target_failed, kind_name, socks5_rejected, tunnel_created, tunnel_deleted, tunnel_event, tunnel_event_on,
    tunnel_exit, tunnel_exit_on, tunnel_failed, tunnel_failed_on, tunnel_fields, tunnel_health_failed,
    tunnel_health_failed_on, tunnel_line, tunnel_line_on, tunnel_ready, tunnel_ready_on, tunnel_retry_abandoned,
    tunnel_retry_abandoned_on, tunnel_retry_fired, tunnel_retry_scheduled, tunnel_spawn_failed, tunnel_spawned,
    tunnel_start_rejected, tunnel_start_rejected_on, tunnel_start_requested, tunnel_stopped, tunnel_updated,
};
pub use user::{
    action_applied, action_batch, action_cancelled, action_duplicate_skipped, action_duplicate_skipped_on,
    action_requested, action_target_missing, action_target_missing_on, form_kind_index_invalid,
    form_kind_index_invalid_on, import_click_stale, import_committed, import_rejected, mouse_capture_failed,
    port_batch_rejected, port_batch_rejected_on, port_picker_dropped, port_picker_dropped_on, port_scan_closed,
    port_scan_failed, port_scan_failed_on, port_scan_finished, port_scan_finished_on, port_scan_stale,
    port_scan_stale_on, port_scan_started, port_scan_started_on, save_attempted, save_attempted_on, save_key_swallowed,
    save_key_swallowed_on, slow_frame,
};

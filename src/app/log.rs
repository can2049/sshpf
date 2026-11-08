//! Glue between the app's state machine and [`crate::logging`].
//!
//! Kept as one module so the event vocabulary lives in a single place: which
//! events exist, what fields they carry, and which severity each gets is a
//! contract with whoever reads the file later (`docs/logging.md`). Call sites in
//! `app`/`app::runtime`/`main` therefore call a named function here instead of
//! formatting a record inline — that is what keeps `event=` values stable.
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

use crate::logging::{self, Level, Logger};
use crate::model::{FailureInfo, ForwardKind, TunnelConfig, TunnelId};
use crate::ssh::known_hosts::HostKeyPolicy;

/// Stable name for a forward kind, used instead of the display label so the log
/// is not coupled to UI wording.
pub fn kind_name(kind: ForwardKind) -> &'static str {
    match kind {
        ForwardKind::Local => "local",
        ForwardKind::Remote => "remote",
        ForwardKind::Dynamic => "dynamic",
    }
}

/// Fields that identify a tunnel and its endpoints, in a fixed order.
///
/// Every tunnel-scoped record starts with these, which is what makes
/// `grep 'tunnel=3 '` a complete per-tunnel timeline.
pub fn tunnel_fields(cfg: &TunnelConfig) -> [(&'static str, String); 6] {
    [
        ("tunnel", cfg.id.to_string()),
        ("name", cfg.name.clone()),
        ("forward", kind_name(cfg.kind).to_string()),
        ("listen", cfg.local_display()),
        ("target", cfg.remote_display()),
        ("host", cfg.ssh_host.clone()),
    ]
}

/// Emit a tunnel-scoped record: identity fields first, then `extra`.
///
/// `fields` has to outlive the call as a slice of references, so it is built
/// here from owned strings rather than at every call site.
pub fn tunnel_event(level: Level, event: &str, cfg: &TunnelConfig, extra: &[(&str, String)]) {
    tunnel_event_on(logging::logger(), level, event, cfg, extra);
}

/// Same as [`tunnel_event`], against an explicit logger.
///
/// Split out so the record shape can be asserted in tests against a temporary
/// logger: the process-global one can only be installed once per test binary.
pub fn tunnel_event_on(logger: &Logger, level: Level, event: &str, cfg: &TunnelConfig, extra: &[(&str, String)]) {
    let base = tunnel_fields(cfg);
    let mut refs: Vec<(&str, &dyn std::fmt::Display)> = Vec::with_capacity(base.len() + extra.len());
    for (key, value) in &base {
        refs.push((key, value));
    }
    for (key, value) in extra {
        refs.push((key, value));
    }
    logger.event(level, event, &refs);
}

// ---------------------------------------------------------------------------
// lifecycle
// ---------------------------------------------------------------------------

/// The process started: version and where its state lives.
pub fn app_start(version: &str, config_path: &str, log_path: &str) {
    logging::info(
        "app.start",
        &[
            ("version", &version),
            ("pid", &std::process::id()),
            ("config", &config_path),
            ("log", &log_path),
            ("os", &std::env::consts::OS),
            ("arch", &std::env::consts::ARCH),
        ],
    );
}

/// The process is exiting, with a reason and how long it ran.
pub fn app_exit(reason: &str, uptime_ms: u128, tunnels_up: usize) {
    logging::info("app.exit", &[("reason", &reason), ("uptime_ms", &uptime_ms), ("tunnels_up", &tunnels_up)]);
}

/// A CLI mode that does not start the TUI (`--check`, `--list-hosts`, …).
pub fn cli_mode(mode: &str) {
    logging::debug("app.cli_mode", &[("mode", &mode)]);
}

/// A command-line or configuration error that made the process exit non-zero.
pub fn cli_error(event: &str, message: &str) {
    logging::error(event, &[("msg", &message)]);
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// Config loaded. `tunnels` is the number of definitions read.
///
/// A repair (id/name collision, tightened permissions) is a `warn`: the user's
/// file was not what it claimed to be, and knowing that explains later surprises.
pub fn config_loaded(path: &str, tunnels: usize, problems: &[String]) {
    logging::info("config.loaded", &[("path", &path), ("tunnels", &tunnels), ("problems", &problems.len())]);
    for problem in problems {
        logging::warn("config.invalid", &[("path", &path), ("msg", problem)]);
    }
}

/// Loading failed and the app fell back to an empty config. `error`: any tunnel
/// the user thinks they have is about to be invisible.
pub fn config_load_failed(path: &str, error: &str, fallback: &str) {
    logging::error("config.load_failed", &[("path", &path), ("msg", &error), ("fallback", &fallback)]);
}

/// Definitions were repaired on load (duplicate ids, missing timestamps).
pub fn config_repaired(path: &str, count: usize) {
    logging::warn("config.repaired", &[("path", &path), ("entries", &count)]);
}

/// Persisting failed. The change is live but will not survive a restart.
pub fn config_save_failed(path: &str, error: &str) {
    logging::error("config.save_failed", &[("path", &path), ("msg", &error)]);
}

/// The config file was group/world readable and has been tightened to `0600`.
pub fn config_perms_tightened(path: &str, ok: bool) {
    let level = if ok { Level::Warn } else { Level::Error };
    logging::logger().event(level, "config.perms_tightened", &[("path", &path), ("ok", &ok)]);
}

/// The runtime state file could not be written.
pub fn state_save_failed(path: &str, error: &str) {
    logging::warn("state.save_failed", &[("path", &path), ("msg", &error)]);
}

/// A hand-edited default in `config.toml` was out of range and has been replaced.
///
/// `field` is the stable config key (`retry_initial_secs` / `retry_max_secs`),
/// `original` what the file asked for, `used` what the app will actually do.
/// A `retry_initial_secs = 0` is the interesting case: it turns the reconnect
/// loop into a per-frame storm, so it is clamped instead of obeyed.
pub fn config_defaults_clamped(field: &str, original: u64, used: u64) {
    config_defaults_clamped_on(logging::logger(), field, original, used);
}

pub fn config_defaults_clamped_on(logger: &Logger, field: &str, original: u64, used: u64) {
    logger.event(
        Level::Warn,
        "config.defaults_clamped",
        &[("field", &field), ("original", &original), ("used", &used)],
    );
}

/// A retry chain stopped for good: the attempt failed in a way a retry cannot
/// fix (invalid definition, port still in use, unresolvable host, spawn failure).
///
/// `tunnel.failed` covers a session that died; this covers the *backoff loop*
/// giving up, which is what explains why the row stopped flapping on its own.
pub fn tunnel_retry_abandoned(cfg: &TunnelConfig, attempt: u32, stage: &str, reason: &FailureInfo) {
    tunnel_retry_abandoned_on(logging::logger(), cfg, attempt, stage, reason);
}

pub fn tunnel_retry_abandoned_on(logger: &Logger, cfg: &TunnelConfig, attempt: u32, stage: &str, reason: &FailureInfo) {
    tunnel_event_on(
        logger,
        Level::Warn,
        "tunnel.retry_abandoned",
        cfg,
        &[
            ("attempt", attempt.to_string()),
            ("stage", stage.to_string()),
            ("kind", reason.kind.name().to_string()),
            ("short", reason.short.clone()),
        ],
    );
}

/// The tunnel-id space (`u64`) is exhausted, so no new definition can be given a
/// unique id. Only reachable from a hand-edited config with a huge id; the
/// action is refused rather than reusing an id that already exists.
pub fn id_space_exhausted(max_id: TunnelId) {
    id_space_exhausted_on(logging::logger(), max_id);
}

pub fn id_space_exhausted_on(logger: &Logger, max_id: TunnelId) {
    logger.event(Level::Error, "app.id_space_exhausted", &[("max_id", &max_id)]);
}

/// A confirmation was armed for a tunnel id that is no longer in the table.
///
/// The dialog still opens with a fallback sentence, so the user is not stuck;
/// the record is what explains why the prompt had no name in it.
pub fn action_target_missing(action: &str) {
    action_target_missing_on(logging::logger(), action);
}

pub fn action_target_missing_on(logger: &Logger, action: &str) {
    logger.event(Level::Warn, "action.target_missing", &[("action", &action)]);
}

/// The form's `kind_idx` pointed outside `ForwardKind::all()`.
///
/// Only reachable if a caller assigns the index directly; the form falls back to
/// the default kind so the dialog stays usable, and this line says why the
/// highlighted kind may look reset.
pub fn form_kind_index_invalid(index: usize, len: usize) {
    form_kind_index_invalid_on(logging::logger(), index, len);
}

pub fn form_kind_index_invalid_on(logger: &Logger, index: usize, len: usize) {
    logger.event(Level::Warn, "form.kind_index_invalid", &[("index", &index), ("len", &len)]);
}

/// A mouse click resolved to an import row that is no longer in the filtered view.
///
/// The hit map is drawn one frame ahead of the click, so a re-filter or a
/// re-discovery can invalidate it. The click is dropped (rather than being
/// applied to whatever row now occupies that slot) and recorded here.
pub fn import_click_stale(row: usize) {
    logging::debug("import.click_stale", &[("row", &row)]);
}

// ---------------------------------------------------------------------------
// hosts
// ---------------------------------------------------------------------------

/// `~/.ssh/config` was read (or failed to be read).
pub fn hosts_loaded(count: usize) {
    logging::info("sshconfig.loaded", &[("hosts", &count)]);
}

pub fn hosts_failed(error: &str) {
    logging::warn("sshconfig.load_failed", &[("msg", &error)]);
}

/// Stable name for the host-key policy, so the log wording is not the enum's
/// debug formatting.
pub fn host_key_policy_name(policy: HostKeyPolicy) -> &'static str {
    match policy {
        HostKeyPolicy::Strict => "strict",
        HostKeyPolicy::AcceptNew => "accept-new",
        HostKeyPolicy::Ask => "ask",
    }
}

/// A configured host could not be resolved; `detail` is the full error chain.
pub fn host_resolve_failed(cfg: &TunnelConfig, detail: &str) {
    tunnel_event(Level::Error, "sshconfig.resolve_failed", cfg, &[("msg", detail.to_string())]);
}

// ---------------------------------------------------------------------------
// tunnel lifecycle
// ---------------------------------------------------------------------------

/// User (or restore) asked for this tunnel to be up.
pub fn tunnel_start_requested(cfg: &TunnelConfig, trigger: &str) {
    tunnel_event(Level::Info, "tunnel.start", cfg, &[("trigger", trigger.to_string())]);
}

/// Validation or the pre-flight listen check refused the start. Terminal: the
/// user has to edit the definition.
pub fn tunnel_start_rejected(cfg: &TunnelConfig, stage: &str, reason: &FailureInfo) {
    tunnel_start_rejected_on(logging::logger(), cfg, stage, reason);
}

pub fn tunnel_start_rejected_on(logger: &Logger, cfg: &TunnelConfig, stage: &str, reason: &FailureInfo) {
    tunnel_event_on(
        logger,
        Level::Error,
        "tunnel.rejected",
        cfg,
        &[
            ("stage", stage.to_string()),
            ("kind", reason.kind.name().to_string()),
            ("short", reason.short.clone()),
            ("msg", reason.detail.clone()),
        ],
    );
}

/// The session attempt is now running in the background.
pub fn tunnel_spawned(cfg: &TunnelConfig, endpoint: &str, proxy_jump: &[String], policy: &str) {
    tunnel_event(
        Level::Info,
        "tunnel.spawn",
        cfg,
        &[("endpoint", endpoint.to_string()), ("via", proxy_jump.join(",")), ("hostkey_policy", policy.to_string())],
    );
}

/// The forward is genuinely established.
pub fn tunnel_ready(cfg: &TunnelConfig, elapsed_ms: u128) {
    tunnel_ready_on(logging::logger(), cfg, elapsed_ms);
}

pub fn tunnel_ready_on(logger: &Logger, cfg: &TunnelConfig, elapsed_ms: u128) {
    tunnel_event_on(logger, Level::Info, "tunnel.ready", cfg, &[("elapsed_ms", elapsed_ms.to_string())]);
}

/// The session ended. `why` distinguishes a deliberate stop from a failure.
///
/// A session that was `Up` and dropped is a `warn` (the forward had been
/// working); a session that failed before establishing is an `error`.
pub fn tunnel_exit(cfg: &TunnelConfig, was_up: bool, code: Option<i32>, reason: Option<&FailureInfo>) {
    tunnel_exit_on(logging::logger(), cfg, was_up, code, reason);
}

pub fn tunnel_exit_on(
    logger: &Logger,
    cfg: &TunnelConfig,
    was_up: bool,
    code: Option<i32>,
    reason: Option<&FailureInfo>,
) {
    let level = if was_up { Level::Warn } else { Level::Error };
    let code = code.map(|c| c.to_string()).unwrap_or_else(|| "none".to_string());
    match reason {
        Some(reason) => tunnel_event_on(
            logger,
            level,
            "tunnel.exit",
            cfg,
            &[
                ("was_up", was_up.to_string()),
                ("code", code),
                ("kind", reason.kind.name().to_string()),
                ("short", reason.short.clone()),
                ("msg", reason.detail.clone()),
            ],
        ),
        None => tunnel_event(
            Level::Info,
            "tunnel.exit",
            cfg,
            &[("was_up", was_up.to_string()), ("code", code), ("clean", "true".to_string())],
        ),
    }
}

/// A retry has been scheduled with a backoff delay.
pub fn tunnel_retry_scheduled(cfg: &TunnelConfig, attempt: u32, delay_ms: u128, reason: &FailureInfo) {
    tunnel_event(
        Level::Warn,
        "tunnel.retry_scheduled",
        cfg,
        &[
            ("attempt", attempt.to_string()),
            ("delay_ms", delay_ms.to_string()),
            ("kind", reason.kind.name().to_string()),
            ("short", reason.short.clone()),
        ],
    );
}

/// A scheduled retry fired.
pub fn tunnel_retry_fired(cfg: &TunnelConfig, attempt: u32) {
    tunnel_event(Level::Info, "tunnel.retry", cfg, &[("attempt", attempt.to_string())]);
}

/// The tunnel settled into a terminal failure — no automatic retry will follow.
pub fn tunnel_failed(cfg: &TunnelConfig, reason: &FailureInfo) {
    tunnel_failed_on(logging::logger(), cfg, reason);
}

pub fn tunnel_failed_on(logger: &Logger, cfg: &TunnelConfig, reason: &FailureInfo) {
    tunnel_event_on(
        logger,
        Level::Error,
        "tunnel.failed",
        cfg,
        &[
            ("kind", reason.kind.name().to_string()),
            ("short", reason.short.clone()),
            ("retryable", reason.kind.is_retryable().to_string()),
            ("msg", reason.detail.clone()),
        ],
    );
}

/// The user stopped the tunnel.
pub fn tunnel_stopped(cfg: &TunnelConfig, trigger: &str) {
    tunnel_event(Level::Info, "tunnel.stop", cfg, &[("trigger", trigger.to_string())]);
}

/// The tunnel thread could not be spawned at all.
pub fn tunnel_spawn_failed(cfg: &TunnelConfig, error: &str) {
    tunnel_event(Level::Error, "tunnel.spawn_failed", cfg, &[("msg", error.to_string())]);
}

/// One progress line from the driver, mirrored into the file.
///
/// All of these are `debug`: they are the detail panel's material and would
/// otherwise dominate the file. Errors among them are already logged by the
/// state transition that follows.
pub fn tunnel_line(cfg: &TunnelConfig, line: &str) {
    tunnel_line_on(logging::logger(), cfg, line);
}

pub fn tunnel_line_on(logger: &Logger, cfg: &TunnelConfig, line: &str) {
    if logger.enabled(Level::Debug) {
        tunnel_event_on(logger, Level::Debug, "tunnel.line", cfg, &[("msg", line.to_string())]);
    }
}

// ---------------------------------------------------------------------------
// user actions
// ---------------------------------------------------------------------------

/// A destructive/consequential action was confirmed and applied.
pub fn action_applied(action: &str, detail: &str) {
    logging::info("action.applied", &[("action", &action), ("detail", &detail)]);
}

/// A risky action was requested and the confirmation gate is now open.
pub fn action_requested(action: &str, detail: &str) {
    logging::debug("action.requested", &[("action", &action), ("detail", &detail)]);
}

/// A risky action was cancelled at the gate.
pub fn action_cancelled(action: &str) {
    logging::info("action.cancelled", &[("action", &action)]);
}

/// A batch operation finished; `changed` is what actually changed (not what was
/// marked), which is the number the user was shown.
pub fn action_batch(action: &str, marked: usize, changed: usize) {
    logging::info("action.batch", &[("action", &action), ("marked", &marked), ("changed", &changed)]);
}

/// A tunnel definition was created (by the form or by import).
pub fn tunnel_created(cfg: &TunnelConfig, source: &str) {
    tunnel_event(Level::Info, "tunnel.created", cfg, &[("source", source.to_string())]);
}

/// A tunnel definition was edited.
pub fn tunnel_updated(cfg: &TunnelConfig) {
    tunnel_event(Level::Info, "tunnel.updated", cfg, &[]);
}

/// A tunnel definition was deleted.
pub fn tunnel_deleted(cfg: &TunnelConfig) {
    tunnel_event(Level::Info, "tunnel.deleted", cfg, &[]);
}

/// A batch import from `~/.ssh/config` produced `created` tunnels.
pub fn import_committed(host: &str, created: usize) {
    logging::info("import.committed", &[("host", &host), ("created", &created)]);
}

/// Import validation failed; nothing was created.
pub fn import_rejected(problems: &[String]) {
    logging::warn("import.rejected", &[("problems", &problems.len()), ("msg", &problems.join("; "))]);
}

// ---------------------------------------------------------------------------
// thread-side events (no `TunnelConfig` available: only the id)
// ---------------------------------------------------------------------------

/// A server key was learned into `known_hosts` under `accept-new`.
///
/// `warn`, not `info`: this is a new trust anchor being created, and it is the
/// line that explains "why does this host now connect without asking?".
pub fn host_key_learned(id: TunnelId, host: &str, port: u16, verdict: &str) {
    logging::warn("knownhosts.learned", &[("tunnel", &id), ("host", &host), ("port", &port), ("verdict", &verdict)]);
}

/// A server key was refused. `changed`/`revoked` are security events; an
/// unknown key under `strict` is a policy refusal. All are `error`.
pub fn host_key_rejected(id: TunnelId, host: &str, port: u16, policy: &str, detail: &str) {
    logging::error(
        "knownhosts.rejected",
        &[("tunnel", &id), ("host", &host), ("port", &port), ("policy", &policy), ("msg", &detail)],
    );
}

/// Connecting a through-tunnel connection to the target failed.
///
/// The tunnel itself stays up (`ssh -L` behaves the same), so this is a `warn`:
/// it is the record that explains "the port is listening but nothing answers".
pub fn forward_target_failed(id: TunnelId, target: &str, error: &str) {
    logging::warn("forward.target_failed", &[("tunnel", &id), ("target", &target), ("msg", &error)]);
}

/// A SOCKS5 request was rejected or malformed.
pub fn socks5_rejected(id: TunnelId, detail: &str) {
    logging::debug("socks5.rejected", &[("tunnel", &id), ("msg", &detail)]);
}

// ---------------------------------------------------------------------------
// anomalies
// ---------------------------------------------------------------------------

/// The UI frame took far longer than the 200 ms tick — the first thing to check
/// when the TUI feels stuck.
pub fn slow_frame(elapsed_ms: u128) {
    logging::warn("ui.slow_frame", &[("elapsed_ms", &elapsed_ms)]);
}

/// A supervisor event arrived for a tunnel that no longer exists.
pub fn orphan_event(event: &str, id: TunnelId) {
    logging::debug("app.orphan_event", &[("event", &event), ("tunnel", &id)]);
}

/// The user's terminal does not support mouse reporting; the UI keeps working.
pub fn mouse_capture_failed() {
    logging::debug("ui.no_mouse", &[]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::{Config, Level, Logger};
    use crate::model::FailureInfo;
    use std::path::Path;

    fn logger(dir: &Path) -> Logger {
        Logger::open(&Config { dir: dir.to_path_buf(), level: Level::Debug, ..Config::default() }).unwrap()
    }

    fn text(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(crate::logging::LOG_FILE_NAME)).unwrap_or_default()
    }

    fn sample() -> TunnelConfig {
        TunnelConfig {
            id: 7,
            name: "cloud-pc".into(),
            kind: crate::model::ForwardKind::Local,
            ssh_host: "cloud_pc".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 19711,
            remote_host: "127.0.0.1".into(),
            remote_port: 22,
            enabled: true,
            auto_restart: true,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    #[test]
    fn every_tunnel_record_starts_with_the_identity_fields() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        let cfg = sample();
        tunnel_event_on(&log, Level::Info, "tunnel.ready", &cfg, &[("elapsed_ms", "812".to_string())]);

        let line = text(dir.path());
        // A reader must be able to tell which tunnel a line is about without
        // reading the surrounding lines, so the identity fields come first and in
        // a fixed order.
        let expected = "tunnel=7 name=cloud-pc forward=local listen=127.0.0.1:19711 target=127.0.0.1:22 host=cloud_pc";
        assert!(line.contains(expected), "identity fields missing or reordered:\n{line}");
        assert!(line.contains("elapsed_ms=812"), "caller fields follow the identity ones:\n{line}");
        assert!(line.contains(" INFO "), "{line}");
    }

    #[test]
    fn forward_kind_names_are_ascii_and_stable() {
        // The log is read by tools and humans; the Chinese UI label must not leak
        // into it.
        assert_eq!(kind_name(crate::model::ForwardKind::Local), "local");
        assert_eq!(kind_name(crate::model::ForwardKind::Remote), "remote");
        assert_eq!(kind_name(crate::model::ForwardKind::Dynamic), "dynamic");
        for kind in crate::model::ForwardKind::all() {
            assert!(kind_name(kind).is_ascii(), "{kind:?} leaked a non-ASCII name");
        }
    }

    #[test]
    fn a_ready_record_reports_how_long_connecting_took() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        tunnel_ready_on(&log, &sample(), 1499);
        assert!(text(dir.path()).contains("event=tunnel.ready tunnel=7"), "{}", text(dir.path()));
        assert!(text(dir.path()).contains("elapsed_ms=1499"), "{}", text(dir.path()));
    }

    #[test]
    fn a_drop_after_up_is_a_warning_while_a_failed_start_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        let cfg = sample();
        let reason = FailureInfo::new(crate::model::FailureKind::Timeout, "连接超时");

        tunnel_exit_on(&log, &cfg, true, Some(1), Some(&reason));
        let after_up = text(dir.path());
        assert!(after_up.contains(" WARN "), "a tunnel that worked and then dropped is a warning:\n{after_up}");
        assert!(after_up.contains("was_up=true"), "{after_up}");

        tunnel_exit_on(&log, &cfg, false, Some(1), Some(&reason));
        let failure = text(dir.path());
        let last = failure.lines().last().unwrap();
        assert!(last.contains(" ERROR "), "a session that never came up is an error: {last}");
        assert!(last.contains("was_up=false"), "{last}");
        assert!(last.contains("kind=timeout"), "the classified cause travels with the record: {last}");
        assert!(last.contains("short=连接超时"), "and the user-facing phrase too: {last}");
    }

    #[test]
    fn terminal_failure_says_whether_a_retry_could_help() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        // A config-class failure and a network-class one differ in exactly this
        // field, which is what tells a reader whether waiting would help.
        tunnel_failed_on(&log, &sample(), &FailureInfo::new(crate::model::FailureKind::AuthFailed, "认证失败"));
        tunnel_failed_on(&log, &sample(), &FailureInfo::new(crate::model::FailureKind::Timeout, "连接超时"));
        let text = text(dir.path());
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].contains("retryable=false"), "auth failures are not retryable: {}", lines[0]);
        assert!(lines[1].contains("retryable=true"), "timeouts are retryable: {}", lines[1]);
    }

    #[test]
    fn a_rejection_keeps_the_stage_that_refused_it() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        let reason = FailureInfo::with_short(
            crate::model::FailureKind::PortInUse,
            "本地端口被占用",
            "127.0.0.1:8080 已被占用 (pid 42)",
        );
        tunnel_start_rejected_on(&log, &sample(), "preflight_bind", &reason);
        let text = text(dir.path());
        assert!(text.contains("event=tunnel.rejected"), "{text}");
        assert!(text.contains("stage=preflight_bind"), "which check refused it: {text}");
        assert!(text.contains("short=本地端口被占用"), "{text}");
        assert!(text.contains("msg=\"127.0.0.1:8080 已被占用 (pid 42)\""), "the detail is quoted: {text}");
    }

    #[test]
    fn progress_lines_are_debug_only_so_they_do_not_drown_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let log =
            Logger::open(&Config { dir: dir.path().to_path_buf(), level: Level::Info, ..Config::default() }).unwrap();
        tunnel_line_on(&log, &sample(), "转发到 10.0.0.9:80 失败: ConnectFailed");
        assert!(text(dir.path()).is_empty(), "at info level a progress line must not be written");
    }

    // -- panic-hardening events -------------------------------------------

    #[test]
    fn a_clamped_default_names_the_field_and_both_numbers() {
        // A repaired config must be discoverable: which key was wrong, what it
        // said, and what sshpf will actually use.
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        config_defaults_clamped_on(&log, "retry_initial_secs", 0, 1);
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=config.defaults_clamped"), "{text}");
        assert!(text.contains("field=retry_initial_secs"), "{text}");
        assert!(text.contains("original=0"), "{text}");
        assert!(text.contains("used=1"), "{text}");
    }

    #[test]
    fn an_abandoned_retry_says_which_stage_gave_up() {
        // The row stops flapping; this is the line that explains why, and which
        // start stage refused it.
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        let reason = FailureInfo::new(crate::model::FailureKind::PortInUse, "端口被占用");
        tunnel_retry_abandoned_on(&log, &sample(), 4, "preflight_bind", &reason);
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=tunnel.retry_abandoned"), "{text}");
        assert!(text.contains("attempt=4"), "{text}");
        assert!(text.contains("stage=preflight_bind"), "{text}");
        assert!(text.contains("kind=port_in_use"), "{text}");
        assert!(text.contains("tunnel=7 name=cloud-pc"), "identity fields still come first: {text}");
    }

    #[test]
    fn an_exhausted_id_space_is_an_error_that_names_the_maximum() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        id_space_exhausted_on(&log, u64::MAX);
        let text = text(dir.path());
        assert!(text.contains(" ERROR "), "{text}");
        assert!(text.contains("event=app.id_space_exhausted"), "{text}");
        assert!(text.contains("max_id=18446744073709551615"), "{text}");
    }

    #[test]
    fn a_missing_action_target_is_a_warning_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        action_target_missing_on(&log, "delete");
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=action.target_missing"), "{text}");
        assert!(text.contains("action=delete"), "{text}");
    }

    #[test]
    fn an_invalid_kind_index_records_both_the_index_and_the_length() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        form_kind_index_invalid_on(&log, 99, 3);
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=form.kind_index_invalid"), "{text}");
        assert!(text.contains("index=99"), "{text}");
        assert!(text.contains("len=3"), "{text}");
    }

    #[test]
    fn host_key_names_are_stable() {
        assert_eq!(host_key_policy_name(crate::ssh::known_hosts::HostKeyPolicy::Strict), "strict");
        assert_eq!(host_key_policy_name(crate::ssh::known_hosts::HostKeyPolicy::AcceptNew), "accept-new");
        assert_eq!(host_key_policy_name(crate::ssh::known_hosts::HostKeyPolicy::Ask), "ask");
    }
}

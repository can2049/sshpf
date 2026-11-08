//! Tunnel-scoped event vocabulary: start/ready/retry/exit/failed/stop, plus the
//! per-connection events the forwarding drivers emit (`forward.*`, `socks5.*`).
//!
//! Everything here either takes a [`TunnelConfig`] (so the record carries the
//! tunnel's identity fields first — see [`tunnel_fields`]) or only a
//! [`TunnelId`] when the record comes from a tunnel thread that has no access to
//! the definition any more.
//!
//! Add a new tunnel lifecycle event **here**, never inline at a call site: the
//! `event=` value is a contract with `docs/logging.md` and with whoever greps
//! the file later.

use crate::logging::{self, Level, Logger};
use crate::model::{FailureInfo, ForwardKind, TunnelConfig, TunnelId};

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
}

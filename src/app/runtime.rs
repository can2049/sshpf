//! Per-tunnel runtime orchestration: start, stop, restart, and the event pump.
//!
//! This is the layer that talks to the [`Supervisor`](crate::tunnel::Supervisor)
//! and turns ssh's stderr
//! into state transitions. It is deliberately separate from `App`'s own
//! bookkeeping (selection, filters, overlays) so the tricky timing rules —
//! staged readiness, deferred exit classification, retry backoff — sit together
//! in one readable place.

use std::time::Instant;

use crate::app::{App, ConfirmAction};
use crate::config;
use crate::eventlog;
use crate::help;
use crate::logging;
use crate::model::{
    FailureInfo, FailureKind, ForwardKind, LogLevel, LogLine, StatusMsg, TunnelConfig, TunnelId, TunnelStatus,
};
use crate::ssh;
use crate::ssh::config::Endpoint;

impl App {
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
        logging::info("app.restore", &[("tunnels", &n)]);
        for id in ids {
            self.start_tunnel_with(id, "restore");
        }
        self.set_status(StatusMsg::info(help::bi(
            &format!("正在恢复 {n} 条转发…"),
            &format!("Restoring {n} forwards…"),
        )));
    }

    pub(crate) fn log(&mut self, id: TunnelId, level: LogLevel, text: impl Into<String>) {
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.log.push(LogLine::new(level, text));
        }
    }

    pub(crate) fn set_state(&mut self, id: TunnelId, status: TunnelStatus) {
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.status = status;
        }
    }

    pub(crate) fn set_enabled(&mut self, id: TunnelId, enabled: bool) {
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.cfg.enabled = enabled;
        }
        if let Some(c) = self.cfg.find_mut(id) {
            c.enabled = enabled;
        }
    }

    /// Start (or restart) one tunnel.
    pub fn start_tunnel(&mut self, id: TunnelId) {
        self.start_tunnel_with(id, "manual");
    }

    /// Start one tunnel, recording *why* it was started.
    ///
    /// `trigger` is one of `manual`, `restore`, `retry`, `batch`; it goes into the
    /// log so a reconnect storm is distinguishable from a user pressing space.
    ///
    /// The work is three steps, each with its own function, because a start has
    /// three independent ways to be refused and each must log a different `stage`:
    /// [`Self::preflight`] (validate / port / resolve), [`Self::launch`] (spawn)
    /// and [`Self::report_start_failure`] (the shared "refused" bookkeeping).
    pub(crate) fn start_tunnel_with(&mut self, id: TunnelId, trigger: &str) {
        let Some(cfg) = self.tunnels.iter().find(|t| t.cfg.id == id).map(|t| t.cfg.clone()) else {
            eventlog::orphan_event("start", id);
            return;
        };
        eventlog::tunnel_start_requested(&cfg, trigger);
        self.arm_retry(id);

        // A refusal has already been reported inside `preflight`; the start ends
        // there.
        let Ok(endpoint) = self.preflight(&cfg, id) else {
            return;
        };

        self.log_connect_intent(id, &cfg, &endpoint);
        self.saw_listening.remove(&id);
        self.set_state(id, TunnelStatus::Starting { since: Instant::now() });
        self.set_enabled(id, true);
        self.launch(&cfg, id);
        self.persist_quietly();
    }

    /// Steps 1–2 of a start: definition validation and the local-bind pre-flight,
    /// then `~/.ssh/config` resolution. Returns the endpoint to connect to.
    ///
    /// On refusal it has already recorded the failure (state, retry abandonment,
    /// per-tunnel log, status bar), so the caller only has to return.
    fn preflight(&mut self, cfg: &TunnelConfig, id: TunnelId) -> Result<Endpoint, ()> {
        // 1. Definition-level validation.
        if let Err(e) = config::validate_tunnel(cfg) {
            let info = FailureInfo::with_short(FailureKind::Unknown, "配置无效", e.to_string());
            eventlog::tunnel_start_rejected(cfg, "validate", &info);
            self.abandon_retry(cfg, "validate", &info);
            self.set_state(id, TunnelStatus::Failed { reason: info });
            self.log(id, LogLevel::Error, help::bi(&format!("配置无效: {e}"), &format!("Invalid config: {e}")));
            // `e` comes from the config validator and is already bilingual-safe
            // Chinese; the English half states only the category.
            self.set_status(StatusMsg::error(format!("{}: {e} / Invalid config", cfg.name)));
            return Err(());
        }

        // 2. Pre-flight bind check. A `-R` listen happens on the server, so we can
        //    only check our own side for -L/-D.
        if let Err(info) = self.preflight_bind(cfg, id) {
            eventlog::tunnel_start_rejected(cfg, "preflight_bind", &info);
            self.abandon_retry(cfg, "preflight_bind", &info);
            self.set_state(id, TunnelStatus::Failed { reason: info });
            self.persist_quietly();
            return Err(());
        }

        // 3. Resolve the host from ~/.ssh/config.
        //
        //    Resolution happens on the UI thread (it is a file parse, no I/O wait)
        //    so a bad alias is reported immediately instead of appearing as an
        //    async failure a moment later.
        match ssh::config::endpoint_for(cfg) {
            Ok(endpoint) => Ok(endpoint),
            Err(e) => {
                let info = FailureInfo::with_short(FailureKind::AuthFailed, "无法解析 SSH 主机", format!("{e:#}"));
                eventlog::host_resolve_failed(cfg, &format!("{e:#}"));
                eventlog::tunnel_start_rejected(cfg, "resolve_endpoint", &info);
                self.abandon_retry(cfg, "resolve_endpoint", &info);
                self.set_state(id, TunnelStatus::Failed { reason: info });
                self.log(
                    id,
                    LogLevel::Error,
                    help::bi(&format!("解析 SSH 主机失败: {e:#}"), &format!("Cannot resolve the SSH host: {e:#}")),
                );
                self.set_status(StatusMsg::error(help::bi(
                    &format!("{}: 无法解析 SSH 主机", cfg.name),
                    &format!("{}: cannot resolve the SSH host", cfg.name),
                )));
                Err(())
            }
        }
    }

    /// The local-bind half of the pre-flight: only `-L`/`-D` bind here, and a port
    /// already held by someone else is a refusal the user must fix by editing.
    ///
    /// Split out of [`Self::preflight`] so the `-R` early return reads as one
    /// sentence rather than indenting the whole resolution step.
    fn preflight_bind(&mut self, cfg: &TunnelConfig, id: TunnelId) -> Result<(), FailureInfo> {
        if cfg.kind == ForwardKind::Remote {
            return Ok(());
        }
        let (addr, port) = config::listen_endpoint(cfg);
        if ssh::port_available(addr, port) {
            return Ok(());
        }
        let owner = ssh::port_owner(addr, port);
        let detail = match &owner {
            Some(o) => format!("{addr}:{port} 已被占用 ({o})"),
            None => format!("{addr}:{port} 已被占用"),
        };
        let info = FailureInfo::with_short(FailureKind::PortInUse, "本地端口被占用", detail.clone());
        self.log(id, LogLevel::Error, detail.clone());
        self.set_status(StatusMsg::error(help::bi(
            &format!("{}: {detail}", cfg.name),
            &format!("{}: {addr}:{port} is already in use", cfg.name),
        )));
        Err(info)
    }

    /// Record what the session is about to connect to: user-visible progress lines
    /// plus the structured `tunnel.spawn` record.
    fn log_connect_intent(&mut self, id: TunnelId, cfg: &TunnelConfig, endpoint: &Endpoint) {
        let who = format!("{}@{}:{}", endpoint.user_or_current(), endpoint.hostname, endpoint.port);
        self.log(id, LogLevel::Info, help::bi(&format!("连接 {who}"), &format!("Connecting {who}")));
        if !endpoint.proxy_jump.is_empty() {
            let via = endpoint.proxy_jump.join(" → ");
            self.log(id, LogLevel::Info, help::bi(&format!("经跳板机: {via}"), &format!("Via jump hosts: {via}")));
        }
        eventlog::tunnel_spawned(cfg, &who, &endpoint.proxy_jump, eventlog::host_key_policy_name(self.host_key_policy));
    }

    /// Step 3 of a start: spawn the background session and report the outcome.
    fn launch(&mut self, cfg: &TunnelConfig, id: TunnelId) {
        let policy = self.host_key_policy;
        match self.supervisor.spawn(cfg.clone(), policy, self.event_tx.clone()) {
            Ok(()) => {
                self.set_status(StatusMsg::info(help::bi(
                    &format!("{} 正在连接…", cfg.name),
                    &format!("{} connecting…", cfg.name),
                )));
            }
            Err(e) => {
                let short: String = e.to_string().chars().take(28).collect();
                let info = FailureInfo::with_short(FailureKind::SpawnFailed, short, format!("启动隧道失败: {e}"));
                eventlog::tunnel_spawn_failed(cfg, &e.to_string());
                self.abandon_retry(cfg, "spawn", &info);
                self.set_state(id, TunnelStatus::Failed { reason: info });
                self.log(
                    id,
                    LogLevel::Error,
                    help::bi(&format!("启动隧道失败: {e}"), &format!("Cannot start the tunnel: {e}")),
                );
                self.set_status(StatusMsg::error(help::bi(
                    &format!("{} 启动失败: {e}", cfg.name),
                    &format!("{} failed to start", cfg.name),
                )));
            }
        }
    }

    /// Stop the automatic retry chain for a tunnel whose *retry* failed in a way
    /// another attempt cannot fix.
    ///
    /// Without this the backoff loop keeps resurrecting the tunnel forever: each
    /// `tick_retries` fires the due entry, `start_tunnel_with` refuses it at
    /// validation/bind/resolve/spawn, and the next tick fires again — a reconnect
    /// storm that writes a log line per attempt and never settles into `Failed`.
    /// A first manual start has `attempt == 0`, so it does not count as giving up
    /// and nothing is logged for it.
    pub(crate) fn abandon_retry(&mut self, cfg: &crate::model::TunnelConfig, stage: &str, reason: &FailureInfo) {
        let Some(attempt) = self.retry_state.remove(&cfg.id).map(|r| r.attempt) else {
            return;
        };
        if attempt >= 1 {
            eventlog::tunnel_retry_abandoned(cfg, attempt, stage, reason);
        }
    }

    /// Stop one tunnel and clear its `enabled` flag (user intent).
    pub fn stop_tunnel(&mut self, id: TunnelId) {
        self.stop_tunnel_with(id, "manual");
    }

    /// Stop one tunnel, recording *why* (stop-all, batch, quit, …).
    pub(crate) fn stop_tunnel_with(&mut self, id: TunnelId, trigger: &str) {
        match self.tunnels.iter().find(|t| t.cfg.id == id) {
            Some(view) => eventlog::tunnel_stopped(&view.cfg, trigger),
            None => eventlog::orphan_event("stop", id),
        }
        self.supervisor.stop(id);
        self.forget_runtime_state(id);
        self.set_enabled(id, false);
        self.set_state(id, TunnelStatus::Stopped);
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.retries = 0;
        }
        self.log(id, LogLevel::Info, help::bi("已停止", "Stopped"));
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
}

#[cfg(test)]
mod hardening_tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::TunnelConfig;

    fn tunnel(id: TunnelId, name: &str, port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "testhost".into(),
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

    fn app_with(tunnels: Vec<TunnelConfig>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
        crate::config::save(&paths.config, &cfg).unwrap();
        (dir, App::new(paths))
    }

    #[test]
    fn a_manual_start_that_cannot_succeed_does_not_create_a_retry_entry() {
        // `attempt == 0` is a first attempt, not a giving-up: it must leave no
        // retry bookkeeping behind either.
        let (_d, mut app) = app_with(vec![tunnel(1, "broken", 18001)]);
        {
            let v = app.tunnels.iter_mut().find(|t| t.cfg.id == 1).unwrap();
            v.cfg.ssh_host = String::new();
        }
        app.start_tunnel(1);
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Failed { .. }));
        assert_eq!(app.retry_state.get(&1).map(|r| r.attempt), None);
    }

    #[test]
    fn a_retry_that_still_fails_keeps_its_attempt_count_bounded() {
        // Even while retries *do* reach the spawn stage, the attempt counter is
        // saturating: `on_exit` increments it once per failure and never resets
        // it while the tunnel keeps flapping.
        let (_d, mut app) = app_with(vec![tunnel(1, "flap", 18002)]);
        app.cfg.defaults.auto_restart = true;
        app.arm_retry(1);
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.on_exit(1, Some(255));
        // Force the counter near the top and make sure the schedule is sane.
        if let Some(r) = app.retry_state.get_mut(&1) {
            r.attempt = u32::MAX;
        }
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.on_exit(1, Some(255));
        let attempt = app.retry_state.get(&1).map(|r| r.attempt).unwrap_or(0);
        assert_eq!(attempt, u32::MAX, "the counter must saturate, not overflow");
        if let TunnelStatus::Retrying { next_at, .. } = app.tunnels[0].status {
            assert!(next_at > Instant::now(), "a saturated attempt still schedules a real delay");
        }
    }
}

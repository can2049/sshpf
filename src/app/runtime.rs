//! Per-tunnel runtime orchestration: start, stop, restart, and the event pump.
//!
//! This is the layer that talks to the [`Supervisor`](crate::tunnel::Supervisor)
//! and turns ssh's stderr
//! into state transitions. It is deliberately separate from `App`'s own
//! bookkeeping (selection, filters, overlays) so the tricky timing rules —
//! staged readiness, deferred exit classification, retry backoff — sit together
//! in one readable place.

use std::time::{Duration, Instant};

use crate::app::log as eventlog;
use crate::app::{App, ConfirmAction, MAX_EVENTS_PER_FRAME, RetryState, backoff, deadline_after};
use crate::config;
use crate::help;
use crate::logging;
use crate::model::{FailureInfo, FailureKind, ForwardKind, LogLevel, LogLine, StatusMsg, TunnelId, TunnelStatus};
use crate::ssh;
use crate::tunnel::TunnelEvent;

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
    pub(crate) fn start_tunnel_with(&mut self, id: TunnelId, trigger: &str) {
        let Some(cfg) = self.tunnels.iter().find(|t| t.cfg.id == id).map(|t| t.cfg.clone()) else {
            eventlog::orphan_event("start", id);
            return;
        };
        eventlog::tunnel_start_requested(&cfg, trigger);
        self.arm_retry(id);

        // 1. Definition-level validation.
        if let Err(e) = config::validate_tunnel(&cfg) {
            let info = FailureInfo::with_short(FailureKind::Unknown, "配置无效", e.to_string());
            eventlog::tunnel_start_rejected(&cfg, "validate", &info);
            self.abandon_retry(&cfg, "validate", &info);
            self.set_state(id, TunnelStatus::Failed { reason: info });
            self.log(id, LogLevel::Error, help::bi(&format!("配置无效: {e}"), &format!("Invalid config: {e}")));
            // `e` comes from the config validator and is already bilingual-safe
            // Chinese; the English half states only the category.
            self.set_status(StatusMsg::error(format!("{}: {e} / Invalid config", cfg.name)));
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
                eventlog::tunnel_start_rejected(&cfg, "preflight_bind", &info);
                self.abandon_retry(&cfg, "preflight_bind", &info);
                self.set_state(id, TunnelStatus::Failed { reason: info });
                self.log(id, LogLevel::Error, detail.clone());
                self.set_status(StatusMsg::error(help::bi(
                    &format!("{}: {detail}", cfg.name),
                    &format!("{}: {addr}:{port} is already in use", cfg.name),
                )));
                self.persist_quietly();
                return;
            }
        }

        // 4. Resolve the host from ~/.ssh/config and start the tunnel.
        //
        //    Resolution happens on the UI thread (it is a file parse, no I/O
        //    wait) so a bad alias is reported immediately instead of appearing
        //    as an async failure a moment later.
        match ssh::config::endpoint_for(&cfg) {
            Ok(endpoint) => {
                self.log(
                    id,
                    LogLevel::Info,
                    help::bi(
                        &format!("连接 {}@{}:{}", endpoint.user_or_current(), endpoint.hostname, endpoint.port),
                        &format!("Connecting {}@{}:{}", endpoint.user_or_current(), endpoint.hostname, endpoint.port),
                    ),
                );
                if !endpoint.proxy_jump.is_empty() {
                    self.log(
                        id,
                        LogLevel::Info,
                        help::bi(
                            &format!("经跳板机: {}", endpoint.proxy_jump.join(" → ")),
                            &format!("Via jump hosts: {}", endpoint.proxy_jump.join(" → ")),
                        ),
                    );
                }
                eventlog::tunnel_spawned(
                    &cfg,
                    &format!("{}@{}:{}", endpoint.user_or_current(), endpoint.hostname, endpoint.port),
                    &endpoint.proxy_jump,
                    eventlog::host_key_policy_name(self.host_key_policy),
                );
            }
            Err(e) => {
                let info = FailureInfo::with_short(FailureKind::AuthFailed, "无法解析 SSH 主机", format!("{e:#}"));
                eventlog::host_resolve_failed(&cfg, &format!("{e:#}"));
                eventlog::tunnel_start_rejected(&cfg, "resolve_endpoint", &info);
                self.abandon_retry(&cfg, "resolve_endpoint", &info);
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
                return;
            }
        }

        self.saw_listening.remove(&id);
        self.set_state(id, TunnelStatus::Starting { since: Instant::now() });
        self.set_enabled(id, true);

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
                eventlog::tunnel_spawn_failed(&cfg, &e.to_string());
                self.abandon_retry(&cfg, "spawn", &info);
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
        self.persist_quietly();
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

    /// Drain supervisor events. Called once per UI frame — never blocks, and
    /// never drains without a bound.
    ///
    /// The per-frame budget matters: the channel is fed by one reader thread per
    /// tunnel, and a verbose one (a reconnect loop writing a line per attempt, a
    /// `-v` ssh option the user added) can keep the queue non-empty forever. An
    /// unbounded `while let` would then never return, so the frame would never
    /// render, no key would be read, and the TUI would look frozen while it was
    /// actually busy. Whatever is left stays queued for the next frame.
    pub fn drain_events(&mut self) {
        for _ in 0..MAX_EVENTS_PER_FRAME {
            let Ok(ev) = self.event_rx.try_recv() else {
                break;
            };
            match ev {
                TunnelEvent::Ready { id } => self.on_ready(id),
                TunnelEvent::Line { id, line } => self.on_line(id, line),
                TunnelEvent::Exited { id, code, reason } => {
                    // Remember the classified cause; `StderrClosed` may arrive
                    // before or after this.
                    self.pending_reason.insert(id, reason);
                    self.pending_exit.insert(id, (code, Instant::now()));
                }
                TunnelEvent::StderrClosed { id } => {
                    if let Some((code, _)) = self.pending_exit.remove(&id) {
                        self.on_exit(id, code);
                    }
                }
                TunnelEvent::SpawnFailed { id, error } => {
                    let info = FailureInfo::with_short(FailureKind::SpawnFailed, "无法启动 ssh", error);
                    match self.tunnels.iter().find(|t| t.cfg.id == id) {
                        Some(view) => eventlog::tunnel_spawn_failed(&view.cfg, &info.detail),
                        None => eventlog::orphan_event("spawn_failed", id),
                    }
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
    pub(crate) fn flush_pending_exits(&mut self) {
        const GRACE: Duration = Duration::from_millis(500);
        let ready: Vec<TunnelId> =
            self.pending_exit.iter().filter(|(_, (_, at))| at.elapsed() >= GRACE).map(|(id, _)| *id).collect();
        for id in ready {
            if let Some((code, _)) = self.pending_exit.remove(&id) {
                self.on_exit(id, code);
            }
        }
    }

    /// The tunnel driver reported a genuinely established forward.
    ///
    /// This is the *only* path to `TunnelStatus::Up`. Because it comes from the
    /// real `bind` / `tcpip_forward` result, none of the old false-ready cases
    /// (ProxyJump's merged stderr, `-R`'s early "Entering interactive session.")
    /// can trigger it.
    pub(crate) fn on_ready(&mut self, id: TunnelId) {
        let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == id) else {
            return;
        };
        if matches!(view.status, TunnelStatus::Up { .. }) {
            return;
        }
        let name = view.cfg.name.clone();
        let cfg = view.cfg.clone();
        let started = match view.status {
            TunnelStatus::Starting { since } => since.elapsed().as_millis(),
            _ => 0,
        };
        eventlog::tunnel_ready(&cfg, started);
        self.set_state(id, TunnelStatus::Up { since: Instant::now() });
        self.retry_state.remove(&id);
        self.pending_exit.remove(&id);
        self.saw_listening.insert(id);
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.retries = 0;
        }
        self.set_status(StatusMsg::ok(help::bi(&format!("✓ {name} 已连通"), &format!("✓ {name} up"))));
    }

    /// Handle one progress line from a tunnel.
    ///
    /// Readiness is **not** inferred here any more: the driver signals it from
    /// the actual `bind` / `tcpip_forward` result, so the old staged-readiness
    /// state machine (`saw_listening`, `Entering interactive session.`,
    /// `remote forward success for`) is gone along with the false-ready bugs it
    /// worked around.
    pub(crate) fn on_line(&mut self, id: TunnelId, line: String) {
        let level = if line.contains("失败") || line.contains("错误") { LogLevel::Warn } else { LogLevel::Info };
        if let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == id) {
            eventlog::tunnel_line(&view.cfg, &line);
        }
        self.log(id, level, line);
    }

    /// Forget everything we know about a tunnel's *current* run.
    ///
    /// Called whenever a tunnel stops or restarts. Five call sites used to
    /// hand-roll this cleanup and they had already drifted apart (some forgot
    /// `saw_listening`), so it lives in one place now.
    pub(crate) fn forget_runtime_state(&mut self, id: TunnelId) {
        self.retry_state.remove(&id);
        self.pending_exit.remove(&id);
        self.pending_reason.remove(&id);
        self.saw_listening.remove(&id);
    }

    pub(crate) fn on_exit(&mut self, id: TunnelId, code: Option<i32>) {
        let classified = self.pending_reason.remove(&id).flatten();
        let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == id) else {
            eventlog::orphan_event("exit", id);
            return;
        };
        let status = view.status.clone();
        let cfg = view.cfg.clone();
        // A deliberate stop already reset the status; nothing to do.
        if matches!(status, TunnelStatus::Stopped) {
            return;
        }
        let was_up = matches!(status, TunnelStatus::Up { .. });
        self.log(
            id,
            if was_up { LogLevel::Warn } else { LogLevel::Error },
            help::bi(&format!("连接已结束 (code {code:?})"), &format!("Connection ended (code {code:?})")),
        );

        let reason = match status.failure().cloned().or(classified) {
            Some(f) => f,
            None => {
                let tail = self
                    .tunnels
                    .iter()
                    .find(|t| t.cfg.id == id)
                    .and_then(|t| t.log.iter().rev().find(|l| l.level != LogLevel::Debug).map(|l| l.text.clone()))
                    .unwrap_or_else(|| format!("连接已结束，代码 {code:?}"));
                FailureInfo::with_short(FailureKind::Exited, "连接异常结束", tail)
            }
        };

        // The session is already gone, but make sure no thread is left
        // supervised for a tunnel that never established its forward.
        if !was_up && !matches!(reason.kind, FailureKind::Exited) {
            self.supervisor.stop(id);
        }

        let auto = self.cfg.defaults.auto_restart;
        let retryable = reason.kind.is_retryable() || was_up;
        if auto
            && retryable
            && let Some(entry) = self.retry_state.get_mut(&id)
        {
            entry.attempt = entry.attempt.saturating_add(1);
            let delay = backoff(entry.attempt, self.cfg.defaults.retry_initial_secs, self.cfg.defaults.retry_max_secs);
            // `Instant + Duration` panics on overflow; the delay is capped by
            // `backoff`, but the addition is still done through `checked_add` so
            // an extreme `retry_max_secs` cannot abort the process.
            entry.next_at = deadline_after(Instant::now(), delay);
            let (attempt, next_at) = (entry.attempt, entry.next_at);
            // The classified cause of a *drop* is not a failure the user must
            // act on: recording it at warn with the scheduled delay is what tells
            // a reconnect storm apart from a one-off blip.
            eventlog::tunnel_exit(&cfg, was_up, code, Some(&reason));
            eventlog::tunnel_retry_scheduled(&cfg, attempt, delay.as_millis(), &reason);
            self.set_state(id, TunnelStatus::Retrying { attempt, next_at, reason });
            if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
                v.retries = attempt;
            }
            return;
        }
        eventlog::tunnel_exit(&cfg, was_up, code, Some(&reason));
        eventlog::tunnel_failed(&cfg, &reason);
        self.set_state(id, TunnelStatus::Failed { reason });
    }

    pub(crate) fn tick_retries(&mut self) {
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
                r.next_at = deadline_after(now, delay);
            }
            self.log(
                id,
                LogLevel::Info,
                help::bi(&format!("第 {attempt} 次自动重连…"), &format!("Auto-reconnect attempt {attempt}…")),
            );
            self.start_tunnel_with(id, "retry");
        }
    }

    /// Register retry bookkeeping for a tunnel that is about to start.
    ///
    /// The placeholder has `attempt = 0`, which [`App::tick_retries`] ignores,
    /// so it can never cause a spurious immediate reconnect. `on_exit` promotes
    /// it to `attempt >= 1` with a real backoff deadline.
    pub(crate) fn arm_retry(&mut self, id: TunnelId) {
        self.retry_state.entry(id).or_insert(RetryState { attempt: 0, next_at: Instant::now() });
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
            remote_port: 22,
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

    /// Arm a retry that is already due, as `on_exit` would after a real drop.
    fn arm_due_retry(app: &mut App, id: TunnelId, attempt: u32) {
        app.retry_state.insert(id, RetryState { attempt, next_at: Instant::now() - Duration::from_secs(1) });
    }

    #[test]
    fn a_retry_chain_stops_when_the_attempt_cannot_succeed() {
        // The stall this pins: an invalid definition kept being rescheduled.
        // `tick_retries` fired the due entry, `start_tunnel_with` refused it at
        // validation, `on_exit` re-armed the retry, and the loop ran forever —
        // a log line per frame, a permanently `Failed`/`Retrying` row, and no
        // way for the user to tell it had given up.
        let (_d, mut app) = app_with(vec![tunnel(1, "broken", 18001)]);
        app.cfg.defaults.auto_restart = true;
        {
            // A definition that can never validate: no listen port.
            let v = app.tunnels.iter_mut().find(|t| t.cfg.id == 1).unwrap();
            v.cfg.local_port = 0;
            v.cfg.ssh_host = String::new();
        }
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        arm_due_retry(&mut app, 1, 3);

        app.tick_retries();

        assert!(matches!(app.tunnels[0].status, TunnelStatus::Failed { .. }), "{:?}", app.tunnels[0].status);
        assert_eq!(app.retry_state.get(&1).map(|r| r.attempt), None, "the retry chain must be dropped");
        // A second tick must not resurrect it.
        app.tick_retries();
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Failed { .. }));
        assert!(app.retry_state.is_empty());
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

    #[test]
    fn drain_events_is_bounded_per_frame() {
        // A reader thread that never stops producing must not starve the frame
        // loop. Send more events than the budget and check the rest survive.
        let (_d, mut app) = app_with(vec![tunnel(1, "chatty", 18003)]);
        let overflow = 500;
        for _ in 0..(MAX_EVENTS_PER_FRAME + overflow) {
            app.event_tx.send(crate::tunnel::TunnelEvent::Line { id: 1, line: "debug1: x".into() }).unwrap();
        }
        app.drain_events();
        let mut left = 0;
        while app.event_rx.try_recv().is_ok() {
            left += 1;
        }
        assert_eq!(left, overflow, "one frame consumes exactly its budget and queues the rest");
    }

    #[test]
    fn tick_retries_is_a_no_op_when_auto_restart_is_off() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18004)]);
        app.cfg.defaults.auto_restart = false;
        arm_due_retry(&mut app, 1, 5);
        app.tick_retries();
        assert_eq!(app.retry_state.get(&1).map(|r| r.attempt), Some(5), "the entry must be left untouched");
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Stopped));
    }

    #[test]
    fn events_for_unknown_tunnels_are_ignored() {
        // A deleted tunnel's reader thread can still deliver; nothing may panic
        // or create state for it.
        let (_d, mut app) = app_with(vec![]);
        for ev in [
            crate::tunnel::TunnelEvent::Ready { id: 42 },
            crate::tunnel::TunnelEvent::Line { id: 42, line: "x".into() },
            crate::tunnel::TunnelEvent::SpawnFailed { id: 42, error: "boom".into() },
            crate::tunnel::TunnelEvent::StderrClosed { id: 42 },
        ] {
            app.event_tx.send(ev).unwrap();
        }
        app.drain_events();
        assert!(app.tunnels.is_empty());
        assert!(app.retry_state.is_empty());
    }

    #[test]
    fn flush_pending_exits_never_drops_the_exit_code() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18005)]);
        app.set_state(1, TunnelStatus::Starting { since: Instant::now() });
        app.pending_exit.insert(1, (Some(7), Instant::now() - Duration::from_secs(2)));
        app.flush_pending_exits();
        assert!(app.pending_exit.is_empty(), "the deferred exit must be consumed");
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Failed { .. }));
    }

    #[test]
    fn an_orphan_exit_is_recorded_and_dropped() {
        // `pending_reason` for an id with no view must not leak into the map.
        let (_d, mut app) = app_with(vec![]);
        app.pending_reason.insert(9, Some(FailureInfo::new(FailureKind::Timeout, "t")));
        app.pending_exit.insert(9, (Some(1), Instant::now() - Duration::from_secs(2)));
        app.flush_pending_exits();
        assert!(!app.pending_reason.contains_key(&9), "the reason must be consumed with the exit");
    }
}

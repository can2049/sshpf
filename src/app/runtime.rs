//! Per-tunnel runtime orchestration: start, stop, restart, and the event pump.
//!
//! This is the layer that talks to the [`Supervisor`] and turns ssh's stderr
//! into state transitions. It is deliberately separate from `App`'s own
//! bookkeeping (selection, filters, overlays) so the tricky timing rules —
//! staged readiness, deferred exit classification, retry backoff — sit together
//! in one readable place.

use std::time::{Duration, Instant};

use crate::app::{App, ConfirmAction, RetryState, backoff};
use crate::config;
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
        for id in ids {
            self.start_tunnel(id);
        }
        self.set_status(StatusMsg::info(format!("正在恢复 {n} 条转发…")));
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
        self.forget_runtime_state(id);
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

    pub(crate) fn on_line(&mut self, id: TunnelId, line: String) {
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

    /// Forget everything we know about a tunnel's *current* run.
    ///
    /// Called whenever a tunnel stops or restarts. Five call sites used to
    /// hand-roll this cleanup and they had already drifted apart (some forgot
    /// `saw_listening`), so it lives in one place now.
    pub(crate) fn forget_runtime_state(&mut self, id: TunnelId) {
        if let Some(path) = self.synth_paths.remove(&id) {
            let _ = std::fs::remove_file(path);
        }
        self.retry_state.remove(&id);
        self.pending_exit.remove(&id);
        self.saw_listening.remove(&id);
    }

    /// Kill the ssh child of a tunnel that failed to establish its forward.
    ///
    /// `ssh -N` keeps running when a forwarding request is rejected (`-R`
    /// remote forward failure, or a partially bound `-L`): the session is up
    /// but the tunnel is useless. We stop it so the table never shows a
    /// "failed" row backed by a live process, and so the local port is not
    /// held.
    pub(crate) fn stop_after_failure(&mut self, id: TunnelId) {
        self.supervisor.stop(id);
        self.forget_runtime_state(id);
        // Keep `enabled` as the user set it; only the process goes away.
    }

    pub(crate) fn on_exit(&mut self, id: TunnelId, code: Option<i32>) {
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
    pub(crate) fn arm_retry(&mut self, id: TunnelId) {
        self.retry_state.entry(id).or_insert(RetryState { attempt: 0, next_at: Instant::now() });
    }
}

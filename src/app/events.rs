//! The event pump: what the UI thread does with what a tunnel thread reports.
//!
//! Split out of [`crate::app::runtime`] because it is the *read* side of the
//! runtime: `runtime` starts/stops sessions, this file turns the resulting
//! [`TunnelEvent`](crate::tunnel::TunnelEvent)s into state transitions, retries
//! and log lines. Keeping them apart is what makes the timing rules (deferred
//! exit classification, per-frame event budget, backoff scheduling) readable in
//! one place.
//!
//! The ordering inside one frame matters and is deliberate:
//!
//! 1. drain the channel, bounded by [`MAX_EVENTS_PER_FRAME`];
//! 2. reap children whose stderr EOF raced their exit;
//! 3. classify deferred exits whose grace window elapsed;
//! 4. fire due retries;
//! 5. expire the transient status message.
//!
//! Steps 2–4 can each produce more state changes, so they run after the drain
//! rather than interleaved with it.

use std::time::{Duration, Instant};

use crate::app::{App, MAX_EVENTS_PER_FRAME, RetryState, backoff, deadline_after};
use crate::eventlog;
use crate::help;
use crate::model::{FailureInfo, FailureKind, LogLevel, StatusMsg, TunnelConfig, TunnelId, TunnelStatus};
use crate::tunnel::TunnelEvent;

impl App {
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
        self.drain_port_scan();
        // A mirror's "events" are server messages, and they are applied by
        // `client::sync::drain`, which `main`'s frame loop calls right after this
        // one. Deliberately not called here: `app` must not depend on `client`
        // (see `docs/architecture.md`), and the two must not both drain the same
        // channel or a frame's messages would be split between them.
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
        let warned = line.contains("失败") || line.contains("错误") || line.contains("警告");
        let level = if warned { LogLevel::Warn } else { LogLevel::Info };
        if let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == id) {
            eventlog::tunnel_line(&view.cfg, &line);
        }
        // A tunnel-level warning is a degraded-but-working state the user should
        // see without opening the detail pane (ignoring a changed host key is the
        // case this was added for), so it also reaches the status bar once.
        if warned && line.contains("警告") {
            self.set_status(StatusMsg::warn(line.clone()));
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

    /// Classify a finished session and move the row to its next state.
    ///
    /// Two halves, because they answer different questions: [`Self::classify_exit`]
    /// decides *what happened* (was it up, what is the cause), and
    /// [`Self::apply_exit`] decides *what to do about it* (retry with backoff, or
    /// settle into `Failed`) while writing the records.
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
        let reason = self.classify_exit(id, code, &status, classified, was_up);
        self.apply_exit(id, &cfg, code, was_up, reason);
    }

    /// Decide the cause of an exit: the classified one if the driver reported it,
    /// else the status' own failure, else the last non-debug line of the panel.
    ///
    /// The user-visible "connection ended" line is written here too, because its
    /// severity depends on whether the forward had been up — the one fact this
    /// function has already computed.
    fn classify_exit(
        &mut self,
        id: TunnelId,
        code: Option<i32>,
        status: &TunnelStatus,
        classified: Option<FailureInfo>,
        was_up: bool,
    ) -> FailureInfo {
        self.log(
            id,
            if was_up { LogLevel::Warn } else { LogLevel::Error },
            help::bi(&format!("连接已结束 (code {code:?})"), &format!("Connection ended (code {code:?})")),
        );
        match status.failure().cloned().or(classified) {
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
        }
    }

    /// Apply the classified exit: schedule the backoff retry, or settle into a
    /// terminal failure, recording the matching events.
    fn apply_exit(&mut self, id: TunnelId, cfg: &TunnelConfig, code: Option<i32>, was_up: bool, reason: FailureInfo) {
        // The session is already gone, but make sure no thread is left supervised
        // for a tunnel that never established its forward.
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
            // The classified cause of a *drop* is not a failure the user must act
            // on: recording it at warn with the scheduled delay is what tells a
            // reconnect storm apart from a one-off blip.
            eventlog::tunnel_exit(cfg, was_up, code, Some(&reason));
            eventlog::tunnel_retry_scheduled(cfg, attempt, delay.as_millis(), &reason);
            self.set_state(id, TunnelStatus::Retrying { attempt, next_at, reason });
            if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
                v.retries = attempt;
            }
            return;
        }
        eventlog::tunnel_exit(cfg, was_up, code, Some(&reason));
        eventlog::tunnel_failed(cfg, &reason);
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
mod tests {
    use super::*;
    use crate::app::{MAX_EVENTS_PER_FRAME, RetryState};
    use crate::config::{ConfigFile, Paths};
    use crate::model::{FailureInfo, FailureKind, ForwardKind, TunnelConfig};
    use std::time::Duration;

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

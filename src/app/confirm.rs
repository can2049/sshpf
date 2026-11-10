//! Risk gate: arming, rendering text for, and applying confirmed actions.
//!
//! Every destructive operation funnels through [`App::ask_confirm`], which both
//! builds the prompt and records what to do on "yes". Keeping the prompt text
//! next to the action that runs makes it hard to add a risky operation without
//! also adding its confirmation.

use crate::app::{App, ConfirmAction, Overlay};
use crate::eventlog;
use crate::help;
use crate::model::{StatusMsg, TunnelId};

/// Stable name for a gated action, used as the `action=` field in the log.
fn action_name(action: &ConfirmAction) -> String {
    match action {
        ConfirmAction::Delete(_) => "delete".to_string(),
        ConfirmAction::Stop(_) => "stop".to_string(),
        ConfirmAction::StopAll => "stop_all".to_string(),
        ConfirmAction::Restart(_) => "restart".to_string(),
        ConfirmAction::StartAll => "start_all".to_string(),
        ConfirmAction::ApplyEdit(_) => "apply_edit".to_string(),
        ConfirmAction::DiscardForm => "discard_form".to_string(),
        ConfirmAction::Batch { action, .. } => format!("batch_{}", action.name()),
        ConfirmAction::Quit => "quit".to_string(),
    }
}

/// Human-readable target of a gated action (tunnel name or count).
fn action_detail(app: &App, action: &ConfirmAction) -> String {
    match action {
        ConfirmAction::Delete(id)
        | ConfirmAction::Stop(id)
        | ConfirmAction::Restart(id)
        | ConfirmAction::ApplyEdit(id) => app.tunnel(*id).map(|t| t.cfg.name.clone()).unwrap_or_default(),
        ConfirmAction::Batch { count, .. } => format!("{count}"),
        ConfirmAction::StopAll | ConfirmAction::StartAll => format!("{}", app.tunnels.len()),
        _ => String::new(),
    }
}

/// Fallback prompt for a named action whose tunnel is no longer in the table.
///
/// Reaching this means the id was stale (deleted by a batch while the dialog was
/// open, say). It is a degraded path, not a crash: the dialog still shows a
/// usable sentence, and the record explains why the name is missing. Never
/// `unreachable!` — "the tunnel must still exist" is exactly the assumption a
/// panic would fail to falsify.
fn unknown_target_prompt(action: &ConfirmAction) -> String {
    eventlog::action_target_missing(&action_name(action));
    help::bi(
        "确认执行该操作？目标转发已不存在，可能不会产生任何效果。",
        "Run this action? Its target no longer exists, so it may do nothing.",
    )
}

impl App {
    /// Arm a confirmation for `action`.
    ///
    /// The prompt is rendered by the UI from the same `ConfirmAction`, so a
    /// new risky operation cannot accidentally ship without a dialog.
    pub fn ask_confirm(&mut self, action: ConfirmAction) {
        let prompt = self.confirm_prompt(&action);
        eventlog::action_requested(&action_name(&action), &prompt);
        self.overlay = Overlay::Confirm { action, prompt };
    }

    /// The text shown on the confirmation gate for `action`.
    ///
    /// Split out of [`App::ask_confirm`] so the "which prompt" decision is a
    /// pure function of `(App, action)` and can be asserted without arming an
    /// overlay. Every arm produces *some* prompt: a missing arm degrades to a
    /// generic "continue?" and is reported to the log, because silently
    /// rendering nothing would leave a dialog the user cannot read and a
    /// `panic!` would kill a session over a UI string.
    /// The sentence shown in the confirmation dialog for `action`.
    ///
    /// Named targets (delete / stop / restart / apply-edit) read better with the
    /// tunnel's name; the rest describe a count or are fixed. Every sentence is
    /// bilingual, Chinese first — the Chinese half is what the tests and the
    /// user's scanning eye key off, the English half makes the dialog usable for
    /// a non-Chinese reader.
    ///
    /// Split in two so the "which words for this action" table
    /// ([`App::confirm_prompt`]) stays separate from the "which tunnel / how many"
    /// arithmetic ([`App::named_prompt`] / [`App::count_prompt`]).
    fn confirm_prompt(&self, action: &ConfirmAction) -> String {
        match action {
            ConfirmAction::Delete(id)
            | ConfirmAction::Stop(id)
            | ConfirmAction::Restart(id)
            | ConfirmAction::ApplyEdit(id) => self.named_prompt(action, id),
            ConfirmAction::StopAll | ConfirmAction::StartAll | ConfirmAction::Quit => self.count_prompt(action),
            ConfirmAction::DiscardForm => help::bi(
                "确认放弃未保存的修改？已输入的内容会丢失。",
                "Discard the unsaved changes? What you typed will be lost.",
            ),
            // The batch path builds its prompt from the action and the effective
            // count, so this arm is only reachable if a caller forgot to do that.
            ConfirmAction::Batch { action, count } => action.prompt(*count),
        }
    }

    /// A sentence that names one tunnel, or the "it disappeared" fallback.
    fn named_prompt(&self, action: &ConfirmAction, id: &TunnelId) -> String {
        let Some(name) = self.tunnel(*id).map(|t| t.cfg.name.clone()) else {
            return unknown_target_prompt(action);
        };
        let (zh, en) = match action {
            ConfirmAction::Delete(_) => {
                (format!("确认删除「{name}」？此操作不可撤销。"), format!("Delete \"{name}\"? This cannot be undone."))
            }
            ConfirmAction::Stop(_) => (
                format!("确认停用「{name}」？将断开其现有连接。"),
                format!("Stop \"{name}\"? Its live connection will be dropped."),
            ),
            ConfirmAction::Restart(_) => (
                format!("确认重启「{name}」？连接会短暂中断。"),
                format!("Restart \"{name}\"? The connection will briefly drop."),
            ),
            // A save always rebuilds a running connection, so say so.
            _ => (
                format!("确认保存对「{name}」的修改？运行中的连接会重建。"),
                format!("Save the changes to \"{name}\"? A running connection will be rebuilt."),
            ),
        };
        help::bi(&zh, &en)
    }

    /// A sentence built from the live counters (all-rows actions and quit).
    fn count_prompt(&self, action: &ConfirmAction) -> String {
        let counts = self.counts();
        match action {
            ConfirmAction::StopAll => {
                let n = counts.up + counts.starting + counts.retrying;
                help::bi(
                    &format!("确认停用全部 {n} 条运行中的转发？将断开所有连接。"),
                    &format!("Stop all {n} running forwards? Every connection will be dropped."),
                )
            }
            ConfirmAction::StartAll => {
                let n = counts.off;
                help::bi(
                    &format!("确认启用全部转发？将新开 {n} 个监听端口。"),
                    &format!("Start all forwards? {n} new listening ports will open."),
                )
            }
            ConfirmAction::Quit => {
                // The count comes from the rendered state rather than from the
                // supervisor: a row can be `Ready` while its session is being torn
                // down, and the prompt must name what the user actually sees.
                let running = counts.up + counts.starting + counts.retrying;
                if running == 0 {
                    help::bi("确认退出 sshpf？", "Quit sshpf?")
                } else {
                    help::bi(
                        &format!("还有 {running} 条转发在运行，退出将全部停止。"),
                        &format!("{running} forwards are still running; quitting stops them all."),
                    )
                }
            }
            // Only reachable if a caller passes a non-counted action here.
            _ => unknown_target_prompt(action),
        }
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
        eventlog::action_applied(&action_name(&action), &action_detail(self, &action));
        self.apply_confirmed(action);
    }

    /// Cancel whatever gate is open. Logged because "the user hit esc" is the
    /// explanation when a tunnel that was supposed to stop is still running.
    pub fn confirm_no(&mut self) {
        if let Overlay::Confirm { action, .. } = self.overlay.clone() {
            eventlog::action_cancelled(&action_name(&action));
        }
        self.close_overlay();
    }

    pub(crate) fn apply_confirmed(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Delete(id) => {
                // Delete *this* id, not "whatever row is selected now". The
                // dialog named one tunnel; if the id went stale while it was
                // open (a batch delete behind the dialog), falling back to the
                // cursor would silently remove a different definition.
                let vis = self.visible_indices();
                if let Some(pos) = vis.iter().position(|i| self.tunnels.get(*i).is_some_and(|t| t.cfg.id == id)) {
                    self.selected = pos;
                }
                if self.tunnel(id).is_none() {
                    eventlog::action_target_missing(&action_name(&ConfirmAction::Delete(id)));
                    self.set_status(StatusMsg::warn("该转发已不存在，未删除任何内容"));
                    return;
                }
                self.delete_tunnel(id);
                self.set_status(StatusMsg::info("已删除"));
            }
            ConfirmAction::Stop(id) => {
                self.stop_tunnel(id);
                self.set_status(StatusMsg::info(help::bi("已停用", "Stopped")));
            }
            ConfirmAction::StopAll => self.stop_all(),
            ConfirmAction::Restart(id) => {
                self.supervisor.stop(id);
                self.forget_runtime_state(id);
                self.start_tunnel(id);
            }
            ConfirmAction::StartAll => self.start_all(),
            ConfirmAction::ApplyEdit(id) => self.commit_form_edit(id),
            ConfirmAction::DiscardForm => {
                self.close_overlay();
                self.set_status(StatusMsg::info(help::bi("已放弃修改", "Changes discarded")));
            }
            ConfirmAction::Quit => self.should_quit = true,
            ConfirmAction::Batch { action, .. } => self.apply_batch(action),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{BatchAction, FailureInfo, FailureKind, ForwardKind, TunnelConfig, TunnelStatus};
    use std::time::Instant;

    fn tunnel(id: TunnelId, name: &str, port: u16) -> TunnelConfig {
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

    fn app_with(tunnels: Vec<TunnelConfig>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
        crate::config::save(&paths.config, &cfg).unwrap();
        (dir, App::new(paths))
    }

    #[test]
    fn a_named_action_on_a_missing_tunnel_still_produces_a_prompt() {
        // Regression: the named branches used to fall through to
        // `unreachable!()` when `self.tunnel(id)` returned `None`. The id can go
        // stale — a batch delete behind an open dialog does exactly that — and a
        // UI string is not worth aborting a session over.
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        for action in [
            ConfirmAction::Delete(99),
            ConfirmAction::Stop(99),
            ConfirmAction::Restart(99),
            ConfirmAction::ApplyEdit(99),
        ] {
            app.ask_confirm(action.clone());
            match &app.overlay {
                Overlay::Confirm { prompt, .. } => {
                    assert!(!prompt.is_empty(), "{action:?}: the gate must still say something");
                    assert!(prompt.contains("确认"), "{action:?}: expected a confirmation sentence, got {prompt}");
                }
                other => panic!("{action:?}: no confirmation armed, got {other:?}"),
            }
        }
    }

    #[test]
    fn every_confirm_action_has_a_non_empty_prompt() {
        // Exhaustive over `ConfirmAction`: adding a variant without a prompt arm
        // must not become the next `unreachable!()`.
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.set_state(1, TunnelStatus::Up { since: Instant::now() });
        let actions = [
            ConfirmAction::Delete(1),
            ConfirmAction::Stop(1),
            ConfirmAction::StopAll,
            ConfirmAction::Restart(1),
            ConfirmAction::StartAll,
            ConfirmAction::ApplyEdit(1),
            ConfirmAction::DiscardForm,
            ConfirmAction::Batch { action: BatchAction::Delete, count: 2 },
            ConfirmAction::Quit,
        ];
        for action in actions {
            app.ask_confirm(action.clone());
            match &app.overlay {
                Overlay::Confirm { prompt, .. } => {
                    assert!(!prompt.is_empty(), "{action:?}: empty prompt");
                    assert!(!prompt.contains('\n'), "{action:?}: the gate is one line");
                }
                other => panic!("{action:?}: expected a confirmation, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_named_prompt_carries_the_tunnel_name() {
        let (_d, mut app) = app_with(vec![tunnel(1, "cloud-pc", 18001)]);
        app.ask_confirm(ConfirmAction::Delete(1));
        match &app.overlay {
            Overlay::Confirm { prompt, .. } => assert!(prompt.contains("cloud-pc"), "{prompt}"),
            other => panic!("expected a confirmation, got {other:?}"),
        }
    }

    #[test]
    fn the_stop_all_prompt_counts_running_tunnels_only() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001), tunnel(2, "b", 18002), tunnel(3, "c", 18003)]);
        app.set_state(1, TunnelStatus::Up { since: Instant::now() });
        app.set_state(2, TunnelStatus::Failed { reason: FailureInfo::new(FailureKind::AuthFailed, "x") });
        app.ask_confirm(ConfirmAction::StopAll);
        match &app.overlay {
            // 1 up + 0 starting + 0 retrying; the failed row is not running.
            Overlay::Confirm { prompt, .. } => assert!(prompt.contains('1'), "{prompt}"),
            other => panic!("expected a confirmation, got {other:?}"),
        }
    }

    #[test]
    fn confirming_an_action_on_a_missing_tunnel_is_a_harmless_no_op() {
        // The stale-id prompt must not arm a destructive operation afterwards.
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.ask_confirm(ConfirmAction::Delete(99));
        app.confirm_yes();
        assert_eq!(app.tunnels.len(), 1, "the real tunnel must survive a stale id");
        assert!(!app.should_quit);
    }
}

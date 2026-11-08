//! Risk gate: arming, rendering text for, and applying confirmed actions.
//!
//! Every destructive operation funnels through [`App::ask_confirm`], which both
//! builds the prompt and records what to do on "yes". Keeping the prompt text
//! next to the action that runs makes it hard to add a risky operation without
//! also adding its confirmation.

use crate::app::log as eventlog;
use crate::app::{App, ConfirmAction, Overlay};
use crate::model::StatusMsg;

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

impl App {
    /// Arm a confirmation for `action`.
    ///
    /// The prompt is rendered by the UI from the same `ConfirmAction`, so a
    /// new risky operation cannot accidentally ship without a dialog.
    pub fn ask_confirm(&mut self, action: ConfirmAction) {
        let name = match &action {
            ConfirmAction::Delete(id)
            | ConfirmAction::Stop(id)
            | ConfirmAction::Restart(id)
            | ConfirmAction::ApplyEdit(id) => self.tunnel(*id).map(|t| t.cfg.name.as_str()).map(|n| n.to_string()),
            _ => None,
        };
        // Leak-free: keep the name in the overlay so the prompt can borrow it.
        let prompt = match &name {
            Some(n) => match &action {
                ConfirmAction::Delete(_) => format!("确认删除「{n}」？此操作不可撤销。"),
                ConfirmAction::Stop(_) => format!("确认停用「{n}」？将断开其现有连接。"),
                ConfirmAction::Restart(_) => format!("确认重启「{n}」？连接会短暂中断。"),
                ConfirmAction::ApplyEdit(_) => format!("确认保存对「{n}」的修改？运行中的连接会重建。"),
                _ => unreachable!(),
            },
            None => match &action {
                ConfirmAction::StopAll => {
                    let n = self.counts().up + self.counts().starting + self.counts().retrying;
                    format!("确认停用全部 {n} 条运行中的转发？将断开所有连接。")
                }
                ConfirmAction::StartAll => {
                    let n = self.counts().off;
                    format!("确认启用全部转发？将新开 {n} 个监听端口。")
                }
                ConfirmAction::DiscardForm => "确认放弃未保存的修改？已输入的内容会丢失。".to_string(),
                // The batch path builds its prompt from the action and the
                // effective count, so this arm is only reachable if a caller
                // forgot to do that.
                ConfirmAction::Batch { action, count } => action.prompt(*count),
                ConfirmAction::Quit => {
                    let running = self.supervisor.running_count();
                    if running == 0 {
                        "确认退出 sshpf？".to_string()
                    } else {
                        format!("还有 {running} 条转发在运行，退出将全部停止。")
                    }
                }
                _ => "确认执行该操作？".to_string(),
            },
        };
        eventlog::action_requested(&action_name(&action), &prompt);
        self.overlay = Overlay::Confirm { action, prompt };
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
                let vis = self.visible_indices();
                if let Some(pos) = vis.iter().position(|i| self.tunnels.get(*i).is_some_and(|t| t.cfg.id == id)) {
                    self.selected = pos;
                }
                self.delete_selected();
            }
            ConfirmAction::Stop(id) => {
                self.stop_tunnel(id);
                self.set_status(StatusMsg::info("已停用"));
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
                self.set_status(StatusMsg::info("已放弃修改"));
            }
            ConfirmAction::Quit => self.should_quit = true,
            ConfirmAction::Batch { action, .. } => self.apply_batch(action),
        }
    }
}

//! Multi-row selection on the main table: marking rows and running one
//! operation over all of them.
//!
//! The single-row flow asks per action ("delete *this* tunnel?") which does not
//! scale to "stop these eight". Selection mode makes the target set explicit and
//! visible *before* any key does something, so a batch action can never surprise
//! the user about its scope.
//!
//! Two invariants are load-bearing:
//!
//! * Marks are `TunnelId`s, never row indices. Filtering and deletion move rows;
//!   a mark that silently retargeted a different tunnel would be catastrophic
//!   for batch delete.
//! * Batch actions still go through the confirmation gate. Even "enable" opens N
//!   listeners at once — the same reason single-row enable-all asks.

use crate::app::{App, ConfirmAction, Overlay};
use crate::eventlog;
use crate::help;
use crate::model::{BatchAction, StatusMsg};

impl App {
    /// `v`: enter or leave selection mode.
    pub fn toggle_select_mode(&mut self) {
        if self.select_mode.active {
            self.exit_select_mode();
        } else {
            self.select_mode.enter();
            self.select_mode.anchor = Some(self.selected);
            if self.visible_ids().is_empty() {
                self.set_status(StatusMsg::info(help::bi(
                    "选择模式：没有可选的条目",
                    "Selection mode: nothing to select",
                )));
            } else {
                self.set_status(StatusMsg::info(help::bi(
                    "选择模式：space 勾选 · a 全选 · esc 退出",
                    "Selection mode: space marks · a selects all · esc exits",
                )));
            }
        }
    }

    pub fn exit_select_mode(&mut self) {
        self.select_mode.exit();
        self.set_status(StatusMsg::info(help::bi("已退出选择模式", "Left selection mode")));
    }

    /// `space` in selection mode: toggle the row under the cursor.
    pub fn toggle_mark_selected(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        self.select_mode.toggle(id);
        self.after_mark_change();
    }

    /// Shift+j / shift+↓: extend the mark range from the anchor to the cursor.
    pub fn extend_mark_to_cursor(&mut self) {
        let ids = self.visible_ids();
        // `mark_range` clamps the range, so the only thing to guard here is the
        // empty view (a negative index would otherwise reach the anchor math).
        let Some(last) = ids.len().checked_sub(1) else {
            return;
        };
        let to = self.selected.min(last);
        let from = self.select_mode.anchor.unwrap_or(to);
        self.select_mode.mark_range(&ids, from, to);
        self.select_mode.anchor = Some(to);
        self.after_mark_change();
    }

    /// `a` in selection mode: mark every row in the *filtered* view.
    ///
    /// Filtered, deliberately: with a filter active the user is looking at a
    /// subset, and a "select all" that also marked hidden rows would make the
    /// following batch delete far wider than what is on screen.
    pub fn mark_all_visible(&mut self) {
        let ids = self.visible_ids();
        self.select_mode.mark_all(&ids);
        self.select_mode.anchor = Some(ids.len().saturating_sub(1));
        self.after_mark_change();
    }

    pub fn mark_none(&mut self) {
        self.select_mode.clear();
        self.after_mark_change();
    }

    pub fn mark_all_toggle(&mut self) {
        let ids = self.visible_ids();
        let all_marked = !ids.is_empty() && ids.iter().all(|id| self.select_mode.is_marked(*id));
        if all_marked {
            self.mark_none();
        } else {
            self.mark_all_visible();
        }
    }

    fn after_mark_change(&mut self) {
        let n = self.select_mode.len();
        self.set_status(StatusMsg::info(help::bi(&format!("已选 {n} 条"), &format!("{n} marked"))));
    }

    /// Ask for confirmation before applying `action` to the marked set.
    ///
    /// An empty selection and a selection that is already in the desired state
    /// are both no-ops: asking the user to confirm doing nothing is noise.
    pub fn request_batch(&mut self, action: BatchAction) {
        let ids = self.marked_ids_in_view_order();
        if ids.is_empty() {
            self.set_status(StatusMsg::warn(help::bi("没有选中任何条目", "Nothing marked")));
            return;
        }
        let effective = ids.iter().filter(|id| self.batch_would_change(action, **id)).count();
        if effective == 0 {
            self.set_status(StatusMsg::info(help::bi(
                &format!("选中的 {} 条都无需{}", ids.len(), action.label()),
                &format!("None of the {} marked forwards needs {}", ids.len(), action.label_en()),
            )));
            return;
        }
        // Confirm with the *effective* count: the prompt should describe what
        // will actually happen, not how many rows happen to be marked.
        self.overlay = Overlay::Confirm {
            action: ConfirmAction::Batch { action, count: effective },
            prompt: action.prompt(effective),
        };
    }

    /// Whether `action` would do anything to `id` (so the prompt can be honest).
    fn batch_would_change(&self, action: BatchAction, id: crate::model::TunnelId) -> bool {
        let Some(view) = self.tunnel(id) else {
            return false;
        };
        match action {
            BatchAction::Start => !view.status.is_running_or_pending(),
            BatchAction::Stop | BatchAction::Restart => view.status.is_running_or_pending(),
            BatchAction::Delete => true,
        }
    }

    /// Apply a confirmed batch action to every marked tunnel.
    pub fn apply_batch(&mut self, action: BatchAction) {
        let ids = self.marked_ids_in_view_order();
        if ids.is_empty() {
            return;
        }
        let n = ids.len();
        // "marked" and "changed" differ (a batch stop over already-stopped rows),
        // and the discarded difference is exactly what makes a batch drain fast
        // with no visible effect — worth recording.
        let mut changed = 0usize;
        for id in &ids {
            match action {
                BatchAction::Start => {
                    if self.tunnel(*id).is_some_and(|t| !t.status.is_running_or_pending()) {
                        self.start_tunnel_with(*id, "batch");
                        changed += 1;
                    }
                }
                BatchAction::Stop => {
                    if self.tunnel(*id).is_some_and(|t| t.status.is_running_or_pending()) {
                        self.stop_tunnel_with(*id, "batch");
                        changed += 1;
                    }
                }
                BatchAction::Restart => {
                    // One command to the owner (see `ConfirmAction::Restart`): a
                    // mirror has no supervisor to stop first.
                    if self.forward(crate::ipc::Op::Restart { tunnel_id: *id }) {
                        changed += 1;
                        continue;
                    }
                    self.supervisor.stop(*id);
                    self.forget_runtime_state(*id);
                    self.start_tunnel_with(*id, "batch");
                    changed += 1;
                }
                BatchAction::Delete => {
                    self.delete_tunnel(*id);
                    changed += 1;
                }
            }
        }
        eventlog::action_batch(action.name(), n, changed);
        // Deleting invalidates the marked ids; the others stay marked so the
        // user can follow up (e.g. start, then restart) without re-selecting.
        if action == BatchAction::Delete {
            self.select_mode.clear();
            self.clamp_selection();
        } else {
            let existing: Vec<crate::model::TunnelId> = self.tunnels.iter().map(|t| t.cfg.id).collect();
            self.select_mode.retain_existing(|id| existing.contains(&id));
        }
        self.set_status(StatusMsg::ok(help::bi(
            &format!("已{} {n} 条", action.label()),
            &format!("{} done for {n} forwards", action.label_en()),
        )));
    }
}

#[cfg(test)]
mod tests;

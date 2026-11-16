//! Group lifecycle: browsing, working state, aliases and deletion.
//!
//! A **group** is the unit of mutual exclusion. At most one group is *working*
//! (`active_group`); its enabled forwards run, and every other group is stopped
//! but still browsable and editable. Switching is deliberately asymmetric:
//!
//! * browsing ([`App::browse_group`]) only changes what the table shows;
//! * activating ([`App::activate_group`]) stops the previous group's runtime
//!   **without clearing its `enabled` intent**, then starts the new group's
//!   enabled forwards;
//! * deactivating ([`App::deactivate_group`]) stops the runtime and leaves no
//!   working group, which is a legal state.
//!
//! The table, the counters and all row-level actions are scoped to
//! `browsed_group`; the runtime is scoped to `active_group`.
//!
//! Belongs here: group selection, activation, alias prompts, group deletion.
//! Does not belong: rendering (`ui::group`) or key dispatch (`keys::group`).

use crate::app::{App, ConfirmAction, Overlay};
use crate::eventlog;
use crate::help;
use crate::model::{
    FormMode, ForwardKind, GroupConfig, GroupId, InputBuf, LogLevel, StatusMsg, TunnelConfig, TunnelId, TunnelStatus,
};

/// What the alias popup is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupNameMode {
    Create,
    Rename(GroupId),
}

/// One open alias input. Lives on `App` next to the form, not in `Overlay`,
/// because the text buffer is state while the overlay is only a key route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupNamePrompt {
    pub mode: GroupNameMode,
    pub input: InputBuf,
}

impl App {
    pub fn browsed_group_cfg(&self) -> Option<&GroupConfig> {
        self.cfg.group(self.browsed_group)
    }

    pub fn active_group_cfg(&self) -> Option<&GroupConfig> {
        self.active_group.and_then(|id| self.cfg.group(id))
    }

    pub fn group_of(&self, tunnel: TunnelId) -> Option<GroupId> {
        self.cfg.find(tunnel).map(|t| t.group)
    }

    pub fn is_group_active(&self, group: GroupId) -> bool {
        self.active_group == Some(group)
    }

    /// Whether table actions that touch the runtime are allowed right now.
    pub fn browsed_group_is_active(&self) -> bool {
        self.is_group_active(self.browsed_group)
    }

    pub fn group_tunnel_count(&self, group: GroupId) -> usize {
        self.cfg.tunnels.iter().filter(|t| t.group == group).count()
    }

    pub fn group_running_count(&self, group: GroupId) -> usize {
        self.tunnels.iter().filter(|t| t.cfg.group == group && t.status.is_running_or_pending()).count()
    }

    /// Local ports already claimed by managed tunnels **in the browsed group**.
    ///
    /// Another group's ports are not a conflict: groups never listen at the same
    /// time, so the same local port is a legitimate choice in each of them.
    pub fn taken_local_ports(&self) -> Vec<u16> {
        let editing = self.form.as_ref().and_then(|f| match f.mode {
            FormMode::Edit(id) => Some(id),
            FormMode::Create => None,
        });
        self.tunnels
            .iter()
            .filter(|t| t.cfg.group == self.browsed_group)
            .filter(|t| Some(t.cfg.id) != editing)
            .filter(|t| t.cfg.kind != ForwardKind::Remote)
            .map(|t| t.cfg.local_port)
            .collect()
    }

    /// Recompute the local-port suggestion for the open form.
    pub fn refresh_local_port_suggestion(&mut self) {
        let taken = self.taken_local_ports();
        if let Some(form) = self.form.as_mut() {
            form.suggest_local_port(&taken);
            form.revalidate();
        }
    }

    /// Name of a forward in the same group whose listening endpoint collides.
    pub fn listen_conflict(&self, candidate: &TunnelConfig) -> Option<String> {
        for t in &self.tunnels {
            if t.cfg.id == candidate.id || t.cfg.group != candidate.group {
                continue;
            }
            if candidate.listen_overlap(&t.cfg) {
                return Some(t.cfg.name.clone());
            }
        }
        None
    }

    /// `[` / `]`: move the browse cursor, wrapping at the ends.
    pub fn browse_group(&mut self, delta: isize) {
        let n = self.cfg.groups.len();
        if n == 0 {
            return;
        }
        let pos = self.cfg.groups.iter().position(|g| g.id == self.browsed_group).unwrap_or(0);
        let next = (pos as isize + delta).rem_euclid(n as isize) as usize;
        if let Some(group) = self.cfg.groups.get(next) {
            let id = group.id;
            self.browse_to(id);
        }
    }

    /// Show `group` in the table. Never stops or starts anything.
    pub fn browse_to(&mut self, group: GroupId) {
        let Some(cfg) = self.cfg.group(group) else {
            return;
        };
        let name = cfg.alias();
        self.browsed_group = group;
        self.selected = 0;
        let zh = if self.is_group_active(group) {
            format!("浏览分组「{name}」（工作中）")
        } else {
            format!("浏览分组「{name}」（未工作，按 w 开始）")
        };
        let en = if self.is_group_active(group) {
            format!("Browsing group \"{name}\" (working)")
        } else {
            format!("Browsing group \"{name}\" (idle, press w)")
        };
        self.set_status(StatusMsg::info(help::bi(&zh, &en)));
    }

    /// `w`: make the browsed group the working one, or stop working if it
    /// already is.
    pub fn request_toggle_work(&mut self) {
        if self.browsed_group_is_active() {
            self.request_deactivate();
        } else {
            self.request_activate(self.browsed_group);
        }
    }

    fn request_activate(&mut self, group: GroupId) {
        if self.is_group_active(group) {
            return;
        }
        let running = self.active_group.map(|old| self.group_running_count(old)).unwrap_or(0);
        if running > 0 {
            self.ask_confirm(ConfirmAction::ActivateGroup(group));
        } else {
            self.activate_group(group);
        }
    }

    fn request_deactivate(&mut self) {
        let Some(old) = self.active_group else {
            return;
        };
        if self.group_running_count(old) > 0 {
            self.ask_confirm(ConfirmAction::DeactivateGroup(old));
        } else {
            self.deactivate_group();
        }
    }

    /// Make `group` the only working group and start its enabled forwards.
    pub fn activate_group(&mut self, group: GroupId) {
        let Some(cfg) = self.cfg.group(group) else {
            return;
        };
        let name = cfg.alias();
        if self.active_group == Some(group) {
            self.browsed_group = group;
            return;
        }
        if let Some(old) = self.active_group {
            self.stop_group_runtime(old);
        }
        self.active_group = Some(group);
        self.cfg.active_group = Some(group);
        self.browsed_group = group;
        self.selected = 0;

        let ids: Vec<TunnelId> =
            self.tunnels.iter().filter(|t| t.cfg.group == group && t.cfg.enabled).map(|t| t.cfg.id).collect();
        let n = ids.len();
        for id in ids {
            self.start_tunnel_with(id, "group");
        }
        self.persist_quietly();
        self.set_status(StatusMsg::ok(help::bi(
            &format!("分组「{name}」开始工作（{n} 条转发）"),
            &format!("Group \"{name}\" is now working ({n} forwards)"),
        )));
    }

    /// Stop the working group without forgetting what should run next time.
    pub fn deactivate_group(&mut self) {
        let Some(old) = self.active_group else {
            return;
        };
        let name = self.cfg.group(old).map(|g| g.alias()).unwrap_or_else(|| format!("#{old}"));
        self.stop_group_runtime(old);
        self.active_group = None;
        self.cfg.active_group = None;
        self.persist_quietly();
        self.set_status(StatusMsg::info(help::bi(
            &format!("分组「{name}」已停止工作（保留启用意图）"),
            &format!("Group \"{name}\" stopped working (enabled intent kept)"),
        )));
    }

    /// Stop every tunnel of `group` at the runtime level.
    ///
    /// Deliberately does **not** clear `enabled`: switching groups is not the
    /// same as turning forwards off, and clearing the flag would make the group
    /// come back empty the next time the user switches to it.
    fn stop_group_runtime(&mut self, group: GroupId) {
        let ids: Vec<TunnelId> = self.tunnels.iter().filter(|t| t.cfg.group == group).map(|t| t.cfg.id).collect();
        for id in ids {
            let had_runtime = self.tunnel(id).is_some_and(|t| t.status.is_running_or_pending());
            if had_runtime {
                if let Some(cfg) = self.cfg.find(id).cloned() {
                    eventlog::tunnel_stopped(&cfg, "group");
                }
                self.supervisor.stop(id);
                self.log(id, LogLevel::Info, help::bi("分组切换，已停止", "Stopped by group switch"));
            }
            self.forget_runtime_state(id);
            // Even a `Failed` row becomes `Stopped` here: its failure belonged to
            // the run it had while this group was working, not to the definition
            // the user is now browsing.
            self.set_state(id, TunnelStatus::Stopped);
            if let Some(view) = self.tunnels.iter_mut().find(|t| t.cfg.id == id) {
                view.retries = 0;
            }
        }
    }

    // -- aliases -----------------------------------------------------------

    /// `N`: ask for a name for a new group.
    pub fn open_group_create(&mut self) {
        self.group_prompt = Some(GroupNamePrompt { mode: GroupNameMode::Create, input: InputBuf::new("") });
        self.overlay = Overlay::GroupName;
    }

    /// `R`: rename the browsed group. Valid at any time, working or not.
    pub fn open_group_rename(&mut self) {
        let Some(group) = self.browsed_group_cfg() else {
            return;
        };
        let input = InputBuf::new(group.name.clone());
        self.group_prompt = Some(GroupNamePrompt { mode: GroupNameMode::Rename(group.id), input });
        self.overlay = Overlay::GroupName;
    }

    /// Commit the alias popup (`enter`).
    pub fn submit_group_prompt(&mut self) {
        let Some(prompt) = self.group_prompt.take() else {
            self.close_overlay();
            return;
        };
        match prompt.mode {
            GroupNameMode::Create => {
                let id = self.cfg.next_group_id();
                let name = self.cfg.unique_group_name(prompt.input.value(), None);
                self.cfg.groups.push(GroupConfig::new(id, name));
                self.browsed_group = id;
                self.selected = 0;
                let alias = self.cfg.group(id).map(|g| g.alias()).unwrap_or_else(|| format!("#{id}"));
                self.close_overlay();
                self.persist_quietly();
                self.set_status(StatusMsg::info(help::bi(
                    &format!("已新建分组「{alias}」；按 w 让它开始工作"),
                    &format!("Created group \"{alias}\"; press w to make it work"),
                )));
            }
            GroupNameMode::Rename(id) => {
                let name = self.cfg.unique_group_name(prompt.input.value(), Some(id));
                let Some(group) = self.cfg.group_mut(id) else {
                    self.close_overlay();
                    return;
                };
                group.name = name;
                let alias = group.alias();
                self.close_overlay();
                self.persist_quietly();
                self.set_status(StatusMsg::info(help::bi(
                    &format!("分组别名已改为「{alias}」"),
                    &format!("Group alias set to \"{alias}\""),
                )));
            }
        }
    }

    // -- deletion ----------------------------------------------------------

    /// `D`: delete the browsed group and every forward in it, after confirmation.
    pub fn request_delete_group(&mut self) {
        let Some(group) = self.browsed_group_cfg() else {
            return;
        };
        if self.cfg.groups.len() <= 1 {
            self.set_status(StatusMsg::warn(help::bi(
                "至少保留一个分组，未删除",
                "At least one group must remain; nothing was deleted",
            )));
            return;
        }
        let id = group.id;
        self.ask_confirm(ConfirmAction::DeleteGroup(id));
    }

    /// Apply a confirmed group deletion.
    pub fn delete_group(&mut self, group: GroupId) {
        let Some(cfg) = self.cfg.group(group) else {
            return;
        };
        let name = cfg.alias();
        let was_active = self.active_group == Some(group);
        if was_active {
            self.stop_group_runtime(group);
            self.active_group = None;
            self.cfg.active_group = None;
        }
        let ids: Vec<TunnelId> = self.cfg.tunnels.iter().filter(|t| t.group == group).map(|t| t.id).collect();
        for id in ids {
            self.supervisor.stop(id);
            self.forget_runtime_state(id);
        }
        self.cfg.tunnels.retain(|t| t.group != group);
        self.tunnels.retain(|t| t.cfg.group != group);
        self.cfg.groups.retain(|g| g.id != group);

        if self.browsed_group == group
            && let Some(first) = self.cfg.groups.first()
        {
            self.browsed_group = first.id;
        }
        self.clamp_selection();
        self.persist_quietly();
        let zh = if was_active {
            format!("已删除分组「{name}」；当前没有工作中的分组")
        } else {
            format!("已删除分组「{name}」")
        };
        let en = if was_active {
            format!("Deleted group \"{name}\"; no group is working now")
        } else {
            format!("Deleted group \"{name}\"")
        };
        self.set_status(StatusMsg::info(help::bi(&zh, &en)));
    }
}

#[cfg(test)]
mod tests;

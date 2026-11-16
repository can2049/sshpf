//! Group definitions: the unit that owns an independent set of forwards.
//!
//! At most one group is *working* at a time; every other group can still be
//! browsed and edited. Definitions never leak across groups: two groups may
//! contain the same forward, and duplicate/listen checks are scoped to one
//! group. This file owns the persisted shape of a group and the repair rules
//! applied when an older `config.toml` is loaded.
//!
//! Belongs here: a group's id/alias and load-time normalisation of the
//! group list. Does not belong: which group is active (that is runtime state in
//! `app`), or anything that needs a socket or a renderer.

use super::{GroupId, TunnelConfig, now_unix};

/// A named bucket of tunnel definitions.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GroupConfig {
    pub id: GroupId,
    /// Optional alias the user can set at any time. Empty means "unnamed"; the
    /// display falls back to `#<id>`.
    #[serde(default)]
    pub name: String,
    /// Unix timestamp of creation, informational only.
    #[serde(default)]
    pub created_at: i64,
}

impl GroupConfig {
    pub fn new(id: GroupId, name: impl Into<String>) -> Self {
        GroupConfig { id, name: name.into(), created_at: now_unix() }
    }

    /// The text the user sees: the trimmed alias, or `#<id>` when unnamed.
    pub fn alias(&self) -> String {
        let trimmed = self.name.trim();
        if trimmed.is_empty() { format!("#{}", self.id) } else { trimmed.to_string() }
    }
}

/// What [`normalize_groups`] repaired, so the caller can log it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupRepair {
    /// Group ids that were `0` or collided with another group.
    pub ids: usize,
    /// Tunnels that referenced a missing group and were moved to the first one.
    pub moved_tunnels: usize,
    /// An `active_group` that no longer exists (was cleared).
    pub active_cleared: bool,
    /// A legacy config (no `[[groups]]`) that got its implicit group.
    pub adopted_legacy: bool,
}

impl GroupRepair {
    pub fn total_repaired(self) -> usize {
        self.ids + self.moved_tunnels + usize::from(self.active_cleared)
    }
}

/// Make a loaded config's groups self-consistent.
///
/// Two jobs, both about *old or hand-edited* files:
///
/// * a config written before groups existed has `[[tunnels]]` and no
///   `[[groups]]`. It becomes one implicit group holding every tunnel, and that
///   group is made active so the previous "restore enabled forwards" behaviour
///   survives the upgrade;
/// * a file can name a missing group, reuse a group id, or leave `active_group`
///   pointing at a deleted group. None of those may make a tunnel unreachable.
///
/// The first group is the fallback target: it is the one the legacy migration
/// creates, and it is stable because the file order is what the group bar draws.
pub fn normalize_groups(
    groups: &mut Vec<GroupConfig>,
    tunnels: &mut [TunnelConfig],
    active: &mut Option<GroupId>,
) -> GroupRepair {
    let mut repair = GroupRepair::default();
    let legacy = groups.is_empty();
    if legacy {
        // A fresh `ConfigFile::default()` already carries this group, so this
        // branch is reached only by a file that predates groups or by a hand
        // edit that removed them.
        groups.push(GroupConfig::new(1, ""));
        *active = Some(1);
        repair.adopted_legacy = true;
    }

    let now = now_unix();
    let mut seen: Vec<GroupId> = Vec::with_capacity(groups.len());
    let candidate_limit = groups.len() as u64 + 1;
    for group in groups.iter_mut() {
        if group.id == 0 || seen.contains(&group.id) {
            // Bounded search rather than `id += 1`: a hand-edited `u64::MAX` id
            // must not make normalisation loop forever.
            group.id = (1..=candidate_limit).find(|id| !seen.contains(id)).unwrap_or(1);
            repair.ids += 1;
        }
        seen.push(group.id);
        if group.created_at == 0 {
            group.created_at = now;
        }
    }

    let fallback = groups.first().map(|g| g.id).unwrap_or(1);
    for tunnel in tunnels.iter_mut() {
        if !seen.contains(&tunnel.group) {
            tunnel.group = fallback;
            repair.moved_tunnels += 1;
        }
    }

    if let Some(id) = *active
        && !seen.contains(&id)
    {
        *active = None;
        repair.active_cleared = true;
    }
    repair
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ForwardKind, TunnelConfig};

    fn tunnel(id: u64, group: GroupId) -> TunnelConfig {
        TunnelConfig {
            id,
            group,
            name: format!("t{id}"),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8000 + id as u16,
            remote_host: "127.0.0.1".into(),
            remote_port: 22,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    #[test]
    fn a_legacy_config_becomes_one_active_group() {
        let mut groups = Vec::new();
        let mut tunnels = vec![tunnel(1, 0), tunnel(2, 0)];
        let mut active = None;
        let repair = normalize_groups(&mut groups, &mut tunnels, &mut active);
        assert!(repair.adopted_legacy);
        assert_eq!(groups.len(), 1);
        assert_eq!(active, Some(groups[0].id));
        assert!(tunnels.iter().all(|t| t.group == groups[0].id));
    }

    #[test]
    fn group_ids_are_repaired_and_missing_membership_gets_the_first_group() {
        let mut groups = vec![GroupConfig::new(0, "a"), GroupConfig::new(0, "b")];
        let mut tunnels = vec![tunnel(1, 99)];
        let mut active = Some(99);
        let repair = normalize_groups(&mut groups, &mut tunnels, &mut active);
        assert_eq!(repair.ids, 2, "both zero ids are replaced");
        assert_ne!(groups[0].id, groups[1].id);
        assert_eq!(repair.moved_tunnels, 1);
        assert_eq!(tunnels[0].group, groups[0].id);
        assert!(repair.active_cleared);
        assert_eq!(active, None);
    }

    #[test]
    fn a_hand_edited_max_id_is_repaired_without_looping() {
        let mut groups = vec![GroupConfig::new(u64::MAX, "a"), GroupConfig::new(u64::MAX, "b")];
        let mut tunnels = vec![];
        let mut active = None;
        let repair = normalize_groups(&mut groups, &mut tunnels, &mut active);
        assert_eq!(repair.ids, 1);
        assert_ne!(groups[0].id, groups[1].id);
        assert_ne!(groups[1].id, 0);
    }

    #[test]
    fn a_missing_alias_falls_back_to_the_id() {
        assert_eq!(GroupConfig::new(7, "  ").alias(), "#7");
        assert_eq!(GroupConfig::new(7, " work ").alias(), "work");
    }
}

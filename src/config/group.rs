//! Group-owned parts of `ConfigFile`: lookup, id/alias allocation and
//! validation.
//!
//! Kept beside `config.rs` rather than inside it for the same reason the file
//! has a size budget: group logic is one responsibility, and the parent file is
//! the map + I/O. Everything here is pure over the parsed config.

use anyhow::{Result, bail};

use crate::model::{ForwardKind, GroupConfig, GroupId, TunnelConfig};

use super::ConfigFile;

impl ConfigFile {
    pub fn group(&self, id: GroupId) -> Option<&GroupConfig> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn group_mut(&mut self, id: GroupId) -> Option<&mut GroupConfig> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    pub fn max_group_id(&self) -> GroupId {
        self.groups.iter().map(|g| g.id).max().unwrap_or(0)
    }

    /// First identifier not used by a group. Like [`ConfigFile::next_id`],
    /// saturating and collision-checked because the file is hand-editable.
    pub fn next_group_id(&self) -> GroupId {
        let next = self.max_group_id().saturating_add(1);
        if next != 0 && self.group(next).is_none() {
            return next;
        }
        (1..=self.groups.len() as u64 + 1).find(|id| self.group(*id).is_none()).unwrap_or(1)
    }

    /// An alias not already used by another group. Empty stays empty (the UI
    /// falls back to `#id`).
    pub fn unique_group_name(&self, base: &str, skip: Option<GroupId>) -> String {
        let base = base.trim();
        if base.is_empty() {
            return String::new();
        }
        let taken = |n: &str| self.groups.iter().any(|g| g.name.trim() == n && Some(g.id) != skip);
        if !taken(base) {
            return base.to_string();
        }
        for i in 2..1000 {
            let candidate = format!("{base}-{i}");
            if !taken(&candidate) {
                return candidate;
            }
        }
        format!("{base}-{}", self.next_group_id())
    }

    /// Validate cross-entry invariants. Returns human-readable problems.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for t in &self.tunnels {
            if let Err(e) = validate_tunnel(t) {
                problems.push(format!("{}: {e}", t.name));
            }
            if self.group(t.group).is_none() {
                problems.push(format!("{}: 不属于任何分组", t.name));
            }
        }
        // Duplicate listening endpoints among enabled tunnels **of one group**.
        // Different groups never listen at the same time, so their ports may
        // deliberately repeat.
        for (i, a) in self.tunnels.iter().enumerate() {
            if !a.enabled {
                continue;
            }
            for b in self.tunnels.iter().skip(i + 1) {
                if !b.enabled || a.group != b.group {
                    continue;
                }
                if a.listen_overlap(b) {
                    problems.push(format!("{} 与 {} 监听同一地址 {}", a.name, b.name, a.local_display()));
                }
            }
        }
        problems
    }
}

/// Field-level validation used by the form and by `load`.
pub fn validate_tunnel(cfg: &TunnelConfig) -> Result<()> {
    if cfg.name.trim().is_empty() {
        bail!("名称不能为空");
    }
    if cfg.ssh_host.trim().is_empty() {
        bail!("SSH 主机不能为空");
    }
    if cfg.ssh_host.chars().any(|c| c.is_whitespace()) {
        bail!("SSH 主机不能包含空格");
    }
    if cfg.local_port == 0 {
        bail!("本地端口不能为 0");
    }
    if cfg.kind != ForwardKind::Dynamic && cfg.remote_port == 0 {
        bail!("远端端口不能为 0");
    }
    if cfg.kind != ForwardKind::Dynamic && cfg.remote_host.trim().is_empty() {
        bail!("远端地址不能为空");
    }
    if cfg.kind != ForwardKind::Remote && cfg.local_addr.trim().is_empty() {
        bail!("本地地址不能为空");
    }
    if cfg.kind == ForwardKind::Remote && cfg.remote_host.trim().is_empty() {
        bail!("远端监听地址不能为空");
    }
    Ok(())
}

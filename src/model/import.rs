//! Forwards discovered in `~/.ssh/config`, and the rules for turning one into a
//! tunnel: which local port to listen on, and how a column filter matches.
//!
//! Discovery data only: an `SshForward` is never persisted as-is (the form turns
//! the rows the user keeps into `TunnelConfig`s, where `id`/`enabled`/
//! `auto_restart` live). The table that shows these rows is [`table`]; its
//! columns are [`row`]; its filters are [`filter`].
//!
//! Belongs here: the shape of a discovered directive, and the *one*
//! implementation of the local-port rules ([`plan_local_port`]) that the form
//! and the importer share.
//! Does not belong: anything that reads the file (that is `ssh::forwards`).

use super::tunnel::{ForwardKind, identity_of};

pub(super) mod filter;
pub(super) mod row;
pub(super) mod table;

pub use row::{ImportColumn, ImportRow};
pub use table::ImportTable;

/// One forward directive read out of a `Host` block in `~/.ssh/config`.
///
/// Addresses are kept as written in the config (`""` / `"*"` / `"0.0.0.0"` /
/// a hostname) because OpenSSH gives them distinct meanings; the form decides
/// how to normalise them, and the table shows exactly what was configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshForward {
    /// Which directive it came from, so the table can show `L` / `R` / `D`.
    pub kind: ForwardKind,
    /// Listen address as written (`""` when the directive omitted it).
    pub listen_addr: String,
    /// Listen port (the local port for `-L`/`-D`, the remote one for `-R`).
    pub listen_port: u16,
    /// Target host as written (`-L`: reachable from the server; `-R`: local).
    /// Always empty for `-D`.
    pub target_host: String,
    /// Target port (0 for `-D`).
    pub target_port: u16,
    /// `Host` alias this directive belongs to.
    pub host_alias: String,
    /// 1-based line number in the config file, for "where did this come from".
    pub line: usize,
    /// The directive verbatim, e.g. `LocalForward 8080 127.0.0.1:80`. Shown in
    /// the table so the user can recognise a row without decoding it.
    pub raw: String,
}

impl SshForward {
    /// Stable identity of the *forward itself*, independent of where it was
    /// declared. Used to de-duplicate rows that several matching `Host` blocks
    /// contribute, and to remember which rows the user unticked across
    /// re-filtering.
    pub fn key(&self) -> String {
        match self.kind {
            ForwardKind::Dynamic => format!("D|{}|{}", self.listen_addr, self.listen_port),
            _ => format!(
                "{:?}|{}|{}|{}|{}",
                self.kind, self.listen_addr, self.listen_port, self.target_host, self.target_port
            ),
        }
    }

    /// `LocalForward` / `RemoteForward` / `DynamicForward`.
    pub fn directive(&self) -> &'static str {
        match self.kind {
            ForwardKind::Local => "LocalForward",
            ForwardKind::Remote => "RemoteForward",
            ForwardKind::Dynamic => "DynamicForward",
        }
    }

    /// `listen` or `listen → target`, for the 目标 column.
    pub fn target_display(&self) -> String {
        match self.kind {
            ForwardKind::Dynamic => "(SOCKS5)".to_string(),
            _ => format!("{}:{}", self.target_host, self.target_port),
        }
    }

    /// `addr:port`, with an empty address rendered as the OpenSSH default.
    pub fn listen_display(&self) -> String {
        let addr = if self.listen_addr.is_empty() { "localhost" } else { self.listen_addr.as_str() };
        format!("{addr}:{}", self.listen_port)
    }

    /// Whether this row can be imported at all. Unix-domain-socket forwards and
    /// non-numeric ports cannot be expressed as a `TunnelConfig`, so they are
    /// listed but never selectable (the table renders them dimmed with a
    /// reason) instead of silently disappearing.
    pub fn unsupported_reason(&self) -> Option<&'static str> {
        if self.listen_port == 0 {
            return Some("端口无法解析");
        }
        if self.kind != ForwardKind::Dynamic && (self.target_host.is_empty() || self.target_port == 0) {
            return Some("目标地址无法解析");
        }
        None
    }

    /// English half of [`SshForward::unsupported_reason`], for the bilingual UI.
    ///
    /// Mirrors the same two conditions rather than matching on the Chinese
    /// string: the reason and its translation then cannot drift apart, and a
    /// third reason added above becomes a compile-visible edit here too.
    pub fn unsupported_reason_en(&self) -> Option<&'static str> {
        if self.listen_port == 0 {
            return Some("Unparseable port");
        }
        if self.kind != ForwardKind::Dynamic && (self.target_host.is_empty() || self.target_port == 0) {
            return Some("Unparseable target");
        }
        None
    }

    /// Suggested tunnel name, e.g. `cloud_pc-L-8080` — unique enough to sort,
    /// short enough for the NAME column.
    pub fn suggested_name(&self) -> String {
        format!("{}-{}-{}", self.host_alias, self.kind.tag(), self.listen_port)
    }

    /// Which side of this directive becomes which side of a `TunnelConfig`.
    ///
    /// Pure, and deliberately *before* any local-port planning: the planner probes
    /// sockets and may bump the number, so the answer depends on what is running
    /// right now. The duplicate question must not depend on that — see
    /// [`SshForward::forward_identity`].
    ///
    /// The mapping is not symmetric, which is why it lives in one function:
    /// `-L`/`-D` listen locally, but `-R` listens *on the server* and lands
    /// locally, so the directive's listen side becomes the config's remote side.
    /// Getting this backwards would silently forward the wrong port.
    pub fn ends(&self, bind: &str) -> ForwardEnds {
        let listen_addr = if self.listen_addr.is_empty() { bind.to_string() } else { self.listen_addr.clone() };
        match self.kind {
            ForwardKind::Local => ForwardEnds {
                local_addr: listen_addr,
                local_port: self.listen_port,
                remote_host: self.target_host.clone(),
                remote_port: self.target_port,
            },
            ForwardKind::Remote => ForwardEnds {
                local_addr: self.target_host.clone(),
                local_port: self.target_port,
                remote_host: listen_addr,
                remote_port: self.listen_port,
            },
            ForwardKind::Dynamic => ForwardEnds {
                local_addr: listen_addr,
                local_port: self.listen_port,
                remote_host: String::new(),
                remote_port: 0,
            },
        }
    }

    /// The [`TunnelConfig::forward_identity`] this directive *means*, without
    /// planning a local port.
    ///
    /// Used to ask "do we already have this forward?" before the import pane
    /// offers the row. Comparing against a planned config would answer a
    /// different question: the planner's output depends on what is listening
    /// right now, so the same row could look new on one run and old on the next.
    ///
    /// `-L`'s local port drops out of the identity anyway (see
    /// [`crate::model::identity_of`]), so this is exact for `-L`. For `-R`/`-D`,
    /// where the local port *is* identity, the directive's own port is used —
    /// that is the port the user asked for, which is what the persisted side means
    /// too unless a bump happened (and `-R` is never bumped; `-D`'s bump is
    /// visible because the row is skipped as a duplicate of the saved tunnel).
    pub fn forward_identity(&self, bind: &str) -> String {
        let ends = self.ends(bind);
        identity_of(self.kind, &self.host_alias, &ends.local_addr, ends.local_port, &ends.remote_host, ends.remote_port)
    }
}

/// One discovered forward, split into the two ends a `TunnelConfig` stores.
///
/// Produced by [`SshForward::ends`], consumed by the importer (which then plans a
/// local port on top). It exists so the direction mapping is one function instead
/// of a match at each call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardEnds {
    pub local_addr: String,
    pub local_port: u16,
    pub remote_host: String,
    pub remote_port: u16,
}

/// Why [`plan_local_port`] picked the port it did, for the form's hint text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPortReason {
    /// Same number as the remote port (the intuitive case).
    SameAsRemote,
    /// The remote port is privileged, so this is a friendly unprivileged alias
    /// (`22 -> 2222`).
    PrivilegedAlias,
    /// The natural choice was taken, so the scan walked upwards.
    Bumped,
}

/// Candidates the upward scan may probe before giving up.
pub const PORT_SCAN_LIMIT: usize = 200;

/// Choose a local listen port for a forward landing on `remote`.
///
/// This is the *one* implementation of the local-port rules; the form and the
/// ssh-config importer both call it, because they already drifted once. The
/// rules, in order:
///
/// 1. `remote` itself, when it is unprivileged and free — "the same number on
///    both ends" is what people expect.
/// 2. For a privileged `remote`, a friendly alias (`22 -> 2222`). Scanning up
///    from a privileged port instead would be futile: binding one needs root, so
///    `available` reports every candidate below 1024 as busy and the whole scan
///    budget burns inside the privileged range. That bug was caught on the form
///    path and is pinned by `privileged_remote_gets_an_alias_not_a_privileged_scan`;
///    the importer must not reintroduce it.
/// 3. Otherwise walk upwards **from 1024**, skipping anything `taken`.
///
/// `available` is a closure so this stays pure and unit-testable without binding
/// real sockets.
pub fn plan_local_port(remote: u16, taken: &[u16], available: impl Fn(u16) -> bool) -> Option<(u16, LocalPortReason)> {
    if remote == 0 {
        return None;
    }
    let usable = |c: u16| c >= 1024 && !taken.contains(&c) && available(c);

    if remote >= 1024 && usable(remote) {
        return Some((remote, LocalPortReason::SameAsRemote));
    }
    if remote < 1024 {
        // The conventional alias: 22 -> 2222, 80 -> 2280, 443 -> 2643.
        let alias = 2000u16.saturating_add(remote).saturating_add(200);
        if usable(alias) {
            return Some((alias, LocalPortReason::PrivilegedAlias));
        }
    }
    (remote.max(1024)..=u16::MAX)
        .take(PORT_SCAN_LIMIT)
        .find(|candidate| usable(*candidate))
        .map(|port| (port, LocalPortReason::Bumped))
}

/// Case-insensitive substring match used by the per-column filters.
///
/// Substring rather than the fuzzy subsequence matcher used by the main table:
/// a column filter is a *scope narrowing* tool (`8080`, `localhost`, `-R`), and
/// fuzzy matching on a port would happily return unrelated rows.
pub fn col_match(needle: &str, haystack: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return true;
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardKind;

    #[test]
    fn a_free_unprivileged_remote_port_is_reused_verbatim() {
        let (port, reason) = plan_local_port(8080, &[], |_| true).unwrap();
        assert_eq!(port, 8080);
        assert_eq!(reason, LocalPortReason::SameAsRemote);
    }

    #[test]
    fn a_privileged_remote_port_gets_an_unprivileged_alias() {
        // The trap this pins: scanning up from 80 would stay below 1024, where
        // every bind needs root. The old importer code did exactly that and
        // produced another privileged port (280 for a requested 80).
        for (remote, alias) in [(22u16, 2222u16), (80, 2280), (443, 2643)] {
            let (port, reason) = plan_local_port(remote, &[], |_| true).unwrap();
            assert_eq!(port, alias, "remote {remote} should alias to {alias}");
            assert!(port >= 1024, "alias for {remote} must be unprivileged, got {port}");
            assert_eq!(reason, LocalPortReason::PrivilegedAlias);
        }
    }

    #[test]
    fn a_privileged_remote_never_yields_a_privileged_result() {
        // Even when the alias is taken, the fallback scan must stay >= 1024.
        for remote in 1u16..=1023 {
            let (port, _) = plan_local_port(remote, &[], |_| true).expect("should always find something");
            assert!(port >= 1024, "remote {remote} produced privileged local {port}");
        }
    }

    #[test]
    fn a_taken_port_is_bumped_upwards() {
        // 8080 taken by another tunnel -> the next free one, never a lower port.
        let (port, reason) = plan_local_port(8080, &[8080], |c| c != 8081).unwrap();
        assert_eq!(port, 8082, "must skip both the taken and the unavailable port");
        assert_eq!(reason, LocalPortReason::Bumped);
        assert!(port > 8080, "bumping must not wrap downwards");
    }

    #[test]
    fn taken_ports_below_the_unprivileged_floor_are_irrelevant() {
        // A privileged port in `taken` cannot affect the >=1024 result.
        let (port, _) = plan_local_port(8080, &[22, 80], |_| true).unwrap();
        assert_eq!(port, 8080);
    }

    #[test]
    fn port_zero_is_not_plannable() {
        assert!(plan_local_port(0, &[], |_| true).is_none(), "port 0 means 'unknown', not 'pick one'");
    }

    #[test]
    fn planning_gives_up_rather_than_hanging_when_nothing_is_free() {
        // A closure that rejects everything must terminate, not spin.
        assert!(plan_local_port(8080, &[], |_| false).is_none());
    }

    #[test]
    fn planning_never_lands_inside_the_taken_set() {
        // Take a contiguous block and check the plan lands outside it.
        let taken: Vec<u16> = (8080..8085).collect();
        let (port, _) = plan_local_port(8080, &taken, |_| true).unwrap();
        assert!(!taken.contains(&port), "{port} is in the taken set");
        assert_eq!(port, 8085);
    }

    #[test]
    fn an_empty_listen_addr_renders_as_the_openssh_default() {
        let f = SshForward {
            kind: ForwardKind::Local,
            listen_addr: String::new(),
            listen_port: 8080,
            target_host: "10.0.0.1".into(),
            target_port: 80,
            host_alias: "h".into(),
            line: 1,
            raw: String::new(),
        };
        assert_eq!(f.listen_display(), "localhost:8080", "an omitted bind address is localhost");
    }
}

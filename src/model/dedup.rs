//! Folding already-saved entries that describe the **same forward** into one.
//!
//! The duplicate guard (`app::duplicate`) stops new duplicates from being
//! *created*. It cannot help a config that already contains them: they were
//! written before the guard existed, or by an older version, or by hand. Left
//! alone they are permanent — the table shows several rows for one intent, several
//! SSH connections are dialled for it, and deleting one only shrinks the count
//! until the next scan proposes it again.
//!
//! So load-time repair: group the saved tunnels by
//! [`TunnelConfig::forward_identity`] and keep one per group. This module is the
//! *decision* (which one survives, what happens to the rest); `config` performs the
//! repair on the parsed file and logs it, because it is the layer that owns
//! `config.toml`.
//!
//! # Which entry survives
//!
//! The **first** in file order. Not the oldest `created_at`, not the one whose
//! local port happens to be free: file order is what the user sees in the table,
//! so "the top one stays" is the only rule that is predictable without reading
//! this file. The survivors are then returned in their original order, so a
//! repaired config does not reshuffle the table.
//!
//! # What "the same forward" means
//!
//! Exactly [`TunnelConfig::forward_identity`] — the same intent rule the create
//! paths use. Nothing here re-derives it, so a future change to the identity
//! cannot make repair and creation disagree about what a duplicate is.
//!
//! # Deliberately not done here
//!
//! * **No deletion from `~/.ssh/config`.** The directive may be the user's only
//!   record outside sshpf; the pane shows it as "already exists" instead.
//! * **No port re-planning.** The survivor keeps its persisted local port. A
//!   repair that started choosing ports would be a silent behaviour change on
//!   start-up, which is a worse surprise than the duplicate it fixes.
//! * **No I/O, no logging.** Pure over the parsed list; the caller records what
//!   changed.

use super::tunnel::TunnelConfig;

/// One folded group: the entry that survives and the ones that duplicated it.
///
/// Kept as data rather than just a filtered `Vec` so the caller can *report* what
/// happened. "Three of your forwards were merged into one" is information the user
/// needs (their table got shorter on start-up) and a log line alone is not enough
/// to explain it in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folded {
    /// The surviving tunnel, unchanged.
    pub kept: String,
    /// The names removed because they meant the same forward.
    pub dropped: Vec<String>,
}

/// Remove every tunnel whose forward identity was already seen earlier in `list`.
///
/// Returns the surviving tunnels in their original order, plus one [`Folded`] per
/// group that actually lost a member. An empty report means the config was already
/// clean, which is the normal case and must stay free of side effects.
///
/// # Why the *first* wins, and not the "best" one
///
/// Every other rule needs a justification the user cannot see:
///
/// * "the oldest `created_at`" — `created_at` is informational and can be 0 after a
///   hand edit, so the rule would be untestable from the user's side;
/// * "the one on a free local port" — the answer changes with what is running, so
///   two starts could fold the same config differently;
/// * "the enabled one" — the user's *choice* about which duplicate to keep is not
///   recoverable from the data; silently preferring an enabled entry would start a
///   listener they turned off.
///
/// File order is what the table draws, so "the first row survives" is the one rule
/// a user can predict by looking at the list they already have.
pub fn fold_duplicates(list: Vec<TunnelConfig>) -> (Vec<TunnelConfig>, Vec<Folded>) {
    let mut kept: Vec<TunnelConfig> = Vec::with_capacity(list.len());
    // Identity -> index into `report`, for the group that identity opened. One
    // lookup structure, not two: `kept` and `report` are written in lockstep, so a
    // second `seen` list of indices into `kept` could only disagree with it.
    let mut groups: Vec<(String, usize)> = Vec::new();
    let mut report: Vec<Folded> = Vec::new();

    for tunnel in list {
        let identity = tunnel.forward_identity();
        match groups.iter().find(|(id, _)| *id == identity).map(|(_, at)| *at) {
            // A repeat: enough for a report entry, then discarded. The survivor
            // keeps its persisted definition untouched — repairing the *ports*
            // would be a start-up behaviour change nobody asked for.
            Some(at) => {
                if let Some(group) = report.get_mut(at) {
                    group.dropped.push(tunnel.name);
                }
            }
            // First of its kind: this is the one that survives, and it opens the
            // group later duplicates point at.
            None => {
                groups.push((identity, report.len()));
                report.push(Folded { kept: tunnel.name.clone(), dropped: Vec::new() });
                kept.push(tunnel);
            }
        }
    }
    // A group only earns a line in the report by losing something.
    report.retain(|group| !group.dropped.is_empty());
    (kept, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ForwardKind, TunnelConfig};

    fn t(id: u64, name: &str, local_port: u16, remote_port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "cloud_pc".into(),
            local_addr: "127.0.0.1".into(),
            local_port,
            remote_host: "127.0.0.1".into(),
            remote_port,
            enabled: true,
            auto_restart: true,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    #[test]
    fn a_clean_config_is_returned_untouched_and_without_a_report() {
        // The normal case must not report anything: "repaired 0 entries" on every
        // start-up would train the user to ignore the message.
        let list = vec![t(1, "a", 18789, 18789), t(2, "b", 44897, 44897)];
        let (kept, report) = fold_duplicates(list.clone());
        assert_eq!(kept, list);
        assert!(report.is_empty());
    }

    #[test]
    fn the_same_target_on_a_bumped_local_port_is_one_forward() {
        // The reported shape: `cloud_pc`'s 18789 forward exists three times
        // because each import bumped the local port. Local ports differ, the
        // intent does not.
        let list = vec![
            t(3, "cloud_pc-L-18789-2", 18789, 18789),
            t(5, "cloud_pc-L-18789", 18790, 18789),
            t(7, "cloud_pc-L-18789-3", 18791, 18789),
        ];
        let (kept, report) = fold_duplicates(list);
        assert_eq!(kept.len(), 1, "one intent, one entry: {kept:?}");
        assert_eq!(kept[0].name, "cloud_pc-L-18789-2", "the first row survives");
        assert_eq!(kept[0].local_port, 18789, "the survivor keeps its persisted port");
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].kept, "cloud_pc-L-18789-2");
        assert_eq!(report[0].dropped, vec!["cloud_pc-L-18789", "cloud_pc-L-18789-3"]);
    }

    #[test]
    fn distinct_targets_and_hosts_are_never_folded() {
        let mut other_host = t(4, "dev-L-18789", 18789, 18789);
        other_host.ssh_host = "dev-box".into();
        let mut other_target = t(5, "cloud_pc-L-8080", 8080, 80);
        other_target.remote_port = 80;
        let list = vec![t(1, "cloud_pc-L-18789", 18789, 18789), other_host, other_target];
        let (kept, report) = fold_duplicates(list);
        assert_eq!(kept.len(), 3, "a different host or target is a different forward");
        assert!(report.is_empty());
    }

    #[test]
    fn each_group_is_reported_once_and_keeps_the_original_order() {
        // Two independent duplicate groups interleaved in the file: both must fold,
        // and the survivors must stay in the order the user sees them.
        let list = vec![
            t(1, "a", 18789, 18789),
            t(2, "b", 44897, 44897),
            t(3, "a-2", 18790, 18789),
            t(4, "b-2", 44898, 44897),
        ];
        let (kept, report) = fold_duplicates(list);
        let names: Vec<&str> = kept.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "survivors keep file order");
        assert_eq!(report.len(), 2);
        assert_eq!(report[0].dropped, vec!["a-2"]);
        assert_eq!(report[1].dropped, vec!["b-2"]);
    }

    #[test]
    fn a_remote_and_a_local_forward_on_the_same_numbers_stay_separate() {
        // `-R` and `-L` with one set of numbers are different forwards: their
        // identities start with a different tag and their ends mean opposite
        // things. Folding them would delete a real tunnel.
        let mut remote = t(2, "r", 2222, 7000);
        remote.kind = ForwardKind::Remote;
        let local = t(1, "l", 2222, 7000);
        let (kept, report) = fold_duplicates(vec![local, remote]);
        assert_eq!(kept.len(), 2);
        assert!(report.is_empty());
    }

    #[test]
    fn a_single_entry_is_never_folded_and_never_reported() {
        let list = vec![t(1, "only", 18789, 18789)];
        let (kept, report) = fold_duplicates(list.clone());
        assert_eq!(kept, list);
        assert!(report.is_empty());
    }

    #[test]
    fn an_empty_config_folds_to_nothing() {
        let (kept, report) = fold_duplicates(Vec::new());
        assert!(kept.is_empty());
        assert!(report.is_empty());
    }
}

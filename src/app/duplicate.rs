//! Finding an already-saved forward that means the same thing as a candidate.
//!
//! The batch paths (`import`, `port_batch`) can propose a forward the user
//! already has. Re-importing `LocalForward 18789 127.0.0.1:18789` while a tunnel
//! already forwards that target created a second entry for one intent: the local
//! port the planner hands out is an environment detail, and a busy port is bumped
//! upwards. This module answers the question those paths ask before creating
//! anything: *is this the same forward as one we already have?*
//!
//! Identity comes from [`TunnelConfig::forward_identity`] — a pure data
//! comparison, not a socket check. A **listen-port conflict** (two forwards
//! fighting over one bind address) is a different question with a different
//! answer; it lives in `TunnelConfig::listen_endpoint` and is checked separately, so
//! both keep working.
//!
//! # Why the local listen port is not part of `-L`'s identity
//!
//! The local listen port is a **derived value the environment may rewrite**:
//! `plan_local_port` scans upwards when the natural port is busy, and that bumped
//! number is what gets persisted. So `config.toml` may hold 18790 for a directive
//! that asked for 18789, and the original request is not recoverable afterwards.
//! Any test that compares a saved local port against a requested one therefore
//! misses the real-world case where the bump happened on an *earlier* run — which
//! is exactly the duplicate this feature exists to stop. Only user intent takes
//! part in the identity, and the local listen port is not intent.
//!
//! `-R` and `-D` are different: for `-R` both ends are intent (the peer-side
//! listener *and* our local target), and for `-D` the local listener is the whole
//! forward. Their local ports stay in the identity.
//!
//! # Known boundary
//!
//! Because `-L`'s local port is excluded, `LocalForward 8080 host:80` and
//! `LocalForward 9090 host:80` are judged the *same* forward: same SSH host, same
//! remote target, so only one is kept. That is deliberate. Two local listeners
//! for one remote target are redundant, and the opposite error — missing a real
//! duplicate — leaves the user unable to get rid of the duplicate at all.
//!
//! Belongs here: the two lookups over saved tunnels and over a batch being built.
//! Does not belong: building the configs (`import`, `port_batch`), the definition
//! of identity (`model::tunnel`), rendering, key handling.

use crate::app::App;
use crate::model::TunnelConfig;

impl App {
    /// The name of an already-saved tunnel with the same forward identity.
    ///
    /// `cfg.id` itself is skipped, so editing a tunnel and re-submitting it does
    /// not collide with its own stored definition. Returns the *existing* name so
    /// the caller can tell the user which row was already there.
    pub fn duplicate_of(&self, cfg: &TunnelConfig) -> Option<String> {
        let identity = cfg.forward_identity();
        self.cfg
            .tunnels
            .iter()
            .filter(|t| t.id != cfg.id)
            .find(|t| t.forward_identity() == identity)
            .map(|t| t.name.clone())
    }

    /// The first name colliding with `cfg`: a saved tunnel, else `batch`.
    ///
    /// Two rows of one batch can describe one forward (`Host *` repeating a
    /// block). The first accepted row is kept and later ones are reported as
    /// duplicates of it, so the answer names whichever row wins.
    pub(crate) fn duplicate_in_batch(&self, cfg: &TunnelConfig, batch: &[TunnelConfig]) -> Option<String> {
        self.duplicate_of(cfg).or_else(|| {
            let identity = cfg.forward_identity();
            batch.iter().find(|other| other.forward_identity() == identity).map(|other| other.name.clone())
        })
    }

    /// The name of a saved tunnel that already *is* `forward`, without planning
    /// anything.
    ///
    /// The import pane asks this **before** it decides what to pre-tick. Two
    /// reasons it must not go through [`Self::duplicate_of`]:
    ///
    /// 1. That function takes a built `TunnelConfig`, and building one plans a
    ///    local port — which probes sockets. The pane is a *list*; a question asked
    ///    once per row while drawing would make the answer depend on what is
    ///    listening at that instant, so a busy port could hide an existing forward.
    /// 2. The pane asks about every row of a host, including ones it is about to
    ///    leave unticked. Answering from intent alone keeps that cheap and
    ///    side-effect free.
    ///
    /// Identity comes from [`SshForward::forward_identity`], which is the same
    /// recipe as [`TunnelConfig::forward_identity`] — the two cannot drift.
    pub(crate) fn saved_forward_name(&self, forward: &crate::model::SshForward) -> Option<String> {
        let identity = forward.forward_identity(&self.cfg.defaults.bind);
        self.cfg.tunnels.iter().find(|t| t.forward_identity() == identity).map(|t| t.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::ForwardKind;

    fn app_with(tunnels: Vec<TunnelConfig>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile { tunnels, ..ConfigFile::default() }).unwrap();
        (dir, App::new(paths))
    }

    fn cfg(id: crate::model::TunnelId, name: &str, kind: ForwardKind) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind,
            ssh_host: "cloud_pc".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 18789,
            remote_host: "127.0.0.1".into(),
            remote_port: 18789,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    #[test]
    fn a_persisted_bumped_local_port_is_still_the_same_forward() {
        // The real-world shape: the saved tunnel's local port was bumped when it
        // was created (18789 was busy), and the bumped value is what got persisted.
        // Re-importing the same directive asks for 18789 again. Only the identity
        // can recognise this, because the original request is gone.
        let mut saved = cfg(1, "cloud_pc-L-18789", ForwardKind::Local);
        saved.local_port = 18790;
        let (_d, app) = app_with(vec![saved]);

        let candidate = cfg(2, "cloud_pc-L-18789", ForwardKind::Local); // requests 18789
        assert_eq!(
            app.duplicate_of(&candidate),
            Some("cloud_pc-L-18789".into()),
            "a local port bumped on an earlier run must not hide the duplicate"
        );
    }

    #[test]
    fn an_equal_local_port_is_of_course_still_the_same_forward() {
        let (_d, app) = app_with(vec![cfg(1, "cloud_pc-L-18789", ForwardKind::Local)]);
        let candidate = cfg(2, "cloud_pc-L-18789-2", ForwardKind::Local);
        assert_eq!(app.duplicate_of(&candidate), Some("cloud_pc-L-18789".into()));
    }

    #[test]
    fn two_listen_ports_on_one_target_collapse_into_one_forward() {
        // Deliberate trade-off, documented as a known boundary in the module docs:
        // `-L`'s local listen port is not identity, so two declarations that differ
        // only there are judged redundant. Missing a real duplicate is the worse
        // error — it leaves the user unable to clear the duplicate row.
        let mut saved = cfg(1, "saved", ForwardKind::Local);
        saved.local_port = 8080;
        saved.remote_host = "10.0.0.1".into();
        saved.remote_port = 80;
        let (_d, app) = app_with(vec![saved]);

        let mut other_listener = cfg(2, "other", ForwardKind::Local);
        other_listener.local_port = 9090;
        other_listener.remote_host = "10.0.0.1".into();
        other_listener.remote_port = 80;
        assert_eq!(
            app.duplicate_of(&other_listener),
            Some("saved".into()),
            "one target, one forward: the second local listener is redundant"
        );
    }

    #[test]
    fn a_different_remote_end_or_host_or_local_addr_is_not_a_duplicate() {
        let (_d, app) = app_with(vec![cfg(1, "kept", ForwardKind::Local)]);

        let mut remote_port = cfg(2, "x", ForwardKind::Local);
        remote_port.remote_port = 22;
        assert_eq!(app.duplicate_of(&remote_port), None);

        let mut remote_host = cfg(3, "x", ForwardKind::Local);
        remote_host.remote_host = "10.9.9.9".into();
        assert_eq!(app.duplicate_of(&remote_host), None);

        let mut host = cfg(4, "x", ForwardKind::Local);
        host.ssh_host = "dev-box".into();
        assert_eq!(app.duplicate_of(&host), None);

        // `local_addr` is still identity: binding a different address is a
        // different listening intent, even for the same target.
        let mut addr = cfg(5, "x", ForwardKind::Local);
        addr.local_addr = "0.0.0.0".into();
        assert_eq!(app.duplicate_of(&addr), None);

        // A different kind describing the same numbers is a different forward.
        let remote_kind = cfg(6, "x", ForwardKind::Remote);
        assert_eq!(app.duplicate_of(&remote_kind), None);
    }

    #[test]
    fn a_tunnel_never_duplicates_itself() {
        let (_d, app) = app_with(vec![cfg(1, "only", ForwardKind::Local)]);
        // Editing the stored tunnel: same identity, same id — not a collision.
        let mut edited = cfg(1, "only", ForwardKind::Local);
        edited.name = "renamed".into();
        assert_eq!(app.duplicate_of(&edited), None);
    }

    #[test]
    fn a_duplicate_inside_the_batch_names_the_first_row() {
        let (_d, app) = app_with(vec![]);
        let first = cfg(10, "first", ForwardKind::Local);
        let mut second = cfg(11, "second", ForwardKind::Local);
        second.local_port = 19000; // bumped by the planner
        let batch = vec![first];
        assert_eq!(app.duplicate_in_batch(&second, &batch), Some("first".into()));
        // A third distinct forward finds nothing.
        let mut third = cfg(12, "third", ForwardKind::Local);
        third.remote_port = 8080;
        assert_eq!(app.duplicate_in_batch(&third, &batch), None);
    }

    #[test]
    fn a_saved_tunnel_wins_over_the_batch() {
        let (_d, app) = app_with(vec![cfg(1, "saved", ForwardKind::Local)]);
        let batch = vec![cfg(10, "batch-row", ForwardKind::Local)];
        let candidate = cfg(11, "candidate", ForwardKind::Local);
        assert_eq!(
            app.duplicate_in_batch(&candidate, &batch),
            Some("saved".into()),
            "the already-saved tunnel is the more useful name to report"
        );
    }

    #[test]
    fn an_empty_batch_never_matches() {
        let (_d, app) = app_with(vec![cfg(1, "saved", ForwardKind::Local)]);
        let mut fresh = cfg(2, "fresh", ForwardKind::Local);
        fresh.remote_port = 9999;
        assert_eq!(app.duplicate_in_batch(&fresh, &[]), None);
    }

    #[test]
    fn dynamic_and_remote_identity_already_carries_the_local_side() {
        // `-D` has no target and a local listener that is the whole intent.
        let dynamic = cfg(1, "socks", ForwardKind::Dynamic);
        let (_d, app) = app_with(vec![dynamic]);
        let mut other_listener = cfg(2, "socks2", ForwardKind::Dynamic);
        other_listener.local_port = 1081;
        assert_eq!(app.duplicate_of(&other_listener), None, "a different SOCKS port is a different listener");
        let same = cfg(3, "socks-again", ForwardKind::Dynamic);
        assert_eq!(app.duplicate_of(&same), Some("socks".into()));

        // `-R`: both ends are intent, and the local side is in the identity.
        let remote = cfg(4, "r", ForwardKind::Remote);
        let (_d2, app2) = app_with(vec![remote]);
        let mut other_target = cfg(5, "r2", ForwardKind::Remote);
        other_target.local_port = 2222;
        assert_eq!(app2.duplicate_of(&other_target), None, "a different local target is a different forward");
    }
}

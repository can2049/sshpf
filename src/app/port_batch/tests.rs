//! Unit tests for `port_batch.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::config::{ConfigFile, Paths};
use crate::model::{PortPick, RemotePort, TunnelStatus};

fn app_fresh() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
    (dir, App::new(paths))
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// A create form with a host and a name, plus a picker holding `ports`.
fn app_with_ports(ports: &[u16]) -> (tempfile::TempDir, App) {
    let (dir, mut app) = app_fresh();
    app.open_form_create();
    {
        let f = app.form.as_mut().unwrap();
        f.ssh_host.set("dev-box");
        f.name.set("svc");
        f.remote_host.set("10.0.0.5");
    }
    let mut pick = PortPick::scanning("dev-box", true, 1);
    let rows = ports.iter().map(|p| RemotePort::new(*p, "127.0.0.1")).collect::<Vec<_>>();
    assert!(pick.install(1, rows, 0));
    app.ports = Some(pick);
    (dir, app)
}

#[test]
fn one_tunnel_per_ticked_port_with_unique_names_and_ports() {
    let (p1, p2, p3) = (free_port(), free_port(), free_port());
    let (_d, mut app) = app_with_ports(&[p1, p2, p3]);
    app.ports.as_mut().unwrap().check_all();
    let configs = app.validated_port_batch().expect("the batch should validate");
    assert_eq!(configs.len(), 3);
    // `checked_ports` sorts, so the batch order is deterministic rather than
    // "whatever order the scan happened to report".
    let mut expected = vec![p1, p2, p3];
    expected.sort_unstable();
    assert_eq!(configs.iter().map(|c| c.remote_port).collect::<Vec<_>>(), expected);
    let names: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
    let unique: std::collections::HashSet<&&str> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "names must be unique: {names:?}");
    assert!(names.iter().all(|n| n.contains("svc")), "each name keeps the typed base: {names:?}");
    assert!(names.iter().all(|n| *n != "svc"), "a batch disambiguates with the port: {names:?}");
}

#[test]
fn a_batch_lands_on_the_ports_the_peer_reports() {
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_ports(&[p1, p2]);
    app.ports.as_mut().unwrap().check_all();
    let configs = app.validated_port_batch().unwrap();
    // A free remote port is reused locally: "same number on both ends" is what
    // people expect, and it is what the manual form does.
    for cfg in &configs {
        assert_eq!(cfg.local_port, cfg.remote_port, "a free port must be reused verbatim");
    }
    assert!(configs.iter().all(|c| c.kind == ForwardKind::Local), "a scan is the -L direction");
    assert!(configs.iter().all(|c| c.remote_host == "10.0.0.5"), "the typed target address is kept");
}

#[test]
fn two_rows_never_share_a_local_port() {
    // Both ticked ports are the same number at the remote end is impossible
    // (they are unique), but the *local* ports can collide with a managed
    // tunnel — the second row must move up instead of failing at bind time.
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = busy.local_addr().unwrap().port();
    let (_d, mut app) = app_with_ports(&[taken]);
    app.ports.as_mut().unwrap().check_all();
    let configs = app.validated_port_batch().unwrap();
    assert_ne!(configs[0].local_port, taken, "a busy local port must be steered away from");
    assert!(configs[0].local_port > taken);
    drop(busy);
}

#[test]
fn the_batch_is_exposed_only_while_the_pane_is_visible() {
    // Collapsing the list to type by hand must hand the submit back to the
    // single form: a batch nobody can see is a batch nobody asked for.
    let (_d, mut app) = app_with_ports(&[free_port()]);
    app.ports.as_mut().unwrap().check_all();
    assert!(app.port_batch_intent());
    app.ports.as_mut().unwrap().open = false;
    assert!(!app.port_batch_intent());
}

#[test]
fn an_edit_never_batches() {
    // Editing one tunnel must stay one tunnel; silently creating N would be a
    // data-loss bug.
    let (_d, mut app) = app_fresh();
    {
        let cfg = TunnelConfig {
            id: 1,
            name: "existing".into(),
            kind: ForwardKind::Local,
            ssh_host: "dev-box".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 18000,
            remote_host: "10.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        };
        app.cfg.tunnels.push(cfg.clone());
        app.tunnels.push(TunnelView::new(cfg));
    }
    app.open_form_edit();
    let mut pick = PortPick::scanning("dev-box", false, 1);
    assert!(pick.install(1, vec![RemotePort::new(free_port(), "127.0.0.1")], 0));
    app.ports = Some(pick);
    assert!(!app.port_batch_intent(), "editing is one tunnel by definition");
    assert!(app.validated_port_batch().is_none());
}

#[test]
fn a_read_only_picker_produces_no_batch() {
    let (_d, mut app) = app_with_ports(&[free_port()]);
    app.ports.as_mut().unwrap().multi = false;
    app.ports.as_mut().unwrap().check_all();
    assert_eq!(app.ports.as_ref().unwrap().checked_count(), 0, "a read-only list cannot be ticked");
    assert!(!app.port_batch_intent());
}

#[test]
fn an_empty_tick_selection_produces_no_batch() {
    let (_d, mut app) = app_with_ports(&[free_port()]);
    assert!(!app.port_batch_intent(), "a fresh scan ticks nothing");
    assert!(app.validated_port_batch().is_none());
}

#[test]
fn committing_creates_persists_and_starts_every_tunnel() {
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_ports(&[p1, p2]);
    app.ports.as_mut().unwrap().check_all();
    let configs = app.validated_port_batch().unwrap();
    app.commit_port_batch(configs);
    assert_eq!(app.tunnels.len(), 2);
    assert!(app.form.is_none(), "the form closes on success");
    assert!(app.ports.is_none(), "and the picker with it");
    let reloaded = crate::config::load(&app.paths.config).unwrap();
    assert_eq!(reloaded.tunnels.len(), 2, "the batch must be persisted, not just in memory");
    assert!(reloaded.tunnels.iter().all(|t| t.enabled));
    assert_eq!(app.selected_id(), reloaded.tunnels.iter().map(|t| t.id).min(), "cursor lands on the first");
    assert!(!matches!(app.tunnels[0].status, TunnelStatus::Stopped), "creating should attempt to run the tunnels");
}

#[test]
fn a_missing_host_is_reported_not_created() {
    let (_d, mut app) = app_with_ports(&[free_port()]);
    app.form.as_mut().unwrap().ssh_host.set("   ");
    app.ports.as_mut().unwrap().check_all();
    assert!(app.validated_port_batch().is_none());
    let errors = app.form.as_ref().unwrap().errors.clone();
    assert!(errors.iter().any(|e| e.contains("SSH 主机")), "{errors:?}");
}

#[test]
fn the_note_records_that_the_port_came_from_a_scan() {
    let (_d, mut app) = app_with_ports(&[free_port()]);
    app.ports.as_mut().unwrap().check_all();
    let cfg = app.validated_port_batch().unwrap().remove(0);
    assert!(cfg.note.contains("扫描"), "the note should say where the port came from: {}", cfg.note);
}

#[test]
fn a_name_is_derived_when_the_form_left_it_empty() {
    let port = free_port();
    let (_d, mut app) = app_with_ports(&[port]);
    app.form.as_mut().unwrap().name.set("");
    app.ports.as_mut().unwrap().check_all();
    let cfg = app.validated_port_batch().unwrap().remove(0);
    assert!(cfg.name.contains("dev-box"), "the host stands in for a missing name: {}", cfg.name);
    assert!(cfg.name.contains(&port.to_string()), "and the port makes it unique: {}", cfg.name);
}

#[test]
fn committing_an_empty_vec_is_a_no_op() {
    let (_d, mut app) = app_fresh();
    app.commit_port_batch(Vec::new());
    assert!(app.tunnels.is_empty());
    assert_eq!(app.selected, 0);
}

#[test]
fn unique_within_avoids_repeats() {
    assert_eq!(unique_within("a", vec![]), "a");
    assert_eq!(unique_within("a", vec!["a".into()]), "a-2");
    assert_eq!(unique_within("a", vec!["a".into(), "a-2".into()]), "a-3");
}

mod duplicate_tests {
    use crate::app::port_batch::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{PortPick, RemotePort, TunnelStatus};

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// A saved `-L` tunnel from `local_port` to `remote_host:remote_port`.
    fn saved(
        id: crate::model::TunnelId,
        name: &str,
        local_port: u16,
        remote_host: &str,
        remote_port: u16,
    ) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "dev-box".into(),
            local_addr: "127.0.0.1".into(),
            local_port,
            remote_host: remote_host.into(),
            remote_port,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    /// A create form plus a multi-row picker holding `ports`, with `tunnels`
    /// already saved.
    fn app_with(tunnels: Vec<TunnelConfig>, ports: &[u16]) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile { tunnels, ..ConfigFile::default() }).unwrap();
        let mut app = App::new(paths);
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("dev-box");
            f.name.set("svc");
            f.remote_host.set("10.0.0.5");
        }
        let mut pick = PortPick::scanning("dev-box", true, 1);
        let rows = ports.iter().map(|p| RemotePort::new(*p, "127.0.0.1")).collect::<Vec<_>>();
        assert!(pick.install(1, rows, 0));
        pick.check_all();
        app.ports = Some(pick);
        (dir, app)
    }

    #[test]
    fn a_ticked_port_that_is_already_saved_is_skipped() {
        // The scan proposes 8080 -> 10.0.0.5:8080, which is exactly what is
        // already saved. Re-creating it would duplicate the forward (possibly
        // with a bumped local port), so it must be skipped, not created.
        let port = free_port();
        let (_d, mut app) = app_with(vec![saved(1, "svc-8080", port, "10.0.0.5", port)], &[port]);

        let batch = app.validated_port_batch_outcome().expect("a batch exists");
        assert!(batch.configs.is_empty(), "the duplicate must not be planned for creation");
        assert_eq!(batch.skipped.len(), 1, "and it must be reported: {:?}", batch.skipped);
    }

    #[test]
    fn one_duplicate_port_and_one_new_port_create_only_the_new_one() {
        let dup = free_port();
        let new = free_port();
        let (_d, mut app) = app_with(vec![saved(1, "svc-old", dup, "10.0.0.5", dup)], &[dup, new]);

        let configs = app.validated_port_batch().expect("the new port still validates");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].remote_port, new);
        assert_eq!(app.pending_port_skipped.len(), 1);

        app.commit_port_batch(configs);
        assert_eq!(app.tunnels.len(), 2, "the saved tunnel plus the one new forward");
        let status = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        assert!(status.contains("跳过") || status.contains("skip"), "the status must explain the gap: {status:?}");
    }

    #[test]
    fn a_persisted_bumped_local_port_is_still_the_same_forward() {
        // The saved tunnel's local port was bumped to `bumped` on an earlier run
        // (that bumped value is what got persisted); the scan proposes `port`
        // again. The local port is a derived value, so this is one forward, not
        // two — the regression anchor for the real-world duplicate.
        let port = free_port();
        let bumped = port.wrapping_add(1).max(1024);
        let (_d, mut app) = app_with(vec![saved(1, "svc-old", bumped, "10.0.0.5", port)], &[port]);
        let batch = app.validated_port_batch_outcome().expect("batch");
        assert!(batch.configs.is_empty(), "the scanned forward is already saved under a bumped port");
        assert_eq!(batch.skipped.len(), 1);
    }

    #[test]
    fn a_different_target_host_is_a_different_forward() {
        let port = free_port();
        let (_d, mut app) = app_with(vec![saved(1, "svc-other", port, "10.0.0.9", port)], &[port]);
        let batch = app.validated_port_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1, "the scanned target is another address");
        assert!(batch.skipped.is_empty());
    }

    #[test]
    fn a_different_ssh_host_is_a_different_forward() {
        let port = free_port();
        let mut other = saved(1, "other-host", port, "10.0.0.5", port);
        other.ssh_host = "another-box".into();
        let (_d, mut app) = app_with(vec![other], &[port]);
        let batch = app.validated_port_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1);
        assert!(batch.skipped.is_empty());
    }

    #[test]
    fn an_all_duplicate_selection_closes_the_form_instead_of_complaining() {
        // "No port is ticked" describes a field the user did not leave empty; the
        // honest answer is "these already exist".
        let port = free_port();
        let (_d, mut app) = app_with(vec![saved(1, "svc-8080", port, "10.0.0.5", port)], &[port]);
        assert!(app.port_batch_intent(), "the pane is visible with one tick");
        assert!(app.validated_port_batch().is_none());
        assert!(app.form.is_none(), "the form closes: there was nothing wrong to fix");
        let status = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        assert!(status.contains("已存在") || status.contains("already exist"), "{status:?}");
        assert!(app.tunnels.len() == 1, "and nothing was created");
    }

    #[test]
    fn committing_a_fresh_batch_still_works_unchanged() {
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with(vec![], &[p1, p2]);
        let configs = app.validated_port_batch().expect("batch");
        assert_eq!(configs.len(), 2);
        assert!(app.pending_port_skipped.is_empty(), "nothing to report when nothing was skipped");
        app.commit_port_batch(configs);
        assert_eq!(app.tunnels.len(), 2);
        assert!(!matches!(app.tunnels[0].status, TunnelStatus::Stopped));
    }
}

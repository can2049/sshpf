//! Unit tests for group browsing, working-state switching and deletion.
#![cfg(test)]

use super::*;
use crate::config::{ConfigFile, Paths};
use crate::model::{ForwardKind, GroupConfig, SshForward, TunnelConfig, TunnelStatus};
use std::time::Instant;

fn tunnel(id: crate::model::TunnelId, group: crate::model::GroupId, enabled: bool) -> TunnelConfig {
    TunnelConfig {
        id,
        group,
        name: format!("t{id}"),
        kind: ForwardKind::Local,
        ssh_host: "h".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 18000 + id as u16,
        remote_host: "127.0.0.1".into(),
        remote_port: 22,
        enabled,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    }
}

fn app_with(groups: Vec<GroupConfig>, active: Option<u64>, tunnels: Vec<TunnelConfig>) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let cfg = ConfigFile { groups, active_group: active, tunnels, ..ConfigFile::default() };
    crate::config::save(&paths.config, &cfg).unwrap();
    (dir, App::new(paths))
}

fn two_groups() -> (tempfile::TempDir, App) {
    app_with(
        vec![GroupConfig::new(1, "work"), GroupConfig::new(2, "home")],
        Some(1),
        vec![tunnel(1, 1, false), tunnel(2, 2, false)],
    )
}

#[test]
fn browsing_changes_only_the_visible_rows() {
    let (_d, mut app) = two_groups();
    assert_eq!(app.browsed_group, 1);
    assert_eq!(app.visible_ids(), vec![1]);
    app.browse_group(1);
    assert_eq!(app.browsed_group, 2);
    assert_eq!(app.visible_ids(), vec![2], "the table follows the browsed group");
    assert_eq!(app.active_group, Some(1), "browsing never changes the working group");
    // The cursor is reset rather than left pointing at a row that moved away.
    assert_eq!(app.selected, 0);
}

#[test]
fn an_idle_group_refuses_runtime_actions_with_a_hint() {
    let (_d, mut app) = two_groups();
    app.browse_group(1); // group 2, idle
    app.toggle_selected();
    assert!(matches!(app.tunnels[1].status, TunnelStatus::Stopped), "nothing may start");
    let text = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
    assert!(text.contains("未在工作") && text.contains("w"), "the hint must name w: {text}");

    app.restart_selected();
    assert!(matches!(app.tunnels[1].status, TunnelStatus::Stopped));
}

#[test]
fn working_a_group_keeps_the_old_groups_enabled_intent() {
    let (_d, mut app) = two_groups();
    // Pretend group 1 is running so switching has something to stop.
    app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
    app.tunnels[0].cfg.enabled = true;
    app.cfg.find_mut(1).unwrap().enabled = true;

    app.activate_group(2);

    assert_eq!(app.active_group, Some(2));
    assert_eq!(app.cfg.active_group, Some(2));
    assert!(matches!(app.tunnels[0].status, TunnelStatus::Stopped), "the old group stops");
    assert!(app.cfg.find(1).unwrap().enabled, "its enabled intent is kept for next time");
}

#[test]
fn switching_starts_the_new_groups_enabled_forwards() {
    let (_d, mut app) = app_with(
        vec![GroupConfig::new(1, "work"), GroupConfig::new(2, "home")],
        Some(1),
        vec![tunnel(1, 1, true), tunnel(2, 2, true)],
    );
    app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
    // Avoid a real connection attempt for the new group: validation fails first.
    app.tunnels[1].cfg.ssh_host.clear();
    app.cfg.find_mut(2).unwrap().ssh_host.clear();

    app.activate_group(2);

    assert!(matches!(app.tunnels[0].status, TunnelStatus::Stopped), "the old group stops");
    assert!(
        matches!(app.tunnels[1].status, TunnelStatus::Failed { .. }),
        "the new group's enabled forward is attempted: {:?}",
        app.tunnels[1].status
    );
}

#[test]
fn deactivating_leaves_no_working_group() {
    let (_d, mut app) = two_groups();
    app.deactivate_group();
    assert_eq!(app.active_group, None);
    assert_eq!(app.cfg.active_group, None);
}

#[test]
fn only_the_active_groups_enabled_tunnels_are_restored() {
    let (_d, mut app) = app_with(
        vec![GroupConfig::new(1, "work"), GroupConfig::new(2, "home")],
        Some(1),
        vec![tunnel(1, 1, true), tunnel(2, 2, true)],
    );
    // Make the active group's row fail validation, so restore does not dial
    // anything; the point is which group was attempted at all.
    app.tunnels[0].cfg.ssh_host.clear();
    app.cfg.find_mut(1).unwrap().ssh_host.clear();
    app.restore_enabled();
    assert!(matches!(app.tunnels[0].status, TunnelStatus::Failed { .. }), "the active group is attempted");
    assert!(matches!(app.tunnels[1].status, TunnelStatus::Stopped), "the idle group is not");
}

#[test]
fn creating_and_renaming_a_group_persists_and_browses_the_new_one() {
    let (_d, mut app) = two_groups();
    app.open_group_create();
    app.group_prompt.as_mut().unwrap().input.set("new");
    app.submit_group_prompt();
    assert_eq!(app.cfg.groups.len(), 3);
    let id = app.browsed_group;
    assert_eq!(app.cfg.group(id).unwrap().alias(), "new");
    assert_eq!(app.cfg.group(id).unwrap().name, "new");

    app.open_group_rename();
    app.group_prompt.as_mut().unwrap().input.set("renamed");
    app.submit_group_prompt();
    assert_eq!(app.cfg.group(id).unwrap().alias(), "renamed");
    // Persisted, not just in memory.
    let back = crate::config::load(&app.paths.config).unwrap();
    assert_eq!(back.group(id).unwrap().alias(), "renamed");
}

#[test]
fn renaming_uniquifies_aliases_and_allows_empty() {
    let (_d, mut app) = two_groups();
    app.browse_group(1); // home (id 2)
    app.open_group_rename();
    app.group_prompt.as_mut().unwrap().input.set("work");
    app.submit_group_prompt();
    assert_eq!(app.cfg.group(2).unwrap().alias(), "work-2", "a duplicate alias gets a suffix");

    app.open_group_rename();
    app.group_prompt.as_mut().unwrap().input.set("   ");
    app.submit_group_prompt();
    assert_eq!(app.cfg.group(2).unwrap().alias(), "#2", "empty falls back to the id");
}

#[test]
fn deleting_a_group_removes_its_tunnels_and_never_the_last_group() {
    let (_d, mut app) = app_with(
        vec![GroupConfig::new(1, "work"), GroupConfig::new(2, "home")],
        Some(2),
        vec![tunnel(1, 1, false), tunnel(2, 2, false)],
    );
    app.browse_to(2);
    app.delete_group(2);
    assert_eq!(app.cfg.groups.len(), 1);
    assert!(app.cfg.tunnels.iter().all(|t| t.group == 1));
    assert!(app.tunnels.iter().all(|t| t.cfg.group == 1));
    assert_eq!(app.active_group, None, "deleting the working group leaves none working");
    assert_eq!(app.browsed_group, 1);

    app.request_delete_group();
    assert_eq!(app.cfg.groups.len(), 1, "the last group is never deleted");
}

#[test]
fn foreign_group_ports_exclude_the_browsed_group() {
    let (_d, mut app) = two_groups();
    assert_eq!(app.foreign_group_local_ports(), vec![18002], "group 2's port is foreign while browsing 1");
    app.browse_to(2);
    assert_eq!(app.foreign_group_local_ports(), vec![18001], "and the other way around");
}

#[test]
fn importing_into_a_group_can_reuse_a_port_held_by_another_group() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a local listener");
    let port = listener.local_addr().expect("bound address").port();
    let (_d, mut app) =
        app_with(vec![GroupConfig::new(1, "work"), GroupConfig::new(2, "home")], Some(1), vec![tunnel(1, 1, true)]);
    app.tunnels[0].cfg.local_port = port;
    app.cfg.find_mut(1).unwrap().local_port = port;
    app.browse_to(2);

    let forward = SshForward {
        kind: ForwardKind::Local,
        listen_addr: String::new(),
        listen_port: port,
        target_host: "127.0.0.1".into(),
        target_port: port,
        host_alias: "h".into(),
        line: 1,
        raw: String::new(),
    };
    let cfg = app.config_from_forward(9, &forward, &[]);
    assert_eq!(cfg.group, 2);
    assert_eq!(cfg.local_port, port, "the other group's live port must not shift this import");
}

#[test]
fn a_forward_may_repeat_in_another_group() {
    let (_d, mut app) =
        app_with(vec![GroupConfig::new(1, "work"), GroupConfig::new(2, "home")], Some(1), vec![tunnel(1, 1, false)]);
    app.browse_to(2);
    let candidate = tunnel(3, 2, false);
    assert_eq!(app.duplicate_of(&candidate), None, "group 1's identical forward is not a duplicate here");

    // The same intent in group 2 *is* a duplicate there.
    app.cfg.tunnels.push(tunnel(2, 2, false));
    assert_eq!(app.duplicate_of(&candidate), Some("t2".into()));
}

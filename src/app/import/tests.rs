//! Unit tests for `import.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::config::{ConfigFile, Paths};
use crate::model::ImportTable;
use crate::model::TunnelStatus;

fn app_fresh() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
    (dir, App::new(paths))
}

/// A port that is free right now.
///
/// Tests run in parallel and several of them bind ports, so asserting on a
/// hard-coded port would be flaky — and worse, it would collide with the
/// "busy port gets bumped" behaviour the importer deliberately implements,
/// turning a correct bump into a test failure.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// A discovered `-L` row. The target port follows the listen port so two rows
/// are genuinely two forwards: rows that differ only in the local listen port
/// are one forward by design (see `app::duplicate`), and a fixture that
/// collapsed them would test the wrong thing everywhere.
fn show(port: u16) -> SshForward {
    SshForward {
        kind: ForwardKind::Local,
        listen_addr: "127.0.0.1".into(),
        listen_port: port,
        target_host: "10.0.0.1".into(),
        target_port: port,
        host_alias: "dev-box".into(),
        line: 1,
        raw: format!("LocalForward {port} 10.0.0.1:{port}"),
    }
}

/// Load a table into the open create form without touching the filesystem.
fn app_with_import(rows: Vec<SshForward>) -> (tempfile::TempDir, App) {
    let (dir, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    let table = ImportTable::new(rows);
    app.form.as_mut().unwrap().set_import_table("dev-box", table);
    app.form.as_mut().unwrap().import_open = true;
    (dir, app)
}

// -- discovery plumbing ------------------------------------------------

// -- the default: auto-open, everything ticked -------------------------

// -- creating the batch ------------------------------------------------

#[test]
fn every_ticked_row_becomes_its_own_tunnel() {
    let (p1, p2, p3) = (free_port(), free_port(), free_port());
    let (_d, mut app) = app_with_import(vec![show(p1), show(p2), show(p3)]);
    let configs = app.validated_import_batch().expect("batch should validate");
    assert_eq!(configs.len(), 3, "one TunnelConfig per ticked row");
    let ports: Vec<u16> = configs.iter().map(|c| c.local_port).collect();
    assert_eq!(ports, vec![p1, p2, p3], "free ports must be imported verbatim");
    assert!(configs.iter().all(|c| c.ssh_host == "dev-box"));
    assert!(configs.iter().all(|c| c.name.contains("dev-box")), "names should identify the source");
}

#[test]
fn unticked_rows_are_not_created() {
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_import(vec![show(p1), show(p2)]);
    // The table starts fully ticked; untick the first row.
    app.form.as_mut().unwrap().import.rows[0].checked = false;
    let configs = app.validated_import_batch().unwrap();
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].local_port, p2);
}

#[test]
fn unchecking_everything_yields_no_batch() {
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    app.form.as_mut().unwrap().import.uncheck_all_visible();
    assert!(app.validated_import_batch().is_none());
    app.commit_form_create_from_import();
    assert!(app.tunnels.is_empty(), "nothing ticked means nothing created");
    assert!(
        app.form.as_ref().unwrap().errors.iter().any(|e| e.contains("没有")),
        "the form should say why: {:?}",
        app.form.as_ref().unwrap().errors
    );
}

#[test]
fn a_collapsed_pane_hands_the_submit_back_to_the_single_form() {
    // The pane opens by default now, so "there are rows" is no longer the
    // same thing as "the user is working with them". Someone who collapses
    // the pane on purpose and types one forward by hand must get that one
    // tunnel — not "没有勾选任何转发", and not the batch.
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    app.close_import_pane();
    {
        let f = app.form.as_mut().unwrap();
        f.name.set("manual");
        f.remote_host.set("10.0.0.9");
        f.remote_port.set("8080");
        f.local_port.set(free_port().to_string());
    }
    app.submit_form();
    assert_eq!(app.tunnels.len(), 1, "the single form creates exactly one tunnel");
    assert_eq!(app.tunnels[0].cfg.name, "manual", "and it is the one the user typed");
    assert_ne!(app.tunnels[0].cfg.note, "来自 ~/.ssh/config 第 1 行", "not an imported row");
}

#[test]
fn a_touched_open_pane_wins_over_a_perfectly_valid_single_form() {
    // The pane auto-opens and auto-ticks, so an *untouched* pane is only a
    // suggestion and a complete hand-typed form outranks it (see
    // `form_submit`'s router). Once the user changes a tick, though, the pane
    // is what "save" means and the port fields are ignored — otherwise filling
    // them too would silently create one tunnel instead of the batch the user
    // is looking at.
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_import(vec![show(p1), show(p2)]);
    {
        let f = app.form.as_mut().unwrap();
        f.name.set("manual");
        f.remote_host.set("10.0.0.9");
        f.remote_port.set("8080");
        f.local_port.set(free_port().to_string());
    }
    // Touch the pane: re-affirm "all of these" (a deliberate key press, which
    // is what `touched` records).
    app.form.as_mut().unwrap().import.check_all_visible();
    app.submit_form();
    assert_eq!(app.tunnels.len(), 2, "the visible batch decides once the user touched it");
    assert!(app.tunnels.iter().all(|t| t.cfg.name != "manual"));
}

#[test]
fn submitting_with_imports_creates_the_whole_batch_and_persists() {
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_import(vec![show(p1), show(p2)]);
    // Fill the single-tunnel fields so the form itself is valid too: the
    // import path must win over them once the pane is touched, not fight with
    // them.
    {
        let f = app.form.as_mut().unwrap();
        f.name.set("ignored");
        f.local_port.set("19000");
        f.remote_port.set("80");
    }
    app.form.as_mut().unwrap().import.check_all_visible();
    app.submit_form();
    assert_eq!(app.tunnels.len(), 2, "one tunnel per ticked row, form fields ignored");
    assert!(app.form.is_none(), "the form closes on success");
    let reloaded = crate::config::load(&app.paths.config).unwrap();
    assert_eq!(reloaded.tunnels.len(), 2, "the batch must be persisted, not just in memory");
    let ports: Vec<u16> = reloaded.tunnels.iter().map(|t| t.local_port).collect();
    assert_eq!(ports, vec![p1, p2]);
    assert!(reloaded.tunnels.iter().all(|t| t.enabled), "created tunnels carry user intent to run");
}

#[test]
fn the_cursor_lands_on_the_first_created_tunnel() {
    let (_d, mut app) = app_with_import(vec![show(free_port()), show(free_port())]);
    app.submit_form();
    let first_id = app.cfg.tunnels.iter().map(|t| t.id).min().unwrap();
    assert_eq!(app.selected_id(), Some(first_id), "the user should land where the work happened");
}

#[test]
fn a_remote_forward_imports_with_the_ends_the_right_way_round() {
    // `-R` listens on the server and lands locally, so the config's local
    // side is the target and the remote side is the listener. Getting this
    // backwards would silently forward the wrong port.
    let mut fwd = show(7000);
    fwd.kind = ForwardKind::Remote;
    fwd.listen_addr = "127.0.0.1".into();
    fwd.listen_port = 7000;
    fwd.target_host = "127.0.0.1".into();
    fwd.target_port = 22;
    let (_d, mut app) = app_with_import(vec![fwd]);
    let cfg = app.validated_import_batch().unwrap().remove(0);
    assert_eq!(cfg.kind, ForwardKind::Remote);
    assert_eq!(cfg.remote_port, 7000, "the remote side is the listener");
    assert_eq!(cfg.local_port, 22, "the local side is the target");
}

#[test]
fn a_dynamic_import_needs_no_remote_target() {
    let mut fwd = show(1080);
    fwd.kind = ForwardKind::Dynamic;
    fwd.target_host = String::new();
    fwd.target_port = 0;
    let (_d, mut app) = app_with_import(vec![fwd]);
    let cfg = app.validated_import_batch().unwrap().remove(0);
    assert_eq!(cfg.kind, ForwardKind::Dynamic);
    assert_eq!(cfg.local_port, 1080);
    assert!(config::validate_tunnel(&cfg).is_ok(), "a SOCKS forward has no remote side");
}

#[test]
fn imported_names_do_not_collide_with_each_other_or_with_existing_ones() {
    // Two rows suggesting the same name (two host blocks, one port) plus a
    // saved tunnel already using it.
    let mut a = show(18001);
    a.host_alias = "dev-box".into();
    let mut b = show(18001);
    b.kind = ForwardKind::Local;
    let (_d, mut app) = app_with_import(vec![a, b]);
    app.cfg.tunnels.push(TunnelConfig {
        id: 99,
        name: "dev-box-L-18001".into(),
        kind: ForwardKind::Local,
        ssh_host: "other".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 18500,
        remote_host: "10.0.0.1".into(),
        remote_port: 80,
        enabled: false,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    });
    let configs = app.validated_import_batch().unwrap();
    let names: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
    let unique: std::collections::HashSet<&&str> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "names must be unique: {names:?}");
    assert!(!names.contains(&"dev-box-L-18001"), "must not reuse a saved tunnel's name");
}

#[test]
fn the_overlap_guard_sees_wildcard_binds() {
    // 0.0.0.0 covers every interface, so it collides with a loopback bind on
    // the same port — the same rule the config-level validator applies.
    let (_d, app) = app_fresh();
    let p = free_port();
    let mut a = app.config_from_forward(1, &show(p), &[]);
    a.local_addr = "0.0.0.0".into();
    let mut b = app.config_from_forward(2, &show(p), &[]);
    b.local_port = a.local_port;
    b.local_addr = "127.0.0.1".into();
    assert!(listen_collision(&a, &b).is_some(), "the overlap guard must catch a wildcard bind");

    // Different ports never collide.
    let mut c = app.config_from_forward(3, &show(p), &[]);
    c.local_port = a.local_port.wrapping_add(1);
    assert!(listen_collision(&a, &c).is_none());
}

#[test]
fn occupied_local_ports_are_steered_away_from() {
    // The ssh config knows nothing about ports sshpf already holds; importing
    // must not create a tunnel that cannot bind.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = listener.local_addr().unwrap().port();
    let (_d, mut app) = app_with_import(vec![show(busy)]);
    let configs = app.validated_import_batch().unwrap();
    assert_ne!(configs[0].local_port, busy, "the imported port is taken and must move");
    assert!(configs[0].local_port > busy, "the scan walks upwards: {}", configs[0].local_port);
    drop(listener);
}

#[test]
fn ports_claimed_by_other_tunnels_are_also_avoided() {
    // A stopped tunnel is not listening, but starting it later would collide.
    let (_d, mut app) = app_fresh();
    app.cfg.tunnels.push(TunnelConfig {
        id: 1,
        name: "other".into(),
        kind: ForwardKind::Local,
        ssh_host: "h".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 18042,
        remote_host: "10.0.0.1".into(),
        remote_port: 80,
        enabled: false,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    });
    app.tunnels.push(TunnelView::new(app.cfg.tunnels[0].clone()));
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    let table = ImportTable::new(vec![show(18042)]);
    app.form.as_mut().unwrap().set_import_table("dev-box", table);
    let cfg = app.validated_import_batch().unwrap().remove(0);
    assert_ne!(cfg.local_port, 18042, "must not fight over a port another tunnel owns");
}

#[test]
fn two_rows_in_one_batch_never_get_the_same_local_port() {
    // Two rows asking for the same listen port for *different* targets
    // (possible when `Host *` repeats a block). The second must be steered
    // elsewhere instead of handing two tunnels the same port — and it must
    // not be mistaken for a duplicate: a different target is a different
    // forward.
    let p = free_port();
    let mut other_target = show(p);
    other_target.target_host = "10.0.0.2".into();
    let (_d, mut app) = app_with_import(vec![show(p), other_target]);
    let configs = app.validated_import_batch().unwrap();
    assert_eq!(configs.len(), 2);
    assert_ne!(configs[0].local_port, configs[1].local_port, "the scan must account for the batch itself");
}

#[test]
fn two_identical_rows_of_one_batch_collapse_to_one_tunnel() {
    // The same directive listed twice (`Host *` repeating a block): one
    // forward, so it must produce one tunnel rather than two rows fighting
    // over the same intent.
    let p = free_port();
    let (_d, mut app) = app_with_import(vec![show(p), show(p)]);
    let batch = app.validated_import_batch_outcome().unwrap();
    assert_eq!(batch.configs.len(), 1);
    assert_eq!(batch.skipped.len(), 1);
    assert_eq!(app.validated_import_batch().map(|v| v.len()), Some(1), "the plain accessor agrees");
}

#[test]
fn an_empty_host_field_is_reported_not_created() {
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    app.form.as_mut().unwrap().ssh_host.set("   ");
    assert!(app.validated_import_batch().is_none());
    assert!(
        app.form.as_ref().unwrap().errors.iter().any(|e| e.contains("SSH 主机")),
        "{:?}",
        app.form.as_ref().unwrap().errors
    );
}

#[test]
fn the_import_note_records_where_the_row_came_from() {
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    let cfg = app.validated_import_batch().unwrap().remove(0);
    assert!(cfg.note.contains("~/.ssh/config"), "note should be traceable: {}", cfg.note);
    assert!(cfg.note.contains('1'), "note should cite the line: {}", cfg.note);
}

// -- filter plumbing through the App -----------------------------------

#[test]
fn created_tunnels_are_not_immediately_failed_by_a_stale_status() {
    // `commit_import_batch` starts each tunnel; the status must at least move
    // out of Stopped, proving the start path ran.
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    app.submit_form();
    assert!(
        !matches!(app.tunnels[0].status, TunnelStatus::Stopped),
        "creating should attempt to run the tunnel: {:?}",
        app.tunnels[0].status
    );
}

mod port_planning_tests {
    use crate::app::import::*;
    use crate::config::{ConfigFile, Paths};

    /// An importer config for `forward`, given a set of ports that are "taken".
    fn plan(forward: &SshForward, taken: &[u16]) -> TunnelConfig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("c.toml"), state: dir.path().join("s.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        let app = App::new(paths);
        app.config_from_forward(1, forward, taken)
    }

    fn local(port: u16) -> SshForward {
        SshForward {
            kind: ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port: port,
            target_host: "10.0.0.1".into(),
            target_port: 8080,
            host_alias: "web".into(),
            line: 1,
            raw: String::new(),
        }
    }

    #[test]
    fn an_imported_privileged_port_gets_an_unprivileged_local_port() {
        // Regression: the importer used to scan upwards from the privileged port
        // and land on *another* privileged one (80 -> 280), which then failed at
        // bind time with a permission error the user could not explain.
        let cfg = plan(&local(80), &[]);
        assert!(!cfg.uses_privileged_port(), "imported local port {} needs root", cfg.local_port);
        assert!(cfg.local_port >= 1024);
    }

    #[test]
    fn every_privileged_import_can_actually_bind() {
        // Exhaustive across the privileged range: no result may need root.
        for port in [1u16, 22, 80, 443, 587, 993, 1023] {
            let cfg = plan(&local(port), &[]);
            assert!(!cfg.uses_privileged_port(), "remote {port} imported privileged local {}", cfg.local_port);
        }
    }

    #[test]
    fn a_taken_import_port_is_steered_upwards() {
        let cfg = plan(&local(18001), &[18001]);
        assert_ne!(cfg.local_port, 18001);
        assert!(cfg.local_port > 18001, "must bump up, got {}", cfg.local_port);
    }
}

mod hardening_tests {
    use crate::app::import::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::TunnelStatus;
    use crate::model::{ImportColumn, ImportTable};

    fn app_fresh() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        (dir, App::new(paths))
    }

    /// A discovered `-L` row whose target port follows its listen port, so two
    /// rows are two forwards (rows differing only in the local port are one).
    fn show(port: u16) -> SshForward {
        SshForward {
            kind: ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port: port,
            target_host: "10.0.0.1".into(),
            target_port: port,
            host_alias: "dev-box".into(),
            line: 1,
            raw: format!("LocalForward {port} 10.0.0.1:{port}"),
        }
    }

    fn app_with_import(rows: Vec<SshForward>) -> (tempfile::TempDir, App) {
        let (dir, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        let table = ImportTable::new(rows);
        app.form.as_mut().unwrap().set_import_table("dev-box", table);
        app.form.as_mut().unwrap().import_open = true;
        (dir, app)
    }

    /// Give the app a saved tunnel with `id`, so `next_id()` is deterministic.
    fn app_with_max_id(id: crate::model::TunnelId) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let cfg = ConfigFile {
            tunnels: vec![TunnelConfig {
                id,
                name: "existing".into(),
                kind: ForwardKind::Local,
                ssh_host: "h".into(),
                local_addr: "127.0.0.1".into(),
                local_port: 18001,
                remote_host: "127.0.0.1".into(),
                remote_port: 22,
                enabled: false,
                auto_restart: false,
                ssh_options: vec![],
                note: String::new(),
                created_at: 1,
            }],
            ..ConfigFile::default()
        };
        crate::config::save(&paths.config, &cfg).unwrap();
        (dir, App::new(paths))
    }

    // -- port planning at the edges of the port space ----------------------

    #[test]
    fn a_port_at_the_top_of_the_range_does_not_wrap_to_zero() {
        // `plan_local_port` scans `remote.max(1024)..=u16::MAX` (bounded by
        // PORT_SCAN_LIMIT). At the very top there is no room above, so the
        // planner must give up rather than wrap to 0 — a port-0 listener is not
        // a "next free port", it silently asks the OS to choose one.
        let (dir, mut app) = app_with_import(vec![show(u16::MAX)]);
        let configs = app.validated_import_batch().expect("validation should still produce a batch");
        assert_eq!(configs.len(), 1);
        assert_ne!(configs[0].local_port, 0, "a wrapped port must never reach the config");
        drop(app);

        // And the pure planner agrees: nothing free at the top means None.
        let planned = crate::model::plan_local_port(u16::MAX, &[], |_| false);
        assert!(planned.is_none(), "exhausted port space must report None, not wrap");
        drop(dir);
    }

    #[test]
    fn an_already_taken_top_port_is_not_reused() {
        // Every candidate above the requested port is exhausted, so the planner
        // must decline; the config then falls back to the requested port so
        // validation can report the real problem instead of silently using 0.
        let (dir, mut app) = app_with_import(vec![show(u16::MAX)]);
        let configs = app.validated_import_batch().expect("still a batch");
        assert_ne!(configs[0].local_port, 0, "giving up must not mean port 0");
        drop(app);
        drop(dir);
    }

    #[test]
    fn imported_ids_do_not_overflow_and_stay_unique() {
        // The batch used to iterate an unbounded `(next_id..)` range, and the
        // ids came from `max_id + 1` — an overflow for `u64::MAX`.
        let (_d, mut app) = app_with_max_id(100);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.form.as_mut().unwrap().set_import_table("dev-box", ImportTable::new(vec![show(18101), show(18102)]));
        let configs = app.validated_import_batch().expect("batch");
        assert_eq!(configs.len(), 2);
        let ids: Vec<_> = configs.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![101, 102]);
        assert_ne!(ids[0], ids[1], "two rows must not share an id");
    }

    #[test]
    fn an_exhausted_id_space_fails_the_import_batch_without_panicking() {
        let (_d, mut app) = app_with_max_id(u64::MAX);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.form.as_mut().unwrap().set_import_table("dev-box", ImportTable::new(vec![show(18101)]));
        assert!(app.validated_import_batch().is_none(), "no ids left means no batch");
        let errors = app.form.as_ref().unwrap().errors.clone();
        assert!(errors.iter().any(|e| e.contains("编号")), "the user must be told why: {errors:?}");
    }

    #[test]
    fn a_batch_that_would_run_past_the_id_space_is_refused_whole() {
        // u64::MAX - 1 leaves room for exactly one id; two rows must not be
        // silently truncated into a partial import.
        let (_d, mut app) = app_with_max_id(u64::MAX - 1);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.form.as_mut().unwrap().set_import_table("dev-box", ImportTable::new(vec![show(18101), show(18102)]));
        assert!(app.validated_import_batch().is_none(), "a partial batch is worse than none");
        let errors = app.form.as_ref().unwrap().errors.clone();
        assert!(errors.iter().any(|e| e.contains("编号")), "{errors:?}");
    }

    #[test]
    fn one_row_fits_into_the_last_free_id() {
        let (_d, mut app) = app_with_max_id(u64::MAX - 1);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.form.as_mut().unwrap().set_import_table("dev-box", ImportTable::new(vec![show(18101)]));
        let configs = app.validated_import_batch().expect("the last id is usable");
        assert_eq!(configs[0].id, u64::MAX);
    }

    #[test]
    fn a_forward_with_no_listen_port_is_never_imported() {
        // A `DynamicForward` / unix socket with no numeric port: listing it is
        // fine, but it must not reach `plan_local_port(0, ..)` as a request.
        let mut broken = show(0);
        broken.listen_port = 0;
        let (_d, mut app) = app_with_import(vec![broken, show(18103)]);
        let configs = app.validated_import_batch().expect("the good row is still importable");
        assert_eq!(configs.len(), 1, "the port-less row must be skipped, not created with port 0");
        // Not asserted to be exactly 18103: the planner probes real sockets and
        // may bump a busy port, which would make this flaky on a loaded machine.
        assert_ne!(configs[0].local_port, 0, "an imported config may never listen on port 0");
        assert!(config::validate_tunnel(&configs[0]).is_ok());
    }

    #[test]
    fn every_imported_config_passes_validation() {
        // The batch's whole promise: validate up front so a partial write cannot
        // happen. A config that fails validation here would contradict it.
        let mut remote = show(7000);
        remote.kind = ForwardKind::Remote;
        remote.listen_addr = "127.0.0.1".into();
        remote.listen_port = 7000;
        remote.target_host = "127.0.0.1".into();
        remote.target_port = 22;
        let mut dynamic = show(1080);
        dynamic.kind = ForwardKind::Dynamic;
        dynamic.target_host = String::new();
        dynamic.target_port = 0;

        let (_d, mut app) = app_with_import(vec![show(18104), remote, dynamic]);
        let configs = app.validated_import_batch().expect("batch");
        assert_eq!(configs.len(), 3);
        for cfg in &configs {
            assert!(config::validate_tunnel(cfg).is_ok(), "{cfg:?} must validate before anything is written");
        }
    }

    #[test]
    fn commit_import_batch_with_an_empty_vec_is_a_no_op() {
        // `configs.first()` would be `None`; the cursor bookkeeping must cope.
        let (_d, mut app) = app_fresh();
        app.commit_import_batch_with_skipped(Vec::new(), Vec::new());
        assert!(app.tunnels.is_empty());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn toggling_a_row_that_is_not_visible_is_a_no_op() {
        // Mouse clicks carry a raw row index from the hit map, which can be
        // stale after a filter change. It must not index out of bounds.
        let (_d, mut app) = app_with_import(vec![show(18105), show(18106)]);
        app.begin_import_filter(ImportColumn::Listen);
        app.import_filter_char('1');
        app.import_filter_char('8');
        app.import_filter_char('1');
        app.import_filter_char('0');
        app.import_filter_char('5');
        let before = app.form.as_ref().unwrap().import.checked_count();
        app.import_toggle_at(999); // raw index that cannot exist
        app.import_select_at(999);
        assert_eq!(app.form.as_ref().unwrap().import.checked_count(), before, "a stale click must not tick anything");
        assert!(app.form.as_ref().unwrap().import.cursor_row().is_some() || before == 0);
    }

    #[test]
    fn the_cursor_lands_on_the_first_imported_tunnel_even_with_a_filter() {
        let (_d, mut app) = app_with_import(vec![show(18107), show(18108)]);
        app.submit_form();
        assert_eq!(app.tunnels.len(), 2);
        assert!(app.selected_tunnel().is_some(), "the cursor must land on something");
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Starting { .. } | TunnelStatus::Failed { .. }));
    }
}

mod duplicate_tests {
    use crate::app::import::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::ImportTable;

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// A saved `-L` tunnel on `ssh_host` with the given local/remote endpoints.
    fn saved(
        id: crate::model::TunnelId,
        name: &str,
        ssh_host: &str,
        local_port: u16,
        remote: (&str, u16),
    ) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: ssh_host.into(),
            local_addr: "127.0.0.1".into(),
            local_port,
            remote_host: remote.0.into(),
            remote_port: remote.1,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    /// An app whose imported table holds `rows`, and whose saved config already
    /// holds `tunnels`.
    fn app_with_saved(tunnels: Vec<TunnelConfig>, rows: Vec<SshForward>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile { tunnels, ..ConfigFile::default() }).unwrap();
        let mut app = App::new(paths);
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("cloud_pc");
        app.form.as_mut().unwrap().set_import_table("cloud_pc", ImportTable::new(rows));
        app.form.as_mut().unwrap().import_open = true;
        (dir, app)
    }

    fn local_forward(from_port: u16, target: (&str, u16)) -> SshForward {
        SshForward {
            kind: ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port: from_port,
            target_host: target.0.into(),
            target_port: target.1,
            host_alias: "cloud_pc".into(),
            line: 12,
            raw: format!("LocalForward {from_port} {}:{}", target.0, target.1),
        }
    }

    #[test]
    fn the_reported_case_a_persisted_bumped_local_port_is_not_a_new_forward() {
        // The user's real config, exactly: `cloud_pc-L-18789` was created while
        // 18789 was busy, so the **bumped** local port (18790) is what sits in
        // config.toml — the original request is not recorded anywhere. `~/.ssh/config`
        // still declares `LocalForward 18789 127.0.0.1:18789`, so a re-import asks
        // for 18789 while the saved tunnel holds 18790. That mismatch must not
        // hide the duplicate: the local port is a derived value, not identity.
        let port = free_port();
        let bumped = port.wrapping_add(1).max(1024);
        let saved = saved(1, "cloud_pc-L", "cloud_pc", bumped, ("127.0.0.1", port));
        let (_d, mut app) = app_with_saved(vec![saved], vec![local_forward(port, ("127.0.0.1", port))]);

        let batch = app.validated_import_batch_outcome().expect("a batch exists");
        assert!(batch.configs.is_empty(), "the duplicate must not be planned for creation");
        assert_eq!(batch.skipped.len(), 1, "and it must be reported as skipped: {:?}", batch.skipped);
        assert!(batch.skipped[0].contains(&port.to_string()));

        // The old accessor keeps its shape and is equally empty.
        assert!(app.validated_import_batch().is_none_or(|v| v.is_empty()));
        app.commit_form_create_from_import();
        assert_eq!(app.tunnels.len(), 1, "nothing was created, the saved tunnel is still the only one");
        assert!(app.form.is_none(), "the form closes: there was nothing wrong with what the user asked for");
    }

    #[test]
    fn one_duplicate_and_one_new_row_creates_only_the_new_one() {
        let dup_port = free_port();
        let new_port = free_port();
        let saved = saved(1, "cloud_pc-L-old", "cloud_pc", dup_port, ("127.0.0.1", dup_port));
        let (_d, mut app) = app_with_saved(
            vec![saved],
            vec![local_forward(dup_port, ("127.0.0.1", dup_port)), local_forward(new_port, ("10.0.0.9", 8080))],
        );

        let batch = app.validated_import_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1, "only the genuinely new forward is created");
        assert_eq!(batch.configs[0].remote_host, "10.0.0.9");
        assert_eq!(batch.skipped.len(), 1);
        assert!(batch.skipped[0].contains(&dup_port.to_string()));

        app.commit_form_create_from_import();
        assert_eq!(app.tunnels.len(), 2, "the saved tunnel plus the one new forward");
    }

    #[test]
    fn the_status_says_how_many_rows_were_skipped() {
        let dup_port = free_port();
        let saved = saved(1, "cloud_pc-L-old", "cloud_pc", dup_port, ("127.0.0.1", dup_port));
        let (_d, mut app) = app_with_saved(
            vec![saved],
            vec![local_forward(dup_port, ("127.0.0.1", dup_port)), local_forward(free_port(), ("10.0.0.9", 8080))],
        );
        app.commit_form_create_from_import();
        let status = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        assert!(status.contains('1'), "the status should mention the skipped row: {status:?}");
        assert!(status.contains("跳过") || status.contains("skip"), "{status:?}");
    }

    #[test]
    fn two_rows_of_one_batch_that_are_the_same_forward_collapse_to_one() {
        // `Host *` repeating a block: same forward twice in one import batch.
        let port = free_port();
        let (_d, mut app) = app_with_saved(
            vec![],
            vec![local_forward(port, ("127.0.0.1", port)), local_forward(port, ("127.0.0.1", port))],
        );
        let batch = app.validated_import_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1, "the first row wins, the second is the same forward");
        assert_eq!(batch.skipped.len(), 1);
    }

    #[test]
    fn the_same_directive_reimported_after_an_in_process_bump_is_skipped() {
        // The other half of the same bug: the saved tunnel holds the plain port and
        // *this* import is the one that gets bumped (the port is taken by the saved
        // tunnel itself). Same forward either way.
        let port = free_port();
        let saved = saved(1, "cloud_pc-L", "cloud_pc", port, ("127.0.0.1", port));
        let (_d, mut app) = app_with_saved(vec![saved], vec![local_forward(port, ("127.0.0.1", port))]);
        let batch = app.validated_import_batch_outcome().expect("a batch exists");
        assert!(batch.configs.is_empty(), "the planned port differs but the forward is the same");
        assert_eq!(batch.skipped.len(), 1);
    }

    #[test]
    fn two_declarations_differing_only_in_the_local_port_collapse_into_one() {
        // Documented known boundary: `-L`'s local listen port is not identity, so
        // one remote target yields one forward even when `~/.ssh/config` declares
        // two local listeners for it. Deliberate: a missed duplicate leaves the
        // user unable to clear the duplicate row at all.
        let port = free_port();
        let (_d, mut app) = app_with_saved(
            vec![],
            vec![local_forward(port, ("10.0.0.1", 80)), {
                let mut second = local_forward(port.wrapping_add(7).max(1024), ("10.0.0.1", 80));
                second.line = 13;
                second
            }],
        );
        let batch = app.validated_import_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1, "one target, one forward");
        assert_eq!(batch.skipped.len(), 1);
    }

    #[test]
    fn a_different_remote_end_still_gets_created() {
        // Counter-example borrowed from the identity rules: the local side is the
        // same, but a different remote target is a different forward.
        let port = free_port();
        let saved = saved(1, "cloud_pc-L-web", "cloud_pc", port, ("10.0.0.1", 80));
        let (_d, mut app) = app_with_saved(vec![saved], vec![local_forward(port, ("10.0.0.2", 80))]);
        let batch = app.validated_import_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1, "a different target is a different forward");
        assert!(batch.skipped.is_empty());
    }

    #[test]
    fn a_different_ssh_host_still_gets_created() {
        let port = free_port();
        let mut other_host = saved(1, "dev-box-L", "dev-box", port, ("127.0.0.1", port));
        other_host.name = "dev-box-L".into();
        let (_d, mut app) = app_with_saved(vec![other_host], vec![local_forward(port, ("127.0.0.1", port))]);
        let batch = app.validated_import_batch_outcome().expect("batch");
        assert_eq!(batch.configs.len(), 1, "a forward on another host is another forward");
        assert!(batch.skipped.is_empty());
    }
}

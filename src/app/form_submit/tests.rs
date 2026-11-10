//! Unit tests for `form_submit.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use crate::app::App;
use crate::config::{ConfigFile, Paths};
use crate::model::{ForwardKind, SshForward};

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

/// One `~/.ssh/config` row. `target_port` is spelled out so two rows can be
/// kept semantically distinct: the duplicate guard treats `-L` rows with the
/// same target as the same forward regardless of the local listen port
/// (that port is bumped by availability, not chosen by the user).
fn show(listen_port: u16, target_port: u16) -> SshForward {
    SshForward {
        kind: ForwardKind::Local,
        listen_addr: "127.0.0.1".into(),
        listen_port,
        target_host: "10.0.0.1".into(),
        target_port,
        host_alias: "dev-box".into(),
        line: 1,
        raw: format!("LocalForward {listen_port} 10.0.0.1:{target_port}"),
    }
}

/// A create form for `dev-box` whose import pane holds `rows`, auto-opened
/// exactly the way discovery opens it (`install_import_rows`) — so `touched`
/// starts false, which is the whole point of these tests.
fn app_with_default_pane(rows: Vec<SshForward>) -> (tempfile::TempDir, App) {
    let (dir, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    app.install_import_rows("dev-box", rows);
    (dir, app)
}

/// Fill the typed fields with one complete, valid forward.
fn fill_hand_typed(app: &mut App, name: &str, remote_port: u16, local_port: u16) {
    let f = app.form.as_mut().unwrap();
    f.name.set(name);
    f.ssh_host.set("dev-box");
    f.remote_host.set("10.0.0.9");
    f.remote_port.set(remote_port.to_string());
    f.local_port.set(local_port.to_string());
}

/// True when this tunnel carries the importer's provenance note, i.e. it was
/// created from a `~/.ssh/config` row rather than typed by hand.
fn came_from_ssh_config(cfg: &crate::model::TunnelConfig) -> bool {
    cfg.note.contains("~/.ssh/config")
}

#[test]
fn a_complete_hand_typed_form_outranks_the_untouched_default_pane() {
    // The user-reported regression: host discovered 2 forwards, the pane
    // auto-opened and auto-ticked them, the user filled the form by hand for
    // port 3082 and pressed ctrl+s — and only the 2 imported rows appeared.
    // An untouched pane is a *default offer*; a complete form is an explicit
    // instruction, so it must win.
    let (p1, p2) = (free_port(), free_port());
    let (typed_port, remote_port) = (free_port(), free_port());
    let (_d, mut app) = app_with_default_pane(vec![show(p1, 80), show(p2, 8080)]);
    assert!(!app.form.as_ref().unwrap().import.touched, "precondition: auto-open is not a user action");
    fill_hand_typed(&mut app, "cloud_pc-3082", remote_port, typed_port);

    app.submit_form();

    assert_eq!(app.tunnels.len(), 1, "only the typed forward may be created");
    assert_eq!(app.tunnels[0].cfg.name, "cloud_pc-3082");
    assert_eq!(app.tunnels[0].cfg.local_port, typed_port);
    assert_eq!(app.tunnels[0].cfg.remote_port, remote_port);
    assert!(!came_from_ssh_config(&app.tunnels[0].cfg), "the created tunnel must be the typed one");
    assert!(
        !app.tunnels.iter().any(|t| t.cfg.local_port == p1 || t.cfg.local_port == p2),
        "not one import row may be created"
    );
    // The pane vanishes without its rows being created; silence would read as
    // "the batch ran too".
    let msg = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
    assert!(msg.contains("未创建"), "the user must be told the pane's rows were skipped: {msg}");
    assert!(msg.contains('2'), "and how many: {msg}");
}

#[test]
fn an_emptied_pane_hands_the_save_key_back_to_the_complete_typed_form() {
    // The reported regression: the pane auto-opened with the host's two
    // forwards, the user unticked *all* of them and typed the forward they
    // actually wanted (port 3081), then pressed ctrl+s. Unticking is an
    // instruction, but an emptied pane has nothing left to create — the
    // complete typed form is the only instruction with a body, so it must
    // win. The old rule read "touched ⇒ the pane decides" and answered
    // "没有勾选任何转发 / No forward is ticked" over a perfectly valid form.
    let (p1, p2) = (free_port(), free_port());
    let (typed_port, wanted_port) = (free_port(), 3081u16);
    let (_d, mut app) = app_with_default_pane(vec![show(p1, 80), show(p2, 8080)]);
    app.form.as_mut().unwrap().import.uncheck_all_visible();
    assert!(app.form.as_ref().unwrap().import.touched, "unticking every row is still a user instruction");
    assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 0, "precondition: nothing is ticked");
    fill_hand_typed(&mut app, "cloud_pc-3081", wanted_port, typed_port);

    app.submit_form();

    assert_eq!(app.tunnels.len(), 1, "the typed forward is the only thing left to create");
    assert_eq!(app.tunnels[0].cfg.name, "cloud_pc-3081");
    assert_eq!(app.tunnels[0].cfg.remote_port, wanted_port);
    assert_eq!(app.tunnels[0].cfg.local_port, typed_port);
    assert!(app.form.is_none(), "the form closes: nothing was wrong with what the user typed");
    assert!(
        !app.tunnels.iter().any(|t| t.cfg.local_port == p1 || t.cfg.local_port == p2),
        "the unticked import rows must still not be created"
    );
}

#[test]
fn an_emptied_pane_still_reports_nothing_ticked_when_no_forward_was_typed() {
    // The counterpart, so the fix cannot become "an empty pane never
    // decides": untick everything and type nothing, and the only sensible
    // answer is that there is nothing to save.
    let (_d, mut app) = app_with_default_pane(vec![show(free_port(), 80)]);
    app.form.as_mut().unwrap().import.uncheck_all_visible();

    app.submit_form();

    assert!(app.tunnels.is_empty(), "nothing ticked and nothing typed creates nothing");
    assert!(app.form.is_some(), "the form stays open to explain why");
    let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
    assert!(errors.iter().any(|e| e.contains("没有勾选任何转发")), "{errors:?}");
}

#[test]
fn a_touched_pane_outranks_a_complete_hand_typed_form() {
    // The counterpart: once the user has unticked a row, the pane's ticks are
    // the instruction, and a filled-in form is just leftover scratch input.
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_default_pane(vec![show(p1, 80), show(p2, 8080)]);
    app.form.as_mut().unwrap().import.toggle_cursor(); // untick row 0 → touched
    assert!(app.form.as_ref().unwrap().import.touched);
    fill_hand_typed(&mut app, "ignored", free_port(), free_port());

    app.submit_form();

    assert_eq!(app.tunnels.len(), 1, "the batch creates one tunnel per still-ticked row");
    assert_eq!(app.tunnels[0].cfg.local_port, p2, "and it is the row that stayed ticked");
    assert!(came_from_ssh_config(&app.tunnels[0].cfg));
    assert!(app.tunnels.iter().all(|t| t.cfg.name != "ignored"), "the typed fields must be ignored");
}

#[test]
fn an_incomplete_form_still_lets_the_default_pane_win() {
    // The headline workflow must survive: pick a host, press ctrl+s, import
    // everything. Nothing was typed that could be a tunnel, so the default
    // offer is the only instruction there is.
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_default_pane(vec![show(p1, 80), show(p2, 8080)]);
    assert!(!app.form.as_ref().unwrap().is_complete(), "precondition: an empty form is not a tunnel");

    app.submit_form();

    assert_eq!(app.tunnels.len(), 2, "both imported rows must be created");
    assert!(app.tunnels.iter().all(|t| came_from_ssh_config(&t.cfg)));
    assert!(app.form.is_none(), "the form closes on success");
}

#[test]
fn only_choosing_a_row_counts_as_touching_the_pane() {
    // Clicking a row only moves the cursor/highlight — it expresses no
    // intention about the ticks, so it must not let the pane hijack ctrl+s.
    // Clicking the checkbox *does* change the selection, so it must.
    let (_d, mut app) = app_with_default_pane(vec![show(free_port(), 80), show(free_port(), 8080)]);
    app.import_select_at(0);
    assert!(!app.form.as_ref().unwrap().import.touched, "selecting a row is not an instruction");

    app.import_toggle_at(0);
    assert!(app.form.as_ref().unwrap().import.touched, "ticking a row is");
}

#[test]
fn a_new_host_starts_with_a_fresh_untouched_offer() {
    // Switching host installs a different set of rows: the previous host's
    // "I ticked things here" intention cannot carry over to brand-new rows.
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_default_pane(vec![show(p1, 80)]);
    app.form.as_mut().unwrap().import.toggle_cursor();
    assert!(app.form.as_ref().unwrap().import.touched);

    app.install_import_rows("dev-box", vec![show(p2, 8080)]);
    assert!(!app.form.as_ref().unwrap().import.touched, "a new host's rows are a new default offer");

    // And the consequence: a complete hand-typed form wins over them.
    let typed_port = free_port();
    fill_hand_typed(&mut app, "typed", free_port(), typed_port);
    app.submit_form();
    assert_eq!(app.tunnels.len(), 1);
    assert_eq!(app.tunnels[0].cfg.local_port, typed_port);
    assert!(!came_from_ssh_config(&app.tunnels[0].cfg));
}

#[test]
fn an_exhausted_id_space_is_reported_into_the_form() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let mut cfg = ConfigFile::default();
    cfg.tunnels.push(crate::model::TunnelConfig {
        id: u64::MAX,
        name: "huge".into(),
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
    });
    crate::config::save(&paths.config, &cfg).unwrap();
    let mut app = App::new(paths);
    app.open_form_create();
    {
        let f = app.form.as_mut().unwrap();
        f.ssh_host.set("host");
        f.name.set("fresh");
        f.remote_port.set("22");
        f.local_port.set("18002");
    }
    app.submit_form();
    assert_eq!(app.tunnels.len(), 1, "no new definition without a free id");
    let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
    assert!(errors.iter().any(|e| e.contains("编号")), "{errors:?}");
}

#[test]
fn a_new_tunnel_lands_on_the_row_it_was_added_at() {
    // `commit_form_create` used to recover the new row as `len() - 1`; the
    // cursor must still land on that row.
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
    let mut app = App::new(paths);
    app.open_form_create();
    {
        let f = app.form.as_mut().unwrap();
        f.ssh_host.set("host");
        f.name.set("fresh");
        f.remote_port.set("22");
        f.local_port.set("18003");
    }
    app.submit_form();
    assert_eq!(app.tunnels.len(), 1);
    assert_eq!(app.selected_tunnel().map(|t| t.cfg.name.as_str()), Some("fresh"));
}

// -- the single-form create: the same duplicate guard --------------------

/// A saved `-L` tunnel: `local_port` → `remote_host:remote_port` on `ssh_host`.
fn saved_tunnel(local_port: u16, remote_port: u16, ssh_host: &str) -> crate::model::TunnelConfig {
    crate::model::TunnelConfig {
        id: 1,
        name: "already-there".into(),
        kind: ForwardKind::Local,
        ssh_host: ssh_host.into(),
        local_addr: "127.0.0.1".into(),
        local_port,
        remote_host: "127.0.0.1".into(),
        remote_port,
        enabled: false,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    }
}

/// An app whose config already holds one tunnel, with a fresh create form.
fn app_with_saved(saved: crate::model::TunnelConfig) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let cfg = ConfigFile { tunnels: vec![saved], ..ConfigFile::default() };
    crate::config::save(&paths.config, &cfg).unwrap();
    let mut app = App::new(paths);
    app.open_form_create();
    (dir, app)
}

#[test]
fn typing_a_forward_that_already_exists_creates_nothing() {
    // The single-form half of the de-duplication rule: the user re-types a
    // forward they already have. Nothing is created, and the reason is a
    // field error naming the row that already holds it — not the listen-port
    // collision message, which would describe the symptom instead.
    let (_d, mut app) = app_with_saved(saved_tunnel(18789, 18789, "dev-box"));
    {
        let f = app.form.as_mut().unwrap();
        f.name.set("typed-again");
        f.ssh_host.set("dev-box");
        f.remote_host.set("127.0.0.1");
        f.remote_port.set("18789");
        f.local_port.set("18099");
    }
    app.submit_form();
    assert_eq!(app.tunnels.len(), 1, "the duplicate must not be added");
    assert!(app.form.is_some(), "the form stays open to explain why");
    let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
    assert!(
        errors.iter().any(|e| e.contains("已存在相同的转发") && e.contains("already-there")),
        "the error must name the existing tunnel: {errors:?}"
    );
}

#[test]
fn a_saved_tunnel_whose_local_port_was_bumped_is_still_a_duplicate() {
    // The shape the user actually hit: the saved entry was created earlier,
    // when 18789 was busy, so what got persisted is local 18790 → remote
    // 18789. Typing `18789 -> 127.0.0.1:18789` now asks for an intent that is
    // already satisfied; only the local port differs, and that port is a
    // derived value, not the user's intent.
    let (_d, mut app) = app_with_saved(saved_tunnel(18790, 18789, "dev-box"));
    {
        let f = app.form.as_mut().unwrap();
        f.name.set("typed-again");
        f.ssh_host.set("dev-box");
        f.remote_host.set("127.0.0.1");
        f.remote_port.set("18789");
        f.local_port.set("18789");
    }
    app.submit_form();
    assert_eq!(app.tunnels.len(), 1, "a bumped persisted local port must not defeat the duplicate check");
    let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
    assert!(errors.iter().any(|e| e.contains("已存在相同的转发")), "{errors:?}");
}

#[test]
fn a_different_target_or_host_is_not_a_duplicate() {
    // The guard must not swallow legitimate forwards. Each case changes one
    // identity field and also picks a free local port, so the *listen-port*
    // check (a separate rule) cannot be what refuses it: a refusal here would
    // have to come from the duplicate guard.
    for (remote_port, ssh_host) in [(9999u16, "dev-box"), (18789, "other-box")] {
        let (_d, mut app) = app_with_saved(saved_tunnel(18789, 18789, "dev-box"));
        let local_port = free_port();
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("legit");
            f.ssh_host.set(ssh_host);
            f.remote_host.set("127.0.0.1");
            f.remote_port.set(remote_port.to_string());
            f.local_port.set(local_port.to_string());
        }
        app.submit_form();
        assert_eq!(app.tunnels.len(), 2, "remote_port={remote_port} ssh_host={ssh_host} is a new forward");
    }
}

#[test]
fn adding_one_new_port_never_re_creates_the_hosts_existing_forwards() {
    // The reported bug, end to end and through the real submit path. `cloud_pc`
    // declares two forwards in `~/.ssh/config`; both are already managed. The
    // user opens "new tunnel", the pane fills with those two, and they type a
    // third one by hand. `ctrl+s` used to create all three — then four, then
    // six — because the pane offered the existing two as *ticked defaults* on
    // every open, and each copy got a freshly bumped local port so the
    // duplicate check saw nothing.
    //
    // Built through the `built_in_cfg` + `install_import_rows` pair rather than
    // by hand, so it exercises the real discovery → table → submit chain
    // (including the `existing` marking), not a mock of it.
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let cfg = ConfigFile {
        tunnels: vec![saved_tunnel(18789, 18789, "dev-box"), {
            let mut second = saved_tunnel(44897, 44897, "dev-box");
            second.id = 2;
            second.name = "already-there-2".into();
            second
        }],
        ..ConfigFile::default()
    };
    crate::config::save(&paths.config, &cfg).unwrap();
    let mut app = App::new(paths);
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    // The host's two directives, exactly as discovery would report them.
    app.install_import_rows(
        "dev-box",
        vec![
            SshForward {
                kind: ForwardKind::Local,
                listen_addr: String::new(),
                listen_port: 18789,
                target_host: "127.0.0.1".into(),
                target_port: 18789,
                host_alias: "dev-box".into(),
                line: 30,
                raw: "LocalForward 18789 127.0.0.1:18789".into(),
            },
            SshForward {
                kind: ForwardKind::Local,
                listen_addr: String::new(),
                listen_port: 44897,
                target_host: "127.0.0.1".into(),
                target_port: 44897,
                host_alias: "dev-box".into(),
                line: 32,
                raw: "LocalForward 44897 127.0.0.1:44897".into(),
            },
        ],
    );
    // Both already exist: nothing is offered as a default to create.
    assert_eq!(
        app.form.as_ref().unwrap().import.checked_count(),
        0,
        "the host's existing forwards must not arrive ticked"
    );
    assert_eq!(app.form.as_ref().unwrap().import.existing_count(), 2);

    // The user types the one they actually want and saves.
    let wanted = free_port();
    fill_hand_typed(&mut app, "cloud_pc-3081", wanted, wanted);
    app.submit_form();

    assert_eq!(
        app.tunnels.len(),
        3,
        "exactly one new tunnel: {:#?}",
        app.tunnels.iter().map(|t| t.cfg.name.clone()).collect::<Vec<_>>()
    );
    let created: Vec<&str> = app
        .tunnels
        .iter()
        .map(|t| t.cfg.name.as_str())
        .filter(|n| *n != "already-there" && *n != "already-there-2")
        .collect();
    assert_eq!(created.len(), 1, "the two pre-existing entries must not be duplicated");
    assert_eq!(created[0], "cloud_pc-3081");
    let new_tunnel = app.tunnels.iter().find(|t| t.cfg.name == "cloud_pc-3081").unwrap();
    assert_eq!(new_tunnel.cfg.remote_port, wanted);
    assert_eq!(new_tunnel.cfg.local_port, wanted);
}

#[test]
fn a_pane_of_only_already_saved_rows_never_blocks_a_complete_typed_form() {
    // The state PR #29 created and that the unticked-pane rule has to cover:
    // a host whose every `~/.ssh/config` forward is already managed gets a
    // pane whose rows are all un-tickable (`existing`, so `creatable()` is
    // false). `space`/`i` still opens it — to explain the dimmed lines — and
    // opening by hand sets `touched`. `checked_count()` is 0 no matter what
    // the user does, so without the "emptied pane" rule `ctrl+s` would answer
    // "没有勾选任何转发" over a perfectly good typed forward.
    //
    // The distinguishing detail versus PR #29's auto-open case: here the pane
    // is *open* and *touched*, which is exactly what used to hand it the save
    // key.
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let cfg = ConfigFile { tunnels: vec![saved_tunnel(18789, 18789, "dev-box")], ..ConfigFile::default() };
    crate::config::save(&paths.config, &cfg).unwrap();
    let mut app = App::new(paths);
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    app.install_import_rows(
        "dev-box",
        vec![SshForward {
            kind: ForwardKind::Local,
            listen_addr: String::new(),
            listen_port: 18789,
            target_host: "127.0.0.1".into(),
            target_port: 18789,
            host_alias: "dev-box".into(),
            line: 30,
            raw: "LocalForward 18789 127.0.0.1:18789".into(),
        }],
    );
    // Auto-open declined (nothing creatable to offer); the user opens it by
    // hand to see why, which marks it touched.
    assert!(!app.form.as_ref().unwrap().import_open, "precondition: nothing creatable, so nothing auto-opens");
    app.toggle_import_pane();
    {
        let f = app.form.as_ref().unwrap();
        assert!(f.import_open, "the manual open must succeed so it can explain itself");
        assert!(f.import.touched, "opening by hand is a user action");
        assert_eq!(f.import.checked_count(), 0, "an existing row can never be ticked");
    }

    let wanted = free_port();
    fill_hand_typed(&mut app, "cloud_pc-3081", wanted, wanted);
    app.submit_form();

    assert_eq!(app.tunnels.len(), 2, "exactly the typed one on top of the saved one");
    assert_eq!(app.tunnels[1].cfg.name, "cloud_pc-3081");
    assert_eq!(app.tunnels[1].cfg.remote_port, wanted);
    assert!(app.form.is_none(), "the form closes: the typed forward was valid all along");
}

#[test]
fn the_same_forward_typed_with_a_taken_local_port_says_already_exists() {
    // Both rules apply to a re-typed duplicate (same intent *and* a listen
    // collision). The duplicate message is the one that describes what is
    // actually going on, so it must be the one the user reads.
    let (_d, mut app) = app_with_saved(saved_tunnel(18789, 18789, "dev-box"));
    {
        let f = app.form.as_mut().unwrap();
        f.name.set("typed-again");
        f.ssh_host.set("dev-box");
        f.remote_host.set("127.0.0.1");
        f.remote_port.set("18789");
        f.local_port.set("18789");
    }
    app.submit_form();
    let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
    assert!(errors.iter().any(|e| e.contains("已存在相同的转发")), "{errors:?}");
    assert!(
        !errors.iter().any(|e| e.contains("监听地址与")),
        "the symptom (listen collision) must not be reported instead: {errors:?}"
    );
}

#[test]
fn editing_a_tunnel_does_not_collide_with_its_own_definition() {
    // The duplicate lookup skips the candidate's own id, so re-saving an edit
    // is not reported as "already exists".
    let (_d, mut app) = app_with_saved(saved_tunnel(18789, 18789, "dev-box"));
    app.selected = 0;
    app.open_form_edit();
    app.form.as_mut().unwrap().name.set("renamed");
    app.submit_form();
    // An edit goes through the confirmation gate; it must not be reported as
    // a duplicate in the form.
    let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
    assert!(errors.is_empty(), "editing a tunnel is not creating a duplicate: {errors:?}");
    assert!(matches!(app.overlay, crate::app::Overlay::Confirm { .. }), "the edit still asks first");
}

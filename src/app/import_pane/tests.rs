//! Unit tests for `import_pane.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use crate::app::App;
use crate::config::{ConfigFile, Paths};
use crate::model::{ForwardKind, ImportColumn, ImportTable, SshForward, TunnelConfig, TunnelView};

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

fn show(port: u16) -> SshForward {
    SshForward {
        kind: crate::model::ForwardKind::Local,
        listen_addr: "127.0.0.1".into(),
        listen_port: port,
        target_host: "10.0.0.1".into(),
        target_port: 80,
        host_alias: "dev-box".into(),
        line: 1,
        raw: format!("LocalForward {port} 10.0.0.1:80"),
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

/// A discovered `-L` row with its own target port, so two rows are genuinely
/// two forwards. [`show`] keeps a fixed target — two rows that differ only in
/// their local listen port are *one* forward by design (see `app::duplicate`),
/// and a fixture that collapsed them would test the wrong thing.
fn show_to(listen_port: u16, target_port: u16) -> SshForward {
    SshForward { target_port, ..show(listen_port) }
}

/// A saved `-L` tunnel for `dev-box`, used as the "already have it" side.
fn saved(id: crate::model::TunnelId, name: &str, local_port: u16) -> TunnelConfig {
    TunnelConfig {
        id,
        name: name.into(),
        kind: ForwardKind::Local,
        ssh_host: "dev-box".into(),
        local_addr: "127.0.0.1".into(),
        local_port,
        remote_host: "10.0.0.1".into(),
        remote_port: 80,
        enabled: false,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    }
}

#[test]
fn a_forward_that_is_already_saved_is_listed_but_never_ticked() {
    // The reported bug: every "add a new port" pass also re-offered cloud_pc's
    // two `~/.ssh/config` forwards as *ticked* defaults, so ctrl+s re-created
    // them (and the local-port planner bumped each copy, producing the 18789 /
    // 18790 / 18791 ladder). The pane must still list them — they really are
    // in the file, and hiding them would look like sshpf lost a directive —
    // but "already saved" cannot be a *default to create*.
    let port = free_port();
    let (_d, mut app) = app_with_import(vec![show(port)]);
    // Already saved, with a local port bumped by an earlier run — the shape the
    // duplicate check exists for (`-L`'s local port is not identity).
    app.cfg.tunnels = vec![saved(9, "dev-box-L-80", port + 1)];
    app.tunnels = crate::app::build_views(&app.cfg);

    app.install_import_rows("dev-box", vec![show(port)]);

    let form = app.form.as_ref().unwrap();
    assert_eq!(form.import.len(), 1, "the row stays visible: it is in ~/.ssh/config");
    assert_eq!(form.import.rows[0].existing.as_deref(), Some("dev-box-L-80"), "and names the tunnel it is");
    assert!(!form.import.rows[0].checked, "an existing forward must not be a ticked default");
    assert_eq!(form.import.checked_count(), 0);
    assert_eq!(form.import.existing_count(), 1);
    // No creatable row left, so the pane does not cover the form by itself —
    // `space`/`i` still opens it to explain the dimmed line.
    assert!(!app.import_pane_open(), "nothing to create, so nothing auto-opens");
}

#[test]
fn an_existing_row_cannot_be_ticked_back_on_by_any_key() {
    // Every route to a tick has to skip it: `space`, `a` (select all) and `I`
    // (invert). A keyboard path to a known duplicate would undo the whole fix,
    // and "select all" is the one users reach for by reflex.
    let (existing_port, new_port) = (free_port(), free_port());
    // Two *distinct* targets: the saved one and one new forward, so "which row
    // is creatable" is a real question rather than a same-identity artefact.
    let rows = vec![show_to(existing_port, 80), show_to(new_port, 8080)];
    let (_d, mut app) = app_with_import(rows.clone());
    app.cfg.tunnels = vec![saved(9, "dev-box-L-80", existing_port + 1)];
    app.tunnels = crate::app::build_views(&app.cfg);
    app.install_import_rows("dev-box", rows);

    app.import_uncheck_all();
    for action in ["space-on-row", "check-all", "invert"] {
        app.import_uncheck_all();
        match action {
            "space-on-row" => app.import_toggle_at(0),
            "check-all" => app.import_check_all(),
            _ => app.import_invert(),
        }
        let rows = &app.form.as_ref().unwrap().import.rows;
        assert!(!rows[0].checked, "{action} must not tick an existing forward");
        assert_eq!(
            app.form.as_ref().unwrap().import.checked_count(),
            if action == "space-on-row" { 0 } else { 1 },
            "{action}: only the genuinely new row can be ticked"
        );
    }
}

#[test]
fn opening_the_pane_by_hand_marks_the_rows_as_used() {
    // Auto-open is an offer, so it leaves `touched` false. Opening by hand is
    // the user saying "show me these rows", which must count.
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    app.install_import_rows("dev-box", vec![show(free_port())]);
    app.close_import_pane();
    assert!(!app.form.as_ref().unwrap().import.touched);

    app.toggle_import_pane();
    assert!(app.import_pane_open());
    assert!(app.form.as_ref().unwrap().import.touched, "a manual open is a user action");
}

#[test]
fn focusing_a_column_filter_marks_the_rows_as_used() {
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    assert!(!app.form.as_ref().unwrap().import.touched, "precondition: untouched table");
    app.begin_import_filter(ImportColumn::Listen);
    assert!(app.form.as_ref().unwrap().import.touched);
}

#[test]
fn refresh_loads_the_table_for_the_typed_host_and_opens_the_pane() {
    // End-to-end through the discovery result: typing an alias and committing
    // it must be enough to get the rows on screen, with the pane opening by
    // itself. `apply_import_discovery` is the seam that takes rows from
    // discovery, so this asserts the whole "default offer" rule without
    // touching the process environment (which Rust 2024 makes `unsafe`).
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    let port = free_port();
    app.apply_import_discovery("dev-box".to_string(), vec![show(port)], None);

    let form = app.form.as_ref().unwrap();
    assert_eq!(form.import.len(), 1);
    assert_eq!(form.import.rows[0].forward.listen_port, port);
    assert!(form.import.rows[0].checked, "rows start ticked");
    assert!(app.import_pane_open(), "a host with importable forwards opens the pane by itself");
    assert_eq!(app.import_error, None);
}

#[test]
fn a_discovery_error_is_recorded_without_dropping_the_rows() {
    // A failed read is degraded-but-usable: the message is kept for the pane,
    // and the (empty) table still replaces the previous host's, so the pane
    // cannot show stale rows for a host we could not read.
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.apply_import_discovery("dev-box".to_string(), vec![show(free_port())], None);
    assert!(app.import_pane_open());

    app.apply_import_discovery("broken".to_string(), Vec::new(), Some("读取失败".to_string()));
    assert_eq!(app.import_error.as_deref(), Some("读取失败"));
    assert!(app.form.as_ref().unwrap().import.is_empty(), "stale rows must not survive a failed read");
    assert!(!app.import_pane_open());
}

#[test]
fn discovery_is_not_repeated_for_the_same_host() {
    // Cheap guard: typing one character at a time would otherwise re-parse
    // the config and discard the user's tick marks on every keystroke.
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    let table = ImportTable::new(vec![show(free_port())]);
    app.form.as_mut().unwrap().set_import_table("dev-box", table);
    // Untick it, then refresh for the *same* host: the mark must survive.
    app.form.as_mut().unwrap().import.rows[0].checked = false;
    app.refresh_import_for_current_host();
    assert!(!app.form.as_ref().unwrap().import.rows[0].checked, "tick marks must survive a no-op refresh");
}

#[test]
fn a_host_with_importable_forwards_opens_the_pane_already_ticked() {
    // The headline behaviour: "the forwards this host declares" is the
    // *default* offer, not something behind a key press. Every importable
    // row arrives ticked, so the user's job is to untick the exceptions.
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.install_import_rows("dev-box", vec![show(p1), show(p2)]);
    let form = app.form.as_ref().unwrap();
    assert!(form.import_open, "the pane must open on its own");
    assert!(app.import_pane_open(), "and be actually drawable");
    assert_eq!(form.import.checked_count(), 2, "all importable rows start ticked");
    assert_eq!(form.import.checked_forwards().len(), 2);
}

#[test]
fn a_host_with_no_forwards_does_not_open_an_empty_pane() {
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.install_import_rows("no-forwards", Vec::new());
    assert!(!app.form.as_ref().unwrap().import_open, "the flag itself must stay down");
    assert!(!app.import_pane_open(), "an empty pane would only cover the form");
    assert!(app.form.as_ref().unwrap().import.is_empty());
}

#[test]
fn a_host_whose_forwards_are_all_unimportable_does_not_auto_open() {
    // Unix-socket / target-less directives are listed (silently hiding them
    // would look like the tool missed the config) but there is nothing to
    // tick, so auto-opening would be a pane full of `[–]`. `space`/`i` still
    // opens it to read the reasons.
    let mut broken = show(0);
    broken.target_host = String::new();
    broken.target_port = 0;
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.install_import_rows("dev-box", vec![broken]);
    {
        let form = app.form.as_ref().unwrap();
        assert_eq!(form.import.len(), 1, "the row must still be listed");
        assert_eq!(form.import.importable_count(), 0);
        assert!(!form.import_open, "nothing can be ticked, so the pane stays closed");
        assert!(!app.import_pane_open());
    }

    // ... and the manual escape hatch still works: this is the only way the
    // user ever sees *why* the row cannot be imported, so it must not have
    // been closed off along with the auto-open.
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    app.toggle_import_pane();
    assert!(app.import_pane_open(), "space/i must still open a pane holding only `[–]` rows");
    assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 0, "and nothing became tickable");
}

#[test]
fn switching_hosts_reopens_the_pane_for_a_host_that_has_forwards() {
    // Host A has forwards (pane auto-open), the user collapses it, then picks
    // host B which also has forwards: B's rows are the new default, so the
    // pane comes back. Collapsing is per-host, not a permanent preference.
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.install_import_rows("host-a", vec![show(p1)]);
    assert!(app.import_pane_open());
    app.close_import_pane();
    assert!(!app.import_pane_open());

    app.install_import_rows("host-b", vec![show(p2)]);
    assert!(app.import_pane_open(), "a different host's forwards are a fresh default");
    assert_eq!(app.form.as_ref().unwrap().import.rows[0].forward.listen_port, p2, "and B's rows replace A's");
}

#[test]
fn collapsing_the_pane_survives_a_refresh_of_the_same_host() {
    // `enter`/`esc` on the host field refreshes discovery. For an unchanged
    // alias that must be a no-op, or a deliberate collapse would pop back
    // open every time the user leaves the field.
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    app.install_import_rows("dev-box", vec![show(free_port())]);
    app.close_import_pane();
    app.refresh_import_for_current_host();
    assert!(!app.import_pane_open(), "refreshing the same host must not undo the collapse");
    assert_eq!(app.form.as_ref().unwrap().import.len(), 1, "and must not drop the rows either");
}

#[test]
fn an_empty_host_field_installs_nothing_instead_of_the_global_forwards() {
    // `Host *` matches the empty string, so scanning "" would list every
    // global forward under an alias the user never picked — and `ctrl+s`
    // would then create them.
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.install_import_rows("dev-box", vec![show(free_port())]);
    app.form.as_mut().unwrap().ssh_host.set("   ");
    app.refresh_import_for_current_host();
    let form = app.form.as_ref().unwrap();
    assert!(form.import.is_empty(), "no host means no rows");
    assert!(!form.import_open);
    assert_eq!(app.import_error, None, "not scanning is not a failure to report");
}

#[test]
fn editing_an_existing_tunnel_never_imports() {
    // Importing is a create-time convenience; silently turning an edit into a
    // batch create would be a data-loss bug.
    let (_d, mut app) = app_fresh();
    app.cfg.tunnels.push(TunnelConfig {
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
    });
    app.tunnels.push(TunnelView::new(app.cfg.tunnels[0].clone()));
    app.open_form_edit();
    app.form.as_mut().unwrap().ssh_host.set("dev-box");
    app.refresh_import_for_current_host();
    assert!(app.form.as_ref().unwrap().import.is_empty());
}

#[test]
fn toggling_the_pane_on_a_host_without_forwards_says_so() {
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("no-such-host-xyzzy");
    app.toggle_import_pane();
    assert!(!app.import_pane_open(), "an empty pane must not open");
    let msg = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
    assert!(msg.contains("没有端口转发"), "the user must be told why nothing opened: {msg}");
}

#[test]
fn pane_visibility_needs_rows_as_well_as_the_flag() {
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().import_open = true;
    assert!(!app.import_pane_open(), "the flag alone must not show an empty pane");
    let table = ImportTable::new(vec![show(free_port())]);
    app.form.as_mut().unwrap().set_import_table("dev-box", table);
    assert!(app.import_pane_open());
}

#[test]
fn starting_a_column_filter_takes_the_keyboard_away_from_the_field() {
    // With the pane open by default, both the form field and a column filter
    // can plausibly "have the cursor". `handle_form_key` gives the field
    // priority while `is_editing_field()`, so a mouse click on a column
    // header while a field was being edited would draw the filter cursor in
    // the pane (it renders last) while every keystroke still went into the
    // field. The click has to end field editing to stay honest.
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    app.form.as_mut().unwrap().field = crate::model::FormField::Name;
    app.form.as_mut().unwrap().begin_edit();
    assert!(app.form.as_ref().unwrap().is_editing_field());

    app.begin_import_filter(ImportColumn::Listen);
    assert!(!app.form.as_ref().unwrap().is_editing_field(), "the filter owns the keyboard now");
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Listen));
}

#[test]
fn filter_keys_reach_the_table() {
    let (_d, mut app) = app_with_import(vec![show(free_port()), show(free_port())]);
    let first = app.form.as_ref().unwrap().import.rows[0].forward.listen_port.to_string();
    app.begin_import_filter(ImportColumn::Listen);
    for c in first.chars() {
        app.import_filter_char(c);
    }
    assert_eq!(app.form.as_ref().unwrap().import.visible(), vec![0], "only the matching row survives");
    app.import_filter_backspace();
    assert_eq!(app.form.as_ref().unwrap().import.filter(ImportColumn::Listen), &first[..first.len() - 1]);
    app.clear_import_filter(ImportColumn::Listen);
    assert_eq!(app.form.as_ref().unwrap().import.visible().len(), 2);
}

#[test]
fn undo_reports_when_there_is_nothing_to_undo() {
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    app.undo_import_filter();
    let msg = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
    assert!(msg.contains("没有可撤销"), "silence would look broken: {msg}");
    app.begin_import_filter(ImportColumn::Kind);
    app.import_filter_char('L');
    app.undo_import_filter();
    assert!(!app.form.as_ref().unwrap().import.has_filters());
}

#[test]
fn a_new_host_does_not_inherit_the_previous_hosts_filters() {
    // Filters narrow *this* host's rows; carrying them over would silently
    // hide rows of a different host for no visible reason.
    let (p1, p2) = (free_port(), free_port());
    let (_d, mut app) = app_with_import(vec![show(p1)]);
    app.form.as_mut().unwrap().import.set_filter(ImportColumn::Kind, "R");
    app.form.as_mut().unwrap().set_import_table("other-host", ImportTable::new(vec![show(p2)]));
    assert!(!app.form.as_ref().unwrap().import.has_filters());
}

#[test]
fn import_interactions_do_not_open_the_pane_for_an_empty_host() {
    let (_d, mut app) = app_fresh();
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("nope");
    app.begin_import_filter(ImportColumn::Kind);
    // `begin_import_filter` flips the open flag; the visibility predicate
    // still refuses because there are no rows.
    assert!(!app.import_pane_open());
}

#[test]
fn closing_the_pane_also_ends_filter_editing() {
    let (_d, mut app) = app_with_import(vec![show(free_port())]);
    app.begin_import_filter(ImportColumn::Listen);
    assert!(app.form.as_ref().unwrap().import.editing.is_some());
    app.close_import_pane();
    assert!(!app.import_pane_open());
    assert!(app.form.as_ref().unwrap().import.editing.is_none(), "a hidden pane must not keep the keyboard");
}

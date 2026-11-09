//! The save key must reach the form from **every** UI state the form can be in.
//!
//! Reported failure: 「为 cloud_pc 填好 3081 端口转发，`ctrl+s` 之后端口没有得到
//! 转发」. The log file was empty — no `tunnel.created`, no rejection, nothing —
//! which made "the key never arrived" indistinguishable from "it arrived and was
//! refused".
//!
//! Two sub-focuses inside the form ended their dispatch with
//! `KeyCode::Char(c) if !c.is_control()`. `ctrl+s` is a `Char('s')` whose `char`
//! is *not* a control character, so that arm typed the save key into a text
//! buffer:
//!
//! * the **host dropdown** appended it to the query (`cloud_pcs`);
//! * an **import column filter** appended it to the filter (`s`).
//!
//! Either way the form never saved: no tunnel, no error, no log line. These tests
//! drive the real key entry point (`sshpf::keys::handle_key`), because the defect
//! was in dispatch — a test that called `App::submit_form` directly would pass
//! while the user still could not save.
//!
//! Run with `cargo test --test save_key_reachability`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sshpf::app::App;
use sshpf::config::{self, ConfigFile, Paths};
use sshpf::model::{FormField, ForwardKind, ImportColumn, TunnelConfig};
use sshpf::ssh::SshHost;

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn f10() -> KeyEvent {
    KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE)
}

/// A saved `-L` tunnel, as the user already has for `cloud_pc`.
fn saved(id: u64, name: &str, local: u16, remote: u16, ssh_host: &str) -> TunnelConfig {
    TunnelConfig {
        id,
        name: name.into(),
        kind: ForwardKind::Local,
        ssh_host: ssh_host.into(),
        local_addr: "127.0.0.1".into(),
        local_port: local,
        remote_host: "127.0.0.1".into(),
        remote_port: remote,
        enabled: false,
        auto_restart: true,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    }
}

/// An app with two saved `cloud_pc` forwards, a create form, `cloud_pc` picked
/// from the dropdown (which is what fills the import pane), and the fields typed
/// for the forward the user actually wants: `3081`.
///
/// The pane's rows all name forwards that already exist, so under the current
/// "already-saved rows cannot be ticked" rule they arrive unticked — the state
/// the pane is in right after picking a host.
fn form_with_typed_forward() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let tunnels = vec![
        saved(3, "cloud_pc-L-18789-2", 18789, 18789, "cloud_pc"),
        saved(4, "cloud_pc-L-44897-2", 44897, 44897, "cloud_pc"),
    ];
    config::save(&paths.config, &ConfigFile { tunnels, ..ConfigFile::default() }).unwrap();
    let mut app = App::new(paths);
    app.hosts = vec![SshHost {
        alias: "cloud_pc".into(),
        hostname: "10.244.119.128".into(),
        user: Some("wangcan".into()),
        port: 22,
        proxy_jump: Vec::new(),
        identity_file: None,
    }];
    app.open_form_create();
    app.open_host_picker();
    app.picker_accept();
    app.refresh_import_for_current_host();
    {
        let f = app.form.as_mut().unwrap();
        f.name.set("cloud_pc-3081");
        f.remote_host.set("127.0.0.1");
        f.remote_port.set("3081");
        // A port that is free in any CI sandbox: the point under test is dispatch,
        // not port availability.
        f.local_port.set("38081");
        f.mark_local_port_manual();
    }
    (dir, app)
}

/// True when the forward the user typed exists. Creation is what "the save
/// worked" means to the user; how the row is named is not part of this contract.
fn typed_forward_was_created(app: &App) -> bool {
    app.tunnels.iter().any(|t| t.cfg.remote_port == 3081 && t.cfg.ssh_host == "cloud_pc")
}

#[test]
fn ctrl_s_saves_while_the_host_dropdown_is_open() {
    // The dropdown is a *search aid*: the host it is searching for is already in
    // the field, so `ctrl+s` means "save the forward I typed", not "the letter s".
    let (_d, mut app) = form_with_typed_forward();
    app.open_host_picker();
    assert!(app.host_picker_open(), "precondition: the dropdown is open");

    sshpf::keys::handle_key(&mut app, ctrl('s'));

    assert!(
        typed_forward_was_created(&app),
        "ctrl+s must save instead of typing `s` into the dropdown query (query={:?}, errors={:?})",
        app.picker.as_ref().map(|p| p.query.clone()),
        app.form.as_ref().map(|f| f.errors.clone())
    );
    // A successful save closes the form (and with it the dropdown), so there is
    // no query left to have been polluted — the assertion above is the real one.
    assert!(app.picker.is_none(), "the dropdown closes with the form after a successful save");
}

#[test]
fn ctrl_s_saves_while_an_import_column_filter_is_focused() {
    // The filter catch-all is the same shape. The pane's own footer advertises
    // `ctrl+s 保存`, so a focused filter cannot be allowed to eat it.
    let (_d, mut app) = form_with_typed_forward();
    app.begin_import_filter(ImportColumn::Raw);
    assert!(
        app.form.as_ref().is_some_and(|f| f.import.editing.is_some()),
        "precondition: a column filter owns the keyboard"
    );

    sshpf::keys::handle_key(&mut app, ctrl('s'));

    assert!(
        typed_forward_was_created(&app),
        "ctrl+s must save instead of typing `s` into the filter (filter={:?}, errors={:?})",
        app.form.as_ref().map(|f| f.import.filter(ImportColumn::Raw).to_string()),
        app.form.as_ref().map(|f| f.errors.clone())
    );
    // Same as above: the form closed, so nothing can have been typed into it.
    assert!(app.form.is_none(), "the form closes after a successful save");
}

#[test]
fn a_single_left_press_does_not_put_the_save_key_out_of_reach() {
    // `←`/`→` in the *form* reaches the pane's column filter first while the pane
    // is open (the pane claims arrows before the form does). So one stray arrow
    // key used to move the keyboard into a filter and make every later `ctrl+s`
    // dead. Whatever the arrow does, save must keep working afterwards.
    let (_d, mut app) = form_with_typed_forward();
    app.form.as_mut().unwrap().field = FormField::RemotePort;
    sshpf::keys::handle_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));

    sshpf::keys::handle_key(&mut app, ctrl('s'));

    assert!(
        typed_forward_was_created(&app),
        "after one arrow press the save key must still save (errors={:?})",
        app.form.as_ref().map(|f| f.errors.clone())
    );
}

#[test]
fn f10_also_survives_a_focused_column_filter() {
    // Same defect by the other save binding: `f10` has no `Char`, so it fell
    // through the filter's `Char(c)` arm and did nothing at all.
    let (_d, mut app) = form_with_typed_forward();
    app.begin_import_filter(ImportColumn::Target);

    sshpf::keys::handle_key(&mut app, f10());

    assert!(
        typed_forward_was_created(&app),
        "f10 must save too (errors={:?})",
        app.form.as_ref().map(|f| f.errors.clone())
    );
}

#[test]
fn ctrl_s_still_saves_a_plain_form_with_the_pane_untouched() {
    // The counterweight: the fix must not disable the ordinary path. A complete
    // hand-filled form with an untouched pane saves the typed forward, exactly as
    // before (`#26`'s rule).
    let (_d, mut app) = form_with_typed_forward();
    assert!(!app.host_picker_open());
    assert!(!app.form.as_ref().is_some_and(|f| f.import.touched), "precondition: the pane is only offering");

    sshpf::keys::handle_key(&mut app, ctrl('s'));

    assert!(typed_forward_was_created(&app), "the ordinary save path must keep working");
}

#[test]
fn ctrl_s_in_a_name_field_still_types_nothing_but_saves() {
    // Regression guard for the other half of the same confusion: while a text
    // field is being edited, `ctrl+s` must save and must not insert `s` into the
    // buffer (that path was fixed earlier; this keeps it fixed now that the
    // predicate is shared).
    let (_d, mut app) = form_with_typed_forward();
    app.form.as_mut().unwrap().field = FormField::Name;
    app.form.as_mut().unwrap().begin_edit();

    sshpf::keys::handle_key(&mut app, ctrl('s'));

    assert!(typed_forward_was_created(&app), "ctrl+s while editing a field must save, not type `s`");
    assert!(
        !app.form.as_ref().is_some_and(|f| f.name.value().ends_with('s')),
        "the save key must not be inserted into the name buffer"
    );
}

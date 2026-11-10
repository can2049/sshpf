//! Unit tests for `form.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::app::Overlay;
use crate::config;
use crate::keys::handle_key;
use crate::keys::testkit::*;

// -- form: two-stage editing -------------------------------------------

#[test]
fn arrows_do_not_edit_values_in_navigating_mode() {
    // The whole point of two-stage editing: arrowing past a field must not
    // type into it.
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().field = FormField::Name;
    assert!(!app.form.as_ref().unwrap().is_editing_field());
    handle_key(&mut app, ch('x'));
    assert_eq!(app.form.as_ref().unwrap().name.value(), "", "typing while navigating must be ignored");
}

#[test]
fn enter_starts_editing_and_esc_stops() {
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().field = FormField::Name;
    handle_key(&mut app, key(KeyCode::Enter));
    assert!(app.form.as_ref().unwrap().is_editing_field(), "enter must start editing");
    handle_key(&mut app, ch('h'));
    handle_key(&mut app, ch('i'));
    assert_eq!(app.form.as_ref().unwrap().name.value(), "hi");
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(!app.form.as_ref().unwrap().is_editing_field(), "esc must end editing");
    // The form is still open: esc only leaves editing here.
    assert!(matches!(app.overlay, Overlay::Form));
    assert_eq!(app.form.as_ref().unwrap().name.value(), "hi");
}

#[test]
fn tab_in_navigating_mode_changes_the_field() {
    let (_d, mut app) = form_app();
    let first = app.form.as_ref().unwrap().field;
    handle_key(&mut app, key(KeyCode::Tab));
    assert_ne!(app.form.as_ref().unwrap().field, first);
    handle_key(&mut app, key(KeyCode::BackTab));
    assert_eq!(app.form.as_ref().unwrap().field, first);
}

#[test]
fn up_down_move_between_fields_in_navigating_mode() {
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().field = FormField::SshHost;
    handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.form.as_ref().unwrap().field, FormField::Name);
    handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.form.as_ref().unwrap().field, FormField::Kind);
    handle_key(&mut app, key(KeyCode::Up));
    assert_eq!(app.form.as_ref().unwrap().field, FormField::Name);
}

#[test]
fn enter_on_kind_cycles_instead_of_editing() {
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().field = FormField::Kind;
    let before = app.form.as_ref().unwrap().kind();
    handle_key(&mut app, key(KeyCode::Enter));
    assert_ne!(app.form.as_ref().unwrap().kind(), before);
    assert!(!app.form.as_ref().unwrap().is_editing_field(), "toggle fields are not text-editable");
}

#[test]
fn enter_on_autorestart_toggles_it() {
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().field = FormField::AutoRestart;
    let before = app.form.as_ref().unwrap().auto_restart;
    handle_key(&mut app, key(KeyCode::Enter));
    assert_ne!(app.form.as_ref().unwrap().auto_restart, before);
}

#[test]
fn remote_port_typing_derives_the_local_port() {
    let (_d, mut app) = form_app();
    {
        let f = app.form.as_mut().unwrap();
        f.field = FormField::RemotePort;
    }
    handle_key(&mut app, key(KeyCode::Enter));
    for c in "8080".chars() {
        handle_key(&mut app, ch(c));
    }
    assert_eq!(app.form.as_ref().unwrap().remote_port.value(), "8080");
    assert_eq!(app.form.as_ref().unwrap().local_port.value(), "8080");
}

#[test]
fn local_port_typed_by_hand_is_not_overwritten() {
    let (_d, mut app) = form_app();
    {
        let f = app.form.as_mut().unwrap();
        f.field = FormField::LocalPort;
    }
    handle_key(&mut app, key(KeyCode::Enter));
    for c in "9000".chars() {
        handle_key(&mut app, ch(c));
    }
    // Now change the remote port; the manual local port must survive.
    {
        let f = app.form.as_mut().unwrap();
        f.end_edit();
        f.field = FormField::RemotePort;
    }
    handle_key(&mut app, key(KeyCode::Enter));
    for c in "7000".chars() {
        handle_key(&mut app, ch(c));
    }
    assert_eq!(app.form.as_ref().unwrap().local_port.value(), "9000");
}

#[test]
fn ctrl_s_saves_the_form() {
    let (_d, mut app) = form_app();
    {
        let f = app.form.as_mut().unwrap();
        f.ssh_host.set("testhost");
        f.name.set("saved");
        f.remote_port.set("22");
        f.local_port.set("18050");
    }
    handle_key(&mut app, ctrl('s'));
    assert!(matches!(app.overlay, Overlay::None), "ctrl+s closes the form on success");
    assert_eq!(app.tunnels.len(), 1);
    let reloaded = config::load(&app.paths.config).unwrap();
    assert_eq!(reloaded.tunnels[0].name, "saved");
}

#[test]
fn ctrl_s_saves_while_a_field_is_being_edited() {
    // Regression: `edit_field` inserts every `KeyCode::Char` without looking
    // at modifiers, so ctrl+s used to type the letter `s` into the field and
    // leave the form open. The save keys are stage-independent.
    let (_d, mut app) = form_app();
    {
        let f = app.form.as_mut().unwrap();
        f.ssh_host.set("testhost");
        f.name.set("saved");
        f.remote_port.set("22");
        f.local_port.set("18050");
        f.field = FormField::Name;
    }
    handle_key(&mut app, key(KeyCode::Enter)); // start editing NAME
    handle_key(&mut app, ch('x'));
    handle_key(&mut app, ctrl('s'));
    assert!(matches!(app.overlay, Overlay::None), "ctrl+s must save from the editing stage");
    assert_eq!(app.tunnels.len(), 1);
    let reloaded = config::load(&app.paths.config).unwrap();
    assert_eq!(reloaded.tunnels[0].name, "savedx", "the typed text is kept, and no stray 's' is appended");
}

#[test]
fn f10_saves_from_both_form_stages() {
    // The navigating stage already bound f10; the editing stage used to fall
    // through to `_ => return`, so the same key silently did nothing there.
    for edit_first in [false, true] {
        let (_d, mut app) = form_app();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("testhost");
            f.name.set("via-f10");
            f.remote_port.set("22");
            f.local_port.set("18051");
            f.field = FormField::Name;
        }
        if edit_first {
            handle_key(&mut app, key(KeyCode::Enter));
        }
        handle_key(&mut app, key(KeyCode::F(10)));
        assert!(matches!(app.overlay, Overlay::None), "f10 must save (editing={edit_first})");
        assert_eq!(app.tunnels.len(), 1, "f10 must save (editing={edit_first})");
    }
}

#[test]
fn esc_on_a_dirty_form_asks_before_discarding() {
    // Note: esc is overloaded. In the *navigating* stage it means "leave the
    // form" (which asks when there is unsaved input); in the *editing* stage
    // it means "stop editing this field" and must not pop a dialog.
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().name.set("typed");
    assert!(!app.form.as_ref().unwrap().is_editing_field());
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(matches!(app.overlay, Overlay::Confirm { .. }), "dirty form must confirm discard");
    assert!(app.form.is_some());
}

#[test]
fn esc_while_editing_a_field_only_stops_editing() {
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().field = FormField::Name;
    handle_key(&mut app, key(KeyCode::Enter)); // start editing
    handle_key(&mut app, ch('x'));
    handle_key(&mut app, key(KeyCode::Esc)); // first esc: leave editing
    assert!(!app.form.as_ref().unwrap().is_editing_field());
    assert!(matches!(app.overlay, Overlay::Form), "must stay in the form");
    assert_eq!(app.form.as_ref().unwrap().name.value(), "x");
    // Second esc: now it is a discard request.
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(matches!(app.overlay, Overlay::Confirm { .. }));
}

#[test]
fn esc_on_a_pristine_form_closes_it_directly() {
    let (_d, mut app) = form_app();
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(matches!(app.overlay, Overlay::None), "pristine form needs no confirmation");
    assert!(app.form.is_none());
}

#[test]
fn shift_tab_in_editing_mode_commits_and_moves_back() {
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().field = FormField::Name;
    handle_key(&mut app, key(KeyCode::Enter));
    handle_key(&mut app, ch('z'));
    handle_key(&mut app, key(KeyCode::BackTab));
    assert!(!app.form.as_ref().unwrap().is_editing_field());
    assert_eq!(app.form.as_ref().unwrap().name.value(), "z");
    assert_ne!(app.form.as_ref().unwrap().field, FormField::Name);
}

#[test]
fn a_bogus_kind_index_survives_the_kind_keys() {
    // `kind_idx` is public; a caller writing past the end must not turn the
    // next `l`/`h`/enter into a slice panic.
    let (_d, mut app) = form_app();
    app.form.as_mut().unwrap().kind_idx = 99;
    app.form.as_mut().unwrap().field = FormField::Kind;
    for code in [KeyCode::Char('l'), KeyCode::Char('h'), KeyCode::Enter, KeyCode::Right, KeyCode::Left] {
        handle_key(&mut app, key(code));
        let f = app.form.as_ref().unwrap();
        assert!(
            f.kind_idx < crate::model::ForwardKind::all().len(),
            "{code:?} left kind_idx at {}, past the kind list",
            f.kind_idx
        );
    }
    assert!(app.form.is_some(), "the form must survive");
    // And the resulting kind is one of the real variants, not a panic.
    let kind = app.form.as_ref().unwrap().kind();
    assert!(crate::model::ForwardKind::all().contains(&kind));
}

#[test]
fn a_failed_submit_with_no_free_id_reports_instead_of_panicking() {
    // The id space is exhausted by a hand-edited config; ctrl+s must report
    // it into the form.
    let (_d, mut app) = app_with(vec![tunnel(u64::MAX, "huge", 18001)]);
    app.open_form_create();
    {
        let f = app.form.as_mut().unwrap();
        f.ssh_host.set("host");
        f.name.set("fresh");
        f.remote_port.set("22");
        f.local_port.set("18002");
    }
    handle_key(&mut app, ctrl('s'));
    assert_eq!(app.tunnels.len(), 1, "nothing may be created");
    assert!(app.form.is_some(), "the form stays open with the error");
}

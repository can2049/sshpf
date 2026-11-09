//! Create/edit form: field navigation, text editing, and the save keys.
//!
//! [`super::main::handle_main_key`] never reaches here; the form is
//! `Overlay::Form`, so [`handle_form_key`] is the entry point. It first offers
//! the key to the host dropdown, the peer port list and the import pane — each
//! only claims what it really owns — then dispatches on the two stages:
//!
//! * not editing → [`handle_field_navigating`] (arrows move the highlight,
//!   `enter` commits to editing, enum fields cycle);
//! * editing → [`handle_field_editing`] (keystrokes fill the buffer, `enter`/`esc`
//!   end editing, `tab` ends and advances).
//!
//! `ctrl+s`/`f10` save from both stages. Document new keys in `src/help.rs`
//! (`SectionId::Form` to navigate, `::FormEditing` while typing) and
//! `docs/keymap.md`.

use super::dropdown::handle_dropdown_key;
use super::import::handle_import_key;
use super::ports::handle_port_key;
use crate::app::App;
use crate::model::FormField;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(super) fn handle_form_key(app: &mut App, key: KeyEvent) {
    // An expanded SSH HOST dropdown owns most keys: it is the primary way to
    // pick a host, so ↑↓/enter/esc must drive the list rather than the form.
    if app.host_picker_open() && handle_dropdown_key(app, key) {
        return;
    }
    // The peer port list comes *before* the import pane on purpose. The two can
    // never be open together (`App::open_port_picker` collapses the import pane,
    // and opening the pane means the user is on the SSH HOST field), so the order
    // is not load-bearing — but it is written down rather than left to accident:
    // the list the user most recently asked for is the one whose keys win.
    //
    // Neither pane claims a key while a text field is being edited: `j`/`a`/`i`
    // are ordinary letters in a tunnel name. Without this guard, typing `api`
    // into 名称 would tick every row (`a`) and close the pane (`i`).
    if !app.form.as_ref().is_some_and(|f| f.is_editing_field()) && app.port_picker_open() && handle_port_key(app, key) {
        return;
    }
    // The import pane owns its row/filter keys with the same "not while typing"
    // guard, and the same reason.
    if !app.form.as_ref().is_some_and(|f| f.is_editing_field()) && app.import_pane_open() && handle_import_key(app, key)
    {
        return;
    }

    // Whatever the key was — claimed above or not — a host or kind change made in
    // the previous frame may have left a port list that belongs to a host the
    // form no longer holds. One cheap string compare per press (the function
    // returns immediately when no list is open) is what keeps "these rows are the
    // host I am looking at" true without a callback in every editing path.
    app.sync_port_picker();

    // The form has two stages with different key semantics; dispatch on stage.
    if app.form.as_ref().is_some_and(|f| f.is_editing_field()) {
        handle_field_editing(app, key);
    } else {
        handle_field_navigating(app, key);
    }
}

/// Stage 2: a text field is actively being edited.
///
/// Keystrokes go into the buffer; `enter`/`esc` end editing and `tab` ends it
/// and advances. Typing in the remote port also re-derives the local-port
/// suggestion, and typing in the local port marks it as hand-set so the
/// suggestion stops overwriting it.
fn handle_field_editing(app: &mut App, key: KeyEvent) {
    // Save keys are stage-independent, and they must be matched *before* the
    // generic `Char(c)` path in `edit_field`:
    //
    // * `edit_field` inserts any `Char`, ignoring modifiers, so without this
    //   branch ctrl+s typed the letter `s` into the field instead of saving —
    //   a silent data corruption, not merely a missing shortcut.
    // * `f10` has no `KeyCode::Char`, so it fell through to `_ => return` and
    //   did nothing at all, while the navigating stage saved on it.
    //
    // The literals stay *here* rather than in the helper: `tests/help_coverage.rs`
    // derives the documented key map from this handler's dispatch, so moving them
    // would make `ctrl+s` / `f10` look undocumented.
    let save = matches!(key.code, KeyCode::F(10))
        || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL));
    if save {
        save_from_editing(app);
        return;
    }
    match key.code {
        // Leave editing; the highlight stays where it is.
        KeyCode::Enter | KeyCode::Esc => {
            end_editing(app);
            // A hand-typed alias must resolve its forwards too, or the pane would
            // keep showing the previous host's rows.
            if was_host_field(app) {
                app.refresh_import_for_current_host();
            }
            return;
        }
        // Tab commits and advances, like leaving the field.
        KeyCode::Tab | KeyCode::BackTab => {
            move_field(app, key.code == KeyCode::Tab);
            if let Some(f) = app.form.as_mut() {
                f.end_edit();
            }
            return;
        }
        _ => {}
    }

    let Some(field) = app.form.as_ref().map(|f| f.field) else {
        return;
    };
    // Typing in the local-port field means "I picked this number myself", which
    // stops the remote-port suggestion from overwriting it.
    if field == FormField::LocalPort
        && matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete)
        && let Some(f) = app.form.as_mut()
    {
        f.mark_local_port_manual();
    }
    edit_field(app, field, &key);
    if field == FormField::RemotePort {
        app.refresh_local_port_suggestion();
    }
}

/// Persist the form from the editing stage.
///
/// Ending the edit first keeps the two stages equivalent: `submit_form` validates
/// the same buffers either way, and a pending edit mode left on the form would
/// make the next frame draw a text cursor over a form that is about to close.
fn save_from_editing(app: &mut App) {
    if let Some(f) = app.form.as_mut() {
        f.end_edit();
        f.revalidate();
    }
    app.submit_form();
}

/// Commit the in-progress edit, keeping the field highlight where it is.
fn end_editing(app: &mut App) {
    if let Some(f) = app.form.as_mut() {
        f.end_edit();
        f.revalidate();
    }
}

/// Whether the cursor is on the SSH-host field (checked *before* the edit ends,
/// because ending it is what the caller is about to do).
fn was_host_field(app: &App) -> bool {
    app.form.as_ref().is_some_and(|f| f.field == FormField::SshHost)
}

/// Stage 1: arrows only move the highlight; `enter` commits to editing.
fn handle_field_navigating(app: &mut App, key: KeyEvent) {
    let Some(field) = app.form.as_ref().map(|f| f.field) else {
        return;
    };

    match key.code {
        KeyCode::Esc => {
            // Asking here (rather than just closing) is what protects typed
            // input: this is the branch users reach with one esc.
            app.request_close_form();
        }
        KeyCode::Tab | KeyCode::BackTab => {
            move_field(app, key.code == KeyCode::Tab);
            app.close_host_picker();
        }
        KeyCode::Down => {
            move_field(app, true);
            app.close_host_picker();
        }
        KeyCode::Up => {
            move_field(app, false);
            app.close_host_picker();
        }
        KeyCode::Enter => activate_field(app, field),
        // Ctrl+S saves from anywhere; plain `enter` is field editing now.
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => app.submit_form(),
        KeyCode::F(10) => app.submit_form(),
        _ => adjust_field(app, field, key),
    }
}

/// `enter` on the highlighted field: act for toggles, edit for text fields.
fn activate_field(app: &mut App, field: FormField) {
    match field {
        FormField::Kind => {
            if let Some(f) = app.form.as_mut() {
                f.cycle_kind(true);
            }
        }
        FormField::AutoRestart => {
            if let Some(f) = app.form.as_mut() {
                f.auto_restart = !f.auto_restart;
            }
        }
        // Enter opens the host list; space/i toggle the import pane. Two
        // different jobs on one field, so they get different keys rather than a
        // mode the user has to discover.
        FormField::SshHost => app.open_host_picker(),
        _ => {
            if let Some(f) = app.form.as_mut() {
                f.begin_edit();
            }
        }
    }
}

/// Keys that adjust the enum-like fields while merely navigating.
fn adjust_field(app: &mut App, field: FormField, key: KeyEvent) {
    match field {
        FormField::Kind => match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(f) = app.form.as_mut() {
                    f.cycle_kind(false);
                }
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                if let Some(f) = app.form.as_mut() {
                    f.cycle_kind(true);
                }
            }
            _ => {}
        },
        FormField::AutoRestart => {
            if matches!(key.code, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right)
                && let Some(f) = app.form.as_mut()
            {
                f.auto_restart = !f.auto_restart;
            }
        }
        FormField::SshHost => match key.code {
            // ctrl+h / F2 open the dropdown without entering text-editing.
            KeyCode::F(2) => app.open_host_picker(),
            KeyCode::Char('h') | KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.open_host_picker()
            }
            // space / i: show the forwards this host already declares.
            KeyCode::Char(' ') | KeyCode::Char('i') => app.toggle_import_pane(),
            _ => {}
        },
        // `p` asks the peer what it is listening on and offers the answer for this
        // field. It is dispatched here rather than in the editing stage on
        // purpose: `p` must stay an ordinary letter while a field is being typed,
        // and `adjust_field` only runs while navigating.
        FormField::RemotePort => {
            if let KeyCode::Char('p') = key.code {
                app.open_port_picker();
            }
        }
        _ => {}
    }
}

/// Move the highlight one field forward or backward, re-validating.
fn move_field(app: &mut App, forward: bool) {
    if let Some(f) = app.form.as_mut() {
        f.field = if forward { f.field.next() } else { f.field.prev() };
        f.revalidate();
    }
}

fn edit_field(app: &mut App, field: FormField, key: &KeyEvent) {
    let Some(form) = app.form.as_mut() else {
        return;
    };
    let Some(buf) = form.buf_mut(field) else {
        return;
    };
    match key.code {
        KeyCode::Char(c) => buf.insert(c),
        KeyCode::Backspace => buf.backspace(),
        KeyCode::Delete => buf.delete(),
        KeyCode::Left => buf.left(),
        KeyCode::Right => buf.right(),
        KeyCode::Home => buf.home(),
        KeyCode::End => buf.end(),
        _ => return,
    }
    form.revalidate();
}

#[cfg(test)]
mod tests {
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
}

//! Unit tests for `import.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::app::Overlay;
use crate::keys::handle_key;
use crate::keys::testkit::*;
use crate::model::FormField;

// -- ssh-config import pane --------------------------------------------

#[test]
fn space_on_the_host_field_opens_the_import_pane() {
    let (_d, mut app) = import_app();
    app.form.as_mut().unwrap().field = FormField::SshHost;
    app.form.as_mut().unwrap().import_open = false;
    handle_key(&mut app, ch(' '));
    assert!(app.import_pane_open(), "space must reveal the host's existing forwards");
    // Once open the pane owns `space` (tick the cursor row), so it is not a
    // second toggle: closing is `i`/esc, which is why the footer says so.
    handle_key(&mut app, ch(' '));
    assert!(app.import_pane_open(), "space must still mean tick, not close");
    assert!(!app.form.as_ref().unwrap().import.rows[0].checked);
    handle_key(&mut app, ch('i'));
    assert!(!app.import_pane_open(), "i closes the pane");
}

#[test]
fn the_import_pane_takes_over_the_arrow_keys() {
    let (_d, mut app) = import_app();
    let field_before = app.form.as_ref().unwrap().field;
    handle_key(&mut app, ch('j'));
    assert_eq!(app.form.as_ref().unwrap().import.cursor, 1, "j walks the rows, not the fields");
    assert_eq!(app.form.as_ref().unwrap().field, field_before, "the form field must not move");
    handle_key(&mut app, key(KeyCode::Down));
    assert_eq!(app.form.as_ref().unwrap().import.cursor, 2);
    handle_key(&mut app, ch('k'));
    assert_eq!(app.form.as_ref().unwrap().import.cursor, 1);
    handle_key(&mut app, ch('g'));
    assert_eq!(app.form.as_ref().unwrap().import.cursor, 0);
    handle_key(&mut app, ch('G'));
    assert_eq!(app.form.as_ref().unwrap().import.cursor, 2);
}

#[test]
fn space_in_the_pane_ticks_and_unticks_a_row() {
    let (_d, mut app) = import_app();
    assert!(app.form.as_ref().unwrap().import.rows[0].checked, "rows start ticked");
    handle_key(&mut app, ch(' '));
    assert!(!app.form.as_ref().unwrap().import.rows[0].checked);
    handle_key(&mut app, ch(' '));
    assert!(app.form.as_ref().unwrap().import.rows[0].checked);
}

#[test]
fn typing_a_name_is_not_swallowed_by_the_default_open_pane() {
    // The pane now opens itself, so it must not steal `j`/`a`/`i`/`k` while a
    // text field is being edited: those are ordinary letters in a tunnel name
    // (`api-db`, `jenkins`). Without the guard, ticking a whole batch and
    // closing the pane would happen behind the user as they type.
    let (_d, mut app) = default_open_import_app();
    assert!(app.import_pane_open(), "precondition: the pane is open by default");
    app.form.as_mut().unwrap().field = FormField::Name;
    handle_key(&mut app, key(KeyCode::Enter)); // start editing 名称
    for c in "api-db".chars() {
        handle_key(&mut app, ch(c));
    }
    assert_eq!(app.form.as_ref().unwrap().name.value(), "api-db", "letters must reach the field");
    assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 3, "`a` must not tick everything");
    assert!(app.import_pane_open(), "`i` must not close the pane either");
}

#[test]
fn the_pane_takes_the_keys_back_once_editing_ends() {
    // The guard is scoped to the editing stage, not "forever after the pane
    // opened": `esc` out of the field and `j` walks the rows again.
    let (_d, mut app) = default_open_import_app();
    app.form.as_mut().unwrap().field = FormField::Name;
    handle_key(&mut app, key(KeyCode::Enter));
    handle_key(&mut app, ch('j'));
    assert_eq!(app.form.as_ref().unwrap().name.value(), "j");
    handle_key(&mut app, key(KeyCode::Esc)); // leave editing
    assert!(!app.form.as_ref().unwrap().is_editing_field());
    handle_key(&mut app, ch('j'));
    assert_eq!(app.form.as_ref().unwrap().import.cursor, 1, "once not editing, j is the row cursor again");
    assert_eq!(app.form.as_ref().unwrap().name.value(), "j", "and the field did not grow a second j");
}

#[test]
fn select_all_and_none_keys_work_in_the_pane() {
    let (_d, mut app) = import_app();
    handle_key(&mut app, ch('U'));
    assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 0);
    handle_key(&mut app, ch('a'));
    assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 3);
    handle_key(&mut app, ch('I'));
    assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 0, "inverting a full selection empties it");
}

#[test]
fn f_cycles_through_the_filter_columns() {
    // Regression: `f` used to always jump back to 类型, which made the UI's
    // "f/←→ 选列" footer a lie — you could not reach 监听/目标/原始指令 with it.
    let (_d, mut app) = import_app();
    handle_key(&mut app, ch('f'));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Kind));
    handle_key(&mut app, ch('f'));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Listen));
    handle_key(&mut app, ch('f'));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Target));
    handle_key(&mut app, ch('f'));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Raw));
    handle_key(&mut app, ch('f'));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Kind), "cycling wraps");
}

#[test]
fn f_starts_a_column_filter_and_typing_reaches_it() {
    let (_d, mut app) = import_app();
    handle_key(&mut app, ch('f'));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Kind));
    handle_key(&mut app, ch('L'));
    assert_eq!(app.form.as_ref().unwrap().import.filter(ImportColumn::Kind), "L");
    // Enter ends editing; a later key is not appended to the filter.
    handle_key(&mut app, key(KeyCode::Enter));
    assert!(app.form.as_ref().unwrap().import.editing.is_none());
    handle_key(&mut app, ch('X'));
    assert_eq!(app.form.as_ref().unwrap().import.filter(ImportColumn::Kind), "L", "typing must not leak in");
}

#[test]
fn arrows_cycle_the_filter_column() {
    let (_d, mut app) = import_app();
    handle_key(&mut app, ch('f'));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Kind));
    handle_key(&mut app, key(KeyCode::Right));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Listen));
    handle_key(&mut app, key(KeyCode::Left));
    assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Kind));
}

#[test]
fn backspace_and_escape_handle_filter_editing() {
    let (_d, mut app) = import_app();
    handle_key(&mut app, ch('f'));
    handle_key(&mut app, ch('1'));
    handle_key(&mut app, ch('8'));
    handle_key(&mut app, key(KeyCode::Backspace));
    assert_eq!(app.form.as_ref().unwrap().import.filter(ImportColumn::Kind), "1");
    // First esc leaves editing but keeps the pane and the filter.
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(app.form.as_ref().unwrap().import.editing.is_none());
    assert!(app.import_pane_open());
    assert_eq!(app.form.as_ref().unwrap().import.filter(ImportColumn::Kind), "1");
}

#[test]
fn escape_closes_the_pane_when_not_filtering() {
    let (_d, mut app) = import_app();
    handle_key(&mut app, key(KeyCode::Esc));
    assert!(!app.import_pane_open(), "esc with no filter editing collapses the pane");
    assert!(matches!(app.overlay, Overlay::Form));
}

#[test]
fn ctrl_r_clears_every_filter_and_z_undoes() {
    let (_d, mut app) = import_app();
    app.form.as_mut().unwrap().import.set_filter(ImportColumn::Kind, "L");
    app.form.as_mut().unwrap().import.set_filter(ImportColumn::Listen, "1800");
    handle_key(&mut app, ctrl('r'));
    assert!(!app.form.as_ref().unwrap().import.has_filters());
    handle_key(&mut app, ch('z'));
    assert!(app.form.as_ref().unwrap().import.has_filters(), "z must bring the filters back");
    // `U` untikcs rather than undoing, so it must not be mistaken for undo.
    handle_key(&mut app, ch('U'));
    assert!(app.import_pane_open());
}

#[test]
fn uppercase_f_clears_the_active_column_filter() {
    let (_d, mut app) = import_app();
    app.form.as_mut().unwrap().import.set_filter(ImportColumn::Listen, "18");
    handle_key(&mut app, ch('F'));
    assert_eq!(app.form.as_ref().unwrap().import.filter(ImportColumn::Listen), "");
    // With nothing filtered, F says so instead of doing nothing silently.
    handle_key(&mut app, ch('F'));
    let msg = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
    assert!(msg.contains("没有筛选"), "{msg}");
}

#[test]
fn saving_with_ticked_rows_creates_the_whole_batch() {
    let (_d, mut app) = import_app();
    handle_key(&mut app, ctrl('s'));
    assert_eq!(app.tunnels.len(), 3, "one tunnel per ticked row");
    assert!(matches!(app.overlay, Overlay::None), "the form closes on success");
}

#[test]
fn a_save_key_is_never_filter_text_even_with_a_column_focused() {
    // The reported failure, driven the way the user actually presses it: the
    // pane is open, a column filter owns the keyboard (one stray `←`/`f` is
    // enough), the form holds the forward they want, they press `ctrl+s`. The
    // filter catch-all `Char(c) if !c.is_control()` used to see the save key as
    // the letter `s`, so the key became filter text, the save never happened,
    // and nothing was logged — the tunnel simply never appeared.
    let (_d, mut app) = default_open_import_app();
    // Clear the pane's default ticks first: this case is about *dispatch*, and
    // a still-ticked pane would legitimately win the submit for its own batch
    // (that is the documented routing rule, tested elsewhere). What matters
    // here is that the focused filter cannot eat the key.
    app.import_uncheck_all();
    app.begin_import_filter(crate::model::ImportColumn::Raw);
    {
        let f = app.form.as_mut().unwrap();
        f.name.set("typed-3081");
        f.remote_host.set("127.0.0.1");
        f.remote_port.set("3081");
        f.local_port.set("3081");
        f.mark_local_port_manual();
    }
    assert!(crate::keys::is_save_key(ctrl('s')), "precondition: ctrl+s is the save key");

    handle_key(&mut app, ctrl('s'));

    assert!(
        app.tunnels.iter().any(|t| t.cfg.remote_port == 3081),
        "ctrl+s must save even with a column filter focused: {:?}",
        app.form.as_ref().map(|f| f.errors.clone())
    );
}

#[test]
fn the_import_pane_scrolls_its_window_with_the_cursor() {
    // `IMPORT_ROWS_VISIBLE` is shared by the key handler (this window) and
    // the renderer (pane height). Walking past the window must move `scroll`
    // with the cursor, or `import_end` leaves the last row out of sight.
    let (_d, mut app) = import_app();
    for _ in 0..(IMPORT_ROWS_VISIBLE + 2) {
        handle_key(&mut app, ch('j'));
    }
    let import = &app.form.as_ref().unwrap().import;
    assert!(import.scroll as usize <= import.cursor, "scroll must follow the cursor down");
    assert!(
        import.cursor < import.scroll as usize + IMPORT_ROWS_VISIBLE,
        "the cursor must stay inside the window: cursor={} scroll={}",
        import.cursor,
        import.scroll
    );
}

#[test]
fn import_end_brings_the_last_row_into_view() {
    // `G` set the cursor without scrolling, so the pane kept showing the top
    // of the list while the highlight was off-screen.
    let (_d, mut app) = import_app();
    handle_key(&mut app, ch('G'));
    let import = &app.form.as_ref().unwrap().import;
    assert!(import.cursor < import.scroll as usize + IMPORT_ROWS_VISIBLE, "the last row must be visible");
}

#[test]
fn import_home_scrolls_back_to_the_top() {
    let (_d, mut app) = import_app();
    handle_key(&mut app, ch('G'));
    handle_key(&mut app, ch('g'));
    let import = &app.form.as_ref().unwrap().import;
    assert_eq!(import.cursor, 0);
    assert_eq!(import.scroll, 0, "home must reset the window");
}

#[test]
fn paging_in_the_pane_clamps_at_both_ends() {
    let (_d, mut app) = import_app();
    for _ in 0..10 {
        handle_key(&mut app, key(KeyCode::PageDown));
    }
    let last = app.form.as_ref().unwrap().import.visible().len().saturating_sub(1);
    assert_eq!(app.form.as_ref().unwrap().import.cursor, last);
    for _ in 0..10 {
        handle_key(&mut app, key(KeyCode::PageUp));
    }
    assert_eq!(app.form.as_ref().unwrap().import.cursor, 0);
}

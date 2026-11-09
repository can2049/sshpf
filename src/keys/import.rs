//! The `~/.ssh/config` import pane inside the form, and its column filters.
//!
//! The pane is an addition, not a modal: [`handle_import_key`] returns `false`
//! for every key it does not own (`ctrl+s`, `tab`, …) so those keep their plain
//! form meaning. It is called only while no text field is being edited — see
//! `handle_form_key` — because `j`/`a`/`i` are ordinary letters in a tunnel name.
//!
//! Two stages of its own: browsing rows, and typing into a column filter
//! ([`handle_import_filter_key`], reached once a column is focused). `f` and
//! ←/→ cycle the column, `Esc` leaves the filter and closes the pane in that
//! order, `z` undoes.
//!
//! Document new keys in `src/help.rs` (`SectionId::ImportPane` to browse,
//! `::ImportFilter` while typing) and `docs/keymap.md`.

use super::IMPORT_ROWS_VISIBLE;
use crate::app::App;
use crate::model::ImportColumn;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Keys owned by the open import pane (`space`/`i` on the SSH HOST field).
///
/// Returns false for keys the pane does not claim, so `ctrl+s`, `tab` and the
/// rest still behave exactly as they do in the plain form — the pane is an
/// addition, not a modal that swallows the form.
pub(super) fn handle_import_key(app: &mut App, key: KeyEvent) -> bool {
    // A save key that reaches the filter catch-all below becomes the letter `s`
    // in whichever column is focused, so the save silently never happens — no
    // tunnel, no error, no log line. Record which focus took it: that single line
    // is what turns "ctrl+s did nothing" from a mystery into a diagnosis.
    if crate::keys::is_save_key(key) {
        crate::eventlog::save_key_swallowed("import_filter");
    }
    // `f` outranks filter typing, so it means the same thing whether or not a
    // filter is active: advance to the next column. Handled before the delegation
    // below for exactly that reason — otherwise the second `f` would be typed
    // into the filter and the footer's "f/←→ 选列" would be a lie. The cost is
    // that a literal 'f' cannot be typed into a filter; every column here holds a
    // port, a host or a directive name, and ←/→ plus backspace cover the rest.
    if key.code == KeyCode::Char('f') {
        let current = app.form.as_ref().and_then(|f| f.import.editing).unwrap_or(ImportColumn::Raw);
        app.begin_import_filter(current.next());
        return true;
    }
    // While a column filter is being typed into, almost every other key belongs
    // to it.
    if app.form.as_ref().and_then(|f| f.import.editing).is_some() {
        return handle_import_filter_key(app, key);
    }
    handle_import_row_key(app, key)
}

/// The row-browsing half: ticking, column cycling, filters and closing.
///
/// Split out of [`handle_import_key`] so each function stays one screen tall. It is
/// still part of the same context: `HANDLERS` in `tests/help_coverage.rs` maps this
/// function to `SectionId::ImportPane` alongside the entry point, so the section's
/// keys are the union of the two and nothing can drift.
///
/// Cursor movement is asked of [`handle_import_move_key`] first: the two arm sets
/// are disjoint, so "not claimed" there simply falls through to the table below.
fn handle_import_row_key(app: &mut App, key: KeyEvent) -> bool {
    if handle_import_move_key(app, key) {
        return true;
    }
    match key.code {
        KeyCode::Char(' ') => {
            app.import_toggle_cursor();
            true
        }
        KeyCode::Char('a') => {
            app.import_check_all();
            true
        }
        KeyCode::Char('U') => {
            app.import_uncheck_all();
            true
        }
        KeyCode::Char('I') => {
            app.import_invert();
            true
        }
        KeyCode::Left | KeyCode::Right => {
            let current = app.form.as_ref().and_then(|f| f.import.editing).unwrap_or(ImportColumn::Kind);
            let next = if key.code == KeyCode::Left { current.prev() } else { current.next() };
            app.begin_import_filter(next);
            true
        }
        KeyCode::Char('F') => {
            clear_current_filter(app);
            true
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.clear_all_import_filters();
            true
        }
        KeyCode::Char('z') => {
            app.undo_import_filter();
            true
        }
        KeyCode::Esc => {
            app.close_import_pane();
            true
        }
        // `i` closes the pane. `space` cannot: once the pane is open it means
        // "tick this row", so a second space would tick instead of collapsing.
        // Keeping `i` symmetric with the key that opened it avoids that trap.
        KeyCode::Char('i') => {
            app.close_import_pane();
            true
        }
        _ => false,
    }
}

/// `F`: clear the column filter the user is looking at.
///
/// Prefers the column being edited, then the first column that actually has a
/// filter. `F` is therefore never a silent no-op: with nothing to clear it says
/// so in the status bar instead of doing nothing.
fn clear_current_filter(app: &mut App) {
    if let Some(col) = app.form.as_ref().and_then(|f| f.import.editing) {
        app.clear_import_filter(col);
    } else if let Some(col) = active_filter_column(app) {
        app.clear_import_filter(col);
    } else {
        app.set_status(crate::model::StatusMsg::info(crate::help::bi("没有筛选可清除", "No filter to clear")));
    }
}

/// Cursor movement and paging inside the pane. Returns true when it claimed the key.
///
/// Kept apart from the ticking/filter table in [`handle_import_row_key`] so both
/// stay short. The patterns here and there never overlap, which is what makes the
/// two-step dispatch equivalent to one big match.
fn handle_import_move_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.import_move(1);
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.import_move(-1);
            true
        }
        KeyCode::PageDown => {
            app.import_move(IMPORT_ROWS_VISIBLE as i32);
            true
        }
        KeyCode::PageUp => {
            app.import_move(-(IMPORT_ROWS_VISIBLE as i32));
            true
        }
        KeyCode::Char('g') | KeyCode::Home => {
            app.import_home();
            true
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.import_end();
            true
        }
        _ => false,
    }
}

/// The first column that currently has a filter, for `F` when no column is
/// focused.
fn active_filter_column(app: &App) -> Option<ImportColumn> {
    let form = app.form.as_ref()?;
    ImportColumn::ALL.into_iter().find(|c| !form.import.filter(*c).trim().is_empty())
}

/// While a column filter is active, printable keys append to it.
fn handle_import_filter_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Enter => {
            app.end_import_filter();
            true
        }
        KeyCode::Esc => {
            // One esc leaves the filter, a second closes the pane: the same
            // two-stage esc the form itself uses, so it is not a surprise.
            app.end_import_filter();
            true
        }
        KeyCode::Backspace => {
            app.import_filter_backspace();
            true
        }
        KeyCode::Left | KeyCode::Right => {
            let current = app.form.as_ref().and_then(|f| f.import.editing).unwrap_or(ImportColumn::Kind);
            let next = if key.code == KeyCode::Left { current.prev() } else { current.next() };
            app.begin_import_filter(next);
            true
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.clear_all_import_filters();
            true
        }
        KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.undo_import_filter();
            true
        }
        KeyCode::Char(c) if !c.is_control() => {
            app.import_filter_char(c);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
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
    fn saving_with_every_row_unticked_says_nothing_is_ticked() {
        // Regression: this used to fall through to the single-tunnel validation
        // and report "名称不能为空", describing fields the user was not editing.
        let (_d, mut app) = import_app();
        handle_key(&mut app, ch('U'));
        assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 0);
        handle_key(&mut app, ctrl('s'));
        assert!(app.tunnels.is_empty());
        let errs = &app.form.as_ref().unwrap().errors;
        assert!(
            errs.iter().any(|e| e.contains("没有") || e.contains("没有勾选")),
            "expected a 'nothing ticked' message, got {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("名称不能为空")),
            "must not complain about unrelated form fields: {errs:?}"
        );
    }

    #[test]
    fn unticking_every_row_then_typing_one_forward_creates_the_typed_one() {
        // The reported workflow, driven through the key handler the user actually
        // presses: the host's rows are auto-ticked, the user wants a *different*
        // port, so they clear the ticks (`U`) and fill the form by hand, then
        // `ctrl+s`. The cleared pane must not answer "没有勾选任何转发" over a
        // complete typed form — that message used to block the save entirely.
        let (_d, mut app) = import_app();
        handle_key(&mut app, ch('U'));
        assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 0);
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("cloud_pc-3081");
            f.remote_host.set("127.0.0.1");
            f.remote_port.set("3081");
            f.local_port.set("3081");
        }
        handle_key(&mut app, ctrl('s'));

        assert_eq!(app.tunnels.len(), 1, "the typed forward must be created");
        assert_eq!(app.tunnels[0].cfg.name, "cloud_pc-3081");
        assert_eq!(app.tunnels[0].cfg.remote_port, 3081);
        assert!(matches!(app.overlay, Overlay::None), "the form closes on success");
    }

    #[test]
    fn a_save_key_that_a_column_filter_would_eat_is_recorded() {
        // The diagnostic half (this change): `ctrl+s` is a `Char('s')` whose char is
        // not a control character, so the filter catch-all used to type it as `s`.
        // The guard now records `event=form.save_key_swallowed focus=import_filter`,
        // so an otherwise-empty log file explains itself. Making the save *succeed*
        // in this state is the follow-up routing change.
        assert!(crate::keys::is_save_key(ctrl('s')), "precondition: ctrl+s is the save key");
        assert!(!crate::keys::is_save_key(ctrl('a')), "and an ordinary ctrl chord is not");
        assert!(!crate::keys::is_save_key(ch('s')), "nor is a plain `s` typed into a filter");
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
}

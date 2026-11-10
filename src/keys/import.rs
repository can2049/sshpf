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
    // A save key is not filter text: the catch-all below is
    // `Char(c) if !c.is_control()`, and `ctrl+s` is a `Char('s')` whose `char` is
    // not a control character, so without this guard the key is typed into
    // whichever column is focused and the save silently never happens — no tunnel,
    // no error, no log line. Record the near-miss (the WARN is what makes an
    // otherwise-empty log file explain itself) and hand the key back: the pane's
    // own footer advertises `ctrl+s 保存`, so it must honour it.
    if crate::keys::is_save_key(key) {
        crate::eventlog::save_key_swallowed("import_filter");
        return false;
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
mod tests;

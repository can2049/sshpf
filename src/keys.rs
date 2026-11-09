//! Key dispatch — the map. Start here, then open the file for the context.
//!
//! Turns a `crossterm` event into `App` mutations. This root file is the only
//! part visible outside the module: [`handle_key`] and [`handle_mouse`] are the
//! two entry points `main`'s event loop calls, and [`IMPORT_ROWS_VISIBLE`] is
//! shared with the renderer so the scroll window cannot disagree with the pane
//! height. Every handler below is `pub(super)` and lives in the submodule named
//! after the context it serves.
//!
//! # Where a key goes
//!
//! The submodules are private (only [`handle_key`]/[`handle_mouse`] are public), so
//! their names below are plain code, not doc links — read them as file paths under
//! `src/keys/`.
//!
//! | Submodule | Context | Called by | Add a key there when… |
//! |---|---|---|---|
//! | `keys/main.rs` | main table, no overlay | `handle_key` | it acts on the tunnel table |
//! | `keys/select.rs` | selection mode (`v`) | `main::handle_main_key` | it re-binds a table key while marking rows |
//! | `keys/form.rs` | create/edit form, both stages | `handle_key` | it navigates fields or types into one |
//! | `keys/import.rs` | import pane and its column filters | `form::handle_form_key` | it moves, ticks or filters import rows |
//! | `keys/ports.rs` | peer port list (`p` on 远端端口) | `form::handle_form_key` | it moves, ticks or accepts a scanned port |
//! | `keys/dropdown.rs` | `SSH HOST` dropdown | `form::handle_form_key` | it drives the host list |
//! | `keys/misc.rs` | help / detail / confirm / `/` filter | `handle_key` | it belongs to one small overlay |
//!
//! # Adding a key is a three-file change
//!
//! `tests/help_coverage.rs` machine-checks the two hand-written lists against
//! the dispatch, in both directions, at compile time — it embeds every file here
//! that binds keys (see `KEYS_SRC`). A key is only "added" once all three agree:
//!
//! 1. dispatch it in the submodule above;
//! 2. document it in `src/help.rs`, in the `SectionId` matching that context;
//! 3. spell it in `docs/keymap.md`.
//!
//! Otherwise the build fails: dispatched-but-undocumented (a silent gap) and
//! documented-but-not-dispatched (a lie) are both errors. `HANDLERS` in that
//! test maps each handler to its section — update it when a handler here is
//! renamed, moved, or newly split out of another one.
//!
//! # Routing
//!
//! [`handle_key`] checks `ctrl+c` first (quit confirmation from any context),
//! then dispatches on `app.overlay`. [`handle_mouse`] forwards to
//! `App::handle_mouse`, whose `HitMap` already identifies the target: mouse
//! gestures are prose in the help, so the test checks `src/ui/hit.rs` for them
//! instead of this file.
//!
//! Shared unit-test fixtures live in `testkit` (test-only, no production code).

use crate::app::{App, Overlay};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use self::form::handle_form_key;
use self::main::handle_main_key;
use self::misc::{handle_confirm_key, handle_detail_key, handle_filter_key, handle_help_key};

mod dropdown;
mod form;
mod import;
mod main;
mod misc;
mod ports;
mod select;
#[cfg(test)]
mod testkit;

/// How many import rows the pane shows at once.
///
/// Shared by the key handler (scroll window) and the renderer (pane height) so
/// the two cannot disagree about how far the cursor can move before it scrolls.
pub const IMPORT_ROWS_VISIBLE: usize = 8;

/// Whether a key press is one of the two **save** bindings (`ctrl+s` / `f10`).
///
/// Shared by every owner that can see a form keystroke before the form's own
/// dispatch does. The host dropdown and the import pane's column filters both end
/// their dispatch with a `KeyCode::Char(c) if !c.is_control()` catch-all — and
/// `ctrl+s` is a `Char('s')` whose `char` is *not* a control character, so that
/// arm typed the save key into a text buffer instead of saving. The user got no
/// tunnel, no error and no log line, which is what made the report 「`ctrl+s` 之后
/// 端口没有得到转发」 impossible to diagnose from the log file.
///
/// One predicate, so the owners cannot disagree about which keys count.
///
/// `pub(crate)` and deliberately **not** listed in `HANDLERS`
/// (`tests/help_coverage.rs`): this is not a dispatcher — it binds no key of its
/// own — and listing it would attribute these literals to a section that does not
/// own them.
pub(crate) fn is_save_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::F(10))
        || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Route one key press to the handler for the active overlay.
///
/// This is the single entry point `main`'s event loop calls, which is what
/// makes the whole key map testable.
pub fn handle_key(app: &mut App, key: KeyEvent) {
    // Ctrl-C always quits the same way `q` does.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.open_confirm_quit();
        return;
    }
    match app.overlay.clone() {
        Overlay::None => handle_main_key(app, key),
        Overlay::Help => handle_help_key(app, key),
        Overlay::Form => handle_form_key(app, key),
        Overlay::Confirm { .. } => handle_confirm_key(app, key),
        Overlay::Detail(_) => handle_detail_key(app, key),
        Overlay::Filter => handle_filter_key(app, key),
    }
}

/// Route one mouse event. Separate from [`handle_key`] because `main`'s event
/// loop receives `Event::Mouse` and must not have to know about overlays; the
/// `HitMap` the renderer recorded already identifies the target.
pub fn handle_mouse(app: &mut App, mouse: crossterm::event::MouseEvent) {
    app.handle_mouse(mouse);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ConfirmAction;
    use crate::keys::testkit::*;
    use crate::model::{BatchAction, ImportColumn};

    // -- the event-loop entry points ---------------------------------------

    #[test]
    fn ctrl_c_asks_before_quitting() {
        let (_d, mut app) = app_with(vec![]);
        handle_key(&mut app, ctrl('c'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        assert!(!app.should_quit);
    }

    // -- mouse dispatch ----------------------------------------------------

    // A click must land on the `HitMap` target the renderer recorded, and a
    // modal must swallow it instead of passing it to the table underneath.

    #[test]
    fn a_modal_swallows_clicks_instead_of_passing_them_to_the_table() {
        // Regression found by cross-checking the docs against the code: help,
        // confirm and detail recorded no hit of their own, so a click on the
        // dialog landed on the table row painted underneath it.
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = three_app();
        app.selected = 0;
        handle_key(&mut app, ch('v'));
        handle_key(&mut app, ch('u')); // no-op; marks stay empty
        // Open a confirm dialog, then paint the blocked popup over row 3.
        app.hits.clear();
        app.hits.push(ratatui::layout::Rect { x: 0, y: 4, width: 60, height: 1 }, HitTarget::TunnelRow(3));
        app.hits.push(ratatui::layout::Rect { x: 0, y: 3, width: 40, height: 8 }, HitTarget::Popup);
        let consumed =
            app.handle_mouse(mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 5, 4));
        assert!(consumed, "a click on the modal must be consumed");
        assert_eq!(app.selected, 0, "the click must not move the table cursor");
        assert!(app.select_mode.is_empty(), "and must not mark a row behind the dialog");
    }

    #[test]
    fn a_click_on_a_table_row_moves_the_cursor() {
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = three_app();
        app.hits.clear();
        app.hits.push(ratatui::layout::Rect { x: 0, y: 4, width: 60, height: 1 }, HitTarget::TunnelRow(3));
        handle_mouse(
            &mut app,
            mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 5, 4),
        );
        assert_eq!(app.selected, 2, "the click must land on tunnel 3's row");
        assert!(!app.select_mode.active, "a plain click must not enter selection mode");
    }

    #[test]
    fn a_click_in_selection_mode_toggles_the_mark() {
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = three_app();
        app.selected = 0;
        handle_key(&mut app, ch('v'));
        app.hits.clear();
        app.hits.push(ratatui::layout::Rect { x: 0, y: 4, width: 60, height: 1 }, HitTarget::TunnelRow(2));
        handle_mouse(
            &mut app,
            mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 5, 4),
        );
        assert_eq!(app.selected, 1);
        assert!(app.select_mode.is_marked(2), "clicking a row in selection mode marks it");
    }

    #[test]
    fn a_click_on_empty_space_does_nothing() {
        let (_d, mut app) = three_app();
        app.selected = 2;
        app.hits.clear();
        let consumed =
            app.handle_mouse(mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 5, 40));
        assert!(!consumed);
        assert_eq!(app.selected, 2, "a miss must not move anything");
    }

    #[test]
    fn clicking_an_import_checkbox_toggles_that_row() {
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = import_app();
        assert!(app.form.as_ref().unwrap().import.rows[1].checked);
        app.hits.clear();
        app.hits.push(ratatui::layout::Rect { x: 2, y: 6, width: 3, height: 1 }, HitTarget::ImportCheckbox(1));
        app.handle_mouse(mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 3, 6));
        assert!(!app.form.as_ref().unwrap().import.rows[1].checked, "the clicked box must flip");
        assert_eq!(app.form.as_ref().unwrap().import.cursor, 1, "and the cursor follows the click");
    }

    #[test]
    fn clicking_an_import_header_starts_filtering_that_column() {
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = import_app();
        app.hits.clear();
        app.hits.push(
            ratatui::layout::Rect { x: 10, y: 5, width: 8, height: 1 },
            HitTarget::ImportColumnHeader(ImportColumn::Target),
        );
        app.handle_mouse(mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 12, 5));
        assert_eq!(app.form.as_ref().unwrap().import.editing, Some(ImportColumn::Target));
    }

    #[test]
    fn clicking_a_filter_chip_deletes_it() {
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = import_app();
        app.form.as_mut().unwrap().import.set_filter(ImportColumn::Kind, "L");
        app.hits.clear();
        app.hits.push(
            ratatui::layout::Rect { x: 0, y: 20, width: 4, height: 1 },
            HitTarget::ImportFilterChip(ImportColumn::Kind),
        );
        app.handle_mouse(mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 1, 20));
        assert_eq!(app.form.as_ref().unwrap().import.filter(ImportColumn::Kind), "");
    }

    #[test]
    fn clicking_a_batch_action_button_asks_for_confirmation() {
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = three_app();
        app.selected = 0;
        handle_key(&mut app, ch('v'));
        handle_key(&mut app, ch('a')); // mark everything
        app.hits.clear();
        app.hits.push(
            ratatui::layout::Rect { x: 10, y: 30, width: 6, height: 1 },
            HitTarget::BatchAction(BatchAction::Delete),
        );
        app.handle_mouse(mouse(crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left), 12, 30));
        match &app.overlay {
            Overlay::Confirm { action: ConfirmAction::Batch { action, .. }, .. } => {
                assert_eq!(*action, BatchAction::Delete);
            }
            other => panic!("expected a batch delete confirmation, got {other:?}"),
        }
        assert_eq!(app.tunnels.len(), 3, "the button must not act before confirmation");
    }

    #[test]
    fn the_wheel_moves_the_cursor_over_the_table() {
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = three_app();
        app.hits.clear();
        app.hits.push(ratatui::layout::Rect { x: 0, y: 4, width: 60, height: 3 }, HitTarget::TunnelList);
        app.handle_mouse(mouse(crossterm::event::MouseEventKind::ScrollDown, 5, 5));
        assert_eq!(app.selected, 2, "one wheel notch scrolls a few rows, clamped at the end");
        app.handle_mouse(mouse(crossterm::event::MouseEventKind::ScrollUp, 5, 5));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn the_wheel_scrolls_the_import_list() {
        use crate::ui::hit::HitTarget;
        let (_d, mut app) = import_app();
        app.hits.clear();
        app.hits.push(ratatui::layout::Rect { x: 0, y: 10, width: 40, height: 3 }, HitTarget::ImportList);
        app.handle_mouse(mouse(crossterm::event::MouseEventKind::ScrollDown, 2, 11));
        assert_eq!(app.form.as_ref().unwrap().import.cursor, 2, "wheel moves the import cursor (clamped)");
    }
}

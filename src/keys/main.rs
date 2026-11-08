//! Main screen: the keys that act on the tunnel table when no overlay is open.
//!
//! `handle_key` routes `Overlay::None` here. With selection mode active this
//! immediately delegates to [`super::select`]: the same key codes get different
//! meanings while rows are being marked, so one table per stage beats
//! interleaving conditions into a single match.
//!
//! Everything row-shaped (`j`/`k`, `g`/`G`, enter, `r`, `d`, `a`, `x`, …) is
//! bound here; the dialogs those keys open belong to [`super::misc`], and the
//! form to [`super::form`]. Adding a table key: bind it here, then document it
//! in `src/help.rs` (`SectionId::Main`) and `docs/keymap.md`.

use super::select::handle_select_key;
use crate::app::{App, Overlay};
use crossterm::event::{KeyCode, KeyEvent};

pub(super) fn handle_main_key(app: &mut App, key: KeyEvent) {
    // Selection mode re-binds a handful of keys (space marks instead of
    // toggling, uppercase batch actions), so it gets its own table rather than
    // interleaving conditions into the normal one.
    if app.select_mode.active {
        handle_select_key(app, key);
        return;
    }
    match key.code {
        KeyCode::Char('q') => app.open_confirm_quit(),
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),
        KeyCode::Char(' ') => app.toggle_selected(),
        KeyCode::Char('r') => app.restart_selected(),
        KeyCode::Char('n') => app.open_form_create(),
        KeyCode::Char('e') | KeyCode::Char('E') => app.open_form_edit(),
        KeyCode::Char('d') => app.open_confirm_delete(),
        KeyCode::Enter | KeyCode::Char('l') => app.open_detail(),
        KeyCode::Char('/') => {
            app.overlay = Overlay::Filter;
        }
        KeyCode::Char('a') => app.request_start_all(),
        KeyCode::Char('x') => app.request_stop_all(),
        KeyCode::Char('?') => app.open_help(),
        // `v` enters selection mode ON the current row, which is what the user
        // is looking at; a mode that starts empty would need a second gesture
        // before anything can be marked.
        KeyCode::Char('v') | KeyCode::Char('V') => app.toggle_select_mode(),
        KeyCode::Esc => app.filter.clear(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::handle_key;
    use crate::keys::testkit::*;
    use crate::model::TunnelStatus;
    use std::time::Instant;

    // -- main screen -------------------------------------------------------

    #[test]
    fn navigation_keys_move_the_cursor() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001), tunnel(2, "b", 18002), tunnel(3, "c", 18003)]);
        assert_eq!(app.selected, 0);
        handle_key(&mut app, ch('j'));
        assert_eq!(app.selected, 1);
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.selected, 2);
        // Clamps at the end rather than wrapping.
        handle_key(&mut app, ch('j'));
        assert_eq!(app.selected, 2);
        handle_key(&mut app, ch('k'));
        assert_eq!(app.selected, 1);
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(app.selected, 0);
        // Clamps at the start.
        handle_key(&mut app, ch('k'));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn g_and_shift_g_jump_to_ends() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001), tunnel(2, "b", 18002), tunnel(3, "c", 18003)]);
        handle_key(&mut app, ch('G'));
        assert_eq!(app.selected, 2);
        handle_key(&mut app, ch('g'));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn q_asks_before_quitting() {
        let (_d, mut app) = app_with(vec![]);
        handle_key(&mut app, ch('q'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }), "q must confirm");
        assert!(!app.should_quit, "must not quit before confirmation");
    }

    #[test]
    fn d_asks_before_deleting() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.selected = 0;
        handle_key(&mut app, ch('d'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        assert_eq!(app.tunnels.len(), 1, "delete must wait for confirmation");
    }

    #[test]
    fn space_stops_only_after_confirmation() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.selected = 0;
        app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
        handle_key(&mut app, ch(' '));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        // Still up while the dialog is open.
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }));
    }

    #[test]
    fn enter_and_l_open_detail() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.selected = 0;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::Detail(1)));
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        handle_key(&mut app, ch('l'));
        assert!(matches!(app.overlay, Overlay::Detail(1)));
    }

    #[test]
    fn a_and_x_ask_before_bulk_changes() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001), tunnel(2, "b", 18002)]);
        handle_key(&mut app, ch('a'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }), "start-all must confirm");
        handle_key(&mut app, key(KeyCode::Esc));

        app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
        handle_key(&mut app, ch('x'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }), "stop-all must confirm");
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }));
    }

    #[test]
    fn n_opens_a_create_form() {
        let (_d, mut app) = app_with(vec![]);
        handle_key(&mut app, ch('n'));
        assert!(matches!(app.overlay, Overlay::Form));
        let form = app.form.as_ref().unwrap();
        assert!(matches!(form.mode, crate::model::FormMode::Create));
        assert!(!form.is_editing_field(), "form opens in navigating mode");
    }

    #[test]
    fn e_opens_an_edit_form_for_the_selection() {
        let (_d, mut app) = app_with(vec![tunnel(7, "editable", 18001)]);
        app.selected = 0;
        handle_key(&mut app, ch('e'));
        assert!(matches!(app.overlay, Overlay::Form));
        assert_eq!(app.form.as_ref().unwrap().name.value(), "editable");
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        let before = app.selected;
        for k in ['Z', '9', '%', '!'] {
            handle_key(&mut app, ch(k));
        }
        assert_eq!(app.selected, before);
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn normal_keys_still_work_outside_selection_mode() {
        // Guard against the select-mode branch swallowing everything.
        let (_d, mut app) = three_app();
        app.selected = 0;
        handle_key(&mut app, ch('e'));
        assert!(matches!(app.overlay, Overlay::Form), "e must still edit");
        handle_key(&mut app, key(KeyCode::Esc));
        handle_key(&mut app, ch('d'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }), "d must still delete");
    }

    #[test]
    fn keys_on_an_empty_table_do_not_panic() {
        // Every main-screen key against zero rows: navigation, toggles, detail,
        // delete and the batch keys must all be no-ops.
        let (_d, mut app) = app_with(vec![]);
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::Char(' '),
            KeyCode::Char('r'),
            KeyCode::Char('e'),
            KeyCode::Char('d'),
            KeyCode::Enter,
            KeyCode::Char('l'),
            KeyCode::Char('v'),
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Home,
            KeyCode::End,
        ] {
            handle_key(&mut app, key(code));
            // Dismiss whatever overlay the key opened, so the next key is routed
            // to the main screen again.
            app.overlay = Overlay::None;
            app.form = None;
        }
        assert!(app.tunnels.is_empty());
        assert!(!app.should_quit);
    }
}

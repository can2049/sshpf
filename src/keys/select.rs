//! Selection mode (`v`): the keys that are *re-bound*, not merely added.
//!
//! [`super::main::handle_main_key`] hands the whole key press over while
//! selection mode is active, so this file is the complete key map for that
//! stage. Uppercase `R`/`D`/`S` are the batch actions so they cannot be confused
//! with the single-row `r`/`d`; every batch action still goes through the same
//! confirmation gate.
//!
//! Document new keys in `src/help.rs` (`SectionId::Selection`) and
//! `docs/keymap.md`.

use crate::app::App;
use crate::model::BatchAction;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Keys that only exist while selection mode is active.
///
/// Uppercase `R`/`D` are batch restart/delete so they cannot be confused with
/// the single-row `r`/`d` of the normal table; both still go through the same
/// confirmation gate.
pub(super) fn handle_select_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('v') | KeyCode::Char('V') => app.exit_select_mode(),
        KeyCode::Char('j') | KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.select_next();
            app.extend_mark_to_cursor();
        }
        KeyCode::Char('k') | KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.select_prev();
            app.extend_mark_to_cursor();
        }
        // Shift+J / Shift+K arrive as uppercase chars on most terminals.
        KeyCode::Char('J') => {
            app.select_next();
            app.extend_mark_to_cursor();
        }
        KeyCode::Char('K') => {
            app.select_prev();
            app.extend_mark_to_cursor();
        }
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),
        KeyCode::Char(' ') => app.toggle_mark_selected(),
        KeyCode::Char('a') => app.mark_all_toggle(),
        KeyCode::Char('s') => app.request_batch(BatchAction::Start),
        KeyCode::Char('x') => app.request_batch(BatchAction::Stop),
        KeyCode::Char('R') => app.request_batch(BatchAction::Restart),
        KeyCode::Char('D') => app.request_batch(BatchAction::Delete),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ConfirmAction, Overlay};
    use crate::keys::handle_key;
    use crate::keys::testkit::*;
    use crate::model::TunnelStatus;
    use std::time::Instant;

    // -- selection mode ----------------------------------------------------

    #[test]
    fn v_toggles_selection_mode() {
        let (_d, mut app) = three_app();
        handle_key(&mut app, ch('v'));
        assert!(app.select_mode.active);
        handle_key(&mut app, ch('v'));
        assert!(!app.select_mode.active);
    }

    #[test]
    fn space_marks_instead_of_toggling_the_tunnel() {
        // The single most dangerous confusion: space must not start/stop a tunnel
        // while selection mode is on.
        let (_d, mut app) = three_app();
        app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
        app.selected = 0;
        handle_key(&mut app, ch('v'));
        handle_key(&mut app, ch(' '));
        assert!(matches!(app.overlay, Overlay::None), "space must not open a stop dialog");
        assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }), "and must not stop the tunnel");
        assert!(app.select_mode.is_marked(1), "it marks the row instead");
    }

    #[test]
    fn esc_and_v_leave_selection_mode() {
        let (_d, mut app) = three_app();
        handle_key(&mut app, ch('v'));
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(!app.select_mode.active);
        handle_key(&mut app, ch('v'));
        handle_key(&mut app, ch('V'));
        assert!(!app.select_mode.active);
    }

    #[test]
    fn batch_keys_ask_before_acting() {
        let (_d, mut app) = three_app();
        app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
        app.selected = 0;
        handle_key(&mut app, ch('v'));
        handle_key(&mut app, ch('a'));
        assert_eq!(app.select_mode.len(), 3);
        for (label, key, expected) in [
            ("启用", 's', BatchAction::Start),
            ("停用", 'x', BatchAction::Stop),
            ("重启", 'R', BatchAction::Restart),
            ("删除", 'D', BatchAction::Delete),
        ] {
            app.overlay = Overlay::None;
            handle_key(&mut app, ch(key));
            match &app.overlay {
                Overlay::Confirm { action: ConfirmAction::Batch { action, .. }, prompt } => {
                    assert_eq!(*action, expected, "{label}: wrong batch action");
                    assert!(!prompt.is_empty());
                }
                other => panic!("{label}: expected a batch confirmation, got {other:?}"),
            }
        }
        // Nothing was destroyed while the dialogs were open.
        assert_eq!(app.tunnels.len(), 3);
    }

    #[test]
    fn a_batch_delete_needs_an_explicit_yes() {
        let (_d, mut app) = three_app();
        handle_key(&mut app, ch('v'));
        handle_key(&mut app, ch('a'));
        handle_key(&mut app, ch('D'));
        assert_eq!(app.tunnels.len(), 3, "must not delete before confirmation");
        handle_key(&mut app, ch('y'));
        assert!(app.tunnels.is_empty(), "confirming the batch delete removes the marked rows");
    }

    #[test]
    fn shift_j_extends_the_mark_range() {
        let (_d, mut app) = app_with((1..=5).map(|i| tunnel(i, &format!("t{i}"), 18000 + i as u16)).collect());
        app.selected = 0;
        handle_key(&mut app, ch('v'));
        handle_key(&mut app, ch('J'));
        handle_key(&mut app, ch('J'));
        let ids = app.select_mode.ids();
        assert_eq!(ids, vec![1, 2, 3], "shift+J extends from the anchor: {ids:?}");
    }

    #[test]
    fn selection_navigation_on_an_empty_table_is_a_no_op() {
        let (_d, mut app) = app_with(vec![]);
        handle_key(&mut app, ch('v'));
        assert!(app.select_mode.active);
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('J'),
            KeyCode::Char('K'),
            KeyCode::Char(' '),
            KeyCode::Char('a'),
            KeyCode::Char('s'),
            KeyCode::Char('x'),
            KeyCode::Char('R'),
            KeyCode::Char('D'),
        ] {
            handle_key(&mut app, key(code));
            app.overlay = Overlay::None;
        }
        assert!(app.tunnels.is_empty());
        assert!(app.select_mode.is_empty());
    }
}

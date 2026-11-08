//! The small overlays: help, detail, confirm, and the `/` filter bar.
//!
//! Each handler is one match table for one `Overlay` variant, and each is called
//! straight from [`super::handle_key`]. They are grouped here because they are
//! small and share a shape (open → navigate → close), not because they share
//! state: every one of them only mutates `App` fields for its own overlay.
//!
//! Document new keys in the matching `SectionId` of `src/help.rs` (`HelpOverlay`,
//! `Detail`, `Confirm`, `FilterBar`) and in `docs/keymap.md`.

use crate::app::App;
use crossterm::event::{KeyCode, KeyEvent};

pub(super) fn handle_help_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => app.close_overlay(),
        KeyCode::Char('j') | KeyCode::Down => app.help_scroll = app.help_scroll.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up => app.help_scroll = app.help_scroll.saturating_sub(1),
        KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
        KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
        _ => {}
    }
}

pub(super) fn handle_detail_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('l') => app.close_overlay(),
        KeyCode::Char('j') | KeyCode::Down => app.detail_scroll = app.detail_scroll.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up => app.detail_scroll = app.detail_scroll.saturating_sub(1),
        KeyCode::PageDown => app.detail_scroll = app.detail_scroll.saturating_add(10),
        KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(10),
        _ => {}
    }
}

pub(super) fn handle_confirm_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => app.confirm_yes(),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.confirm_no(),
        _ => {}
    }
}

pub(super) fn handle_filter_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.filter.clear();
            app.selected = 0;
            app.close_overlay();
        }
        KeyCode::Enter => app.close_overlay(),
        KeyCode::Backspace => {
            app.filter.pop();
            app.selected = 0;
        }
        KeyCode::Char(c) => {
            app.filter.push(c);
            app.selected = 0;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Overlay;
    use crate::keys::handle_key;
    use crate::keys::testkit::*;

    // -- confirm / detail / help / filter ----------------------------------

    #[test]
    fn confirm_accept_and_cancel_keys() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.selected = 0;
        handle_key(&mut app, ch('q'));
        // `n` cancels.
        handle_key(&mut app, ch('n'));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(!app.should_quit);
        // `y` accepts.
        handle_key(&mut app, ch('q'));
        handle_key(&mut app, ch('y'));
        assert!(app.should_quit);
    }

    #[test]
    fn esc_cancels_a_confirmation() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.selected = 0;
        handle_key(&mut app, ch('d'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.tunnels.len(), 1, "cancelled delete must keep the tunnel");
    }

    #[test]
    fn detail_scroll_keys_stay_in_range() {
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.selected = 0;
        handle_key(&mut app, key(KeyCode::Enter));
        handle_key(&mut app, ch('j'));
        assert_eq!(app.detail_scroll, 1);
        handle_key(&mut app, ch('k'));
        handle_key(&mut app, ch('k'));
        assert_eq!(app.detail_scroll, 0, "scroll must not go negative");
    }

    #[test]
    fn question_mark_toggles_help() {
        let (_d, mut app) = app_with(vec![]);
        handle_key(&mut app, ch('?'));
        assert!(matches!(app.overlay, Overlay::Help));
        handle_key(&mut app, ch('?'));
        assert!(matches!(app.overlay, Overlay::None));
    }

    #[test]
    fn filter_typing_narrows_then_esc_clears() {
        let (_d, mut app) = app_with(vec![tunnel(1, "alpha", 18001), tunnel(2, "beta", 18002)]);
        handle_key(&mut app, ch('/'));
        assert!(matches!(app.overlay, Overlay::Filter));
        handle_key(&mut app, ch('a'));
        handle_key(&mut app, ch('l'));
        assert_eq!(app.filter, "al");
        assert_eq!(app.visible_indices(), vec![0]);
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.filter.is_empty(), "esc clears the filter");
    }

    #[test]
    fn filter_backspace_removes_a_char() {
        let (_d, mut app) = app_with(vec![tunnel(1, "alpha", 18001)]);
        handle_key(&mut app, ch('/'));
        handle_key(&mut app, ch('a'));
        handle_key(&mut app, ch('l'));
        handle_key(&mut app, key(KeyCode::Backspace));
        assert_eq!(app.filter, "a");
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::None), "enter keeps the filter but closes editing");
        assert_eq!(app.filter, "a");
    }
}

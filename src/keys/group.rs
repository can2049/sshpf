//! The group-alias input (`N` new group, `R` rename).
//!
//! One text buffer, the same editing keys as the main filter, plus `enter` to
//! save and `esc` to cancel. Kept apart from `misc.rs` because it is its own
//! `SectionId` in the help catalog: the user looking up "what does enter do here"
//! should land on the alias section, not on the confirmation dialog.

use crate::app::App;
use crossterm::event::{KeyCode, KeyEvent};

pub(super) fn handle_group_name_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            app.submit_group_prompt();
            return;
        }
        KeyCode::Esc => {
            app.close_overlay();
            return;
        }
        _ => {}
    }
    let Some(prompt) = app.group_prompt.as_mut() else {
        app.close_overlay();
        return;
    };
    match key.code {
        KeyCode::Backspace => prompt.input.backspace(),
        KeyCode::Delete => prompt.input.delete(),
        KeyCode::Left => prompt.input.left(),
        KeyCode::Right => prompt.input.right(),
        KeyCode::Home => prompt.input.home(),
        KeyCode::End => prompt.input.end(),
        KeyCode::Char(c) if !c.is_control() => prompt.input.insert(c),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Overlay;
    use crate::keys::testkit::*;

    #[test]
    fn the_alias_input_edits_and_saves() {
        let (_d, mut app) = app_with(vec![]);
        app.open_group_create();
        for c in "work".chars() {
            handle_group_name_key(&mut app, ch(c));
        }
        handle_group_name_key(&mut app, key(KeyCode::Backspace));
        handle_group_name_key(&mut app, ch('k'));
        handle_group_name_key(&mut app, key(KeyCode::Enter));
        assert!(matches!(app.overlay, Overlay::None));
        assert_eq!(app.cfg.group(app.browsed_group).map(|g| g.alias()), Some("work".to_string()));
    }

    #[test]
    fn esc_cancels_without_creating() {
        let (_d, mut app) = app_with(vec![]);
        let before = app.cfg.groups.len();
        app.open_group_create();
        handle_group_name_key(&mut app, ch('x'));
        handle_group_name_key(&mut app, key(KeyCode::Esc));
        assert_eq!(app.cfg.groups.len(), before);
        assert!(app.group_prompt.is_none());
    }
}

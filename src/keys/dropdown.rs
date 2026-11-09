//! The expanded `SSH HOST` dropdown inside the form.
//!
//! While the picker is open it is the primary way to choose a host, so
//! ↑↓/enter/esc/page keys drive the list rather than the form: [`handle_dropdown_key`]
//! returns `true` for every key it owns, which is how `handle_form_key` knows
//! whether to stop. `true` means "claimed"; `false` falls back to the form.
//!
//! Document new keys in `src/help.rs` (`SectionId::HostPicker`) and
//! `docs/keymap.md`.

use crate::app::App;
use crossterm::event::{KeyCode, KeyEvent};

/// Keys owned by the expanded host dropdown. Returns true when handled.
pub(super) fn handle_dropdown_key(app: &mut App, key: KeyEvent) -> bool {
    if crate::keys::is_save_key(key) {
        crate::eventlog::save_key_swallowed("host_dropdown");
    }
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => {
            app.picker_move(1);
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.picker_move(-1);
            true
        }
        KeyCode::PageDown => {
            app.picker_move(crate::app::HOST_DROPDOWN_ROWS as i32);
            true
        }
        KeyCode::PageUp => {
            app.picker_move(-(crate::app::HOST_DROPDOWN_ROWS as i32));
            true
        }
        KeyCode::Home => {
            if let Some(p) = app.picker.as_mut() {
                p.home();
            }
            true
        }
        KeyCode::End => {
            if let Some(p) = app.picker.as_mut() {
                p.end(crate::app::HOST_DROPDOWN_ROWS);
            }
            true
        }
        KeyCode::Enter => {
            app.picker_accept();
            // Picking a host is the moment the import table must describe *that*
            // host; doing it here (not only on submit) is what makes the pane
            // show the right rows the instant it is opened.
            app.refresh_import_for_current_host();
            true
        }
        KeyCode::Esc => {
            app.close_host_picker();
            true
        }
        KeyCode::Backspace => {
            app.picker_query_backspace();
            true
        }
        KeyCode::Char(c) if !c.is_control() => {
            app.picker_query_char(c);
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

    // -- host dropdown -----------------------------------------------------

    #[test]
    fn enter_on_host_field_opens_the_dropdown() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::SshHost;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(app.host_picker_open());
        assert!(matches!(app.overlay, Overlay::Form), "dropdown is inline, not a separate overlay");
    }

    #[test]
    fn dropdown_typing_filters_and_enter_accepts() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::SshHost;
        handle_key(&mut app, key(KeyCode::Enter));
        handle_key(&mut app, ch('t'));
        assert_eq!(app.picker.as_ref().unwrap().query, "t");
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(!app.host_picker_open(), "accepting collapses the list");
        assert_eq!(app.form.as_ref().unwrap().ssh_host.value(), "testhost");
    }

    #[test]
    fn a_save_key_taken_by_the_dropdown_still_becomes_a_filter_query() {
        // The other half of the same defect: with the host dropdown open,
        // `ctrl+s` reaches `Char(c) if !c.is_control()` and is typed into the
        // filter box as `s`, so the save never happens and nothing is created.
        //
        // This change instruments it (`event=form.save_key_swallowed
        // focus=host_dropdown`); the follow-up fix makes the save win instead.
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::SshHost;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(app.host_picker_open(), "precondition: the dropdown is open");

        handle_key(&mut app, ctrl('s'));

        assert_eq!(
            app.picker.as_ref().map(|p| p.query.clone()),
            Some("s".to_string()),
            "today the save key is appended to the dropdown query — this is the bug being instrumented"
        );
        assert!(app.tunnels.is_empty(), "and therefore no tunnel is created");
    }

    #[test]
    fn dropdown_esc_collapses_without_changing_the_field() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().ssh_host.set("typed");
        app.form.as_mut().unwrap().field = FormField::SshHost;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(app.host_picker_open());
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(!app.host_picker_open());
        assert_eq!(app.form.as_ref().unwrap().ssh_host.value(), "typed");
    }

    #[test]
    fn the_host_dropdown_end_key_scrolls_the_last_row_into_view() {
        let (_d, mut app) = app_with(vec![]);
        app.hosts = (0..40)
            .map(|i| crate::ssh::SshHost {
                alias: format!("h{i:02}"),
                hostname: "1.1.1.1".into(),
                user: None,
                port: 22,
                proxy_jump: Vec::new(),
                identity_file: None,
            })
            .collect();
        app.open_form_create();
        app.open_host_picker();
        handle_key(&mut app, key(KeyCode::End));
        let p = app.picker.as_ref().unwrap();
        assert_eq!(p.selected, 39);
        assert!(p.selected < p.scroll as usize + crate::app::HOST_DROPDOWN_ROWS, "the last host must be visible");
    }

    #[test]
    fn paging_the_dropdown_stays_inside_the_list() {
        let (_d, mut app) = app_with(vec![]);
        app.hosts = (0..3)
            .map(|i| crate::ssh::SshHost {
                alias: format!("h{i}"),
                hostname: "1.1.1.1".into(),
                user: None,
                port: 22,
                proxy_jump: Vec::new(),
                identity_file: None,
            })
            .collect();
        app.open_form_create();
        app.open_host_picker();
        for _ in 0..5 {
            handle_key(&mut app, key(KeyCode::PageDown));
        }
        assert_eq!(app.picker.as_ref().unwrap().selected, 2);
        for _ in 0..5 {
            handle_key(&mut app, key(KeyCode::PageUp));
        }
        assert_eq!(app.picker.as_ref().unwrap().selected, 0);
    }
}

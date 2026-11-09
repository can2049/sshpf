//! The expanded `SSH HOST` dropdown inside the form.
//!
//! While the picker is open it is the primary way to choose a host, so
//! ↑↓/enter/esc/page keys drive the list rather than the form: [`handle_dropdown_key`]
//! returns `true` for every key it owns, which is how `handle_form_key` knows
//! whether to stop. `true` means "claimed"; `false` falls back to the form.
//!
//! A **save** key is the one exception: it is recorded as swallowed and then handed
//! back to the form. The dropdown is a search aid, and the host it searches for is
//! already in the field, so `ctrl+s` there unambiguously means "save the forward I
//! typed" — not the letter `s` appended to a query.
//!
//! Document new keys in `src/help.rs` (`SectionId::HostPicker`) and
//! `docs/keymap.md`.

use crate::app::App;
use crossterm::event::{KeyCode, KeyEvent};

/// Keys owned by the expanded host dropdown. Returns true when handled.
pub(super) fn handle_dropdown_key(app: &mut App, key: KeyEvent) -> bool {
    // Recorded, so "the save key never reached the form" stops looking like an
    // empty log file, and then handed back so the save actually happens.
    if crate::keys::is_save_key(key) {
        crate::eventlog::save_key_swallowed("host_dropdown");
        return false;
    }
    if handle_dropdown_move_key(app, key) {
        return true;
    }
    match key.code {
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

/// Cursor movement inside the dropdown. Returns true when it claimed the key.
///
/// Split out of [`handle_dropdown_key`] the same way the import pane and the port
/// list split their movement half: the two arm sets are disjoint, so a two-step
/// dispatch is equivalent to one big match, and each function stays readable.
fn handle_dropdown_move_key(app: &mut App, key: KeyEvent) -> bool {
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
    fn a_save_key_is_never_a_dropdown_query_even_with_the_list_open() {
        // The reported failure: the dropdown is open (it is how a host gets
        // picked), so `ctrl+s` reached the `Char(c) if !c.is_control()` arm and
        // was appended to the query as `s` — the save never happened, no tunnel
        // was created and nothing was logged. The dropdown is a search aid, and
        // the host it searches for is already in the field, so the save key must
        // reach the form.
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::SshHost;
        handle_key(&mut app, key(KeyCode::Enter));
        handle_key(&mut app, key(KeyCode::Enter)); // accept the highlighted host
        assert_eq!(app.form.as_ref().unwrap().ssh_host.value(), "testhost");
        handle_key(&mut app, key(KeyCode::Enter)); // reopen: the save key must still work
        assert!(app.host_picker_open(), "precondition: the dropdown is open");
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("typed-3081");
            f.remote_host.set("127.0.0.1");
            f.remote_port.set("3081");
            f.local_port.set("38082");
            f.mark_local_port_manual();
        }

        handle_key(&mut app, ctrl('s'));

        assert!(
            app.tunnels.iter().any(|t| t.cfg.remote_port == 3081),
            "ctrl+s must save instead of typing `s` into the query: query={:?} errors={:?}",
            app.picker.as_ref().map(|p| p.query.clone()),
            app.form.as_ref().map(|f| f.errors.clone())
        );
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

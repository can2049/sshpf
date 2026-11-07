//! Key dispatch: turns a `crossterm` key event into `App` mutations.
//!
//! This lives in the library rather than `main.rs` so the whole key map can be
//! unit-tested without a terminal: every handler here takes an `App` and a
//! `KeyEvent` and is otherwise pure. `main.rs` only owns terminal setup and the
//! event loop.

use crate::app::{App, ConfirmAction, Overlay};
use crate::model::{FormField, LogLevel};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

fn handle_main_key(app: &mut App, key: KeyEvent) {
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
        KeyCode::Esc => app.filter.clear(),
        _ => {}
    }
}

fn handle_help_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => app.close_overlay(),
        KeyCode::Char('j') | KeyCode::Down => app.help_scroll = app.help_scroll.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up => app.help_scroll = app.help_scroll.saturating_sub(1),
        KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
        KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
        _ => {}
    }
}

fn handle_detail_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Char('l') => app.close_overlay(),
        KeyCode::Char('j') | KeyCode::Down => app.detail_scroll = app.detail_scroll.saturating_add(1),
        KeyCode::Char('k') | KeyCode::Up => app.detail_scroll = app.detail_scroll.saturating_sub(1),
        KeyCode::PageDown => app.detail_scroll = app.detail_scroll.saturating_add(10),
        KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(10),
        _ => {}
    }
}

fn handle_confirm_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => app.confirm_yes(),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.close_overlay(),
        _ => {}
    }
}

fn handle_filter_key(app: &mut App, key: KeyEvent) {
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

fn handle_form_key(app: &mut App, key: KeyEvent) {
    // The SSH HOST dropdown, when expanded, owns most keys: it is the primary
    // way to pick a host, so ↑↓/enter/esc must drive the list, not the form.
    if app.host_picker_open() && handle_dropdown_key(app, key) {
        return;
    }

    let editing = app.form.as_ref().is_some_and(|f| f.is_editing_field());
    let field = app.form.as_ref().map(|f| f.field);

    // ---- stage 2: editing the highlighted field -------------------------
    if editing {
        match key.code {
            // Leave editing; the highlight stays where it is.
            KeyCode::Enter | KeyCode::Esc => {
                if let Some(f) = app.form.as_mut() {
                    f.end_edit();
                    f.revalidate();
                }
                return;
            }
            // Tab commits and advances, like leaving the field.
            KeyCode::Tab | KeyCode::BackTab => {
                let forward = key.code == KeyCode::Tab;
                if let Some(f) = app.form.as_mut() {
                    f.end_edit();
                    f.field = if forward { f.field.next() } else { f.field.prev() };
                    f.revalidate();
                }
                return;
            }
            _ => {}
        }
        if let Some(field) = field {
            // Typing in the remote port re-derives the local port suggestion.
            let was_remote_port = field == FormField::RemotePort;
            let was_local_port = field == FormField::LocalPort;
            if was_local_port
                && matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete)
                && let Some(f) = app.form.as_mut()
            {
                f.mark_local_port_manual();
            }
            edit_field(app, field, &key);
            if was_remote_port {
                app.refresh_local_port_suggestion();
            }
        }
        return;
    }

    // ---- stage 1: navigating -------------------------------------------
    // Arrows only move the highlight; `enter` is what starts editing.
    match key.code {
        KeyCode::Esc => {
            // Asking here (not just closing) is what protects typed input:
            // this is the branch users reach with one esc from the form.
            app.request_close_form();
            return;
        }
        KeyCode::Tab | KeyCode::BackTab => {
            let forward = key.code == KeyCode::Tab;
            if let Some(f) = app.form.as_mut() {
                f.field = if forward { f.field.next() } else { f.field.prev() };
                f.revalidate();
            }
            app.close_host_picker();
            return;
        }
        KeyCode::Down | KeyCode::Up => {
            let forward = key.code == KeyCode::Down;
            if let Some(f) = app.form.as_mut() {
                f.field = if forward { f.field.next() } else { f.field.prev() };
                f.revalidate();
            }
            app.close_host_picker();
            return;
        }
        KeyCode::Enter => {
            // On the toggle/select fields `enter` just acts; on text fields it
            // enters editing.
            match field {
                Some(FormField::Kind) => {
                    if let Some(f) = app.form.as_mut() {
                        f.cycle_kind(true);
                    }
                }
                Some(FormField::AutoRestart) => {
                    if let Some(f) = app.form.as_mut() {
                        f.auto_restart = !f.auto_restart;
                    }
                }
                Some(FormField::SshHost) => app.open_host_picker(),
                Some(_) => {
                    if let Some(f) = app.form.as_mut() {
                        f.begin_edit();
                    }
                }
                None => {}
            }
            return;
        }
        // Ctrl+S saves from anywhere; plain `enter` is field editing now.
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.submit_form();
            return;
        }
        KeyCode::F(10) => {
            app.submit_form();
            return;
        }
        _ => {}
    }

    let Some(field) = field else {
        return;
    };

    // Left/right adjust the enum-like fields while merely navigating.
    match field {
        FormField::Kind => match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(f) = app.form.as_mut() {
                    f.cycle_kind(false);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(f) = app.form.as_mut() {
                    f.cycle_kind(true);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(f) = app.form.as_mut() {
                    f.cycle_kind(true);
                }
            }
            _ => {}
        },
        FormField::AutoRestart => {
            if matches!(key.code, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right)
                && let Some(f) = app.form.as_mut()
            {
                f.auto_restart = !f.auto_restart;
            }
        }
        FormField::SshHost => match key.code {
            // ctrl+h / F2 open the dropdown without entering text-editing.
            KeyCode::F(2) => app.open_host_picker(),
            KeyCode::Char('h') | KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.open_host_picker()
            }
            _ => {}
        },
        _ => {}
    }
}

/// Keys owned by the expanded host dropdown. Returns true when handled.
fn handle_dropdown_key(app: &mut App, key: KeyEvent) -> bool {
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
                p.end();
            }
            true
        }
        KeyCode::Enter => {
            app.picker_accept();
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

fn edit_field(app: &mut App, field: FormField, key: &KeyEvent) {
    let Some(form) = app.form.as_mut() else {
        return;
    };
    let Some(buf) = form.buf_mut(field) else {
        return;
    };
    match key.code {
        KeyCode::Char(c) => buf.insert(c),
        KeyCode::Backspace => buf.backspace(),
        KeyCode::Delete => buf.delete(),
        KeyCode::Left => buf.left(),
        KeyCode::Right => buf.right(),
        KeyCode::Home => buf.home(),
        KeyCode::End => buf.end(),
        _ => return,
    }
    form.revalidate();
}

#[allow(dead_code)]
fn status_level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

#[allow(dead_code)]
fn confirm_is_delete(action: &ConfirmAction) -> bool {
    matches!(action, ConfirmAction::Delete(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, ConfigFile, Paths};
    use crate::model::{ForwardKind, TunnelConfig, TunnelStatus};
    use std::time::Instant;

    /// A key event with no modifiers, as a terminal sends for a plain press.
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn tunnel(id: u64, name: &str, port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "testhost".into(),
            local_addr: "127.0.0.1".into(),
            local_port: port,
            remote_host: "127.0.0.1".into(),
            remote_port: 22,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    /// Build an `App` from a temp config. `App::new` spawns nothing.
    fn app_with(tunnels: Vec<TunnelConfig>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
        config::save(&paths.config, &cfg).unwrap();
        (dir, App::new(paths))
    }

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
    fn ctrl_c_asks_before_quitting() {
        let (_d, mut app) = app_with(vec![]);
        handle_key(&mut app, ctrl('c'));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
        assert!(!app.should_quit);
    }

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

    // -- form: two-stage editing -------------------------------------------

    /// Open a create form on an app with one known host.
    fn form_app() -> (tempfile::TempDir, App) {
        let (d, mut app) = app_with(vec![]);
        app.hosts = vec![crate::ssh::SshHost {
            alias: "testhost".into(),
            hostname: "127.0.0.1".into(),
            user: Some("u".into()),
            port: 22,
            proxy_jump: None,
            identity_file: None,
        }];
        handle_key(&mut app, ch('n'));
        (d, app)
    }

    #[test]
    fn arrows_do_not_edit_values_in_navigating_mode() {
        // The whole point of two-stage editing: arrowing past a field must not
        // type into it.
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::Name;
        assert!(!app.form.as_ref().unwrap().is_editing_field());
        handle_key(&mut app, ch('x'));
        assert_eq!(app.form.as_ref().unwrap().name.value(), "", "typing while navigating must be ignored");
    }

    #[test]
    fn enter_starts_editing_and_esc_stops() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::Name;
        handle_key(&mut app, key(KeyCode::Enter));
        assert!(app.form.as_ref().unwrap().is_editing_field(), "enter must start editing");
        handle_key(&mut app, ch('h'));
        handle_key(&mut app, ch('i'));
        assert_eq!(app.form.as_ref().unwrap().name.value(), "hi");
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(!app.form.as_ref().unwrap().is_editing_field(), "esc must end editing");
        // The form is still open: esc only leaves editing here.
        assert!(matches!(app.overlay, Overlay::Form));
        assert_eq!(app.form.as_ref().unwrap().name.value(), "hi");
    }

    #[test]
    fn tab_in_navigating_mode_changes_the_field() {
        let (_d, mut app) = form_app();
        let first = app.form.as_ref().unwrap().field;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_ne!(app.form.as_ref().unwrap().field, first);
        handle_key(&mut app, key(KeyCode::BackTab));
        assert_eq!(app.form.as_ref().unwrap().field, first);
    }

    #[test]
    fn up_down_move_between_fields_in_navigating_mode() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::SshHost;
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.form.as_ref().unwrap().field, FormField::Name);
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(app.form.as_ref().unwrap().field, FormField::Kind);
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(app.form.as_ref().unwrap().field, FormField::Name);
    }

    #[test]
    fn enter_on_kind_cycles_instead_of_editing() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::Kind;
        let before = app.form.as_ref().unwrap().kind();
        handle_key(&mut app, key(KeyCode::Enter));
        assert_ne!(app.form.as_ref().unwrap().kind(), before);
        assert!(!app.form.as_ref().unwrap().is_editing_field(), "toggle fields are not text-editable");
    }

    #[test]
    fn enter_on_autorestart_toggles_it() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::AutoRestart;
        let before = app.form.as_ref().unwrap().auto_restart;
        handle_key(&mut app, key(KeyCode::Enter));
        assert_ne!(app.form.as_ref().unwrap().auto_restart, before);
    }

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
    fn remote_port_typing_derives_the_local_port() {
        let (_d, mut app) = form_app();
        {
            let f = app.form.as_mut().unwrap();
            f.field = FormField::RemotePort;
        }
        handle_key(&mut app, key(KeyCode::Enter));
        for c in "8080".chars() {
            handle_key(&mut app, ch(c));
        }
        assert_eq!(app.form.as_ref().unwrap().remote_port.value(), "8080");
        assert_eq!(app.form.as_ref().unwrap().local_port.value(), "8080");
    }

    #[test]
    fn local_port_typed_by_hand_is_not_overwritten() {
        let (_d, mut app) = form_app();
        {
            let f = app.form.as_mut().unwrap();
            f.field = FormField::LocalPort;
        }
        handle_key(&mut app, key(KeyCode::Enter));
        for c in "9000".chars() {
            handle_key(&mut app, ch(c));
        }
        // Now change the remote port; the manual local port must survive.
        {
            let f = app.form.as_mut().unwrap();
            f.end_edit();
            f.field = FormField::RemotePort;
        }
        handle_key(&mut app, key(KeyCode::Enter));
        for c in "7000".chars() {
            handle_key(&mut app, ch(c));
        }
        assert_eq!(app.form.as_ref().unwrap().local_port.value(), "9000");
    }

    #[test]
    fn ctrl_s_saves_the_form() {
        let (_d, mut app) = form_app();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("testhost");
            f.name.set("saved");
            f.remote_port.set("22");
            f.local_port.set("18050");
        }
        handle_key(&mut app, ctrl('s'));
        assert!(matches!(app.overlay, Overlay::None), "ctrl+s closes the form on success");
        assert_eq!(app.tunnels.len(), 1);
        let reloaded = config::load(&app.paths.config).unwrap();
        assert_eq!(reloaded.tunnels[0].name, "saved");
    }

    #[test]
    fn esc_on_a_dirty_form_asks_before_discarding() {
        // Note: esc is overloaded. In the *navigating* stage it means "leave the
        // form" (which asks when there is unsaved input); in the *editing* stage
        // it means "stop editing this field" and must not pop a dialog.
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().name.set("typed");
        assert!(!app.form.as_ref().unwrap().is_editing_field());
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }), "dirty form must confirm discard");
        assert!(app.form.is_some());
    }

    #[test]
    fn esc_while_editing_a_field_only_stops_editing() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::Name;
        handle_key(&mut app, key(KeyCode::Enter)); // start editing
        handle_key(&mut app, ch('x'));
        handle_key(&mut app, key(KeyCode::Esc)); // first esc: leave editing
        assert!(!app.form.as_ref().unwrap().is_editing_field());
        assert!(matches!(app.overlay, Overlay::Form), "must stay in the form");
        assert_eq!(app.form.as_ref().unwrap().name.value(), "x");
        // Second esc: now it is a discard request.
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::Confirm { .. }));
    }

    #[test]
    fn esc_on_a_pristine_form_closes_it_directly() {
        let (_d, mut app) = form_app();
        handle_key(&mut app, key(KeyCode::Esc));
        assert!(matches!(app.overlay, Overlay::None), "pristine form needs no confirmation");
        assert!(app.form.is_none());
    }

    #[test]
    fn shift_tab_in_editing_mode_commits_and_moves_back() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::Name;
        handle_key(&mut app, key(KeyCode::Enter));
        handle_key(&mut app, ch('z'));
        handle_key(&mut app, key(KeyCode::BackTab));
        assert!(!app.form.as_ref().unwrap().is_editing_field());
        assert_eq!(app.form.as_ref().unwrap().name.value(), "z");
        assert_ne!(app.form.as_ref().unwrap().field, FormField::Name);
    }
}

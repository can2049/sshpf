//! Key dispatch: turns a `crossterm` key event into `App` mutations.
//!
//! This lives in the library rather than `main.rs` so the whole key map can be
//! unit-tested without a terminal: every handler here takes an `App` and a
//! `KeyEvent` and is otherwise pure. `main.rs` only owns terminal setup and the
//! event loop.

use crate::app::{App, ConfirmAction, Overlay};
use crate::model::{BatchAction, FormField, ImportColumn, LogLevel};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How many import rows the pane shows at once.
///
/// Shared by the key handler (scroll window) and the renderer (pane height) so
/// the two cannot disagree about how far the cursor can move before it scrolls.
pub const IMPORT_ROWS_VISIBLE: usize = 8;

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

fn handle_main_key(app: &mut App, key: KeyEvent) {
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

/// Keys that only exist while selection mode is active.
///
/// Uppercase `R`/`D` are batch restart/delete so they cannot be confused with
/// the single-row `r`/`d` of the normal table; both still go through the same
/// confirmation gate.
fn handle_select_key(app: &mut App, key: KeyEvent) {
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
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.confirm_no(),
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
    // An expanded SSH HOST dropdown owns most keys: it is the primary way to
    // pick a host, so ↑↓/enter/esc must drive the list rather than the form.
    if app.host_picker_open() && handle_dropdown_key(app, key) {
        return;
    }
    // The import pane, when open, owns its own navigation and filter keys.
    if app.import_pane_open() && handle_import_key(app, key) {
        return;
    }

    // The form has two stages with different key semantics; dispatch on stage.
    if app.form.as_ref().is_some_and(|f| f.is_editing_field()) {
        handle_field_editing(app, key);
    } else {
        handle_field_navigating(app, key);
    }
}

/// Stage 2: a text field is actively being edited.
///
/// Keystrokes go into the buffer; `enter`/`esc` end editing and `tab` ends it
/// and advances. Typing in the remote port also re-derives the local-port
/// suggestion, and typing in the local port marks it as hand-set so the
/// suggestion stops overwriting it.
fn handle_field_editing(app: &mut App, key: KeyEvent) {
    match key.code {
        // Leave editing; the highlight stays where it is.
        KeyCode::Enter | KeyCode::Esc => {
            let was_host = app.form.as_ref().is_some_and(|f| f.field == FormField::SshHost);
            if let Some(f) = app.form.as_mut() {
                f.end_edit();
                f.revalidate();
            }
            // A hand-typed alias must resolve its forwards too, or the pane would
            // keep showing the previous host's rows.
            if was_host {
                app.refresh_import_for_current_host();
            }
            return;
        }
        // Tab commits and advances, like leaving the field.
        KeyCode::Tab | KeyCode::BackTab => {
            move_field(app, key.code == KeyCode::Tab);
            if let Some(f) = app.form.as_mut() {
                f.end_edit();
            }
            return;
        }
        _ => {}
    }

    let Some(field) = app.form.as_ref().map(|f| f.field) else {
        return;
    };
    if field == FormField::LocalPort
        && matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete)
        && let Some(f) = app.form.as_mut()
    {
        f.mark_local_port_manual();
    }
    edit_field(app, field, &key);
    if field == FormField::RemotePort {
        app.refresh_local_port_suggestion();
    }
}

/// Stage 1: arrows only move the highlight; `enter` commits to editing.
fn handle_field_navigating(app: &mut App, key: KeyEvent) {
    let Some(field) = app.form.as_ref().map(|f| f.field) else {
        return;
    };

    match key.code {
        KeyCode::Esc => {
            // Asking here (rather than just closing) is what protects typed
            // input: this is the branch users reach with one esc.
            app.request_close_form();
        }
        KeyCode::Tab | KeyCode::BackTab => {
            move_field(app, key.code == KeyCode::Tab);
            app.close_host_picker();
        }
        KeyCode::Down => {
            move_field(app, true);
            app.close_host_picker();
        }
        KeyCode::Up => {
            move_field(app, false);
            app.close_host_picker();
        }
        KeyCode::Enter => activate_field(app, field),
        // Ctrl+S saves from anywhere; plain `enter` is field editing now.
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => app.submit_form(),
        KeyCode::F(10) => app.submit_form(),
        _ => adjust_field(app, field, key),
    }
}

/// `enter` on the highlighted field: act for toggles, edit for text fields.
fn activate_field(app: &mut App, field: FormField) {
    match field {
        FormField::Kind => {
            if let Some(f) = app.form.as_mut() {
                f.cycle_kind(true);
            }
        }
        FormField::AutoRestart => {
            if let Some(f) = app.form.as_mut() {
                f.auto_restart = !f.auto_restart;
            }
        }
        // Enter opens the host list; space/i toggle the import pane. Two
        // different jobs on one field, so they get different keys rather than a
        // mode the user has to discover.
        FormField::SshHost => app.open_host_picker(),
        _ => {
            if let Some(f) = app.form.as_mut() {
                f.begin_edit();
            }
        }
    }
}

/// Keys that adjust the enum-like fields while merely navigating.
fn adjust_field(app: &mut App, field: FormField, key: KeyEvent) {
    match field {
        FormField::Kind => match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(f) = app.form.as_mut() {
                    f.cycle_kind(false);
                }
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
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
            // space / i: show the forwards this host already declares.
            KeyCode::Char(' ') | KeyCode::Char('i') => app.toggle_import_pane(),
            _ => {}
        },
        _ => {}
    }
}

/// Keys owned by the open import pane (`space`/`i` on the SSH HOST field).
///
/// Returns false for keys the pane does not claim, so `ctrl+s`, `tab` and the
/// rest still behave exactly as they do in the plain form — the pane is an
/// addition, not a modal that swallows the form.
fn handle_import_key(app: &mut App, key: KeyEvent) -> bool {
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
            // Clear the column the cursor/filter is on; with no filter focus,
            // fall back to the first filtered column so `F` is never a no-op.
            if let Some(col) = app.form.as_ref().and_then(|f| f.import.editing) {
                app.clear_import_filter(col);
            } else if let Some(col) = active_filter_column(app) {
                app.clear_import_filter(col);
            } else {
                app.set_status(crate::model::StatusMsg::info("没有筛选可清除"));
            }
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

/// Move the highlight one field forward or backward, re-validating.
fn move_field(app: &mut App, forward: bool) {
    if let Some(f) = app.form.as_mut() {
        f.field = if forward { f.field.next() } else { f.field.prev() };
        f.revalidate();
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

    fn mouse(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
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
            proxy_jump: Vec::new(),
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

    // -- ssh-config import pane -------------------------------------------

    /// A create form whose import pane already holds three forwards.
    fn import_app() -> (tempfile::TempDir, App) {
        let (d, mut app) = app_with(vec![]);
        app.open_form_create();
        let forwards = (0..3)
            .map(|i| crate::model::SshForward {
                kind: crate::model::ForwardKind::Local,
                listen_addr: "127.0.0.1".into(),
                listen_port: 18001 + i,
                target_host: "10.0.0.1".into(),
                target_port: 80,
                host_alias: "dev-box".into(),
                line: (i as usize) + 1,
                raw: format!("LocalForward {} 10.0.0.1:80", 18001 + i),
            })
            .collect();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("dev-box");
            f.set_import_table("dev-box", crate::model::ImportTable::new(forwards));
            f.import_open = true;
        }
        (d, app)
    }

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

    // -- modals must swallow clicks ----------------------------------------

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

    // -- selection mode ----------------------------------------------------

    fn three_app() -> (tempfile::TempDir, App) {
        app_with(vec![tunnel(1, "a", 18001), tunnel(2, "b", 18002), tunnel(3, "c", 18003)])
    }

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

    // -- mouse dispatch ----------------------------------------------------

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

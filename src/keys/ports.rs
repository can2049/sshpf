//! The peer's listening ports, offered for the 远端端口 field (`p` on it).
//!
//! Like the import pane this is an addition, not a modal: [`handle_port_key`]
//! returns `false` for every key it does not own (`tab`, `ctrl+s`, …) so the form
//! keeps them. It is called only while no text field is being edited — see
//! `handle_form_key` — because `j`/`a`/`i`/`r` are ordinary letters in a tunnel
//! name. The user opens it with `p` on the remote-port field, which dispatches
//! through `form::adjust_field`; that is also where the "only one pane at a time"
//! rule lives (`App::open_port_picker` collapses the import pane).
//!
//! `enter` accepts the highlighted port into the field and collapses the list.
//! Ticking rows (`space` / `a` / `U` / `I`) is the *other* gesture: save then
//! creates one tunnel per ticked port, which is why a read-only list (a `-R`
//! forward, where the listed ports are already occupied) refuses to tick at all.
//! The model enforces that; this file only routes keys ([`crate::model::PortPick`]).
//!
//! Document new keys in `src/help.rs` (`SectionId::PortPicker`) and
//! `docs/keymap.md`; `tests/help_coverage.rs` compares the two against this
//! file's dispatch.

use crate::app::App;
use crate::model::PORT_ROWS_VISIBLE;
use crossterm::event::{KeyCode, KeyEvent};

/// Keys owned by the expanded port list (`p` on the 远端端口 field).
///
/// Returns false for keys the list does not claim, so `tab`, `ctrl+s` and the
/// rest still behave exactly as they do in the plain form.
pub(super) fn handle_port_key(app: &mut App, key: KeyEvent) -> bool {
    if handle_port_move_key(app, key) {
        return true;
    }
    handle_port_row_key(app, key)
}

/// Cursor movement inside the list. Returns true when it claimed the key.
///
/// Kept apart from the ticking/accept table in [`handle_port_row_key`] so both
/// stay short. The two pattern sets never overlap, which is what makes the
/// two-step dispatch equivalent to one match.
fn handle_port_move_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.port_move(1);
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.port_move(-1);
            true
        }
        KeyCode::PageDown => {
            app.port_move(PORT_ROWS_VISIBLE as i32);
            true
        }
        KeyCode::PageUp => {
            app.port_move(-(PORT_ROWS_VISIBLE as i32));
            true
        }
        KeyCode::Char('g') | KeyCode::Home => {
            app.port_home();
            true
        }
        KeyCode::Char('G') | KeyCode::End => {
            app.port_end();
            true
        }
        _ => false,
    }
}

/// The actions half: ticking, accepting, rescanning and closing.
///
/// `esc` and `i` both collapse the list, symmetric with `space`/`i` on the import
/// pane: once the list is open `space` means "tick this row", so closing needs
/// its own key rather than a second press of the opener. `r` re-scans instead of
/// closing, so a port started after the first scan is one key away.
fn handle_port_row_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(' ') => {
            app.port_toggle_cursor();
            true
        }
        KeyCode::Char('a') => {
            app.port_check_all();
            true
        }
        KeyCode::Char('U') => {
            app.port_uncheck_all();
            true
        }
        KeyCode::Char('I') => {
            app.port_invert();
            true
        }
        KeyCode::Enter => {
            app.port_accept_cursor();
            true
        }
        KeyCode::Char('r') => {
            app.rescan_ports();
            true
        }
        KeyCode::Esc | KeyCode::Char('i') => {
            app.close_port_picker();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::handle_key;
    use crate::keys::testkit::*;
    use crate::model::{FormField, PortPick, RemotePort};

    /// The host every fixture scans.
    ///
    /// An inline `host:port` destination pointed at the discard port: a scan that
    /// escapes a test (`r`/`p` really do spawn one) fails on a refused connection
    /// immediately instead of reaching a real machine.
    const FIXTURE_HOST: &str = "127.0.0.1:1";

    /// An expanded list holding `ports`, as the scan would have installed it.
    fn picker(ports: &[u16], multi: bool) -> PortPick {
        let mut pick = PortPick::scanning(FIXTURE_HOST, multi, 1);
        let rows = ports.iter().map(|p| RemotePort::new(*p, "127.0.0.1")).collect();
        assert!(pick.install(1, rows, 0), "the current generation installs");
        pick
    }

    /// A create form on the 远端端口 field with the list already expanded.
    fn ports_app(ports: &[u16], multi: bool) -> (tempfile::TempDir, App) {
        let (d, mut app) = form_app();
        {
            let f = app.form.as_mut().unwrap();
            f.field = FormField::RemotePort;
            f.ssh_host.set(FIXTURE_HOST);
        }
        app.ports = Some(picker(ports, multi));
        (d, app)
    }

    fn cursor(app: &App) -> usize {
        app.ports.as_ref().map(|p| p.cursor).unwrap_or(usize::MAX)
    }

    fn checked(app: &App) -> usize {
        app.ports.as_ref().map(|p| p.checked_count()).unwrap_or(0)
    }

    // -- cursor movement ---------------------------------------------------

    #[test]
    fn j_k_and_the_arrows_walk_the_port_rows() {
        let (_d, mut app) = ports_app(&[8080, 8081, 8082], true);
        let field_before = app.form.as_ref().unwrap().field;
        handle_key(&mut app, ch('j'));
        assert_eq!(cursor(&app), 1, "j walks the rows");
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(cursor(&app), 2);
        handle_key(&mut app, key(KeyCode::Down));
        assert_eq!(cursor(&app), 2, "the cursor clamps at the last row");
        handle_key(&mut app, ch('k'));
        assert_eq!(cursor(&app), 1);
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(cursor(&app), 0);
        handle_key(&mut app, key(KeyCode::Up));
        assert_eq!(cursor(&app), 0, "and clamps at the first");
        assert_eq!(app.form.as_ref().unwrap().field, field_before, "the form field must not move");
    }

    #[test]
    fn paging_moves_by_the_shared_window_size() {
        let ports: Vec<u16> = (0..10).map(|i| 9000 + i).collect();
        let (_d, mut app) = ports_app(&ports, true);
        handle_key(&mut app, key(KeyCode::PageDown));
        assert_eq!(cursor(&app), crate::model::PORT_ROWS_VISIBLE, "one page is one list window");
        handle_key(&mut app, key(KeyCode::PageDown));
        assert_eq!(cursor(&app), 9, "a second page clamps at the end");
        handle_key(&mut app, key(KeyCode::PageUp));
        assert_eq!(cursor(&app), 9usize.saturating_sub(crate::model::PORT_ROWS_VISIBLE));
        handle_key(&mut app, key(KeyCode::PageUp));
        assert_eq!(cursor(&app), 0);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends_and_keep_the_window_in_step() {
        let ports: Vec<u16> = (0..10).map(|i| 9100 + i).collect();
        let (_d, mut app) = ports_app(&ports, true);
        handle_key(&mut app, ch('G'));
        let pick = app.ports.as_ref().unwrap();
        assert_eq!(pick.cursor, 9);
        assert!(
            pick.cursor < pick.scroll as usize + crate::model::PORT_ROWS_VISIBLE,
            "the last row must be inside the scroll window: cursor={} scroll={}",
            pick.cursor,
            pick.scroll
        );
        handle_key(&mut app, key(KeyCode::End));
        assert_eq!(cursor(&app), 9);
        handle_key(&mut app, ch('g'));
        assert_eq!(cursor(&app), 0);
        assert_eq!(app.ports.as_ref().unwrap().scroll, 0, "home must reset the window");
        handle_key(&mut app, key(KeyCode::Home));
        assert_eq!(cursor(&app), 0);
    }

    // -- ticking -----------------------------------------------------------

    #[test]
    fn space_ticks_and_unticks_the_cursor_row() {
        let (_d, mut app) = ports_app(&[8080, 8081], true);
        handle_key(&mut app, ch(' '));
        assert_eq!(checked(&app), 1, "space ticks the highlighted row");
        assert!(app.ports.as_ref().unwrap().rows[0].checked);
        handle_key(&mut app, ch('j'));
        handle_key(&mut app, ch(' '));
        assert_eq!(checked(&app), 2);
        handle_key(&mut app, ch(' '));
        assert_eq!(checked(&app), 1, "a second space unticks the same row");
    }

    #[test]
    fn a_u_and_shift_i_tick_everything_none_and_invert() {
        let (_d, mut app) = ports_app(&[8080, 8081, 8082], true);
        handle_key(&mut app, ch('a'));
        assert_eq!(checked(&app), 3, "a ticks every row");
        handle_key(&mut app, ch('U'));
        assert_eq!(checked(&app), 0, "U unticks every row");
        handle_key(&mut app, ch('I'));
        assert_eq!(checked(&app), 3, "inverting an empty selection fills it");
        handle_key(&mut app, ch('I'));
        assert_eq!(checked(&app), 0, "and inverting a full selection empties it");
    }

    #[test]
    fn a_read_only_list_refuses_to_tick() {
        // A `-R` forward lists the ports already *occupied* on the peer, so a tick
        // could only fail at bind time. The model is the guard; the keys must not
        // silently do nothing either — the list still says "read-only" in its
        // footer, and these keys stay claimed so the form's fields do not move.
        let (_d, mut app) = ports_app(&[80, 443], false);
        for code in [KeyCode::Char(' '), KeyCode::Char('a'), KeyCode::Char('I')] {
            assert!(handle_port_key(&mut app, key(code)), "the list still owns the key");
            assert_eq!(checked(&app), 0, "{code:?} must not tick a read-only list");
        }
    }

    // -- accepting, rescanning, closing ------------------------------------

    #[test]
    fn enter_fills_the_remote_port_and_closes_the_list() {
        let (_d, mut app) = ports_app(&[8080, 9090], true);
        handle_key(&mut app, ch('j'));
        handle_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.form.as_ref().unwrap().remote_port.value(), "9090", "the highlighted port is filled in");
        assert!(!app.port_picker_open(), "accepting collapses the list");
        assert!(app.ports.is_none(), "and drops the scan state");
        assert_eq!(app.form.as_ref().unwrap().field, FormField::RemotePort, "the form stays where it was");
    }

    #[test]
    fn r_rescans_and_keeps_the_ticks() {
        let (_d, mut app) = ports_app(&[8080, 9090], true);
        handle_key(&mut app, ch('a'));
        assert_eq!(checked(&app), 2);
        handle_key(&mut app, ch('r'));
        let pick = app.ports.as_ref().unwrap();
        assert!(pick.state.is_scanning(), "r starts a new scan, it does not close the list");
        assert_eq!(pick.checked_count(), 2, "a rescan is a refresh: the user's ticks survive");
        assert!(app.port_picker_open());
    }

    #[test]
    fn esc_and_i_both_close_the_list() {
        // Two ways in (`p` is not the only thing users try) and two ways out:
        // `esc` is the universal "back out", `i` matches the import pane's key.
        for close in [ch('i'), key(KeyCode::Esc)] {
            let (_d, mut app) = ports_app(&[8080], true);
            handle_key(&mut app, close);
            assert!(!app.port_picker_open(), "the list must collapse");
            assert!(app.ports.is_none(), "and drop the stale scan state");
            assert!(app.form.is_some(), "closing the list must not close the form");
        }
    }

    // -- the form's keys stay the form's -----------------------------------

    #[test]
    fn keys_the_list_does_not_own_are_handed_back() {
        let (_d, mut app) = ports_app(&[8080], true);
        for code in [KeyCode::Char('z'), KeyCode::Char('x'), KeyCode::Tab, KeyCode::F(10)] {
            assert!(!handle_port_key(&mut app, key(code)), "{code:?} must stay with the form");
        }
        // And through the real router, `tab` still moves the form field.
        let before = app.form.as_ref().unwrap().field;
        handle_key(&mut app, key(KeyCode::Tab));
        assert_ne!(app.form.as_ref().unwrap().field, before);
    }

    // -- opening: `p` on the remote port field -----------------------------

    #[test]
    fn p_on_the_remote_port_field_opens_the_list() {
        let (_d, mut app) = form_app();
        {
            let f = app.form.as_mut().unwrap();
            f.field = FormField::RemotePort;
            f.ssh_host.set(FIXTURE_HOST);
        }
        handle_key(&mut app, ch('p'));
        assert!(app.port_picker_open(), "p on 远端端口 must expand the peer port list");
        assert!(app.port_picker_visible(), "and the scan state makes it worth drawing");
        app.close_port_picker();
    }

    #[test]
    fn p_on_another_field_does_not_open_the_list() {
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::Name;
        handle_key(&mut app, ch('p'));
        assert!(!app.port_picker_open(), "only the 远端端口 field binds p");
        assert!(app.ports.is_none());
    }

    #[test]
    fn p_while_a_field_is_being_edited_types_the_letter() {
        // The guard that matters: `adjust_field` runs only while navigating, so a
        // literal `p` in a tunnel name must reach the buffer, not start a scan.
        let (_d, mut app) = form_app();
        app.form.as_mut().unwrap().field = FormField::Name;
        handle_key(&mut app, key(KeyCode::Enter)); // start editing 名称
        for c in "api".chars() {
            handle_key(&mut app, ch(c));
        }
        assert_eq!(app.form.as_ref().unwrap().name.value(), "api", "p must be typed, not interpreted");
        assert!(!app.port_picker_open(), "typing p must not open the list");
        assert!(app.ports.is_none());
    }

    #[test]
    fn the_list_does_not_steal_keys_while_a_text_field_is_being_edited() {
        let (_d, mut app) = ports_app(&[8080, 8081], true);
        // `enter` belongs to the list (it accepts a row), so the editing stage is
        // entered directly here. That state is reachable: the list is an addition
        // to the form, not a modal, and the user can start typing in a field while
        // it is still expanded (e.g. after `tab` back onto 名称).
        app.form.as_mut().unwrap().begin_edit();
        assert!(app.form.as_ref().unwrap().is_editing_field());
        for c in "jai".chars() {
            handle_key(&mut app, ch(c));
        }
        assert_eq!(app.form.as_ref().unwrap().remote_port.value(), "jai", "letters must reach the field");
        assert_eq!(checked(&app), 0, "`a` must not tick everything behind the user");
        assert_eq!(cursor(&app), 0, "`j` must not move the list cursor");
        assert!(app.port_picker_open(), "`i` must not close the list either");
    }

    // -- staleness ---------------------------------------------------------

    #[test]
    fn changing_the_host_drops_a_stale_list() {
        let (_d, mut app) = ports_app(&[8080], true);
        assert!(app.port_picker_open(), "precondition: the list belongs to FIXTURE_HOST");
        app.form.as_mut().unwrap().ssh_host.set("other-box");
        // The next key that the list does not claim reaches sync_port_picker,
        // which drops rows scanned for the host the form no longer holds.
        handle_key(&mut app, key(KeyCode::Tab));
        assert!(!app.port_picker_open(), "a host change must drop the stale port list");
        assert!(app.ports.is_none());
    }
}

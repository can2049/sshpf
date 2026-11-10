//! Unit tests for `portscan.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;

fn picker(ports: &[u16]) -> PortPick {
    let mut p = PortPick::scanning("dev-box", true, 1);
    let rows = ports.iter().map(|port| RemotePort::new(*port, "127.0.0.1")).collect();
    assert!(p.install(1, rows, 0), "the current generation installs");
    p
}

// -- target-host suggestion --------------------------------------------

#[test]
fn a_concrete_bind_address_is_suggested_as_the_target() {
    let row = RemotePort::new(5432, "192.168.1.5");
    assert_eq!(row.suggest_target_host(), Some("192.168.1.5"));
    assert!(!row.is_wildcard());
}

#[test]
fn a_wildcard_bind_address_is_not_suggested_as_the_target() {
    // Dialling `0.0.0.0` is not the same as dialling the loopback: the caller
    // must keep the address the user typed instead.
    for wildcard in ["0.0.0.0", "*", "[::]", "::"] {
        let row = RemotePort::new(8080, wildcard);
        assert_eq!(row.suggest_target_host(), None, "{wildcard} must not become a target");
        assert!(row.is_wildcard(), "{wildcard} must be recognised as a wildcard");
    }
}

#[test]
fn ambiguous_or_scoped_rows_get_no_target_suggestion() {
    // Several addresses: no single right answer, so keep the typed value.
    let mut row = RemotePort::new(8080, "127.0.0.1");
    row.push_addr("10.0.0.9");
    assert_eq!(row.suggest_target_host(), None);

    // A scope-qualified address is not portable across machines.
    let scoped = RemotePort::new(53, "127.0.0.53%lo");
    assert_eq!(scoped.suggest_target_host(), None);

    // No address at all is not a suggestion either.
    assert_eq!(RemotePort::new(1, "").suggest_target_host(), None);
}

#[test]
fn the_address_list_is_capped_and_deduplicated() {
    let mut row = RemotePort::new(8080, "0.0.0.0");
    for i in 0..50 {
        row.push_addr(&format!("10.0.0.{i}"));
    }
    assert!(row.addrs.len() <= MAX_ADDRS, "the list must be capped: {:?}", row.addrs);
    row.push_addr("0.0.0.0");
    assert_eq!(row.addrs[0], "0.0.0.0", "a repeat must not be appended");
    assert_eq!(row.addrs.iter().filter(|a| *a == "0.0.0.0").count(), 1);
}

#[test]
fn display_helpers_are_readable() {
    let row = RemotePort::new(8080, "127.0.0.1");
    assert_eq!(row.port_text(), "8080");
    assert_eq!(row.addrs_text(), "127.0.0.1");
    assert_eq!(RemotePort::new(1, "").addrs_text(), "—", "no address must not render as an empty cell");
}

// -- picker state ------------------------------------------------------

#[test]
fn a_fresh_scan_ticks_nothing() {
    // Discovering a service is not a request to forward it: one keypress must
    // never open several listeners.
    let p = picker(&[22, 8080]);
    assert_eq!(p.checked_count(), 0);
    assert_eq!(p.checked_ports(), Vec::<u16>::new());
    assert_eq!(p.state, PortScanState::Ready { count: 2 });
}

#[test]
fn toggling_and_bulk_keys_follow_the_cursor() {
    let mut p = picker(&[22, 8080, 9000]);
    p.toggle_cursor();
    assert_eq!(p.checked_ports(), vec![22]);
    p.move_cursor(1);
    p.toggle_cursor();
    assert_eq!(p.checked_ports(), vec![22, 8080]);
    p.uncheck_all();
    assert_eq!(p.checked_count(), 0);
    p.check_all();
    assert_eq!(p.checked_ports(), vec![22, 8080, 9000]);
    p.invert();
    assert_eq!(p.checked_count(), 0);
    p.invert();
    assert_eq!(p.checked_count(), 3);
}

#[test]
fn a_read_only_picker_refuses_every_tick() {
    // `-R` lists ports that are *occupied* on the peer: creating a forward on
    // one of them would fail at bind time, so the list is informational.
    let mut p = PortPick::scanning("dev-box", false, 1);
    let rows = vec![RemotePort::new(22, "0.0.0.0"), RemotePort::new(8080, "*")];
    assert!(p.install(1, rows, 0));
    assert!(!p.can_tick());
    p.toggle_cursor();
    p.check_all();
    p.invert();
    assert_eq!(p.checked_count(), 0, "a read-only list must never produce a tick");
    assert_eq!(p.checked_ports(), Vec::<u16>::new());
}

#[test]
fn cursor_movement_is_clamped_and_scrolls_the_window() {
    let mut p = picker(&(1..=20).collect::<Vec<u16>>());
    assert_eq!(p.cursor, 0);
    p.move_cursor(-5);
    assert_eq!(p.cursor, 0, "moving up from the top stays put");
    p.end();
    assert_eq!(p.cursor, 19);
    assert!(p.cursor < p.scroll as usize + PORT_ROWS_VISIBLE, "the last row must be visible");
    p.home();
    assert_eq!((p.cursor, p.scroll), (0, 0));
    for _ in 0..100 {
        p.move_cursor(1);
    }
    assert_eq!(p.cursor, 19, "a huge delta stops at the last row");
    assert!(p.cursor < p.scroll as usize + PORT_ROWS_VISIBLE);
}

#[test]
fn a_huge_negative_delta_does_not_overflow() {
    let mut p = picker(&[22, 8080]);
    p.move_cursor(i32::MIN);
    assert_eq!(p.cursor, 0);
    p.move_cursor(i32::MAX);
    assert_eq!(p.cursor, 1);
}

#[test]
fn a_scroll_left_at_u16_max_is_repaired() {
    let mut p = picker(&(1..=20).collect::<Vec<u16>>());
    p.scroll = u16::MAX;
    p.ensure_visible(PORT_ROWS_VISIBLE);
    assert!(p.scroll as usize <= p.cursor);
    assert!(p.cursor < p.scroll as usize + PORT_ROWS_VISIBLE);
}

#[test]
fn a_stale_scan_result_is_ignored() {
    // A slow probe for a previous host must never overwrite the rows of the
    // host the user is looking at now.
    let mut p = PortPick::scanning("old-host", true, 7);
    assert!(!p.install(6, vec![RemotePort::new(22, "0.0.0.0")], 0), "an older generation must not apply");
    assert!(!p.fail(6, "old failure"));
    assert_eq!(p.state, PortScanState::Scanning);
    assert!(p.is_empty());
    assert!(p.install(7, vec![RemotePort::new(22, "0.0.0.0")], 0));
    assert_eq!(p.state, PortScanState::Ready { count: 1 });
}

#[test]
fn a_rescan_keeps_ticked_ports_until_the_new_answer_arrives() {
    let mut p = picker(&[22, 8080]);
    p.toggle_cursor(); // tick 22
    p.move_cursor(1);
    p.toggle_cursor(); // tick 8080
    p.begin_rescan(2);
    assert_eq!(p.state, PortScanState::Scanning);
    assert_eq!(p.checked_ports(), vec![22, 8080], "ticks survive the refresh of the same host");
    // The new result replaces the rows: a port that stopped listening is gone
    // and its tick with it.
    assert!(p.install(2, vec![RemotePort::new(8080, "127.0.0.1"), RemotePort::new(9000, "127.0.0.1")], 0));
    assert_eq!(p.checked_count(), 0, "installed rows are a fresh decision");
    assert_eq!(p.len(), 2);
}

#[test]
fn visibility_needs_more_than_the_open_flag() {
    let mut p = PortPick::scanning("h", true, 1);
    assert!(p.is_visible(), "a scan in flight is worth showing (it explains the wait)");
    p.open = false;
    assert!(!p.is_visible());
    p.open = true;
    assert!(p.fail(1, "boom"));
    assert!(p.is_visible(), "a failure is worth showing: it is the reason there is nothing to pick");
    assert_eq!(p.state.failure(), Some("boom"));
    assert!(p.state.is_failed());
    assert!(!p.state.is_scanning());
}

#[test]
fn a_default_pick_is_collapsed_and_idle() {
    // `App` may hold a reset picker; drawing it must do nothing.
    let p = PortPick::default();
    assert!(!p.open);
    assert!(!p.is_visible());
    assert_eq!(p.state, PortScanState::Idle);
    assert!(!p.state.is_failed());
    assert!(!p.state.is_scanning());
    assert!(p.cursor_row().is_none());
    assert!(!p.can_tick());
}

#[test]
fn checked_ports_are_unique_and_sorted() {
    let mut p = picker(&[9000, 22, 8080]);
    p.check_all();
    assert_eq!(p.checked_ports(), vec![22, 8080, 9000]);
}

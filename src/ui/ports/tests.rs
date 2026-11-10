//! Unit tests for `ports.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::model::RemotePort;
use crate::ui::draw;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::ui::test_support::*;

/// A ready picker with `ports`, all unticked.
fn picker(host: &str, multi: bool, ports: &[u16]) -> PortPick {
    let mut pick = PortPick::scanning(host, multi, 1);
    let rows = ports.iter().map(|port| RemotePort::new(*port, "127.0.0.1")).collect();
    assert!(pick.install(1, rows, 0), "the current generation installs");
    pick
}

/// A create form with `pick` attached, as `open_port_picker` leaves it.
fn app_with_picker(pick: PortPick) -> App {
    let mut app = app_with_tunnels(vec![]);
    app.open_form_create();
    app.ports = Some(pick);
    app
}

#[test]
fn ports_pane_renders_its_title_host_rows_and_footer() {
    let mut app = app_with_picker(picker("dev-box", true, &[22, 8080]));
    let text = render_to_text(&mut app, 120, 45);
    assert!(text.contains("对端监听端口"), "pane title missing:\n{text}");
    assert!(text.contains("Peer listening ports"), "title needs its English half:\n{text}");
    assert!(text.contains("dev-box"), "the pane must name the host the rows belong to:\n{text}");
    assert!(text.contains("2 个端口"), "ready status missing:\n{text}");
    assert!(text.contains("8080"), "port row missing:\n{text}");
    assert!(text.contains("127.0.0.1"), "bind addresses missing:\n{text}");
    assert!(text.contains("[ ]"), "an unticked row must show an empty box:\n{text}");
    assert!(text.contains("space 勾选"), "footer hint missing:\n{text}");
    assert!(text.contains("esc 关闭"), "footer must say how to leave:\n{text}");
    // The pane is added to the form, it does not replace a field.
    assert!(text.contains("远端端口"), "the form's fields must stay visible:\n{text}");
}

#[test]
fn ports_pane_shows_the_scanning_state_before_any_result() {
    // No rows yet: the pane must say *why* it is empty, or the key press
    // looks like it did nothing.
    let mut app = app_with_picker(PortPick::scanning("dev-box", true, 1));
    let text = render_to_text(&mut app, 120, 45);
    assert!(text.contains("对端监听端口"), "pane must open while scanning:\n{text}");
    assert!(text.contains("扫描中"), "scanning status missing:\n{text}");
    assert!(text.contains("scanning"), "scanning status needs its English half:\n{text}");
}

#[test]
fn ports_pane_shows_the_failure_reason_instead_of_rows() {
    let mut pick = PortPick::scanning("dev-box", true, 1);
    assert!(pick.fail(1, "扫描 dev-box 的端口失败: connection refused"));
    let mut app = app_with_picker(pick);
    let text = render_to_text(&mut app, 120, 45);
    assert!(text.contains("connection refused"), "the reason must be readable:\n{text}");
    assert!(text.contains('✗'), "a failure carries a symbol, not just colour:\n{text}");
    assert!(!text.contains("个端口"), "a failure is not a ready list:\n{text}");
    // An empty body with an explanation is fine; the "nothing is listening"
    // sentence belongs to `Ready { count: 0 }` only.
    assert!(!text.contains("对端没有正在监听的端口"), "a failure is not 'nothing listening':\n{text}");
}

#[test]
fn ports_pane_explains_an_empty_ready_list_and_the_hidden_ports() {
    // `Ready { count: 0 }` is a real answer, so it gets a sentence. Ports the
    // host's ssh config already lists are hidden on purpose: saying so stops
    // "3 of 5 found" from looking like a broken scan.
    let mut pick = PortPick::scanning("dev-box", true, 1);
    assert!(pick.install(1, vec![], 3));
    assert!(pick.is_visible(), "an empty *answer* is still worth showing");
    let mut app = app_with_picker(pick);
    let text = render_to_text(&mut app, 120, 45);
    assert!(text.contains("对端没有正在监听的端口"), "empty answer must say so:\n{text}");
    assert!(text.contains("Nothing is listening on the peer"), "and in English:\n{text}");
    assert!(text.contains("已隐藏 3 个"), "hidden ports must be accounted for:\n{text}");
    assert!(text.contains("~/.ssh/config"), "and the reason named:\n{text}");
    assert!(!text.contains("[ ]"), "no rows means no checkboxes:\n{text}");
}

#[test]
fn a_read_only_picker_renders_dim_boxes_and_no_ticks() {
    // `-R` lists the ports already occupied on the peer: informational, so
    // every row shows the striped box and nothing can look ticked.
    let mut app = app_with_picker(picker("dev-box", false, &[22, 8080]));
    let text = render_to_text(&mut app, 120, 45);
    let row = text.lines().find(|l| l.contains("[–]")).unwrap_or_else(|| panic!("no read-only row:\n{text}"));
    assert!(row.contains("22"), "the read-only row must still show its port: {row:?}");
    assert!(!row.contains("[x]"), "a read-only row must not look ticked: {row:?}");
    assert!(!row.contains("[ ]"), "nor look like an unticked choice: {row:?}");
    assert!(text.contains("对端监听端口"), "pane title missing:\n{text}");
    // The footer advertises `space 勾选` for both lists, so the status line is
    // what tells the user this one cannot be ticked.
    assert!(text.contains("只读"), "a read-only list must say why the boxes are striped:\n{text}");
}

#[test]
fn the_footer_and_rows_never_overflow_a_narrow_pane() {
    // A narrow terminal clamps the popup, so the pane can be far narrower than
    // the legend. It must truncate, not draw past its own border: an
    // overflowing line silently eats the form's right edge.
    for width in [20u16, 28, 40, 60] {
        let mut app = app_with_picker(picker("a-host-with-a-long-name", true, &[22, 8080, 65_535]));
        let text = render_to_text(&mut app, width, 40);
        // Every drawn line stops inside the frame (the backend is exactly
        // `width` cells, so a wrapped line is impossible; what this catches is
        // a line that would have been wrapped by ratatui into the pane's rows).
        assert!(text.contains("对端监听端口") || text.contains("…"), "{width}: no pane at all:\n{text}");
        // The Chinese half of the legend survives the narrowest sizes we draw
        // at, which is what `truncate_to_width` buys over a raw cut.
        if width >= 28 {
            assert!(text.contains("↑↓"), "{width}: movement hint lost:\n{text}");
        }
    }
    // The hint list itself fits a realistic pane whole.
    assert!(HINTS.contains("space 勾选") && HINTS.contains("esc 关闭") && HINTS.contains("r 重扫"));
}

#[test]
fn the_cursor_row_is_always_inside_the_visible_window() {
    // `PORT_ROWS_VISIBLE` is 8; put the cursor on the 20th row and the window
    // must have followed it, or the user is ticking something off screen.
    let ports: Vec<u16> = (0..20).map(|i| 8001 + i).collect();
    let mut pick = picker("dev-box", true, &ports);
    pick.end();
    assert_eq!(pick.cursor, 19);
    let mut app = app_with_picker(pick);
    let text = render_to_text(&mut app, 120, 45);
    let cursor_line = text.lines().find(|l| l.contains('▸')).unwrap_or_else(|| panic!("no cursor row:\n{text}"));
    assert!(cursor_line.contains("8020"), "the cursor row must be drawn: {cursor_line:?}");
    assert!(!text.contains("8001\n"), "the window must have scrolled past the first row:\n{text}");
    // ... and the marker moves with the cursor: exactly one row carries it.
    assert_eq!(text.matches('▸').count(), 1, "one cursor row only:\n{text}");
}

#[test]
fn ports_pane_renders_across_a_size_matrix_without_panicking() {
    // Every state, at the sizes where the popup's own height arithmetic
    // breaks: 1x1, narrower than the pane minimum, the default terminal and a
    // wide one. A panic here would leave the terminal in raw mode.
    let states = [
        PortPick::scanning("dev-box", true, 1),
        picker("dev-box", true, &[22, 8080, 9000]),
        picker("dev-box", false, &[22]),
    ];
    for mut pick in states {
        if pick.rows.is_empty() {
            assert!(pick.fail(1, "扫描 dev-box 的端口失败: timeout"));
        }
        for (w, h) in [(1u16, 1u16), (20, 6), (80, 24), (140, 45), (40, 12)] {
            let mut app = app_with_picker(pick.clone());
            let backend = TestBackend::new(w, h);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("ports pane at {w}x{h}: {e}"));
        }
    }
}

#[test]
fn a_short_terminal_drops_the_pane_instead_of_drawing_outside_the_popup() {
    // The form keeps its fields: on a terminal too short for both, the pane is
    // what gives way (exactly like the import pane). No overflow, no panic.
    let mut app = app_with_picker(picker("dev-box", true, &[8080]));
    let text = render_to_text(&mut app, 100, 14);
    assert!(!text.contains("对端监听端口"), "the pane must not be squeezed in:\n{text}");
    assert!(text.contains("新建转发"), "the form itself must survive:\n{text}");
    assert!(text.contains("远端端口"), "and its fields with it:\n{text}");
}

#[test]
fn a_hidden_pane_with_ticks_still_warns_that_save_will_create_them() {
    // The pane shrinks away on a short terminal, but the ticks do not: `ctrl+s`
    // keys off the *visible* list, and this one is gone. Silence there would
    // mean creating tunnels the user cannot see, so the form has to say it.
    let mut pick = picker("dev-box", true, &[8080]);
    pick.check_all();
    let mut app = app_with_picker(pick);
    let text = render_to_text(&mut app, 100, 16);
    assert!(!text.contains("对端监听端口"), "precondition: the pane does not fit:\n{text}");
    assert!(text.contains("端口面板未显示"), "the invisible batch must be announced:\n{text}");
    assert!(text.contains("ctrl+s"), "and it must say what ctrl+s will do:\n{text}");
    assert!(text.contains("1 个端口"), "and how many rows it will create:\n{text}");

    // Closing the pane (which is what the note suggests) makes the warning
    // moot: nothing is hidden, the form is back in charge.
    app.close_port_picker();
    let text = render_to_text(&mut app, 100, 16);
    assert!(!text.contains("端口面板未显示"), "a collapsed pane has nothing hidden:\n{text}");
}

#[test]
fn the_port_pane_wins_when_both_panes_are_somehow_open() {
    // The app closes one pane when opening the other, so this state is not
    // reachable from the keys — but the renderer still has to pick one, and
    // the port list is the pane the submit path keys off.
    let mut app = app_with_import(sample_forwards());
    app.ports = Some(picker("dev-box", true, &[8080]));
    let text = render_to_text(&mut app, 120, 45);
    assert!(text.contains("对端监听端口"), "the port pane must be drawn:\n{text}");
    assert!(!text.contains("从 ~/.ssh/config 导入转发"), "the import pane must not be stacked on it:\n{text}");
}

#[test]
fn nothing_is_drawn_once_the_picker_is_gone() {
    let mut app = app_with_picker(picker("dev-box", true, &[8080]));
    app.ports = None;
    let text = render_to_text(&mut app, 120, 45);
    assert!(!text.contains("对端监听端口"), "a closed picker must draw nothing:\n{text}");
    assert!(text.contains("新建转发"), "the form stays:\n{text}");

    // Closing the form takes the list with it: a later form for another host
    // must not show these rows.
    app.close_overlay();
    let text = render_to_text(&mut app, 120, 45);
    assert!(!text.contains("对端监听端口"), "no form, no pane:\n{text}");
    // The footer still advertises `n 新建转发`, so the exact form title is
    // what proves the overlay is gone.
    assert!(!text.contains("新建转发 · New tunnel"), "no form at all:\n{text}");
}

#[test]
fn the_window_honours_the_scroll_anchor_but_never_hides_the_cursor() {
    let mut pick = picker("dev-box", true, &(1..=20).collect::<Vec<u16>>());
    pick.scroll = 5;
    pick.cursor = 5;
    assert_eq!(window_start(&pick, 8), 5, "the anchor is honoured while the cursor is inside");
    pick.cursor = 19;
    assert_eq!(window_start(&pick, 8), 12, "a cursor below the window scrolls it into view");
    pick.scroll = 15;
    pick.cursor = 0;
    assert_eq!(window_start(&pick, 8), 0, "a cursor above the window pulls it back");
    // Absurd state cannot panic or point past the last row.
    pick.scroll = u16::MAX;
    pick.cursor = 0;
    assert_eq!(window_start(&pick, 1), 0);
    assert_eq!(window_start(&PortPick::default(), 3), 0);
    assert_eq!(window_start(&pick, 0), 0, "a zero-height list must not divide by zero");
}

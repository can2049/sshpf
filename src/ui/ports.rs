//! The peer-port picker pane: the ports the SSH *peer* is listening on.
//!
//! Rendered *inside* the create form (see [`crate::ui::form::draw_form`]), in the
//! same slot as the import pane: below the field rows, added to the popup's
//! height rather than borrowed from it. Typing a remote port is the one step in
//! "make a tunnel" that asks for something the machine next to the user already
//! knows, so the answer is shown where the field is.
//!
//! # The four states, and why none of them is a blank box
//!
//! | [`PortScanState`] | the pane shows |
//! |---|---|
//! | `Scanning` | `扫描中… / scanning…` (a wait that explains itself) |
//! | `Failed { reason }` | the bilingual reason the scan produced |
//! | `Ready { count: 0 }` | the explicit "nothing is listening on the peer" line |
//! | `Ready { count }` | `N 个端口`, the rows, how many ssh-config ports were hidden, and whether the list is read-only |
//!
//! An empty bordered box is the one thing this pane never draws: it reads as a
//! rendering bug, and the user cannot tell it from "the key did nothing".
//!
//! # The window
//!
//! The rows are windowed by [`PORT_ROWS_VISIBLE`] — the same constant the key
//! handler scrolls by — so the cursor can never leave the visible window. The
//! renderer still nudges the window locally when the pane is shorter than that
//! (a short terminal): drawing must never move app state, exactly like
//! [`crate::ui::import::body`].
//!
//! # No mouse targets, deliberately
//!
//! The pane is keyboard-driven, so it records nothing into the [`HitMap`]. Every
//! `HitTarget` variant must be documented in the in-app help
//! (`tests/help_coverage.rs` asserts it), and a panel with no clickable parts has
//! no business growing that enum.
//!
//! Belongs here: the pane frame, its title, status line, rows and footer.
//! Does not belong: the scan itself (`crate::app::port_scan`), the picker state
//! machine (`crate::model::portscan`), key dispatch, and turning ticked rows into
//! tunnels (`crate::app::port_batch`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::App;
use crate::model::{PORT_ROWS_VISIBLE, PortPick, PortRow, PortScanState};
use crate::text::{display_width, fit, pad_to_width, truncate_to_width};
use crate::ui::hit::HitMap;
use crate::ui::palette::{Palette, palette};
use crate::ui::text::fit_pair_spaced;

/// Height the popup asks for: rounded frame (2) + status line (1) +
/// [`PORT_ROWS_VISIBLE`] rows + the key-hint footer (1).
///
/// `ui::form` adds this to the popup it would draw anyway, so the form grows
/// instead of squeezing the fields whose values the user compares against the
/// rows they tick.
pub const PREF_HEIGHT: u16 = 2 + 1 + PORT_ROWS_VISIBLE as u16 + 1;

/// Below this the pane is not worth a slot: frame (2) + status (1) + one row (1) +
/// footer (1). Shorter and the form keeps the space for its own fields (a pane
/// with no row in it is pure noise).
pub const MIN_HEIGHT: u16 = 5;

/// Widest a port cell can be (`65535`), so the address column always starts on
/// the same cell and the list reads as a column.
const PORT_CELL_W: usize = 5;

/// The footer's whole legend, on one line, clipped by [`truncate_to_width`] when
/// the pane is narrow.
///
/// One line and not a wrapped block like the import pane's: this pane has six
/// bindings, not a dozen, and the status line above already carries the state. On
/// a narrow terminal the tail is what goes — the pane stays usable because every
/// key is also documented in the help overlay.
pub(crate) const HINTS: &str = "↑↓ 选择 · space 勾选 · enter 填入 · a/U/I 全选/清空/反选 · r 重扫 · esc 关闭";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The pane, with its bilingual title (the host name stays last in both halves).
///
/// The `HitMap` is accepted but not written to: see the module docs — this pane
/// has no clickable parts, and the signature mirrors [`crate::ui::import::draw_pane`]
/// so the form's call site stays a match on the pane kind.
pub(crate) fn draw_port_pane(frame: &mut Frame, app: &App, area: Rect, _hits: &mut HitMap) {
    // Same predicate `App::port_picker_visible` uses: a collapsed or never-opened
    // list must draw nothing, and a pane with neither rows nor a phase would be a
    // blank box.
    let Some(pick) = app.ports.as_ref().filter(|pick| pick.is_visible()) else { return };
    if area.width < 8 || area.height < MIN_HEIGHT {
        return;
    }
    let p = palette();
    let block =
        Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(p.fg(p.accent)).title(
            Line::from(Span::styled(
                title(pick, area.width.saturating_sub(2) as usize),
                p.fg(p.accent).add_modifier(Modifier::BOLD),
            )),
        );
    let inner = block.inner(area);
    // `Clear`: the pane sits on top of the form's own background.
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    draw_body(frame, pick, inner, p);
}

/// `对端监听端口 · <host> / Peer listening ports`, fitted to the border width.
fn title(pick: &PortPick, width: usize) -> String {
    // The host is in the *Chinese* half only: it is never repeated, and it is
    // dropped only when even the Chinese half no longer fits — the rows belong to
    // this host, and a list that does not name it is one host switch away from a
    // lie.
    let text = fit(&format!("对端监听端口 · {}", pick.host), "Peer listening ports", " / ", width.saturating_sub(2));
    // One cell of air on each side keeps the title off the rounded corner, the
    // same way every other bordered panel does it. It is added *outside* the
    // fitted text, so the two halves are joined by exactly one ` / `.
    format!(" {text} ")
}

/// Status line on top, rows in the middle, key hints on the bottom.
fn draw_body(frame: &mut Frame, pick: &PortPick, inner: Rect, p: Palette) {
    let width = inner.width as usize;
    frame.render_widget(Paragraph::new(status_line(pick, width, p)), Rect { height: 1, ..inner });
    if inner.height < 2 {
        // One inner row: the status *is* the pane.
        return;
    }
    let footer_y = inner.bottom().saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(truncate_to_width(HINTS, width), p.fg(p.dim)))),
        Rect { x: inner.x, y: footer_y, width: inner.width, height: 1 },
    );
    let rows = Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: footer_y.saturating_sub(inner.y + 1) };
    if rows.height == 0 {
        return;
    }
    draw_rows(frame, pick, rows, width, p);
}

/// The state sentence, with a symbol on failures so `NO_COLOR` still reads.
fn status_line(pick: &PortPick, width: usize, p: Palette) -> Line<'static> {
    let (text, style) = match &pick.state {
        // Unreachable through `App` (an idle picker is not visible), but a plain
        // empty line is the only honest thing to draw if it ever happens.
        PortScanState::Idle => (String::new(), p.fg(p.dim)),
        PortScanState::Scanning => (fit_pair_spaced("扫描中…", "scanning…", width), p.fg(p.pending)),
        PortScanState::Failed { reason } => (truncate_to_width(&format!("✗ {reason}"), width), p.fg(p.failed)),
        PortScanState::Ready { count } => {
            (ready_status(pick, *count, width), p.fg(p.accent).add_modifier(Modifier::BOLD))
        }
    };
    Line::from(Span::styled(text, style))
}

/// `12 个端口` — plus how many ports were hidden because the host's
/// `~/.ssh/config` already declares them.
///
/// The exclusion count is spelled out for the same reason as the empty message:
/// a peer with 30 services that lists 2 rows and says nothing looks like a broken
/// scan, when in fact the other 28 belong to the import pane.
///
/// A read-only list says so too. Its footer still advertises `space 勾选` (the
/// keys are the same for both lists), so without this line the `[–]` boxes and the
/// hint would contradict each other and the user would press `space` at a row that
/// cannot move.
fn ready_status(pick: &PortPick, count: usize, width: usize) -> String {
    let mut zh = format!("{count} 个端口");
    let mut en = format!("{count} ports");
    if pick.excluded > 0 {
        zh.push_str(&format!("，已隐藏 {} 个（~/.ssh/config 已列出）", pick.excluded));
        en.push_str(&format!(", {} hidden (~/.ssh/config declares them)", pick.excluded));
    }
    if !pick.multi {
        zh.push_str(" · 只读（-R 的对端端口已被占用）");
        en.push_str(" · read-only (a -R peer port is taken)");
    }
    fit_pair_spaced(&zh, &en, width)
}

/// The visible rows, one per line, or the one sentence that explains why there
/// are none.
fn draw_rows(frame: &mut Frame, pick: &PortPick, area: Rect, width: usize, p: Palette) {
    if pick.rows.is_empty() {
        // Only `Ready` gets a body message: `Scanning` and `Failed` are already
        // spelled out by the status line, and repeating one fact on two lines
        // spends the pane's scarcest resource. `Ready { count: 0 }` is a real
        // answer to the question, so it must not look like an empty box.
        if matches!(pick.state, PortScanState::Ready { .. }) {
            let msg = fit_pair_spaced("对端没有正在监听的端口", "Nothing is listening on the peer", width);
            frame.render_widget(Paragraph::new(Line::from(Span::styled(msg, p.fg(p.dim)))), Rect { height: 1, ..area });
        }
        return;
    }
    let rows_h = area.height as usize;
    let start = window_start(pick, rows_h);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows_h.min(pick.len()));
    for (i, row) in pick.rows.iter().enumerate().skip(start).take(rows_h) {
        lines.push(row_line(row, i == pick.cursor, pick.multi, width, p));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// One row: `▸[x] 8080  127.0.0.1, 0.0.0.0`.
///
/// `▸` marks the cursor row as a *symbol* as well as a highlight bar, so a
/// `NO_COLOR` run still shows where the selection is (same rule as
/// [`crate::ui::import::body`]). A read-only list (`multi == false`, i.e. a `-R`
/// forward, where the listed ports are the ones already taken on the peer) shows
/// `[–]` and is drawn dim: it is information, not a choice.
fn row_line(row: &PortRow, is_cursor: bool, multi: bool, width: usize, p: Palette) -> Line<'static> {
    let lead = if is_cursor { "▸" } else { " " };
    let check = if !multi {
        "[–]"
    } else if row.checked {
        "[x]"
    } else {
        "[ ]"
    };
    let check_style = if multi && row.checked { p.fg(p.up) } else { p.fg(p.dim) };
    let port_style = if multi { p.fg(p.text).add_modifier(Modifier::BOLD) } else { p.fg(p.dim) };

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(5);
    spans.push(Span::styled(format!("{lead}{check}"), check_style));
    spans.push(Span::raw(" "));
    let mut used = display_width(lead) + display_width(check) + 1;

    // The port is fixed-width so the addresses line up as a column; on a pane too
    // narrow even for that, it is what survives and the addresses go.
    let port_w = PORT_CELL_W.min(width.saturating_sub(used));
    if port_w > 0 {
        let port = truncate_to_width(&row.port.port_text(), port_w);
        spans.push(Span::styled(pad_to_width(&port, port_w), port_style));
        used += port_w;
    }
    let addrs_w = width.saturating_sub(used + 1);
    if addrs_w > 0 {
        spans.push(Span::raw(" "));
        used += 1;
        let addrs = truncate_to_width(&row.port.addrs_text(), addrs_w);
        used += display_width(&addrs);
        spans.push(Span::styled(addrs, p.fg(p.dim)));
    }
    // A highlight bar that stops mid-row reads as a text selection rather than a
    // focused row, so the line is padded to the full width first.
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    let line = Line::from(spans);
    if is_cursor { line.style(p.focused().add_modifier(Modifier::BOLD)) } else { line }
}

/// The first visible row to draw.
///
/// `PortPick::scroll` is the anchor (the key handler keeps it roughly right with
/// [`PORT_ROWS_VISIBLE`]), but the pane's real row budget depends on the terminal,
/// so the window is nudged here when the cursor would otherwise be outside it.
/// Read-only on purpose: rendering must never move app state.
fn window_start(pick: &PortPick, rows_h: usize) -> usize {
    if pick.rows.is_empty() {
        return 0;
    }
    let rows_h = rows_h.max(1);
    let last = pick.rows.len() - 1;
    let mut start = (pick.scroll as usize).min(last);
    let cursor = pick.cursor.min(last);
    if cursor < start {
        start = cursor;
    } else if cursor >= start.saturating_add(rows_h) {
        start = cursor + 1 - rows_h;
    }
    start
}

#[cfg(test)]
mod tests {
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
}

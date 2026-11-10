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
mod tests;

//! The pane's body: the visible rows and the scroll window that decides which
//! ones they are.
//!
//! [`window_start`] is the one place the renderer nudges the list so the cursor
//! stays inside it — read-only on purpose, because drawing must never move app
//! state. Row hit rects are pushed as the rows are drawn, and one drawn below the
//! pane's own area gets none (a click there belongs to the form's fields).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::model::ImportTable;
use crate::text::{fit, truncate_to_width};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::import::footer::{FOOTER_H, draw_footer, footer_summary, pack_hints};
use crate::ui::import::header::{draw_buttons, draw_chips, draw_header};
use crate::ui::import::row::{CHECK_W, ColSpec, columns, row_line};
use crate::ui::palette::Palette;

// ---------------------------------------------------------------------------
// Body: header / rows / filters / buttons / footer
// ---------------------------------------------------------------------------

pub(super) fn draw_body(frame: &mut Frame, table: &ImportTable, inner: Rect, p: Palette, hits: &mut HitMap) {
    let width = inner.width as usize;
    if inner.height < 3 {
        // Not even a header + one row: show only the state summary rather than a
        // half-painted table.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate_to_width(&footer_summary(table), width), p.fg(p.accent)))),
            inner,
        );
        return;
    }

    // Reserve from the bottom up: the footer (summary + wrapped key hints), then
    // the button row, then the chip line — each only while a header and at least
    // one data row still fit above it. The footer's height depends on the pane
    // width, so it is measured first and clipped rather than overflowing.
    let max_footer_rows = inner.height.saturating_sub(2).max(1);
    let hint_lines = pack_hints(width, (FOOTER_H.saturating_sub(1)).min(max_footer_rows.saturating_sub(1)) as usize);
    let footer_rows = (1 + hint_lines.len() as u16).min(max_footer_rows);
    let footer_y = inner.bottom() - footer_rows;
    let mut list_bottom = footer_y;

    let mut buttons_y = None;
    if list_bottom > inner.y + 2 {
        list_bottom -= 1;
        buttons_y = Some(list_bottom);
    }
    let mut chips_y = None;
    if table.has_filters() && list_bottom > inner.y + 2 {
        list_bottom -= 1;
        chips_y = Some(list_bottom);
    }

    let cols = columns(inner.width);
    draw_header(frame, table, &cols, inner, p, hits);

    let list_area =
        Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: list_bottom.saturating_sub(inner.y + 1) };
    draw_rows(frame, table, &cols, list_area, inner, p, hits);

    if let Some(y) = chips_y {
        draw_chips(frame, table, inner, y, p, hits);
    }
    if let Some(y) = buttons_y {
        draw_buttons(frame, table, inner, y, p, hits);
    }
    draw_footer(frame, table, inner, footer_y, inner.bottom(), &hint_lines, p);
}

/// One row per line: `▸[x] | T | 监听 | 目标 | 原始指令`.
pub(super) fn draw_rows(
    frame: &mut Frame,
    table: &ImportTable,
    cols: &[ColSpec],
    list_area: Rect,
    inner: Rect,
    p: Palette,
    hits: &mut HitMap,
) {
    let visible = table.visible();
    // The whole list area is one scrolling target; rows are pushed after it so a
    // click on a row wins over the background.
    hits.push(list_area, HitTarget::ImportList);

    if visible.is_empty() {
        let msg =
            fit("没有匹配的行 · 按 F 清除筛选", "No matching rows · F clears the filter", " ", inner.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate_to_width(&msg, inner.width as usize), p.fg(p.dim)))),
            Rect { height: 1, ..list_area },
        );
        return;
    }
    if list_area.height == 0 {
        return;
    }

    let rows_h = list_area.height as usize;
    let start = window_start(table, &visible, rows_h);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows_h.min(visible.len()));
    for (i, idx) in visible.iter().enumerate().skip(start).take(rows_h) {
        let Some(row) = table.rows.get(*idx) else { continue };
        // Belt and braces on top of `.take(rows_h)`: a row drawn below the pane
        // must not get a hit rect, or a click on the form's own fields would
        // resolve to an import row that is not actually there.
        let y = list_area.y + lines.len() as u16;
        if y >= list_area.bottom() {
            break;
        }
        lines.push(row_line(row, i == table.cursor, cols, inner.width, p));
        let rect = Rect { x: inner.x, y, width: inner.width, height: 1 };
        // Two hits per row, narrow last: a click on the box toggles, a click
        // anywhere else only moves the cursor there.
        hits.push(rect, HitTarget::ImportRow(*idx));
        hits.push(Rect { width: CHECK_W.min(inner.width), ..rect }, HitTarget::ImportCheckbox(*idx));
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// The first visible row to draw.
///
/// `ImportTable::scroll` is the anchor (`App` keeps it roughly right with its own
/// row-count constant), but the pane's real row budget depends on the popup size,
/// so the renderer nudges the window when the cursor would otherwise be outside
/// it. Read-only on purpose: rendering must never move app state.
pub(super) fn window_start(table: &ImportTable, visible: &[usize], rows_h: usize) -> usize {
    if visible.is_empty() {
        return 0;
    }
    let rows_h = rows_h.max(1);
    let last = visible.len() - 1;
    let mut start = (table.scroll as usize).min(last);
    let cursor = table.cursor.min(last);
    if cursor < start {
        start = cursor;
    } else if cursor >= start + rows_h {
        start = cursor + 1 - rows_h;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_follows_the_scroll_anchor_but_never_hides_the_cursor() {
        let visible = [0usize, 1, 2, 3];
        let mut table = ImportTable::new(vec![]);
        table.scroll = 1;
        table.cursor = 1;
        assert_eq!(window_start(&table, &visible, 2), 1, "the anchor is honoured while the cursor is inside");

        table.cursor = 3;
        table.scroll = 0;
        assert_eq!(window_start(&table, &visible, 2), 2, "cursor below the window scrolls it into view");

        table.cursor = 0;
        table.scroll = 3;
        assert_eq!(window_start(&table, &visible, 2), 0, "cursor above the window pulls it back");

        // Absurd scroll offsets cannot panic or point past the last row.
        table.scroll = u16::MAX;
        table.cursor = 0;
        assert_eq!(window_start(&table, &visible, 1), 0);
        assert_eq!(window_start(&table, &[], 3), 0);
    }
}

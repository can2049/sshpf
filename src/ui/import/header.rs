//! The import table's chrome rows: the column headers, the active-filter chips and
//! the select/filter buttons.
//!
//! Three label states, in priority order: `监听:8080` while the column is being
//! typed into (the literal filter text, with the text cursor parked at its end),
//! `监听▾ Listen` when it is filtered (the marker stays glued to the Chinese label
//! — the visual pairing of marker and column depends on it), and the plain
//! bilingual header otherwise. Each state falls back to Chinese only before it
//! truncates, so a header never reads as a half English word.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::model::{ImportColumn, ImportTable};
use crate::text::truncate_to_width;
use crate::text::{bi, display_width};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::import::cursor::LineCursor;
use crate::ui::import::row::{CHECK_W, ColSpec, bilingual_width, cell_span};
use crate::ui::palette::Palette;

/// Header row. A filtered column shows `监听▾`; the column being typed into shows
/// `监听:8080` and takes the text cursor, so it is obvious where input goes.
///
/// Each label comes from [`header_text`], which is bilingual while the column is
/// wide enough and Chinese-only below that. The hit rect always spans the whole
/// column (`spec.width`), so a truncated label is still fully clickable.
pub(super) fn draw_header(
    frame: &mut Frame,
    table: &ImportTable,
    cols: &[ColSpec],
    inner: Rect,
    p: Palette,
    hits: &mut HitMap,
) {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols.len() + 2);
    spans.push(Span::raw(" ".repeat(CHECK_W as usize + 1)));
    let mut x = inner.x + CHECK_W + 1;
    for (i, spec) in cols.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
            x += 1;
        }
        let editing = table.editing == Some(spec.col);
        let filtered = !table.filter(spec.col).trim().is_empty();
        let text = header_text(spec.col, table, spec.width);
        let style = if editing {
            p.focused().add_modifier(Modifier::BOLD)
        } else if filtered {
            p.fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            p.fg(p.dim).add_modifier(Modifier::BOLD)
        };
        spans.push(cell_span(&text, spec.width, style));
        hits.push(Rect { x, y: inner.y, width: spec.width, height: 1 }, HitTarget::ImportColumnHeader(spec.col));
        if editing {
            // `text` is `label:value` with a user-typed value: unbounded, so the
            // cursor offset is clamped in `usize` instead of wrapping a u16.
            let want = usize::from(x) + display_width(&text);
            let cap = usize::from(x) + usize::from(spec.width);
            let cx = want.min(cap);
            if cx < usize::from(inner.right())
                && let Ok(cx) = u16::try_from(cx)
            {
                frame.set_cursor_position((cx, inner.y));
            }
        }
        x += spec.width;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), Rect { height: 1, ..inner });
}

/// The label drawn in one column's header.
///
/// Three states, in priority order:
///
/// * being typed into → `监听:8080`, the literal filter text, so the user can see
///   what they are entering and where the cursor is;
/// * filtered → `监听▾ Listen`: the marker stays glued to the Chinese label (the
///   existing `类型▾` assertion and, more importantly, the visual pairing of
///   marker and column both depend on it), with the English half after it when
///   the column is wide enough;
/// * idle → `监听 Listen`, bilingual while the column is wide enough.
///
/// Every state falls back to Chinese only before it truncates, so a header never
/// reads as a half English word.
pub(super) fn header_text(col: ImportColumn, table: &ImportTable, width: u16) -> String {
    let cells = width as usize;
    if table.editing == Some(col) {
        return truncate_to_width(&format!("{}:{}", col.label(), table.filter(col)), cells);
    }
    let zh = col.label();
    let en = col.label_en();
    if !table.filter(col).trim().is_empty() {
        let with_marker = format!("{zh}▾");
        let both = format!("{with_marker} {en}");
        return if display_width(&both) <= cells { both } else { with_marker };
    }
    if cells >= bilingual_width(col) as usize { bi(zh, en) } else { zh.to_string() }
}

/// Active filters as deletable chips: `筛选: 类型=R ×  监听=8080 × / Filter`.
pub(super) fn draw_chips(frame: &mut Frame, table: &ImportTable, inner: Rect, y: u16, p: Palette, hits: &mut HitMap) {
    let mut line = LineCursor::new(inner.x, y, inner.right(), hits);
    line.text("筛选: ", p.fg(p.dim));
    for col in ImportColumn::ALL {
        let value = table.filter(col).trim();
        if value.is_empty() {
            continue;
        }
        // Chinese label only: a chip is a narrow, clickable token and the `×` is
        // its affordance; the bilingual form would push the second chip off the
        // row long before a user needed it.
        let chip = format!("{}={value} ×", col.label());
        if !line.button(&chip, p.fg(p.accent).add_modifier(Modifier::BOLD), HitTarget::ImportFilterChip(col)) {
            break;
        }
        line.gap(2);
    }
    // English half of the "筛选:" prefix, appended only when it still fits: the
    // chips themselves are the important part of this row.
    line.text(" / Filter", p.fg(p.dim));
    frame.render_widget(Paragraph::new(line.finish()), Rect { x: inner.x, y, width: inner.width, height: 1 });
}

/// `全选 / 反选 / 全不选` and the filter actions. Disabled ones are dim and are
/// *not* clickable, so a click never silently does nothing.
///
/// The five labels do not fit one row at the pane's usual width in bilingual
/// form, and a button that is pushed off the row is a button the mouse cannot
/// reach. So each label asks the cursor whether the bilingual form fits in what
/// is left and drops to Chinese-only when it does not; the clickable rect is
/// always the *rendered* width, so hit testing stays exact.
pub(super) fn draw_buttons(frame: &mut Frame, table: &ImportTable, inner: Rect, y: u16, p: Palette, hits: &mut HitMap) {
    let mut line = LineCursor::new(inner.x, y, inner.right(), hits);
    for (zh, en, target) in [
        ("全选", "All", HitTarget::ImportCheckAll),
        ("反选", "Invert", HitTarget::ImportInvert),
        ("全不选", "None", HitTarget::ImportUncheckAll),
    ] {
        let label = line.fit_label(zh, en);
        if !line.button(&label, p.fg(p.accent), target) {
            break;
        }
        line.gap(1);
    }

    let has_filters = table.has_filters();
    let clear = if has_filters { p.fg(p.accent) } else { p.fg(p.dim) };
    let undo = if table.can_undo_filter() { p.fg(p.accent) } else { p.fg(p.dim) };
    line.gap(2);
    let clear_label = line.fit_label("清除全部筛选", "Clear all");
    if has_filters {
        line.button(&clear_label, clear, HitTarget::ImportClearFilters);
    } else {
        line.text(&clear_label, clear);
    }
    line.gap(1);
    let undo_label = line.fit_label("撤销筛选", "Undo");
    if table.can_undo_filter() {
        line.button(&undo_label, undo, HitTarget::ImportUndoFilter);
    } else {
        line.text(&undo_label, undo);
    }
    frame.render_widget(Paragraph::new(line.finish()), Rect { x: inner.x, y, width: inner.width, height: 1 });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_bilingual_and_marks_a_filtered_column() {
        let mut table = ImportTable::new(vec![]);
        // Wide enough for both languages.
        assert_eq!(header_text(ImportColumn::Listen, &table, bilingual_width(ImportColumn::Listen)), "监听 Listen");
        assert_eq!(header_text(ImportColumn::Kind, &table, bilingual_width(ImportColumn::Kind)), "类型 Kind");
        // Too narrow for both: Chinese only, never a dangling English word.
        assert_eq!(header_text(ImportColumn::Listen, &table, 6), "监听");
        // A filter keeps its `▾` marker whether or not both languages fit.
        table.set_filter(ImportColumn::Kind, "R");
        assert_eq!(header_text(ImportColumn::Kind, &table, 6), "类型▾");
        // The marker stays glued to the Chinese label, with the English after it.
        assert_eq!(header_text(ImportColumn::Kind, &table, 20), "类型▾ Kind");
        // While typing, the header echoes the literal filter text.
        table.begin_filter(ImportColumn::Listen);
        table.filter_char(ImportColumn::Listen, '8');
        assert!(header_text(ImportColumn::Listen, &table, 12).starts_with("监听:8"));
    }
}

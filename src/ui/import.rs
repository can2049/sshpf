//! The "import the forwards `~/.ssh/config` already declares" pane.
//!
//! Rendered *inside* the create form (see [`crate::ui::draw_form`]): the form
//! stays the single place where a new tunnel is defined, and this pane is the
//! shortcut for a host that already describes what should be forwarded, so the
//! user does not retype ports that sit in the config file next to it.
//!
//! Pure rendering like the rest of `ui`: it reads `form.import`, draws, and
//! records what it drew into the mouse [`HitMap`]. Nothing here mutates state —
//! the pane's scroll/window correction is computed locally for the same reason.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::App;
use crate::model::{ImportColumn, ImportRow, ImportTable};
use crate::text::{display_width, pad_to_width, truncate_to_width};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::{Palette, palette};

/// Preferred height of the pane. `ui::draw_form` adds this to the popup it would
/// draw anyway, so the form grows instead of squeezing the fields.
pub const PREF_HEIGHT: u16 = 14;

/// Below this the pane is not worth drawing: two borders + a header + one row +
/// a footer only just fit. Shorter than this and the form keeps the space for
/// its own fields (a pane with no row in it is pure noise).
pub const MIN_HEIGHT: u16 = 5;

/// The checkbox column: ` [x]` / ` [ ]` / ` [–]`, with `▸` marking the cursor
/// row. The cursor keeps a *symbol* as well as the highlight bar so a NO_COLOR
/// run still shows where the selection is.
const CHECK_W: u16 = 4;

/// Column minimums, used both to pick which columns survive a narrow pane and as
/// the base widths the elastic space is added to. Each one is at least as wide as
/// its own header label, so a header is never truncated into `类…`.
const KIND_W: u16 = 5; // 类型 + room for the ▾ filtered marker
const MIN_LISTEN_W: u16 = 9;
const MIN_TARGET_W: u16 = 9;
const MIN_RAW_W: u16 = 9; // 原始指令 + room for the ▾ filtered marker

/// Footer key hints. Mirrors `keys.rs` exactly; see the module docs for the two
/// rules that matter: `a` is a toggle (not "全不选"), and there is no `c`/`A`.
const HINTS: &str = "space 勾选 · f/←→ 选列 · F 清该列 · ctrl+r 清全部 · z 撤销 · I 反选 · U 全不选 · esc 收起";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Draw the pane into `area` (the region the form reserved below its fields).
pub fn draw_pane(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(form) = app.form.as_ref() else { return };
    if area.width < 8 || area.height < 3 {
        return;
    }
    let p = palette();
    let title = format!(" 从 ~/.ssh/config 导入转发 · {} ", form.ssh_host.value());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(p.fg(p.accent))
        .title(Line::from(Span::styled(title, p.fg(p.accent).add_modifier(Modifier::BOLD))));
    let inner = block.inner(area);
    // `Clear` because the pane sits on top of the form's own background.
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if form.import.is_empty() {
        draw_empty(frame, app, inner, p);
        return;
    }
    draw_body(frame, &form.import, inner, p, hits);
}

/// No rows at all: say *why* ("~/.ssh/config has nothing for this host" vs. the
/// discovery error), because an empty table with no explanation looks broken.
fn draw_empty(frame: &mut Frame, app: &App, inner: Rect, p: Palette) {
    let (text, style) = match app.import_error.as_deref() {
        Some(err) => (format!("✗ {err}"), p.fg(p.failed)),
        None => ("~/.ssh/config 中该主机没有端口转发配置".to_string(), p.fg(p.dim)),
    };
    let lines = vec![
        Line::from(Span::styled(truncate_to_width(&text, inner.width as usize), style)),
        Line::from(Span::styled(
            truncate_to_width("esc 收起 · 也可以直接在表单里手填端口", inner.width as usize),
            p.fg(p.dim),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Body: header / rows / filters / buttons / footer
// ---------------------------------------------------------------------------

fn draw_body(frame: &mut Frame, table: &ImportTable, inner: Rect, p: Palette, hits: &mut HitMap) {
    if inner.height < 3 {
        // Not even a header + one row: show only the state summary rather than a
        // half-painted table.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_to_width(&table.summary(), inner.width as usize),
                p.fg(p.accent),
            ))),
            inner,
        );
        return;
    }

    // Reserve from the bottom up: footer always, then the button row, then the
    // chip line — each only while at least one row would still be visible.
    let mut show_buttons = false;
    let mut show_chips = false;
    if inner.height >= 4 {
        show_buttons = true;
    }
    if inner.height >= 5 && table.has_filters() {
        show_chips = true;
    }

    let footer_y = inner.bottom() - 1;
    let buttons_y = footer_y.checked_sub(1);
    let chips_y = buttons_y.and_then(|y| y.checked_sub(1));
    let list_top = inner.y + 1; // below the header
    let list_bottom = if show_chips {
        chips_y.unwrap_or(footer_y)
    } else if show_buttons {
        buttons_y.unwrap_or(footer_y)
    } else {
        footer_y
    };

    let cols = columns(inner.width);
    draw_header(frame, table, &cols, inner, p, hits);

    let list_area = Rect { x: inner.x, y: list_top, width: inner.width, height: list_bottom.saturating_sub(list_top) };
    draw_rows(frame, table, &cols, list_area, inner, p, hits);

    if show_chips && let Some(y) = chips_y {
        draw_chips(frame, table, inner, y, p, hits);
    }
    if show_buttons && let Some(y) = buttons_y {
        draw_buttons(frame, table, inner, y, p, hits);
    }
    draw_footer(frame, table, inner, footer_y, p);
}

/// One row per line: `▸[x] | T | 监听 | 目标 | 原始指令`.
fn draw_rows(
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
        let msg = "没有匹配的行 · 按 F 清除筛选";
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate_to_width(msg, inner.width as usize), p.fg(p.dim)))),
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
fn window_start(table: &ImportTable, visible: &[usize], rows_h: usize) -> usize {
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

/// Header row. A filtered column shows `监听▾`; the column being typed into shows
/// `监听:8080` and takes the text cursor, so it is obvious where input goes.
fn draw_header(frame: &mut Frame, table: &ImportTable, cols: &[ColSpec], inner: Rect, p: Palette, hits: &mut HitMap) {
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
        let text = if editing {
            format!("{}:{}", spec.col.label(), table.filter(spec.col))
        } else if filtered {
            format!("{}▾", spec.col.label())
        } else {
            spec.col.label().to_string()
        };
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
            let cx = (x + display_width(&text) as u16).min(x + spec.width);
            if cx < inner.right() {
                frame.set_cursor_position((cx, inner.y));
            }
        }
        x += spec.width;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), Rect { height: 1, ..inner });
}

/// Active filters as deletable chips: `筛选: 类型=R ×  监听=8080 ×`.
fn draw_chips(frame: &mut Frame, table: &ImportTable, inner: Rect, y: u16, p: Palette, hits: &mut HitMap) {
    let mut line = LineCursor::new(inner.x, y, inner.right(), hits);
    line.text("筛选: ", p.fg(p.dim));
    for col in ImportColumn::ALL {
        let value = table.filter(col).trim();
        if value.is_empty() {
            continue;
        }
        let chip = format!("{}={value} ×", col.label());
        if !line.button(&chip, p.fg(p.accent).add_modifier(Modifier::BOLD), HitTarget::ImportFilterChip(col)) {
            break;
        }
        line.gap(2);
    }
    frame.render_widget(Paragraph::new(line.finish()), Rect { x: inner.x, y, width: inner.width, height: 1 });
}

/// `全选 / 反选 / 全不选` and the filter actions. Disabled ones are dim and are
/// *not* clickable, so a click never silently does nothing.
fn draw_buttons(frame: &mut Frame, table: &ImportTable, inner: Rect, y: u16, p: Palette, hits: &mut HitMap) {
    let mut line = LineCursor::new(inner.x, y, inner.right(), hits);
    for (label, target) in [
        ("全选", HitTarget::ImportCheckAll),
        ("反选", HitTarget::ImportInvert),
        ("全不选", HitTarget::ImportUncheckAll),
    ] {
        if !line.button(label, p.fg(p.accent), target) {
            break;
        }
        line.gap(2);
    }

    let has_filters = table.has_filters();
    let clear = if has_filters { p.fg(p.accent) } else { p.fg(p.dim) };
    let undo = if table.can_undo_filter() { p.fg(p.accent) } else { p.fg(p.dim) };
    line.gap(3);
    if has_filters {
        line.button("清除全部筛选", clear, HitTarget::ImportClearFilters);
    } else {
        line.text("清除全部筛选", clear);
    }
    line.gap(2);
    if table.can_undo_filter() {
        line.button("撤销筛选", undo, HitTarget::ImportUndoFilter);
    } else {
        line.text("撤销筛选", undo);
    }
    frame.render_widget(Paragraph::new(line.finish()), Rect { x: inner.x, y, width: inner.width, height: 1 });
}

/// One line: the state summary (`2/3 条勾选 · 筛选: 类型=R`) plus key hints.
fn draw_footer(frame: &mut Frame, table: &ImportTable, inner: Rect, y: u16, p: Palette) {
    let width = inner.width as usize;
    let summary = truncate_to_width(&table.summary(), width);
    let mut spans = vec![Span::styled(summary.clone(), p.fg(p.accent).add_modifier(Modifier::BOLD))];
    let used = display_width(&summary) + 2;
    if used < width {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(truncate_to_width(HINTS, width - used), p.fg(p.dim)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), Rect { x: inner.x, y, width: inner.width, height: 1 });
}

// ---------------------------------------------------------------------------
// Row / column geometry
// ---------------------------------------------------------------------------

/// One data column: which model column it is, and how many cells it owns.
#[derive(Debug, Clone, Copy)]
struct ColSpec {
    col: ImportColumn,
    width: u16,
}

/// The columns that fit `width`, left to right.
///
/// Degradation order is 「原始指令」→「目标」: the raw directive is the least
/// needed once the row is recognised, and the listen/target pair is what the
/// user actually filters on.
fn columns(width: u16) -> Vec<ColSpec> {
    let mut out: Vec<ColSpec> = Vec::with_capacity(4);
    // checkbox cell + one separating space + the kind column
    let mut used = CHECK_W + 1 + KIND_W;
    out.push(ColSpec { col: ImportColumn::Kind, width: KIND_W });
    for (col, min) in
        [(ImportColumn::Listen, MIN_LISTEN_W), (ImportColumn::Target, MIN_TARGET_W), (ImportColumn::Raw, MIN_RAW_W)]
    {
        if width >= used + 1 + min {
            out.push(ColSpec { col, width: min });
            used += 1 + min;
        }
    }

    // A pane narrower than the fixed part still must not overflow: shrink the
    // type column (the only survivor down here) rather than drawing past the edge.
    if used > width {
        let fixed = CHECK_W + 1;
        let room = width.saturating_sub(fixed);
        if let Some(first) = out.first_mut() {
            first.width = room;
        }
        out.truncate(1);
        if room == 0 {
            out.clear();
        }
        return out;
    }

    // Every spare cell goes to the elastic columns, weighted towards 监听: it is
    // the column a user narrows by when the list is long.
    let spare = width.saturating_sub(used);
    let flexible: Vec<usize> =
        out.iter().enumerate().filter(|(_, spec)| spec.col != ImportColumn::Kind).map(|(i, _)| i).collect();
    if !flexible.is_empty() {
        let weights = [4u16, 3, 3];
        let mut left = spare;
        for (k, &i) in flexible.iter().enumerate() {
            let share = if k + 1 == flexible.len() { left } else { (spare * weights[k.min(2)] / 10).min(left) };
            out[i].width += share;
            left -= share;
        }
    }
    out
}

/// One row, padded to the full width so the cursor bar spans the whole line.
fn row_line(row: &ImportRow, is_cursor: bool, cols: &[ColSpec], width: u16, p: Palette) -> Line<'static> {
    let importable = row.importable();
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols.len() + 2);

    let lead = if is_cursor { "▸" } else { " " };
    let check = if !importable {
        "[–]"
    } else if row.checked {
        "[x]"
    } else {
        "[ ]"
    };
    let check_style = if !importable || !row.checked { p.fg(p.dim) } else { p.fg(p.up) };
    spans.push(Span::styled(format!("{lead}{check}"), check_style));
    spans.push(Span::raw(" "));

    for (i, spec) in cols.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let last = i + 1 == cols.len();
        // For a row we cannot import, the reason is the only thing the user needs
        // from it, so it takes *all* the width the remaining columns would have
        // used instead of being squeezed into the last one. It is the label on
        // the row, not a column value, hence the indent kept below the headers.
        if !importable && last {
            let reason = format!("⚠ {}", row.forward.unsupported_reason().unwrap_or("无法导入"));
            let rest = cols[i..].iter().map(|c| c.width).sum::<u16>() + (cols.len() - i - 1) as u16;
            spans.push(cell_span(&reason, rest.max(spec.width), p.fg(p.failed)));
            continue;
        }
        let style = if !importable {
            p.fg(p.dim)
        } else if spec.col == ImportColumn::Kind {
            p.fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            p.fg(p.text)
        };
        spans.push(cell_span(&spec.col.cell(&row.forward), spec.width, style));
    }

    // Fill the rest of the line: the cursor highlight is a bar, and a bar that
    // stops mid-row reads as a text selection instead of a focused row. The
    // reason span above can already have absorbed the spare width, so this only
    // ever pads — it never truncates.
    let mut used = CHECK_W + 1;
    for (i, spec) in cols.iter().enumerate() {
        used += spec.width + u16::from(i > 0);
    }
    if used < width {
        spans.push(Span::raw(" ".repeat((width - used) as usize)));
    }

    let line = Line::from(spans);
    if is_cursor { line.style(p.focused()) } else { line }
}

/// `text` padded or truncated to exactly `width` cells.
fn cell_span(text: &str, width: u16, style: Style) -> Span<'static> {
    let cells = width as usize;
    Span::styled(pad_to_width(&truncate_to_width(text, cells), cells), style)
}

// ---------------------------------------------------------------------------
// Line building with hit rects
// ---------------------------------------------------------------------------

/// Appends clickable labels to one line while keeping the drawn x position in
/// sync with the recorded hit rects — the one place in this pane where the two
/// could drift apart, so they are computed together.
struct LineCursor<'a> {
    spans: Vec<Span<'static>>,
    hits: &'a mut HitMap,
    x: u16,
    y: u16,
    right: u16,
}

impl<'a> LineCursor<'a> {
    fn new(x: u16, y: u16, right: u16, hits: &'a mut HitMap) -> Self {
        LineCursor { spans: Vec::new(), hits, x, y, right }
    }

    /// Plain text, no hit rect.
    fn text(&mut self, text: &str, style: Style) {
        let w = display_width(text) as u16;
        if self.x + w > self.right {
            return;
        }
        self.spans.push(Span::styled(text.to_string(), style));
        self.x += w;
    }

    /// A clickable label. Returns false when it does not fit, so callers stop
    /// instead of drawing a half-clipped button.
    fn button(&mut self, label: &str, style: Style, target: HitTarget) -> bool {
        let w = display_width(label) as u16;
        if self.x + w > self.right {
            return false;
        }
        self.hits.push(Rect { x: self.x, y: self.y, width: w, height: 1 }, target);
        self.spans.push(Span::styled(label.to_string(), style));
        self.x += w;
        true
    }

    /// Blank separator cells (never clickable).
    fn gap(&mut self, width: u16) {
        let room = self.right.saturating_sub(self.x).min(width);
        if room == 0 {
            return;
        }
        self.spans.push(Span::raw(" ".repeat(room as usize)));
        self.x += room;
    }

    fn finish(self) -> Line<'static> {
        Line::from(self.spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ForwardKind, SshForward};
    use crate::ui::palette_with;

    fn forward(kind: ForwardKind, listen_port: u16, raw: &str) -> SshForward {
        SshForward {
            kind,
            listen_addr: String::new(),
            listen_port,
            target_host: "127.0.0.1".to_string(),
            target_port: 3000,
            host_alias: "dev-box".to_string(),
            line: 7,
            raw: raw.to_string(),
        }
    }

    #[test]
    fn columns_fit_the_width_and_degrade_from_the_right() {
        // Wide: all four data columns, and the footprint equals the budget.
        let wide = columns(90);
        assert_eq!(wide.len(), 4);
        let footprint: u16 =
            CHECK_W + 1 + wide.iter().map(|c| c.width).sum::<u16>() + wide.len().saturating_sub(1) as u16;
        assert_eq!(footprint, 90, "spare width must be fully distributed");

        // Narrow: 原始指令 goes first, then 目标. 类型 always survives.
        for w in 8u16..=120 {
            let cols = columns(w);
            assert!(!cols.is_empty(), "width {w} produced no columns");
            let used: u16 =
                CHECK_W + 1 + cols.iter().map(|c| c.width).sum::<u16>() + cols.len().saturating_sub(1) as u16;
            assert!(used <= w, "width {w} overflows: {cols:?}");
        }
        // Even 20 columns still fits a few columns; 12 leaves only the type.
        assert_eq!(columns(10).iter().map(|c| c.col).collect::<Vec<_>>(), vec![ImportColumn::Kind]);
    }

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

    #[test]
    fn unimportable_rows_render_the_reason_not_the_directive() {
        let p = palette_with(true);
        let cols = columns(90);
        let bad =
            ImportRow { forward: forward(ForwardKind::Local, 0, "LocalForward abc 127.0.0.1:3000"), checked: false };
        let line = row_line(&bad, false, &cols, 90, p);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("⚠ 端口无法解析"), "{text}");
        assert!(text.contains("[–]"), "un-importable rows must not look tickable: {text}");
        assert!(!text.contains("[x]"), "{text}");
    }

    #[test]
    fn cursor_rows_are_flagged_with_a_symbol_too() {
        let p = palette_with(true);
        let cols = columns(90);
        let good =
            ImportRow { forward: forward(ForwardKind::Local, 8080, "LocalForward 8080 127.0.0.1:3000"), checked: true };
        let text = |l: Line<'static>| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text(row_line(&good, true, &cols, 90, p)).starts_with("▸[x]"));
        assert!(text(row_line(&good, false, &cols, 90, p)).starts_with(" [x]"));
    }
}

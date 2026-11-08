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
use crate::help;
use crate::model::{ImportColumn, ImportRow, ImportTable};
use crate::text::{display_width, pad_to_width, truncate_to_width};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::{Palette, palette};

/// Preferred height of the pane. `ui::draw_form` adds this to the popup it would
/// draw anyway, so the form grows instead of squeezing the fields.
///
/// Sized for `IMPORT_ROWS_VISIBLE` rows *and* the bilingual footer: at the
/// form's default width the hints need five rows in `中文 / English` form, and a
/// footer that falls back to Chinese-only on a normal terminal is not bilingual.
pub const PREF_HEIGHT: u16 = 19;

/// Below this the pane is not worth drawing: two borders + a header + one row +
/// a (clipped) footer only just fit. Shorter than this and the form keeps the
/// space for its own fields (a pane with no row in it is pure noise).
pub const MIN_HEIGHT: u16 = 8;

/// The checkbox column: ` [x]` / ` [ ]` / ` [–]`, with `▸` marking the cursor
/// row. The cursor keeps a *symbol* as well as the highlight bar so a NO_COLOR
/// run still shows where the selection is.
const CHECK_W: u16 = 4;

/// Rows the footer gets: the state summary plus the bilingual key hints. Six is
/// what the twelve bindings need at the form's default width; when the pane is
/// shorter the hints first drop to Chinese-only (denser, so nothing is lost) and
/// only then clip.
const FOOTER_H: u16 = 6;

/// Separator between two hints on the same footer row.
const HINT_SEP: &str = " · ";

/// Base minimum width of the type column (its Chinese label plus the `▾` marker).
const MIN_KIND_W: u16 = 5;
const MIN_LISTEN_W: u16 = 9;
const MIN_TARGET_W: u16 = 9;
const MIN_RAW_W: u16 = 9;

/// One footer key hint: the binding, its Chinese wording, its English wording.
///
/// Mirrors `keys.rs` exactly; see the module docs for the rules that matter:
/// `a` is a toggle (not "全不选"), there is no `c`/`A`, `space` ticks a row once
/// the pane is open (so the pane closes with `i`/`esc`), and a literal `f`
/// always means "next column" — it can never be typed into a filter.
struct Hint {
    keys: &'static str,
    zh: &'static str,
    en: &'static str,
}

impl Hint {
    /// `space 勾选` — used when the bilingual form does not fit one row.
    fn zh_text(&self) -> String {
        format!("{} {}", self.keys, self.zh)
    }

    /// `space 勾选 / Select`
    fn bi_text(&self) -> String {
        format!("{} {} / {}", self.keys, self.zh, self.en)
    }
}

/// Footer key hints, in reading order: the tick and filter keys first (the
/// pane's main job), then movement, then how to leave.
const HINTS: &[Hint] = &[
    Hint { keys: "space", zh: "勾选", en: "Select" },
    Hint { keys: "f/←→", zh: "选列", en: "Column" },
    Hint { keys: "F", zh: "清该列", en: "Clear" },
    Hint { keys: "ctrl+r", zh: "清全部", en: "All" },
    Hint { keys: "z", zh: "撤销", en: "Undo" },
    Hint { keys: "a/U/I", zh: "全选/全不选/反选", en: "All/None/Invert" },
    Hint { keys: "j/k ↑↓", zh: "移动", en: "Move" },
    Hint { keys: "g/G", zh: "首尾", en: "Top/End" },
    Hint { keys: "pgup/pgdn", zh: "翻页", en: "Page" },
    Hint { keys: "enter", zh: "结束筛选", en: "Done" },
    Hint { keys: "esc/i", zh: "收起", en: "Close" },
    Hint { keys: "ctrl+s", zh: "保存", en: "Save" },
];

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The pane, with its bilingual title (the host name stays last in both).
pub fn draw_pane(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(form) = app.form.as_ref() else { return };
    if area.width < 8 || area.height < 3 {
        return;
    }
    let p = palette();
    let host = form.ssh_host.value();
    let title = help::fit(
        &format!(" 从 ~/.ssh/config 导入转发 · {host} "),
        &format!(" Import forwards from ~/.ssh/config · {host} "),
        area.width.saturating_sub(2) as usize,
    );
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
    let width = inner.width as usize;
    let (zh, en, style) = match app.import_error.as_deref() {
        Some(err) => (format!("✗ {err}"), None, p.fg(p.failed)),
        None => (
            "~/.ssh/config 中该主机没有端口转发配置".to_string(),
            Some("No port forwards for this host in ~/.ssh/config"),
            p.fg(p.dim),
        ),
    };
    let first = match en {
        Some(en) => help::fit(&zh, en, width),
        None => truncate_to_width(&zh, width),
    };
    let lines = vec![
        Line::from(Span::styled(first, style)),
        Line::from(Span::styled(
            help::fit("esc 收起 · 也可以直接在表单里手填端口", "esc Close · Or type the ports in the form", width),
            p.fg(p.dim),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Body: header / rows / filters / buttons / footer
// ---------------------------------------------------------------------------

fn draw_body(frame: &mut Frame, table: &ImportTable, inner: Rect, p: Palette, hits: &mut HitMap) {
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
        let msg =
            help::fit("没有匹配的行 · 按 F 清除筛选", "No matching rows · F clears the filter", inner.width as usize);
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
///
/// Each label comes from [`header_text`], which is bilingual while the column is
/// wide enough and Chinese-only below that. The hit rect always spans the whole
/// column (`spec.width`), so a truncated label is still fully clickable.
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
            let cx = (x + display_width(&text) as u16).min(x + spec.width);
            if cx < inner.right() {
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
fn header_text(col: ImportColumn, table: &ImportTable, width: u16) -> String {
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
    if cells >= bilingual_width(col) as usize { help::bi(zh, en) } else { zh.to_string() }
}

/// Active filters as deletable chips: `筛选: 类型=R ×  监听=8080 × / Filter`.
fn draw_chips(frame: &mut Frame, table: &ImportTable, inner: Rect, y: u16, p: Palette, hits: &mut HitMap) {
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
fn draw_buttons(frame: &mut Frame, table: &ImportTable, inner: Rect, y: u16, p: Palette, hits: &mut HitMap) {
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

/// The footer: the state summary, then the key hints wrapped to as many rows as
/// the pane can spare.
///
/// The hints are deliberately *not* one clipped line any more: the pane has a
/// dozen bindings and hiding half of them behind `…` is how a help line stops
/// being help. `hints` has already been wrapped by [`pack_hints`], so all this
/// has to do is draw them and clip to the room left above the border.
fn draw_footer(frame: &mut Frame, table: &ImportTable, inner: Rect, y: u16, bottom: u16, hints: &[String], p: Palette) {
    let width = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(1 + hints.len());
    lines.push(Line::from(Span::styled(
        truncate_to_width(&footer_summary(table), width),
        p.fg(p.accent).add_modifier(Modifier::BOLD),
    )));
    for hint in hints {
        lines.push(Line::from(Span::styled(truncate_to_width(hint, width), p.fg(p.dim))));
    }
    let height = bottom.saturating_sub(y).min(lines.len() as u16);
    lines.truncate(height as usize);
    frame.render_widget(Paragraph::new(lines), Rect { x: inner.x, y, width: inner.width, height });
}

/// The footer's first line: `2/3 条勾选 / 2/3 checked · 筛选: 类型=R / filter: 类型=R`.
///
/// Counts come from the table's public API rather than reusing
/// [`ImportTable::summary`], so the English half is composed here next to the
/// Chinese one and the two cannot drift apart.
fn footer_summary(table: &ImportTable) -> String {
    let checked = table.checked_count();
    let importable = table.importable_count();
    let mut s = format!("{checked}/{importable} 条勾选 / {checked}/{importable} checked");
    if table.has_filters() {
        let active = ImportColumn::ALL.into_iter().filter(|c| !table.filter(*c).trim().is_empty());
        let zh: Vec<String> = active.clone().map(|c| format!("{}={}", c.label(), table.filter(c))).collect();
        let en: Vec<String> = active.map(|c| format!("{}={}", c.label_en(), table.filter(c))).collect();
        s.push_str(&format!(" · 筛选: {} / filter: {}", zh.join(", "), en.join(", ")));
    }
    s
}

/// Wrap the [`HINTS`] into at most `max_lines` rows of `width` cells.
///
/// Two passes, because the pane must never *lose* a binding:
///
/// 1. bilingual (`space 勾选 / Select`) — the normal case, and the only one on a
///    wide pane;
/// 2. Chinese only (`space 勾选`), which is roughly 40 % narrower. On a pane too
///    narrow for the bilingual form this is what keeps every key on screen
///    instead of dropping the tail behind `…`.
///
/// Only if even the Chinese pass needs more rows than exist does this clip, and
/// then the *whole* footer is at its narrowest, where a truncated last row is
/// the honest limit rather than a silently unreachable key.
fn pack_hints(width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    if let Some(full) = wrap_hints(width, max_lines, false) {
        return full;
    }
    // Chinese-only is much denser; retry before giving up on any binding.
    wrap_hints(width, max_lines, true).unwrap_or_else(|| {
        let lines = wrap_hints(width, usize::MAX, true).unwrap_or_default();
        lines.into_iter().take(max_lines).collect()
    })
}

/// One wrapping pass. `chinese_only` picks the compact form every hint uses.
///
/// Returns `None` when the hints need more than `max_lines` rows, so the caller
/// can retry in a denser form rather than silently truncating the list.
fn wrap_hints(width: usize, max_lines: usize, chinese_only: bool) -> Option<Vec<String>> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for hint in HINTS {
        let mut text = if chinese_only { hint.zh_text() } else { hint.bi_text() };
        if display_width(&text) > width {
            // Not even this form fits one row: the compact Chinese one might, and
            // otherwise the binding is truncated but still named.
            text = hint.zh_text();
            if display_width(&text) > width {
                text = truncate_to_width(&text, width);
            }
        }
        let w = display_width(&text);
        let sep_w = if cur.is_empty() { 0 } else { display_width(HINT_SEP) };
        if !cur.is_empty() && cur_w + sep_w + w > width {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            if lines.len() > max_lines {
                return None;
            }
        }
        if !cur.is_empty() {
            cur.push_str(HINT_SEP);
            cur_w += sep_w;
        }
        cur.push_str(&text);
        cur_w += w;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    (lines.len() <= max_lines).then_some(lines)
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

/// Minimum width a column needs, derived from its own labels with
/// `display_width` rather than hardcoded numbers.
///
/// The floor is the *bilingual* label (`监听 Listen`) plus one cell for the `▾`
/// filtered marker, so a header is never truncated and a filter marker never
/// pushes the English half out. At this width the data still fits; the larger
/// width a column actually gets is the elastic spare, distributed below.
fn min_width(col: ImportColumn) -> u16 {
    let base = match col {
        ImportColumn::Kind => MIN_KIND_W as usize,
        ImportColumn::Listen => MIN_LISTEN_W as usize,
        ImportColumn::Target => MIN_TARGET_W as usize,
        ImportColumn::Raw => MIN_RAW_W as usize,
    };
    (base.max(bilingual_width(col) as usize) + 1) as u16
}

/// The width at which a column's header can show both languages.
///
/// The `▾` filtered marker is *not* counted here: [`header_text`] reserves it
/// separately, so this is exactly the room the two labels need side by side.
fn bilingual_width(col: ImportColumn) -> u16 {
    (display_width(col.label()) + 1 + display_width(col.label_en())) as u16
}

/// The columns that fit `width`, left to right.
///
/// Degradation order is 「原始指令」→「目标」: the raw directive is the least
/// needed once the row is recognised, and the listen/target pair is what the
/// user actually filters on.
fn columns(width: u16) -> Vec<ColSpec> {
    let mut out: Vec<ColSpec> = Vec::with_capacity(4);
    // checkbox cell + one separating space + the kind column
    let kind_w = min_width(ImportColumn::Kind);
    let mut used = CHECK_W + 1 + kind_w;
    out.push(ColSpec { col: ImportColumn::Kind, width: kind_w });
    for col in [ImportColumn::Listen, ImportColumn::Target, ImportColumn::Raw] {
        let min = min_width(col);
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

/// `中文 English` for the reason a row cannot be imported.
///
/// Both halves come from `model.rs` (`unsupported_reason` /
/// `unsupported_reason_en`), so adding a third un-importable case is a
/// compile-visible edit there and the two languages cannot drift apart — no
/// local translation table that could silently fall back to a generic wording.
///
/// The fallback below is for the case the model does *not* describe: it is only
/// reached for a row that is genuinely un-importable (`importable()` false), yet
/// both accessors returned `None`, which would otherwise print a bare `⚠`.
fn unsupported_label(row: &ImportRow) -> String {
    match (row.forward.unsupported_reason(), row.forward.unsupported_reason_en()) {
        (Some(zh), Some(en)) => help::bi(zh, en),
        _ => help::bi("无法导入", "Cannot import"),
    }
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
            let reason = format!("⚠ {}", unsupported_label(row));
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

    /// `中文/English` when it still fits the row, otherwise `中文`.
    ///
    /// Buttons are the case where the bilingual form can genuinely push a
    /// *later* button off the row, and an unreachable button is worse than an
    /// untranslated one, so the fallback is decided per label against the room
    /// actually left rather than against the pane's full width.
    fn fit_label(&self, zh: &str, en: &str) -> String {
        let room = self.right.saturating_sub(self.x) as usize;
        let both = format!("{zh}/{en}");
        if display_width(&both) <= room { both } else { zh.to_string() }
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
    fn column_minimums_are_derived_from_the_header_label() {
        // The floor must hold the Chinese label plus the `▾` marker, so a header
        // is never truncated into `监…`. The English half is offered on top when
        // the elastic space allows it (see `bilingual_width`).
        for col in ImportColumn::ALL {
            let min = min_width(col) as usize;
            assert!(min > display_width(col.label()), "{col:?} too narrow for its Chinese header");
            // The floor reserves the bilingual labels *and* the `▾` marker.
            let needed = usize::from(bilingual_width(col));
            assert!(min > needed, "{col:?}: floor leaves no room for the English half plus the filter marker");
        }
        // Every column that survives at its minimum renders a full Chinese header.
        for w in [40u16, 60, 90, 120] {
            for spec in columns(w) {
                let table = ImportTable::new(vec![]);
                let text = header_text(spec.col, &table, spec.width);
                assert!(display_width(&text) <= spec.width as usize, "{w}/{col:?}: {text:?}", col = spec.col);
                if spec.width >= min_width(spec.col) {
                    assert!(
                        text.starts_with(spec.col.label()),
                        "{w}/{col:?}: header lost its Chinese label: {text:?}",
                        col = spec.col
                    );
                }
            }
        }
    }

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

    #[test]
    fn footer_hints_cover_every_binding_once() {
        // The pane's whole point is that no key is a secret; a dropped hint is a
        // regression, so every binding the module documents is asserted here.
        // The token set is also cross-checked against `HINTS` itself, so adding a
        // binding without covering it here is a test failure too.
        let expected =
            ["space", "f/←→", "F", "ctrl+r", "z", "a/U/I", "j/k ↑↓", "g/G", "pgup/pgdn", "enter", "esc/i", "ctrl+s"];
        assert_eq!(expected.len(), HINTS.len(), "a hint was added to HINTS without a coverage entry");
        let packed = pack_hints(120, 4).join(" ");
        for token in expected {
            assert!(packed.contains(token), "footer hint {token:?} missing from {packed:?}");
        }
        for hint in HINTS {
            assert!(packed.contains(hint.keys), "binding {:?} lost: {packed:?}", hint.keys);
            assert!(packed.contains(hint.zh), "Chinese half of {:?} lost: {packed:?}", hint.keys);
            assert!(packed.contains(hint.en), "English half of {:?} lost: {packed:?}", hint.keys);
        }
    }

    #[test]
    fn every_binding_survives_a_narrow_or_short_pane() {
        // The real failure mode this guards: clipping the footer's tail at a
        // realistic size, which would make the last few keys undiscoverable.
        // Chinese-only wrapping is what keeps all twelve on screen.
        for (width, rows) in [(120usize, 4usize), (100, 4), (80, 4), (60, 5), (60, 4)] {
            let packed = pack_hints(width, rows).join(" ");
            for hint in HINTS {
                assert!(packed.contains(hint.keys), "{width}x{rows} lost {:?}: {packed:?}", hint.keys);
            }
            assert!(pack_hints(width, rows).len() <= rows, "{width}: more rows than allowed");
        }
    }

    #[test]
    fn footer_hints_wrap_within_the_width_and_degrade_to_chinese() {
        for width in [12usize, 24, 40, 60, 120] {
            for line in pack_hints(width, 6) {
                assert!(display_width(&line) <= width, "hint line overflows {width}: {line:?}");
            }
        }
        // Every line is non-empty: an empty row would waste the footer budget.
        assert!(pack_hints(80, 4).iter().all(|l| !l.is_empty()));
        // A narrow pane keeps the key tokens (Chinese only) rather than dropping
        // the binding entirely.
        let narrow = pack_hints(20, 8).join(" ");
        assert!(narrow.contains("space"), "{narrow:?}");
        // Zero width/rows must not panic or produce content.
        assert!(pack_hints(0, 4).is_empty());
        assert!(pack_hints(80, 0).is_empty());
    }

    #[test]
    fn footer_summary_is_bilingual_and_counts_importable_rows() {
        let table = ImportTable::new(vec![
            forward(ForwardKind::Local, 8080, "LocalForward 8080 127.0.0.1:3000"),
            // Un-importable: it must not inflate the denominator.
            forward(ForwardKind::Local, 0, "LocalForward abc 127.0.0.1:5432"),
        ]);
        let s = footer_summary(&table);
        assert!(s.contains("1/1 条勾选"), "{s}");
        assert!(s.contains("1/1 checked"), "{s}");

        let mut filtered = ImportTable::new(vec![forward(ForwardKind::Local, 8080, "LocalForward 8080 x:1")]);
        filtered.set_filter(ImportColumn::Kind, "L");
        let s = footer_summary(&filtered);
        assert!(s.contains("筛选: 类型=L"), "{s}");
        assert!(s.contains("filter: Kind=L"), "{s}");
    }

    #[test]
    fn unimportable_rows_explain_themselves_in_both_languages() {
        // Built through real `ImportRow`s rather than by passing strings, so this
        // exercises the actual path `row_line` takes (and would break the day the
        // model's two accessors disagree about whether a row is importable).
        let row = |forward: SshForward| ImportRow { forward, checked: false };

        // `listen_port == 0`: the port could not be parsed.
        let bad_port = row(forward(ForwardKind::Local, 0, "LocalForward abc 127.0.0.1:3000"));
        assert!(!bad_port.importable(), "the fixture must really be un-importable");
        assert_eq!(unsupported_label(&bad_port), "端口无法解析 Unparseable port");

        // Non-Dynamic with no target: the destination could not be parsed.
        let mut no_target = forward(ForwardKind::Local, 8080, "LocalForward 8080");
        no_target.target_host = String::new();
        no_target.target_port = 0;
        let bad_target = row(no_target);
        assert!(!bad_target.importable(), "the fixture must really be un-importable");
        assert_eq!(unsupported_label(&bad_target), "目标地址无法解析 Unparseable target");

        // A `-D` forward needs no target, so it is importable and must never be
        // labelled with a failure reason.
        let dynamic = row(forward(ForwardKind::Dynamic, 1080, "DynamicForward 1080"));
        assert!(dynamic.importable(), "a DynamicForward without a target is valid");
        assert_eq!(unsupported_label(&dynamic), "无法导入 Cannot import");
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

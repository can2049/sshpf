//! The tunnel table: column geometry, status words and one row per tunnel.
//!
//! Column selection/width arithmetic lives in [`crate::ui::layout`] (pure, tested
//! at every width); this file turns that layout into ratatui cells. The rules it
//! keeps:
//!
//! * every status carries a **glyph and a word** in both languages, so a
//!   `NO_COLOR` run still reads ([`status_parts`]);
//! * a cell is truncated to its column budget, never allowed to overflow
//!   ([`crate::text::truncate_to_width`]);
//! * in selection mode the table grows a mark gutter and each row records two
//!   hit rects (row + list) for the mouse ([`crate::ui::hit`]).

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, HighlightSpacing, Row, Table, TableState};

use crate::app::App;
use crate::model::{ForwardKind, TunnelStatus, TunnelView};
use crate::text::{display_width, truncate_to_width};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::layout::{COL_SPACING, Col, column_layout};
use crate::ui::palette::{Palette, cell, fmt_uptime, palette};
use crate::ui::text::{fit_pair, warn_once};

// ---------------------------------------------------------------------------
// Main table
// ---------------------------------------------------------------------------

/// `(symbol, Chinese word, English word, palette slot)` for a status.
///
/// The symbol *and* both words are returned so a `NO_COLOR` run keeps the whole
/// meaning; colour is looked up through the palette and is never the only carrier.
pub(crate) fn status_parts(status: &TunnelStatus) -> (&'static str, &'static str, &'static str, fn(&Palette) -> Color) {
    match status {
        TunnelStatus::Up { .. } => ("●", "已连通", "Up", |p| p.up),
        TunnelStatus::Starting { .. } => ("◐", "连接中", "Starting", |p| p.pending),
        TunnelStatus::Retrying { .. } => ("◌", "重试", "Retrying", |p| p.pending),
        TunnelStatus::Failed { .. } => ("✗", "失败", "Failed", |p| p.failed),
        TunnelStatus::Stopped => ("○", "未启用", "Off", |p| p.off),
    }
}

/// Text for the 状态 column: symbol + `中文/English` (+ retry counter).
///
/// `fitting` is the column width, so a squeezed 状态 column keeps the glyph and
/// the Chinese rather than overflowing into its neighbour.
pub(crate) fn status_text(status: &TunnelStatus, fitting: usize) -> String {
    let (sym, zh, en, _) = status_parts(status);
    let word = fit_pair(zh, en, fitting.saturating_sub(display_width(sym) + 1));
    match status {
        TunnelStatus::Retrying { attempt, .. } => format!("{sym} {word}{attempt}"),
        _ => format!("{sym} {word}"),
    }
}

/// Text for the 时长 column.
pub(crate) fn uptime_text(status: &TunnelStatus, fitting: usize) -> String {
    match status {
        TunnelStatus::Up { since } | TunnelStatus::Starting { since } => fmt_uptime(since.elapsed().as_secs()),
        TunnelStatus::Retrying { next_at, .. } => {
            let ms = next_at.saturating_duration_since(Instant::now()).as_millis();
            let secs = ms.div_ceil(1000);
            // `重试 5s` on a normal column, `重试/retry 5s` on a very wide one.
            let tail = format!(" {secs}s");
            let word = fit_pair("重试", "retry", fitting.saturating_sub(display_width(&tail)));
            format!("{word}{tail}")
        }
        _ => "—".to_string(),
    }
}

/// Width of the selection-mode mark gutter (`✓` / `·`). One cell: the glyph is
/// single-width, and the table's own `▶ ` highlight symbol sits next to it.
const MARK_W: u16 = 1;

pub(crate) fn draw_table(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let select = app.select_mode.active;
    // The whole list is a scroll target; rows are pushed afterwards so a row
    // under the pointer shadows the background.
    hits.push(area, HitTarget::TunnelList);

    // In selection mode the table grows a narrow mark column. Its width (plus the
    // extra inter-column gap it introduces) is subtracted from the budget *before*
    // the column layout runs, so the other columns shrink by exactly that much
    // instead of the table overflowing the frame.
    let mark_w = if select { MARK_W } else { 0 };
    let layout = column_layout(area.width.saturating_sub(mark_w + u16::from(select) * COL_SPACING));
    let visible = app.visible_indices();
    let rows = table_rows(app, &layout, p, select);

    let mut widths: Vec<Constraint> = Vec::with_capacity(layout.len() + 1);
    if select {
        widths.push(Constraint::Length(mark_w));
    }
    widths.extend(layout.iter().map(|(_, w)| Constraint::Length(*w)));
    let table = Table::new(rows, widths)
        .header(header_row(&layout, p, select))
        .column_spacing(COL_SPACING)
        .highlight_spacing(HighlightSpacing::Always)
        .highlight_symbol(Line::from(Span::styled("▶ ", p.fg(p.accent))))
        .row_highlight_style(p.bg(p.selection_bg).fg(p.selection_fg).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::TOP).border_style(p.fg(p.dim)));

    let mut state = TableState::new();
    if !visible.is_empty() {
        state.select(Some(app.selected.min(visible.len() - 1)));
    }
    frame.render_stateful_widget(table, area, &mut state);
    push_row_hits(app, &visible, area, hits);
}

/// The header row: one `中文/English` cell per column, plus the mark gutter in
/// selection mode.
///
/// `中文/English` where the column is wide enough, Chinese where it is not: a
/// header that overflowed would push the column boundaries off the cells they
/// describe, which is worse than losing the English.
fn header_row(layout: &[(Col, u16)], p: Palette, select: bool) -> Row<'static> {
    let header_style = p.fg(p.dim).add_modifier(Modifier::BOLD);
    let mut headers: Vec<Cell<'static>> = Vec::with_capacity(layout.len() + 1);
    if select {
        // Single-width on purpose: MARK_W is one cell, and a two-cell header
        // glyph would be truncated to nothing by the column constraint.
        headers.push(cell("✓", header_style));
    }
    headers.extend(layout.iter().map(|(col, w)| {
        let (zh, en) = match col {
            Col::Name => ("名称", "Name"),
            Col::Kind => ("T", ""),
            Col::Local => ("本地", "Local"),
            Col::Remote => ("远端", "Remote"),
            Col::LocalRemote => ("本地→远端", "Local→Remote"),
            Col::SshHost => ("SSH 主机", "SSH host"),
            Col::Status => ("状态", "Status"),
            Col::LastError if *w >= 8 => ("最后错误", "Error"),
            Col::LastError => ("错误", "Err"),
            Col::Uptime => ("时长", "Uptime"),
        };
        cell(fit_pair(zh, en, *w as usize), header_style)
    }));
    Row::new(headers)
}

/// One `Row` per visible tunnel, or the single spanning "nothing here" row.
fn table_rows(app: &App, layout: &[(Col, u16)], p: Palette, select: bool) -> Vec<Row<'static>> {
    let visible = app.visible_indices();
    let mut rows: Vec<Row<'static>> = Vec::with_capacity(visible.len().max(1));
    for idx in &visible {
        if let Some(view) = app.tunnels.get(*idx) {
            rows.push(table_row(view, layout, p, select.then_some(app.select_mode.is_marked(view.cfg.id))));
        }
    }
    if rows.is_empty() {
        let text = if app.tunnels.is_empty() {
            "还没有转发条目，按 n 新建 / No tunnels yet, press n"
        } else {
            "没有匹配的条目，按 esc 清除过滤 / No match, esc clears the filter"
        };
        let mark_w = if select { MARK_W } else { 0 };
        let span = layout.len().max(1) as u16 + mark_w;
        rows.push(Row::new(vec![cell(text, p.fg(p.dim)).column_span(span)]));
    }
    rows
}

/// Record one hit rect per *drawn* row.
///
/// Row geometry: one row for the top border, one for the header, then one per
/// visible row. `draw_table` always passes a fresh `TableState` with no offset,
/// so the mapping is exact — and rows scrolled past the bottom are skipped so a
/// click below the last row cannot resolve to a hidden one.
fn push_row_hits(app: &App, visible: &[usize], area: Rect, hits: &mut HitMap) {
    for (i, idx) in visible.iter().enumerate() {
        let Some(view) = app.tunnels.get(*idx) else { continue };
        // `i` counts visible rows and is bounded by the terminal height in
        // practice, but `as u16` would wrap (and then re-enter the area) for a
        // table taller than 65535 rows. Widening first makes the overflow a
        // plain "off screen", which is what `y >= bottom` already handles.
        let Some(y) = u16::try_from(usize::from(area.y) + 2 + i).ok() else {
            static SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            warn_once(
                &SEEN,
                "ui.render_degraded",
                &[("what", &"table_row"), ("reason", &"row_overflow"), ("row", &i), ("top", &area.y)],
            );
            break;
        };
        if y >= area.bottom() {
            break;
        }
        hits.push(Rect { x: area.x, y, width: area.width, height: 1 }, HitTarget::TunnelRow(view.cfg.id));
    }
}

/// One table row. `marked` is `Some(flag)` only in selection mode, where it adds
/// the leading mark gutter — the `▶` cursor marker is drawn by the table itself,
/// so `▶ ✓ name` means "cursor here, and this one is selected too".
pub(crate) fn table_row(view: &TunnelView, layout: &[(Col, u16)], p: Palette, marked: Option<bool>) -> Row<'static> {
    let cfg = &view.cfg;
    let mut cells: Vec<Cell<'static>> = Vec::with_capacity(layout.len() + 1);
    if let Some(marked) = marked {
        // Symbol + colour, never colour alone: `·` is the unmarked state, and it
        // is visible in a NO_COLOR run.
        let (mark, style) = if marked { ("✓", p.fg(p.up).add_modifier(Modifier::BOLD)) } else { ("·", p.fg(p.dim)) };
        cells.push(cell(mark, style));
    }
    for (col, width) in layout {
        let w = *width as usize;
        let c = match col {
            Col::Name => {
                let style = if cfg.enabled { p.fg(p.text) } else { p.fg(p.dim) };
                cell(truncate_to_width(&cfg.name, w), style)
            }
            Col::Kind => cell(cfg.kind.tag().to_string(), p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Col::Local => cell(truncate_to_width(&cfg.local_display(), w), p.fg(p.text)),
            Col::Remote => cell(truncate_to_width(&cfg.remote_display(), w), p.fg(p.text)),
            Col::LocalRemote => {
                let text = match cfg.kind {
                    ForwardKind::Local => format!("{}→{}", cfg.local_port, cfg.remote_port),
                    ForwardKind::Remote => format!("{}→{}", cfg.remote_port, cfg.local_port),
                    ForwardKind::Dynamic => format!("{}→SOCKS5", cfg.local_port),
                };
                cell(truncate_to_width(&text, w), p.fg(p.text))
            }
            Col::SshHost => cell(truncate_to_width(&cfg.ssh_host, w), p.fg(p.text)),
            Col::Status => {
                let (_, _, _, color) = status_parts(&view.status);
                let text = truncate_to_width(&status_text(&view.status, w), w);
                cell(text, p.fg(color(&p)))
            }
            Col::LastError => match view.status.failure() {
                // The classified phrase in both languages; the raw stderr stays
                // in the detail panel, where there is room for it.
                Some(reason) => {
                    cell(truncate_to_width(&fit_pair(&reason.short, reason.kind.short_en(), w), w), p.fg(p.failed))
                }
                None => cell("—", p.fg(p.dim)),
            },
            Col::Uptime => {
                let style = if matches!(view.status, TunnelStatus::Up { .. }) { p.fg(p.up) } else { p.fg(p.dim) };
                cell(truncate_to_width(&uptime_text(&view.status, w), w), style)
            }
        };
        cells.push(c);
    }
    let row = Row::new(cells);
    if marked == Some(true) {
        // A marked row keeps its normal text style on top of the bar: the mark
        // gutter already says "selected", so the row need not shout.
        row.style(p.fg(p.text))
    } else if matches!(view.status, TunnelStatus::Stopped) {
        row.style(p.fg(p.dim))
    } else {
        row
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FailureInfo, FailureKind};
    use std::time::Duration;

    use crate::ui::test_support::*;

    #[test]
    fn renders_tunnels_with_statuses() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db-prod", 5432)]);
        app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() - Duration::from_secs(134) };
        app.tunnels[0].cfg.enabled = true;
        app.tunnels[1].status = TunnelStatus::Failed {
            reason: FailureInfo::with_short(
                FailureKind::PortInUse,
                "本地端口被占用",
                "bind 127.0.0.1:5432: Address already in use",
            ),
        };

        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("api-dev"), "tunnel name missing:\n{text}");
        assert!(text.contains("db-prod"), "tunnel name missing:\n{text}");
        assert!(text.contains("已连通"), "up status missing:\n{text}");
        assert!(text.contains("失败"), "failed status missing:\n{text}");
        assert!(text.contains("本地端口被占用"), "last error missing:\n{text}");
        assert!(text.contains("02:14"), "uptime missing:\n{text}");
        assert!(text.contains("1 已连通/Up"), "counters wrong:\n{text}");
        assert!(text.contains("1 失败/Failed"), "counters wrong:\n{text}");
        // The status column is bilingual too, glyph included.
        assert!(text.contains("● 已连通/Up"), "bilingual status missing:\n{text}");
        // The last-error column pairs the classified phrase with its English
        // where the column is wide enough, and degrades to Chinese when it is
        // not — so this is asserted on a terminal that can actually fit both.
        let wide = render_to_text(&mut app, 200, 30);
        assert!(wide.contains("本地端口被占用/Local port in use"), "bilingual error missing:\n{wide}");
        // The selected row is highlighted with the ▶ marker.
        assert!(text.contains("▶"), "selection marker missing:\n{text}");
    }

    #[test]
    fn long_names_and_errors_are_truncated_not_overflowed() {
        let mut t = sample(1, &"n".repeat(120), 8080);
        t.ssh_host = "h".repeat(100);
        let mut app = app_with_tunnels(vec![t]);
        app.tunnels[0].status = TunnelStatus::Failed {
            reason: crate::model::FailureInfo::with_short(
                crate::model::FailureKind::PortInUse,
                "很长的错误信息".repeat(20),
                "detail",
            ),
        };
        let text = render_to_text(&mut app, 100, 20);
        // Every rendered line must fit the terminal width.
        for line in text.lines() {
            assert!(display_width(line) <= 100, "line overflows: {line:?}");
        }
    }

    #[test]
    fn table_shows_the_selected_row_marker() {
        let mut app = app_with_tunnels(vec![sample(1, "first", 8080), sample(2, "second", 8081)]);
        app.selected = 1;
        let text = render_to_text(&mut app, 120, 24);
        assert!(text.contains("first") && text.contains("second"));
        assert!(text.contains('▶'), "selection marker missing:\n{text}");
    }

    #[test]
    fn empty_table_invites_the_user_to_create() {
        let mut app = app_with_tunnels(vec![]);
        let text = render_to_text(&mut app, 120, 24);
        assert!(text.contains("新建"), "empty state should mention creating:\n{text}");
    }

    #[test]
    fn status_always_carries_symbol_and_word() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("● 已连通"), "status symbol+word missing:\n{text}");
    }
}

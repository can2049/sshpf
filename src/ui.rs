//! Pure rendering layer (ratatui 0.30).
//!
//! Rules of this module:
//!
//! * No I/O, no process spawning, no touching of tunnel state. The only mutation
//!   allowed is clamping the ratatui scroll offsets (`help_scroll`,
//!   `detail_scroll`) so that a shrunk terminal does not leave the view past the
//!   end of the content.
//! * Everything the user needs must survive `NO_COLOR`: every status carries a
//!   symbol *and* a word, never colour alone.
//! * Column widths are computed from `frame.area().width`; all Chinese text is
//!   measured with [`display_width`] (CJK counts as 2 columns), never
//!   `str::len()`.
//!
//! Layout of the main screen (matches `docs/research/prior-art.md` §7):
//!
//! ```text
//! row 0          title bar      sshpf 0.1.0 · ssh-config N hosts · cfg PATH   HH:MM:SS
//! row 1          counters       ● 4 up  ◐ 2 starting  ◌ 1 retrying  ✗ 3 failed  ○ 5 off
//! rows 2..h-4    tunnel table   名称 | T | 本地 | 远端 | SSH 主机 | 状态 | 最后错误 | 时长
//! row h-4        message bar    app.status, or the selected tunnel's full error
//! rows h-3..h    key hints      two lines
//! ```

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Padding, Paragraph, Row, Table, TableState, Wrap,
};

use crate::app::{App, Overlay};
use crate::config;
use crate::model::{
    BatchAction, FormField, ForwardKind, HostPicker, LogLevel, LogLine, TunnelConfig, TunnelId, TunnelStatus,
    TunnelView, now_hms,
};

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Colour configuration for one frame. Derived from [`palette`] and disabled
/// wholesale when `NO_COLOR` is present in the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Tunnel is forwarding (`●`).
    pub up: Color,
    /// `◐ starting` / `◌ retrying`.
    pub pending: Color,
    /// `✗ failed`.
    pub failed: Color,
    /// `○ off`.
    pub off: Color,
    /// Accent for keys, borders and the type column.
    pub accent: Color,
    /// Default body text.
    pub text: Color,
    /// Secondary text (labels, hints, separators).
    pub dim: Color,
    /// Background of the selected table row / current form field.
    pub selection_bg: Color,
    /// Foreground on top of `selection_bg`.
    pub selection_fg: Color,
    /// `true` when every colour is [`Color::Reset`].
    pub no_color: bool,
}

impl Palette {
    /// Style for a plain foreground colour.
    fn fg(self, color: Color) -> Style {
        Style::default().fg(color)
    }

    fn bg(self, color: Color) -> Style {
        Style::default().bg(color)
    }

    /// Style for the currently focused widget.
    fn focused(self) -> Style {
        Style::default().fg(self.selection_fg).bg(self.selection_bg)
    }
}

/// Palette for the current process, honouring `NO_COLOR`.
pub fn palette() -> Palette {
    palette_with(std::env::var_os("NO_COLOR").is_some())
}

/// Palette with an injected `NO_COLOR` flag (keeps the unit test parallel-safe).
///
/// When `no_color` is set every colour becomes [`Color::Reset`], which makes the
/// glyphs (`● ◐ ◌ ✗ ○`) the only carrier of the status — the accessibility
/// requirement from the spec.
pub fn palette_with(no_color: bool) -> Palette {
    if no_color {
        return Palette {
            up: Color::Reset,
            pending: Color::Reset,
            failed: Color::Reset,
            off: Color::Reset,
            accent: Color::Reset,
            text: Color::Reset,
            dim: Color::Reset,
            selection_bg: Color::Reset,
            selection_fg: Color::Reset,
            no_color: true,
        };
    }
    Palette {
        up: Color::Green,
        pending: Color::Yellow,
        failed: Color::Red,
        off: Color::DarkGray,
        accent: Color::Cyan,
        text: Color::Reset,
        dim: Color::DarkGray,
        selection_bg: Color::DarkGray,
        selection_fg: Color::White,
        no_color: false,
    }
}

// ---------------------------------------------------------------------------
// Small pure helpers (unit tested)
// ---------------------------------------------------------------------------

pub mod hit;
pub mod import;
pub mod layout;

use crate::text::{display_width, pad_to_width, truncate_to_width};
use hit::{HitMap, HitTarget};
use layout::{COL_SPACING, DETAIL_LOG_TAIL, FORM_LABEL_W, FORM_VALUE_X};
pub use layout::{Col, column_layout};

/// Human duration used by the 时长 column.
///
/// * `< 1h`   → `mm:ss`  (`00:00`, `01:30`)
/// * `< 24h`  → `1h06m`
/// * else     → `3d11h`
pub fn fmt_uptime(secs: u64) -> String {
    if secs < 60 * 60 {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86400, (secs % 86400) / 3600)
    }
}

/// Format a Unix timestamp as local `YYYY-MM-DD HH:MM:SS`.
fn fmt_unix(ts: i64) -> String {
    match chrono::TimeZone::timestamp_opt(&chrono::Local, ts, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "—".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Render one frame. `main` calls this inside `terminal.draw(|f| ui::draw(f, &mut app))`.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    // --- mutable phase: clamp scroll offsets ------------------------------
    clamp_scrolls(app, area);

    // --- hit map: rebuilt from scratch, every frame -----------------------
    // A stale map would send a click to last frame's geometry, so it is cleared
    // first. It is *taken out* of `App` rather than borrowed: the renderers
    // below only get `&App` (that is what keeps this module read-only), yet they
    // must still record where they drew things. Written back at the end.
    app.hits.clear();
    let mut hits = std::mem::take(&mut app.hits);

    // --- immutable phase: render ------------------------------------------
    let app_ref: &App = app;

    // The action bar only exists while selecting, and it takes its row from the
    // table instead of leaving a blank strip in normal mode.
    let select_h = u16::from(app_ref.select_mode.active);
    let rows = Layout::vertical([
        Constraint::Length(1),        // title
        Constraint::Length(1),        // counters
        Constraint::Min(1),           // table
        Constraint::Length(1),        // message bar
        Constraint::Length(select_h), // selection-mode action bar
        Constraint::Length(2),        // key hints
    ])
    .split(area);

    draw_title(frame, app_ref, band(&rows, 0, "title"));
    draw_counters(frame, app_ref, band(&rows, 1, "counters"));
    draw_table(frame, app_ref, band(&rows, 2, "table"), &mut hits);
    draw_message(frame, app_ref, band(&rows, 3, "message"));
    if select_h > 0 {
        draw_selection_bar(frame, app_ref, band(&rows, 4, "selection_bar"), &mut hits);
    }
    draw_keys(frame, app_ref, band(&rows, 5, "keys"));

    match &app_ref.overlay {
        Overlay::None => {}
        Overlay::Help => draw_help(frame, app_ref, area, &mut hits),
        Overlay::Form => draw_form(frame, app_ref, area, &mut hits),
        Overlay::Confirm { prompt, .. } => draw_confirm(frame, prompt, area, &mut hits),
        Overlay::Detail(id) => draw_detail(frame, app_ref, *id, area, &mut hits),
        Overlay::Filter => draw_filter(frame, app_ref, area, &mut hits),
    }

    app.hits = hits;
}

/// Clamp the ratatui scroll offsets to the current content length so that a
/// resize (or a long list scrolled to the end) never leaves a blank view.
fn clamp_scrolls(app: &mut App, area: Rect) {
    let help_max = help_line_count().saturating_sub(help_inner_height(area) as usize);
    app.help_scroll = app.help_scroll.min(help_max.min(u16::MAX as usize) as u16);

    if let Overlay::Detail(id) = app.overlay {
        let max = match app.tunnel(id) {
            Some(view) => detail_lines(view, id, palette()).len().saturating_sub(detail_inner_height(area) as usize),
            None => 0,
        };
        app.detail_scroll = app.detail_scroll.min(max.min(u16::MAX as usize) as u16);
    }
}

/// Log a degradation once per process.
///
/// The renderer runs at ~30 frames/s, so a condition that survives a resize
/// would otherwise write a log line per frame until the 2 MiB rotation kicks in
/// and pushes the *interesting* history out. One line per process is the useful
/// amount: it says "this happened", and the frame that triggered it is the only
/// one that differed.
///
/// Returns `true` the first time it fires (so callers can log inline in tests).
fn warn_once(seen: &std::sync::atomic::AtomicBool, event: &str, fields: &[(&str, &dyn std::fmt::Display)]) -> bool {
    use std::sync::atomic::Ordering;
    if seen.swap(true, Ordering::Relaxed) {
        return false;
    }
    crate::logging::warn(event, fields);
    true
}

/// One band of a `Layout::split` result, degrading to an empty rect.
///
/// ratatui hands back exactly one rect per constraint, so the index is an
/// invariant of the solver rather than user data. It is still fetched instead of
/// indexed: a violated contract would otherwise take the whole TUI down
/// mid-frame (the terminal is left in raw mode with no UI), whereas a blank band
/// renders, logs, and recovers on the next frame. Every `draw_*` below treats a
/// zero-sized area as "nothing to draw".
fn band(rows: &[Rect], index: usize, what: &'static str) -> Rect {
    match rows.get(index) {
        Some(rect) => *rect,
        None => {
            static SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            warn_once(
                &SEEN,
                "ui.render_degraded",
                &[("what", &what), ("reason", &"layout_band_missing"), ("index", &index), ("bands", &rows.len())],
            );
            Rect::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Chrome: title / counters / message / keys
// ---------------------------------------------------------------------------

fn draw_title(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let right_w = 9u16.min(area.width);
    let chunks = Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).split(area);
    let (left_area, clock_area) = (band(&chunks, 0, "title_left"), band(&chunks, 1, "title_clock"));

    let ssh_hosts = if app.hosts.is_empty() && app.hosts_error.is_none() {
        "ssh-config: -".to_string()
    } else {
        format!("ssh-config: {} hosts", app.hosts.len())
    };
    // Progressive disclosure: the absolute config path is the first thing to go
    // when it would collide with the clock, then the host count.
    let left = if area.width >= 84 {
        format!(
            " sshpf {}  ·  {}  ·  cfg: {} ",
            env!("CARGO_PKG_VERSION"),
            ssh_hosts,
            config::display_path(&app.paths.config),
        )
    } else if area.width >= 46 {
        format!(" sshpf {}  ·  {} ", env!("CARGO_PKG_VERSION"), ssh_hosts)
    } else {
        format!(" sshpf {} ", env!("CARGO_PKG_VERSION"))
    };
    let left = truncate_to_width(&left, left_area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(left, p.fg(p.text).add_modifier(Modifier::BOLD)))),
        left_area,
    );

    let clock = truncate_to_width(&now_hms(), clock_area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(clock, p.fg(p.dim)))), clock_area);
}

fn draw_counters(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let c = app.counts();

    // Full labels on a wide terminal, the compact `●2 ◐0 ◌1 ✗1 ○2` form when
    // narrow — otherwise the counters are clipped mid-word.
    let compact = area.width < 70;
    let mut spans = vec![Span::raw(" ")];
    let items: [(Color, String); 5] = [
        (p.up, format!("● {}{}", c.up, if compact { "" } else { " up" })),
        (p.pending, format!("◐ {}{}", c.starting, if compact { "" } else { " starting" })),
        (p.pending, format!("◌ {}{}", c.retrying, if compact { "" } else { " retrying" })),
        (p.failed, format!("✗ {}{}", c.failed, if compact { "" } else { " failed" })),
        (p.off, format!("○ {}{}", c.off, if compact { "" } else { " off" })),
    ];
    let gap = if compact { "  " } else { "   " };
    for (i, (color, text)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(gap));
        }
        spans.push(Span::styled(text.clone(), p.fg(*color)));
    }

    // The filter is typed text with no length cap, so its display width can
    // exceed `u16::MAX`; the sum is taken wide and clamped, never cast first.
    let right_w = if app.filter.is_empty() {
        0
    } else {
        let wanted = (display_width(&app.filter) as u32).saturating_add(10);
        if wanted > u32::from(u16::MAX) {
            // Reported before the clamp: the chip is then pinned to the frame's
            // half-width, which is the only thing the layout can guarantee.
            static SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            warn_once(
                &SEEN,
                "ui.render_degraded",
                &[("what", &"counters_filter"), ("reason", &"width_overflow"), ("need", &wanted)],
            );
        }
        wanted.min(u32::from(area.width / 2)) as u16
    };
    let chunks = Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).split(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), band(&chunks, 0, "counters"));

    if right_w > 0 {
        let area = band(&chunks, 1, "counters_filter");
        let text = truncate_to_width(&format!("filter: {} ", app.filter), area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, p.fg(p.accent))).alignment(Alignment::Right)),
            area,
        );
    }
}

/// The non-modal one-line message bar: `app.status`, else the selected tunnel's
/// full failure text, else a hint.
fn draw_message(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();

    let line = if let Some(msg) = &app.status {
        let color = match msg.level {
            LogLevel::Error => p.failed,
            LogLevel::Warn => p.pending,
            LogLevel::Info => p.up,
            LogLevel::Debug => p.dim,
        };
        Line::from(Span::styled(
            truncate_to_width(&format!(" {} ", msg.text), area.width as usize),
            p.fg(color).add_modifier(Modifier::BOLD),
        ))
    } else if let Some(reason) = app.selected_tunnel().and_then(|t| t.status.failure()) {
        let text = truncate_to_width(&format!(" ✗ {}   (enter 查看详情)", reason.detail), area.width as usize);
        Line::from(Span::styled(text, p.fg(p.failed)))
    } else {
        Line::from(Span::styled(" 就绪 · n 新建转发 · ? 帮助 · q 退出", p.fg(p.dim)))
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Two lines of key hints, exactly matching the keys `main` dispatches.
///
/// Both lines change in selection mode, because the mode *re-binds* keys rather
/// than adding to them: `space` marks instead of starting, and `r`/`d`/`n`/`e`
/// are not bound at all there. Leaving the normal hints on screen would
/// advertise keys that do something else or nothing — the exact way a user
/// discovers "the app is broken" while it is working as designed.
fn draw_keys(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let (first, second): (Vec<Hint>, Vec<Hint>) = if app.select_mode.active {
        (
            vec![("j/k ↑↓", "移动"), ("space", "勾选"), ("J/K", "扩选"), ("g/G", "首尾")],
            vec![("a", "全选"), ("s/x/R/D", "批量操作"), ("esc", "退出"), ("?", "帮助")],
        )
    } else {
        (
            vec![
                ("j/k ↑↓", "移动"),
                ("space", "启停"),
                ("r", "重启"),
                ("n", "新建"),
                ("e", "编辑"),
                ("d", "删除"),
                ("enter", "详情"),
                ("l", "日志"),
            ],
            vec![
                ("/", "过滤"),
                ("a", "全启"),
                ("x", "全停"),
                ("g/G", "首尾"),
                ("v", "多选"),
                ("?", "帮助"),
                ("q", "退出"),
            ],
        )
    };
    let lines = vec![key_line(&first, p), key_line(&second, p)];
    frame.render_widget(Paragraph::new(lines), area);
}

/// One `(key, description)` pair of the two hint lines.
type Hint = (&'static str, &'static str);

fn key_line(items: &[Hint], p: Palette) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, desc)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled((*key).to_string(), p.fg(p.accent)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled((*desc).to_string(), p.fg(p.dim)));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Main table
// ---------------------------------------------------------------------------

/// `(symbol, word, palette slot)` for a status. Colour is looked up through the
/// palette so a `NO_COLOR` run keeps the symbol + word intact.
fn status_parts(status: &TunnelStatus) -> (&'static str, &'static str, fn(&Palette) -> Color) {
    match status {
        TunnelStatus::Up { .. } => ("●", "已连通", |p| p.up),
        TunnelStatus::Starting { .. } => ("◐", "连接中", |p| p.pending),
        TunnelStatus::Retrying { .. } => ("◌", "重试", |p| p.pending),
        TunnelStatus::Failed { .. } => ("✗", "失败", |p| p.failed),
        TunnelStatus::Stopped => ("○", "未启用", |p| p.off),
    }
}

/// Text for the 状态 column: symbol + word (+ retry counter).
fn status_text(status: &TunnelStatus) -> String {
    let (sym, word, _) = status_parts(status);
    match status {
        TunnelStatus::Retrying { attempt, .. } => format!("{sym} {word}{attempt}"),
        _ => format!("{sym} {word}"),
    }
}

/// Text for the 时长 column.
fn uptime_text(status: &TunnelStatus) -> String {
    match status {
        TunnelStatus::Up { since } | TunnelStatus::Starting { since } => fmt_uptime(since.elapsed().as_secs()),
        TunnelStatus::Retrying { next_at, .. } => {
            let ms = next_at.saturating_duration_since(Instant::now()).as_millis();
            let secs = ms.div_ceil(1000);
            format!("重试 in {secs}s")
        }
        _ => "—".to_string(),
    }
}

/// Width of the selection-mode mark gutter (`✓` / `·`). One cell: the glyph is
/// single-width, and the table's own `▶ ` highlight symbol sits next to it.
const MARK_W: u16 = 1;

fn cell(text: impl Into<String>, style: Style) -> Cell<'static> {
    Cell::new(Text::from(Line::from(text.into()))).style(style)
}

fn draw_table(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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

    let header_style = p.fg(p.dim).add_modifier(Modifier::BOLD);
    let mut headers: Vec<Cell<'static>> = Vec::with_capacity(layout.len() + 1);
    if select {
        // Single-width on purpose: MARK_W is one cell, and a two-cell header
        // glyph would be truncated to nothing by the column constraint.
        headers.push(cell("✓", header_style));
    }
    headers.extend(layout.iter().map(|(col, w)| {
        // Short labels keep the header readable once a column is squeezed.
        let label = match col {
            Col::Name => "名称",
            Col::Kind => "T",
            Col::Local => "本地",
            Col::Remote => "远端",
            Col::LocalRemote => "本地→远端",
            Col::SshHost => "SSH 主机",
            Col::Status => "状态",
            Col::LastError if *w >= 8 => "最后错误",
            Col::LastError => "错误",
            Col::Uptime => "时长",
        };
        cell(label, header_style)
    }));
    let header = Row::new(headers);

    let visible = app.visible_indices();
    let mut rows: Vec<Row<'static>> = Vec::with_capacity(visible.len().max(1));
    for idx in &visible {
        if let Some(view) = app.tunnels.get(*idx) {
            rows.push(table_row(view, &layout, p, select.then_some(app.select_mode.is_marked(view.cfg.id))));
        }
    }

    if rows.is_empty() {
        let text = if app.tunnels.is_empty() {
            "还没有转发条目，按 n 新建"
        } else {
            "没有匹配的条目，按 esc 清除过滤"
        };
        let span = layout.len().max(1) as u16 + mark_w;
        rows.push(Row::new(vec![cell(text, p.fg(p.dim)).column_span(span)]));
    }

    let mut widths: Vec<Constraint> = Vec::with_capacity(layout.len() + 1);
    if select {
        widths.push(Constraint::Length(mark_w));
    }
    widths.extend(layout.iter().map(|(_, w)| Constraint::Length(*w)));
    let table = Table::new(rows, widths)
        .header(header)
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

    // Row geometry: one row for the top border, one for the header, then one per
    // visible row. `draw_table` always passes a fresh `TableState` with no
    // offset, so the mapping is exact — and rows scrolled past the bottom are
    // skipped so a click below the last row cannot resolve to a hidden one.
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
fn table_row(view: &TunnelView, layout: &[(Col, u16)], p: Palette, marked: Option<bool>) -> Row<'static> {
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
                let (_, _, color) = status_parts(&view.status);
                let text = truncate_to_width(&status_text(&view.status), w);
                cell(text, p.fg(color(&p)))
            }
            Col::LastError => match view.last_error() {
                Some(short) => cell(truncate_to_width(&short, w), p.fg(p.failed)),
                None => cell("—", p.fg(p.dim)),
            },
            Col::Uptime => {
                let style = if matches!(view.status, TunnelStatus::Up { .. }) { p.fg(p.up) } else { p.fg(p.dim) };
                cell(truncate_to_width(&uptime_text(&view.status), w), style)
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

// ---------------------------------------------------------------------------
// Selection mode: action bar
// ---------------------------------------------------------------------------

/// The bottom bar of selection mode: how many are marked, and what can be done.
///
/// Every action is a clickable label recorded in the hit map, so the mouse
/// reaches exactly the same handlers as the keys. The count leads the line
/// because "what is my selection right now" is the question the mode exists to
/// answer; it is stated even when zero rather than being omitted.
fn draw_selection_bar(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let n = app.select_mode.len();
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new("").style(p.bg(p.selection_bg)), area);

    let mut x = area.x;
    let right = area.right();
    let mut spans: Vec<Span<'static>> = Vec::new();

    // The count is emphasised because it is the mode's whole state.
    let count = format!(" 已选 {n} 条 ");
    let count_w = display_width(&count) as u16;
    let count_style = if n == 0 { p.fg(p.pending) } else { p.fg(p.up) }.add_modifier(Modifier::BOLD);
    spans.push(Span::styled(count, count_style));
    x = x.saturating_add(count_w);

    // Layout, not content: "已选 0 条" plus the actions still fits narrow
    // terminals because each label is dropped whole once it stops fitting.
    let buttons: [(BatchAction, &str); 4] = [
        (BatchAction::Start, "s 启用"),
        (BatchAction::Stop, "x 停用"),
        (BatchAction::Restart, "R 重启"),
        (BatchAction::Delete, "D 删除"),
    ];
    for (action, label) in buttons {
        let sep = "  ·  ";
        let sep_w = display_width(sep) as u16;
        let label_w = display_width(label) as u16;
        if x + sep_w + label_w > right {
            break;
        }
        spans.push(Span::styled(sep, p.fg(p.dim)));
        spans.push(Span::styled(label, p.fg(p.accent)));
        x += sep_w;
        let rect = Rect { x, y: area.y, width: label_w, height: 1 };
        hits.push(rect, HitTarget::BatchAction(action));
        x += label_w;
    }

    for (label, target) in [("a 全选", HitTarget::BatchSelectAll), ("v/esc 退出", HitTarget::BatchExit)] {
        let sep = "  ·  ";
        let sep_w = display_width(sep) as u16;
        let label_w = display_width(label) as u16;
        if x + sep_w + label_w > right {
            break;
        }
        spans.push(Span::styled(sep, p.fg(p.dim)));
        spans.push(Span::styled(label, p.fg(p.accent)));
        x += sep_w;
        hits.push(Rect { x, y: area.y, width: label_w, height: 1 }, target);
        x += label_w;
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(p.bg(p.selection_bg)), area);
}

// ---------------------------------------------------------------------------
// Overlay scaffolding
// ---------------------------------------------------------------------------

/// Centered popup of an exact size, clamped to the frame.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    // Both axes are derived from the frame itself (`area.x + (area.width - w)/2`
    // fits in a u16 by construction), but the offsets are added saturating so a
    // nested area with a non-zero origin could never turn centring into an
    // overflow panic.
    Rect {
        x: area.x.saturating_add(area.width.saturating_sub(w) / 2),
        y: area.y.saturating_add(area.height.saturating_sub(h) / 2),
        width: w,
        height: h,
    }
}

/// Popup sized as a percentage of the frame.
fn centered_pct(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (area.width as u32 * pct_w as u32 / 100) as u16;
    let h = (area.height as u32 * pct_h as u32 / 100) as u16;
    centered(area, w.max(24).min(area.width), h.max(6).min(area.height))
}

fn popup_block(title: &str, p: Palette) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(p.fg(p.accent))
        .title(Line::from(Span::styled(format!(" {title} "), p.fg(p.accent).add_modifier(Modifier::BOLD))))
        .padding(Padding::new(1, 1, 0, 0))
}

/// Clear the cells under the popup, then draw its frame. Without `Clear` the
/// table would show through the modal.
fn clear_and_block(frame: &mut Frame, area: Rect, block: Block<'static>) {
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
}

// ---------------------------------------------------------------------------
// Overlay: Help
// ---------------------------------------------------------------------------

/// `(key, description)` pairs shown by the help overlay. An empty description
/// renders `key` as a section header; an empty key renders `description` as
/// indented prose.
const HELP_ITEMS: &[(&str, &str)] = &[
    ("j / ↓", "向下移动选择"),
    ("k / ↑", "向上移动选择"),
    ("g / G", "跳到第一行 / 最后一行"),
    ("space", "启动选中条目（停止需确认）"),
    ("r", "重启选中条目（需确认）"),
    ("n", "新建转发（打开表单）"),
    ("e", "编辑选中条目（保存需确认）"),
    ("d", "删除选中条目（需确认）"),
    ("enter", "打开详情面板"),
    ("l", "打开日志（等同详情）"),
    ("/", "按名称 / 主机过滤列表"),
    ("esc", "关闭弹窗 / 清除过滤"),
    ("a", "启用全部条目（需确认）"),
    ("x", "停用全部条目（需确认）"),
    ("?", "打开 / 关闭本帮助"),
    ("q", "退出（需确认）"),
    ("", ""),
    ("表单内：", ""),
    ("tab / ↑↓", "切换字段"),
    ("← / →", "切换转发类型 L / R / D"),
    ("enter", "在 SSH 主机字段展开 ~/.ssh/config 下拉"),
    ("选中主机", "其声明的转发自动列进导入面板，且默认全部勾选"),
    ("space / i", "收起 / 重新展开「从 ~/.ssh/config 导入转发」面板"),
    ("enter", "校验并保存（保存后立即连接）"),
    ("esc", "取消编辑（有未保存内容会确认）"),
    ("", ""),
    ("导入面板内：", ""),
    ("j/k ↑↓", "移动行（g/G 首尾）"),
    ("space", "勾选 / 取消当前行"),
    ("a / U / I", "全选 / 全不选 / 反选（只作用于筛选后的可见行）"),
    ("f / ←→", "切到下一 / 上一列的筛选"),
    ("F", "清除当前列的筛选"),
    ("ctrl+r", "清除全部筛选"),
    ("z", "撤销上一次筛选改动"),
    ("esc", "先退出筛选输入；再按一次收起面板"),
    ("", ""),
    ("多选模式（v）：", ""),
    ("v / esc", "进入 / 退出多选模式"),
    ("space", "勾选 / 取消光标行"),
    ("J / K", "扩选：按行范围累加标记"),
    ("a", "全选（再按一次取消全选，看「已选 N 条」确认）"),
    ("s / x / R / D", "批量 启用 / 停用 / 重启 / 删除（需确认）"),
    ("", ""),
    ("状态含义：", ""),
    ("● 已连通", "转发已就绪，可开始使用"),
    ("◐ 连接中", "ssh 已启动，等待就绪标记"),
    ("◌ 重试N", "网络类失败，退避后自动重连（第 N 次）"),
    ("✗ 失败", "配置类失败，需修改后手动重启"),
    ("○ 未启用", "用户意图为关闭"),
    ("", ""),
    ("提示：", ""),
    ("", "所有破坏性操作（删除 / 停用 / 重启 / 批量启停 / 保存修改 / 放弃编辑 / 退出）"),
    ("", "都会先弹出确认，需按 y 或 enter 才执行，esc 或 n 取消。"),
    ("", "失败详情（原始 stderr）在详情面板完整保留，表格只放归类短句。"),
    ("", "设置 NO_COLOR=1 关闭全部颜色，状态仍由符号 + 文字表达。"),
    ("", "诊断日志（含崩溃记录）在 ~/.local/state/sshpf/logs/sshpf.log，"),
    ("", "SSHPF_LOG_DIR / SSHPF_LOG_LEVEL 可改目录与级别；详见 docs/logging.md。"),
];

fn help_line_count() -> usize {
    HELP_ITEMS.len()
}

fn help_inner_height(area: Rect) -> u16 {
    centered_pct(area, 78, 84).height.saturating_sub(2)
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let p = palette();
    let popup = centered_pct(area, 78, 84);
    // Modals are drawn after the table, so they must record themselves: without
    // this, a click on the dialog would still hit the table row behind it.
    hits.push(popup, HitTarget::Popup);
    let block = popup_block("帮助 · j/k 滚动 · ? 或 esc 关闭", p);
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let width = inner.width as usize;

    let lines: Vec<Line<'static>> = HELP_ITEMS
        .iter()
        .map(|(key, desc)| {
            if desc.is_empty() {
                Line::from(Span::styled(
                    truncate_to_width(&format!("  {key}"), width),
                    p.fg(p.accent).add_modifier(Modifier::BOLD),
                ))
            } else if key.is_empty() {
                Line::from(Span::styled(truncate_to_width(&format!("  {desc}"), width), p.fg(p.dim)))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {}", pad_to_width(key, 10)), p.fg(p.accent)),
                    Span::styled(truncate_to_width(desc, width.saturating_sub(12)), p.fg(p.text)),
                ])
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).scroll((app.help_scroll, 0)), inner);
}

// ---------------------------------------------------------------------------
// Overlay: Form
// ---------------------------------------------------------------------------

/// The text buffer backing a form field, if it is editable.
fn form_field_buf(form: &crate::app::FormState, field: FormField) -> Option<&crate::model::InputBuf> {
    match field {
        FormField::Name => Some(&form.name),
        FormField::SshHost => Some(&form.ssh_host),
        FormField::LocalAddr => Some(&form.local_addr),
        FormField::LocalPort => Some(&form.local_port),
        FormField::RemoteHost => Some(&form.remote_host),
        FormField::RemotePort => Some(&form.remote_port),
        FormField::Kind | FormField::AutoRestart => None,
    }
}

fn draw_form(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(form) = app.form.as_ref() else { return };
    let p = palette();
    let title =
        if form.is_modifying() { format!("编辑转发 · {}", form.name.value()) } else { "新建转发".to_string() };
    // Tall enough to hold the fields *and* the expanded host dropdown.
    // Rows: title border (2) + one line per field + errors/port hint (2)
    //       + blank (1) + key hint (1) + dropdown frame (2) + dropdown rows.
    let fields_h = FormField::ORDER.len() as u16;
    let base_h = fields_h + 4 + 2 + crate::app::HOST_DROPDOWN_ROWS as u16 + 2;
    // The import pane is *added* to the form's height rather than borrowed from
    // it: a pane that squeezed the fields would hide the very values the user is
    // checking against the rows they tick.
    //
    // Visibility goes through `App::import_pane_open` so the renderer and the key
    // handler cannot disagree. Two extra cases are the renderer's to own, because
    // `import_pane_open` is deliberately just "is there a table worth showing":
    //
    // * discovery *failed* → the table is empty, but the reason is exactly what
    //   the user needs, so the pane opens to show it;
    // * `import_open` set on an empty table → the state is reachable (a filter or
    //   a host switch can empty it), and silently collapsing would look like the
    //   key did nothing. `import::draw_empty` has a branch for each.
    // (`import_error` on its own deliberately does *not* force the pane open:
    // `App::toggle_import_pane` reports the failure on the status line and leaves
    // the pane closed, and two places claiming to show the same reason would be
    // worse than one.)
    let pane_open = app.import_pane_open() || app.form.as_ref().is_some_and(|f| f.import_open);
    let wanted_h = base_h + if pane_open { import::PREF_HEIGHT } else { 0 };
    let popup = centered(area, 74.min(area.width), wanted_h.min(area.height));
    let block = popup_block(&title, p);
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let value_w = inner.width.saturating_sub(FORM_VALUE_X as u16) as usize;
    let current_idx = FormField::ORDER.iter().position(|f| *f == form.field).unwrap_or(0);

    let lines = form_field_lines(app, form, p, value_w, current_idx);
    let mut extra = form_notes(form, p, inner.width as usize);

    // Reserve the pane out of the bottom, but only after the fields and their
    // notes have their rows: on a short terminal the pane is what shrinks (and
    // eventually disappears) so the form stays usable.
    let (body, pane_area) = match pane_open {
        true => {
            let needed = lines.len() as u16 + extra.len() as u16;
            let room = inner.height.saturating_sub(needed).min(import::PREF_HEIGHT);
            if room >= import::MIN_HEIGHT {
                let body = Rect { height: inner.height - room, ..inner };
                let pane = Rect { x: inner.x, y: inner.y + body.height, width: inner.width, height: room };
                (body, Some(pane))
            } else {
                // The pane does not fit, but `import_open` is still true and the
                // submit path keys off it: `ctrl+s` would create the ticked rows
                // while the user cannot see a single one of them. Say so instead
                // of silently acting on an invisible table — and point at the key
                // that gets them out of that state.
                if form.import_open && !form.import.is_empty() {
                    let msg = format!(
                        "⚠ 终端太矮，导入面板未显示；ctrl+s 仍会按已勾选的 {} 条创建。i 收起面板可改用手填",
                        form.import.checked_count()
                    );
                    extra
                        .push(Line::from(Span::styled(truncate_to_width(&msg, inner.width as usize), p.fg(p.pending))));
                }
                (inner, None)
            }
        }
        false => (inner, None),
    };

    let used = lines.len() as u16;
    if body.height > used {
        let rest = Rect { x: body.x, y: body.y + used, width: body.width, height: body.height - used };
        frame.render_widget(Paragraph::new(extra), rest);
    }
    frame.render_widget(Paragraph::new(lines), body);

    // Expand the dropdown *inside* the form, anchored under the SSH HOST row.
    // It is confined to the body so it can never cover the import pane.
    if let Some(picker) = app.picker.as_ref().filter(|pk| pk.open) {
        draw_host_dropdown(frame, app, picker, body, current_idx as u16);
    }

    place_form_cursor(frame, form, body, popup, current_idx);

    // Drawn last, and outside `body`: it owns its own cursor (a column filter
    // being typed into), which must win over the form field cursor.
    if let Some(pane) = pane_area {
        import::draw_pane(frame, app, pane, hits);
    }
}

/// One `Line` per form field, styled for the three highlight states.
///
/// Three visual states make the two-stage model obvious:
///   • highlighted + editing → solid selection bar (text entry active)
///   • highlighted only      → bold accent, no bar (arrows just move)
///   • neither               → plain
fn form_field_lines(
    app: &App,
    form: &crate::app::FormState,
    p: Palette,
    value_w: usize,
    current_idx: usize,
) -> Vec<Line<'static>> {
    let kind = form.kind();
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(FormField::ORDER.len());
    for (i, field) in FormField::ORDER.iter().enumerate() {
        let focused = i == current_idx;
        let editing_here = focused && form.is_editing_field();
        let label_style = if editing_here {
            p.focused().add_modifier(Modifier::BOLD)
        } else if focused {
            p.fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            p.fg(p.dim)
        };
        let value_style = if editing_here {
            p.focused()
        } else if focused {
            p.fg(p.accent)
        } else {
            p.fg(p.text)
        };
        let label = Span::styled(pad_to_width(field.label(), FORM_LABEL_W), label_style);

        if *field == FormField::SshHost {
            lines.push(host_field_line(app, form, p, value_w, editing_here, label));
            continue;
        }
        let value = match field {
            FormField::Kind => Span::styled(
                pad_to_width(&format!("< {} >", kind.label()), value_w),
                if editing_here { p.focused() } else { p.fg(p.accent) },
            ),
            FormField::AutoRestart => {
                let mark = if form.auto_restart { "[x]" } else { "[ ]" };
                Span::styled(pad_to_width(&format!("{mark} 网络类失败时自动退避重试"), value_w), value_style)
            }
            FormField::RemoteHost | FormField::RemotePort if kind == ForwardKind::Dynamic => {
                Span::styled(pad_to_width("(SOCKS5 不需要)", value_w), p.fg(p.dim))
            }
            _ => {
                let text =
                    form_field_buf(form, *field).map(|b| truncate_to_width(b.value(), value_w)).unwrap_or_default();
                Span::styled(pad_to_width(&text, value_w), value_style)
            }
        };
        lines.push(Line::from(vec![label, Span::raw(" "), value]));
    }
    lines
}

/// The SSH HOST row: value on the left, a ▾/▴ affordance on the right.
///
/// The `(N台可选)` hint is deliberately suppressed once a host is chosen — it
/// would push the actual alias out of view, which is what the user is checking.
fn host_field_line(
    app: &App,
    form: &crate::app::FormState,
    p: Palette,
    value_w: usize,
    editing_here: bool,
    label: Span<'static>,
) -> Line<'static> {
    let typed = form.ssh_host.value();
    let open = app.picker.as_ref().is_some_and(|pk| pk.open);
    let marker = if open { "▴" } else { "▾" };
    let drop_style = if editing_here { p.focused() } else { p.fg(p.accent) };
    let hint = if !typed.trim().is_empty() {
        String::new()
    } else if !app.hosts.is_empty() {
        format!(" ({}台可选)", app.hosts.len())
    } else if app.hosts_error.is_some() {
        " (读取失败)".to_string()
    } else {
        " (无主机)".to_string()
    };
    let marker_w = display_width(marker);
    let avail = value_w.saturating_sub(marker_w + 1);
    let shown = truncate_to_width(&format!("{typed}{hint}"), avail);
    let value_style = if editing_here { p.focused() } else { p.fg(p.text) };
    Line::from(vec![
        label,
        Span::raw(" "),
        Span::styled(pad_to_width(&shown, avail), value_style),
        Span::styled(marker, drop_style.add_modifier(Modifier::BOLD)),
    ])
}

/// Validation errors, the auto-suggested port note, and the key hint.
fn form_notes(form: &crate::app::FormState, p: Palette, width: usize) -> Vec<Line<'static>> {
    let mut extra = Vec::new();
    for err in form.errors.iter().take(3) {
        extra.push(Line::from(Span::styled(truncate_to_width(&format!("✗ {err}"), width), p.fg(p.failed))));
    }
    if let Some(note) = &form.local_port_note {
        // The auto-derived local port is a *suggestion*; say so plainly.
        extra.push(Line::from(Span::styled(truncate_to_width(&format!("· {note}（可编辑）"), width), p.fg(p.pending))));
    }
    if let Some(hint) = &form.port_hint {
        let style = if hint.contains("占用") { p.fg(p.pending) } else { p.fg(p.up) };
        extra.push(Line::from(Span::styled(truncate_to_width(&format!("· {hint}"), width), style)));
    }
    extra.push(Line::from(""));
    let hint_text = if form.is_editing_field() {
        "编辑中：直接输入  ·  enter/esc 结束编辑  ·  tab 下一字段"
    } else {
        "↑↓ 选择字段  ·  enter 编辑  ·  ctrl+s 保存  ·  esc 取消"
    };
    extra.push(Line::from(Span::styled(truncate_to_width(hint_text, width), p.fg(p.dim))));
    extra
}

/// Show a text cursor only while a field is actively being edited.
fn place_form_cursor(frame: &mut Frame, form: &crate::app::FormState, inner: Rect, popup: Rect, current_idx: usize) {
    if !form.is_editing_field() {
        return;
    }
    let Some(buf) = form_field_buf(form, form.field) else { return };
    let prefix: String = buf.text.chars().take(buf.cursor).collect();
    // A field value is unbounded typed text (or a config file someone edited by
    // hand), so the cell offset is added in `usize`: a `u16` sum wraps around
    // and would park the cursor in the middle of the popup.
    let x = usize::from(inner.x) + FORM_VALUE_X + display_width(&prefix);
    let y = usize::from(inner.y) + current_idx;
    if x < usize::from(popup.right().saturating_sub(1)) && y < usize::from(inner.bottom()) {
        match (u16::try_from(x), u16::try_from(y)) {
            (Ok(x), Ok(y)) => frame.set_cursor_position((x, y)),
            _ => {
                static SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                warn_once(
                    &SEEN,
                    "ui.render_degraded",
                    &[("what", &"form_cursor"), ("reason", &"cursor_overflow"), ("x", &x), ("y", &y)],
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Inline SSH-host dropdown (part of the form)
// ---------------------------------------------------------------------------

/// Draw the expanded host list directly beneath the SSH HOST field.
///
/// This is deliberately an in-form panel rather than a separate centred modal:
/// the whole point of the change is that choosing from `~/.ssh/config` should
/// be the obvious, visible action when adding a forward.
fn draw_host_dropdown(frame: &mut Frame, app: &App, picker: &HostPicker, inner: Rect, field_row: u16) {
    let p = palette();
    let rows = crate::app::HOST_DROPDOWN_ROWS;
    // The panel starts right under the SSH HOST row and may use every row left
    // in the popup. It is *overlaid* on the remaining fields (normal dropdown
    // behaviour), so the height is capped by the space below the field.
    let below = inner.height.saturating_sub(field_row + 1);
    // Frame (2) + filter line (1) + hint line (1) => at least 1 list row.
    let max_height = 2 + 1 + 1 + rows as u16;
    let height = max_height.min(below);
    if height < 5 {
        // Not enough room for a usable list: keep the field marker only.
        return;
    }
    let area = Rect { x: inner.x, y: inner.y + field_row + 1, width: inner.width, height };

    frame.render_widget(Clear, area);
    let block =
        Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(p.fg(p.accent)).title(
            Line::from(Span::styled(" 从 ~/.ssh/config 选择主机 ", p.fg(p.accent).add_modifier(Modifier::BOLD))),
        );
    let body = block.inner(area);
    frame.render_widget(block, area);
    if body.height == 0 {
        return;
    }

    let width = body.width as usize;
    let list_h = body.height.saturating_sub(2) as usize;

    if app.hosts.is_empty() {
        let msg = app.hosts_error.clone().unwrap_or_else(|| "未在 ~/.ssh/config 中找到主机".to_string());
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate_to_width(&msg, width), p.fg(p.failed)))),
            Rect { height: 1, ..body },
        );
        return;
    }

    // Row 0: filter echo.
    let filter = if picker.query.is_empty() {
        Span::styled("输入以过滤…", p.fg(p.dim))
    } else {
        Span::styled(truncate_to_width(&picker.query, width.saturating_sub(8)), p.fg(p.text))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("过滤: ", p.fg(p.dim)), filter])),
        Rect { y: body.y, height: 1, ..body },
    );

    let list_area = Rect { y: body.y + 1, height: list_h as u16, ..body };
    if picker.matches.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("没有匹配的主机", p.fg(p.dim)))),
            Rect { height: 1, ..list_area },
        );
    } else {
        let mut out: Vec<Line<'static>> = Vec::with_capacity(list_h);
        let start = picker.scroll as usize;
        for (offset, pos) in (start..picker.matches.len()).take(list_h).enumerate() {
            let Some(host) = picker.matches.get(pos).and_then(|i| app.hosts.get(*i)) else {
                continue;
            };
            let sel = start + offset == picker.selected;
            let style = if sel { p.focused().add_modifier(Modifier::BOLD) } else { p.fg(p.text) };
            let alias_w = 24usize.min(width.saturating_sub(2));
            let alias = truncate_to_width(&format!("{}{}", if sel { "▸ " } else { "  " }, host.alias), alias_w);
            out.push(Line::from(vec![
                Span::styled(pad_to_width(&alias, alias_w), style),
                Span::styled(
                    truncate_to_width(&host.endpoint(), width.saturating_sub(alias_w)),
                    if sel { style } else { p.fg(p.dim) },
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(out), list_area);
    }

    // Hint on the last row.
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_to_width("↑↓ 选择   enter 确定   esc 收起   tab 切到下一字段", width),
            p.fg(p.dim),
        ))),
        Rect { y: body.bottom().saturating_sub(1), height: 1, ..body },
    );
}

// ---------------------------------------------------------------------------
// Overlay: Host picker
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Overlay: Confirm
// ---------------------------------------------------------------------------

fn draw_confirm(frame: &mut Frame, prompt: &str, area: Rect, hits: &mut HitMap) {
    let p = palette();
    // Width of the prompt in cells, kept wide: a pasted name is unbounded, so
    // `as u16` on the raw width could wrap a huge prompt into a tiny dialog.
    let prompt_w = display_width(prompt) as u32;
    let min = 40u16.min(area.width);
    let max = 90u16.min(area.width);
    let wanted = prompt_w.saturating_add(8).min(u32::from(u16::MAX)) as u16;
    let wanted = wanted.clamp(min, max.max(min));
    // Fit the dialog to its content: prompt may wrap onto two lines, plus a
    // blank line and the key row, plus the two border rows.
    //
    // The wrap budget drops below 1 on a terminal narrower than 5 columns
    // (`wanted` is then `area.width` itself, and 4 cells go to the borders and
    // the `⚠ ` marker) — `div_ceil` by that is a divide-by-zero panic, so it is
    // floored to one cell and the row count is capped instead.
    let wrap_w = wanted.saturating_sub(4).max(1) as u32;
    let prompt_rows = prompt_w.div_ceil(wrap_w).min(u32::from(u16::MAX)) as u16;
    let popup = centered(area, wanted, (prompt_rows.max(1) + 4).min(area.height));
    // The riskiest click-through to allow: the dialog asks before a destructive
    // action, so a click meant for the dialog must never reach a table row.
    hits.push(popup, HitTarget::Popup);
    let block = popup_block("确认", p);
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 {
        return;
    }

    let lines = vec![
        Line::from(vec![
            Span::styled("⚠ ", p.fg(p.failed).add_modifier(Modifier::BOLD)),
            Span::styled(truncate_to_width(prompt, inner.width.saturating_sub(3) as usize), p.fg(p.text)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" y / enter ", p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("确认    ", p.fg(p.text)),
            Span::styled("n / esc ", p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("取消", p.fg(p.text)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

// ---------------------------------------------------------------------------
// Overlay: Detail
// ---------------------------------------------------------------------------

/// The equivalent ssh command line, shown in the detail panel for copy/paste.
fn tunnel_command(cfg: &TunnelConfig) -> String {
    let flag = match cfg.kind {
        ForwardKind::Local => "-L",
        ForwardKind::Remote => "-R",
        ForwardKind::Dynamic => "-D",
    };
    format!("ssh -N {} {} {}", flag, cfg.forward_spec(), cfg.ssh_host)
}

fn level_tag(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "DBG",
        LogLevel::Info => "INF",
        LogLevel::Warn => "WRN",
        LogLevel::Error => "ERR",
    }
}

fn level_color(level: LogLevel, p: Palette) -> Color {
    match level {
        LogLevel::Debug => p.dim,
        LogLevel::Info => p.text,
        LogLevel::Warn => p.pending,
        LogLevel::Error => p.failed,
    }
}

fn detail_inner_height(area: Rect) -> u16 {
    centered_pct(area, 88, 86).height.saturating_sub(2)
}

fn yes_no(v: bool) -> &'static str {
    if v { "是" } else { "否" }
}

/// Build the scrollable body of the detail overlay: fields, the full failure
/// text (never truncated), the equivalent ssh command and the log tail.
fn detail_lines(view: &TunnelView, id: TunnelId, p: Palette) -> Vec<Line<'static>> {
    let cfg = &view.cfg;
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 16 = widest label ("启用 / 自动重连" = 15 cells) rounded up, so every
    // value in the detail panel starts at the same column.
    let kv = |k: &str, v: String| {
        Line::from(vec![Span::styled(pad_to_width(&format!("{k}:"), 17), p.fg(p.dim)), Span::styled(v, p.fg(p.text))])
    };

    lines.push(kv("名称", cfg.name.clone()));
    lines.push(kv("ID", id.to_string()));
    lines.push(kv("类型", cfg.kind.label().to_string()));
    lines.push(kv("SSH 主机", cfg.ssh_host.clone()));
    lines.push(kv("本地", cfg.local_display()));
    lines.push(kv("远端", cfg.remote_display()));
    lines.push(kv("转发参数", cfg.forward_spec()));
    lines.push(kv("启用 / 自动重连", format!("{} / {}", yes_no(cfg.enabled), yes_no(cfg.auto_restart))));
    lines.push(kv("创建时间", fmt_unix(cfg.created_at)));
    lines.push(kv("状态", format!("{}（重试次数 {}）", view.status.key(), view.retries)));
    if !cfg.note.is_empty() {
        lines.push(kv("备注", cfg.note.clone()));
    }
    if !cfg.ssh_options.is_empty() {
        lines.push(kv("ssh 选项", cfg.ssh_options.join(" ")));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("失败原因（原始 stderr，完整保留）", p.fg(p.dim).add_modifier(Modifier::BOLD))));
    match view.status.failure() {
        Some(reason) => {
            lines.push(Line::from(Span::styled(
                format!("  {}", reason.short),
                p.fg(p.failed).add_modifier(Modifier::BOLD),
            )));
            for raw in reason.detail.lines() {
                lines.push(Line::from(Span::styled(format!("  {raw}"), p.fg(p.text))));
            }
        }
        None => lines.push(Line::from(Span::styled("  无", p.fg(p.dim)))),
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("等价 ssh 命令（可复制）", p.fg(p.dim).add_modifier(Modifier::BOLD))));
    lines.push(Line::from(Span::styled(format!("  {}", tunnel_command(cfg)), p.fg(p.accent))));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("日志（最近 {DETAIL_LOG_TAIL} 行）"),
        p.fg(p.dim).add_modifier(Modifier::BOLD),
    )));
    let tail = view.log.tail(DETAIL_LOG_TAIL);
    if tail.is_empty() {
        lines.push(Line::from(Span::styled("  暂无日志", p.fg(p.dim))));
    } else {
        for LogLine { time, level, text } in tail {
            lines.push(Line::from(vec![
                Span::styled(format!("  {time} "), p.fg(p.dim)),
                Span::styled(format!("{} ", level_tag(level)), p.fg(level_color(level, p))),
                Span::styled(text, p.fg(level_color(level, p))),
            ]));
        }
    }
    lines
}

fn draw_detail(frame: &mut Frame, app: &App, id: TunnelId, area: Rect, hits: &mut HitMap) {
    let p = palette();
    let Some(view) = app.tunnel(id) else { return };
    let popup = centered_pct(area, 88, 86);
    hits.push(popup, HitTarget::Popup);
    let title = format!("详情 · {} · j/k 滚动 · esc 返回", view.cfg.name);
    let block = popup_block(&title, p);
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(detail_lines(view, id, p)).scroll((app.detail_scroll, 0)).wrap(Wrap { trim: false }),
        inner,
    );
}

// ---------------------------------------------------------------------------
// Overlay: Filter
// ---------------------------------------------------------------------------

fn draw_filter(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let p = palette();
    // Occupies the very last screen row: a one-line, non-modal input.
    let row = Rect { x: area.x, y: area.bottom().saturating_sub(1), width: area.width, height: 1 };
    if row.height == 0 || row.width == 0 {
        return;
    }
    frame.render_widget(Clear, row);

    let prefix = " 过滤: ";
    let prefix_w = display_width(prefix) as u16;
    let clear = "  × 清除 ";
    let clear_w = display_width(clear) as u16;
    // Keep the clear affordance on screen even when the typed text is long: it
    // sits on the right, the input echoes in whatever is left of it.
    let hint = "   enter 应用   esc 清除";
    let hint_w = display_width(hint) as u16;
    let value_w = row.width.saturating_sub(prefix_w + clear_w + hint_w);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prefix, p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled(truncate_to_width(&app.filter, value_w as usize), p.fg(p.text)),
            Span::styled(clear, p.fg(p.accent)),
            Span::styled(hint, p.fg(p.dim)),
        ]))
        .style(p.bg(p.selection_bg)),
        row,
    );

    // The input line itself: a click parks the cursor there (the overlay is
    // already modal-ish, so it only needs to keep the click from falling
    // through to the table underneath).
    let bar_w = prefix_w + value_w;
    hits.push(Rect { x: row.x, y: row.y, width: bar_w, height: 1 }, HitTarget::FilterBar);
    if row.width >= clear_w + hint_w {
        hits.push(
            Rect { x: row.x + row.width - hint_w - clear_w, y: row.y, width: clear_w, height: 1 },
            HitTarget::FilterClear,
        );
    }

    // The filter has no length cap, so its cell width is not bounded by u16:
    // compute wide and let an off-screen cursor simply be skipped.
    let x = usize::from(row.x) + usize::from(prefix_w) + display_width(&app.filter);
    let bar_right = usize::from(row.x) + usize::from(bar_w);
    if x < bar_right {
        match u16::try_from(x) {
            Ok(x) => frame.set_cursor_position((x, row.y)),
            Err(_) => {
                static SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                warn_once(
                    &SEEN,
                    "ui.render_degraded",
                    &[("what", &"filter_cursor"), ("reason", &"cursor_overflow"), ("x", &x)],
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, ConfirmAction};
    use crate::config::{ConfigFile, Paths};
    use crate::model::{FailureInfo, FailureKind, ImportColumn, ImportTable, InputBuf, SshForward, TunnelStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::{Duration, Instant};

    // -- pure helpers -------------------------------------------------------

    #[test]
    fn uptime_formats_scale() {
        assert_eq!(fmt_uptime(0), "00:00");
        assert_eq!(fmt_uptime(90), "01:30");
        assert_eq!(fmt_uptime(4000), "1h06m");
        assert_eq!(fmt_uptime(300_000), "3d11h");
        // Boundaries.
        assert_eq!(fmt_uptime(59), "00:59");
        assert_eq!(fmt_uptime(3600), "1h00m");
        assert_eq!(fmt_uptime(86_400), "1d00h");
    }

    #[test]
    fn display_width_counts_cjk_as_two() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("a中b"), 4);
        // Status glyphs are single width; the Chinese word is double.
        assert_eq!(display_width("● 已连通"), 1 + 1 + 6);
        assert_eq!(display_width("✗ 失败"), 1 + 1 + 4);
        // Full-width forms are double width.
        assert_eq!(display_width("Ａ"), 2);
    }

    #[test]
    fn pad_and_truncate_respect_display_width() {
        assert_eq!(pad_to_width("中", 4), "中  ");
        assert_eq!(display_width(&pad_to_width("abc", 5)), 5);
        assert_eq!(truncate_to_width("abc", 5), "abc");
        let cut = truncate_to_width("中文字符串", 5);
        assert!(display_width(&cut) <= 5, "{cut:?} too wide");
        assert!(cut.ends_with('…'), "{cut:?}");
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    // -- palette ------------------------------------------------------------

    #[test]
    fn palette_degrades_with_no_color() {
        let plain = palette_with(true);
        assert!(plain.no_color);
        for c in [
            plain.up,
            plain.pending,
            plain.failed,
            plain.off,
            plain.accent,
            plain.text,
            plain.dim,
            plain.selection_bg,
            plain.selection_fg,
        ] {
            assert_eq!(c, Color::Reset, "NO_COLOR palette must not emit colours");
        }

        let colored = palette_with(false);
        assert!(!colored.no_color);
        assert_eq!(colored.up, Color::Green);
        assert_eq!(colored.pending, Color::Yellow);
        assert_eq!(colored.failed, Color::Red);
        assert_eq!(colored.off, Color::DarkGray);
        assert_ne!(colored.up, colored.failed);
    }

    // -- column layout ------------------------------------------------------

    // -- rendering ----------------------------------------------------------

    fn test_paths(dir: &std::path::Path) -> Paths {
        Paths { config: dir.join("config.toml"), state: dir.join("state.toml") }
    }

    fn sample(id: TunnelId, name: &str, port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "dev-box".into(),
            local_addr: "127.0.0.1".into(),
            local_port: port,
            remote_host: "127.0.0.1".into(),
            remote_port: 3000,
            enabled: false,
            auto_restart: true,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1_700_000_000,
        }
    }

    fn app_with_tunnels(tunnels: Vec<TunnelConfig>) -> App {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
        crate::config::save(&paths.config, &cfg).unwrap();
        // `App::new` never spawns a process, so this is safe in a unit test.
        App::new(paths)
    }

    /// Render one frame and flatten the buffer to text.
    ///
    /// ratatui stores a double-width glyph in its leading cell only and resets
    /// the following cell (which then reads back as a plain space); skipping
    /// those continuation cells keeps CJK words contiguous, so
    /// `contains("已连通")` means what it looks like.
    fn render_to_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut skip = 0u16;
            for x in 0..buf.area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let symbol = buf[(x, y)].symbol();
                out.push_str(symbol);
                skip = (display_width(symbol) as u16).saturating_sub(1);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_empty_state() {
        let mut app = app_with_tunnels(vec![]);
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("sshpf"), "title bar missing:\n{text}");
        assert!(text.contains("cfg:"), "config path missing:\n{text}");
        assert!(text.contains("还没有转发条目"), "empty hint missing:\n{text}");
        assert!(text.contains("新建"), "key hint missing:\n{text}");
        assert!(text.contains("0 up"), "counters missing:\n{text}");
        assert!(text.contains("0 off"), "counters missing:\n{text}");
    }

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
        assert!(text.contains("1 up"), "counters wrong:\n{text}");
        assert!(text.contains("1 failed"), "counters wrong:\n{text}");
        // The selected row is highlighted with the ▶ marker.
        assert!(text.contains("▶"), "selection marker missing:\n{text}");
    }

    #[test]
    fn renders_form_overlay() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.open_form_create();
        if let Some(form) = app.form.as_mut() {
            form.ssh_host.set("bastion-prod");
            form.name.set("pg-repo");
            form.local_port.set("5433");
            form.remote_port.set("5432");
            form.port_hint = Some("127.0.0.1:5433 可用".to_string());
        }
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("新建转发"), "form title missing:\n{text}");
        assert!(text.contains("SSH 主机"), "ssh host field missing:\n{text}");
        assert!(text.contains("bastion-prod"), "form value missing:\n{text}");
        assert!(text.contains("类型"), "kind field missing:\n{text}");
        assert!(text.contains("本地转发"), "kind value missing:\n{text}");
        assert!(text.contains("自动重连"), "auto restart field missing:\n{text}");
        assert!(text.contains("可用"), "port hint missing:\n{text}");
        assert!(text.contains("esc 取消"), "form footer missing:\n{text}");
    }

    #[test]
    fn key_hints_spell_out_space() {
        // The hint used to be abbreviated "spc"; spell it out so it matches the
        // key a user actually presses.
        let mut app = app_with_tunnels(vec![]);
        let text = render_to_text(&mut app, 120, 20);
        assert!(text.contains("space"), "space hint missing:\n{text}");
        assert!(text.contains("启停"), "space description missing:\n{text}");
        assert!(!text.contains("spc"), "abbreviation should be gone:\n{text}");
    }

    /// Every overlay at a width the dialog cannot fit into.
    ///
    /// `draw_confirm` sized its content area from `wanted - 4`, and on a terminal
    /// narrower than 5 columns `wanted == area.width` makes that budget 0 — so
    /// the row count divided by zero and the TUI died mid-frame, leaving the
    /// terminal in raw mode. Widths 1..=10 cover the whole broken band plus the
    /// first widths that merely clip.
    #[test]
    fn confirm_overlay_renders_when_the_terminal_is_narrower_than_the_dialog() {
        // One `App` for the whole matrix: building it loads a config file and
        // allocates a temp dir, which dominates the runtime otherwise. Drawing
        // only touches `hits` and the scroll offsets.
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.overlay =
            Overlay::Confirm { action: ConfirmAction::Delete(1), prompt: "确认删除「api-dev」？(y/N)".into() };
        for w in 1u16..=10 {
            for h in [1u16, 2, 3, 4, 5, 6, 7, 9, 12, 20, 45] {
                let backend = TestBackend::new(w, h);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("confirm at {w}x{h}: {e}"));
            }
        }
    }

    /// One frame per overlay across a sampled matrix of terminal sizes.
    ///
    /// The matrix is what turns "no size may panic" from a claim into a check:
    /// every overlay is a different composition of `Layout::split`, cell
    /// arithmetic and wrapping, and each has its own small-size cliff (a popup
    /// taller than the frame, a zero-width inner area, a one-row body). Widths
    /// 1..=12 are exhaustive because that is where the popups collapse, and the
    /// wider samples straddle every column-selection threshold (64/88/110) plus
    /// the title's progressive-disclosure cuts (46/84).
    #[test]
    fn every_overlay_renders_across_a_sampled_size_matrix() {
        let widths: Vec<u16> =
            (1u16..=12).chain([16, 20, 24, 30, 40, 46, 50, 60, 64, 70, 80, 84, 88, 100, 110, 120, 130, 140]).collect();
        let heights = [1u16, 2, 3, 4, 5, 6, 7, 8, 10, 12, 16, 20, 24, 30, 36, 45];
        let overlays = [
            Overlay::None,
            Overlay::Help,
            Overlay::Form,
            Overlay::Confirm { action: ConfirmAction::Delete(1), prompt: "确认删除「api-dev」？".into() },
            Overlay::Detail(1),
            Overlay::Filter,
        ];
        for ov in overlays {
            let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db", 5432)]);
            app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
            app.open_form_create(); // `Form` needs a state, and Detail needs a tunnel
            for &w in &widths {
                for &h in &heights {
                    // Re-assigned every frame: drawing clamps scrolls and may
                    // close nothing, but the overlay must be the one under test.
                    app.overlay = ov.clone();
                    let backend = TestBackend::new(w, h);
                    let mut term = Terminal::new(backend).unwrap();
                    term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("{ov:?} at {w}x{h}: {e}"));
                }
            }
        }
    }

    /// A cell offset built from typed text must not wrap a `u16`.
    ///
    /// `name`/`ssh_host`/ports come straight out of `config.toml`, so a corrupt
    /// or generated file can carry a 64K-character value. Editing such a field
    /// put the cursor at `inner.x + FORM_VALUE_X + display_width(prefix)`, which
    /// overflows u16 and panics in a debug build. The field value itself renders
    /// truncated; only the cursor arithmetic was unguarded.
    #[test]
    fn a_huge_field_value_cannot_overflow_the_form_cursor() {
        let mut app = app_with_tunnels(vec![sample(1, &"n".repeat(65_530), 8080)]);
        app.open_form_edit();
        if let Some(form) = app.form.as_mut() {
            form.field = FormField::Name;
            form.begin_edit();
        }
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).expect("editing a 64K-character name must not panic");
    }

    /// Same class of failure on the filter line, whose text is typed and has no
    /// length cap. The echo is truncated, but the cursor and the counters chip
    /// both added the full display width in `u16`.
    #[test]
    fn a_huge_filter_cannot_overflow_the_counters_or_the_cursor() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.filter = "x".repeat(65_530);
        for overlay in [Overlay::None, Overlay::Filter] {
            app.overlay = overlay.clone();
            let backend = TestBackend::new(120, 30);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("{overlay:?} with a long filter: {e}"));
        }
    }

    /// `warn_once` must fire exactly once per process, not once per frame.
    ///
    /// The renderer runs at ~30 fps; a degradation that survives a resize would
    /// otherwise fill the 2 MiB log and rotate the interesting history out. The
    /// log call itself goes through `crate::logging::warn` (already covered by
    /// the logging tests) — what is new here is the latch.
    #[test]
    fn warn_once_latches_so_a_per_frame_degradation_logs_once() {
        let seen = std::sync::atomic::AtomicBool::new(false);
        assert!(warn_once(&seen, "ui.render_degraded", &[("what", &"test")]), "first call must report");
        for _ in 0..100 {
            assert!(!warn_once(&seen, "ui.render_degraded", &[("what", &"test")]), "later calls must stay quiet");
        }
        // A fresh latch is per-site, so a second site still gets its own line.
        let other = std::sync::atomic::AtomicBool::new(false);
        assert!(warn_once(&other, "ui.render_degraded", &[("what", &"other")]));
    }

    /// `band` is the degradation path for a layout band that is not there.
    ///
    /// The renderer takes six bands out of one `Layout::split`; indexing them
    /// directly would make a violated solver contract a mid-frame panic. A
    /// missing band must instead come back as an empty rect (every `draw_*`
    /// treats that as "nothing to draw").
    #[test]
    fn a_missing_layout_band_degrades_instead_of_panicking() {
        let bands = [Rect { x: 0, y: 0, width: 10, height: 1 }];
        assert_eq!(band(&bands, 0, "test"), Rect { x: 0, y: 0, width: 10, height: 1 });
        for i in 1..=6 {
            assert_eq!(band(&bands, i, "test"), Rect::default(), "band {i} must degrade to an empty rect");
        }
    }

    #[test]
    fn renders_at_pathological_terminal_sizes() {
        // No size may panic: users resize terminals, and a panic here would
        // leave the terminal in raw mode.
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        for w in [1u16, 2, 3, 5, 10, 20, 40, 60, 80] {
            for h in [1u16, 2, 3, 4, 10, 24] {
                let backend = TestBackend::new(w, h);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| draw(f, &mut app)).expect("draw must not fail");
            }
        }
    }

    #[test]
    fn renders_every_overlay_without_panicking() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.selected = 0;
        let overlays = [
            Overlay::Help,
            Overlay::Form,
            Overlay::Confirm { action: ConfirmAction::Delete(1), prompt: "确认删除「x」？".into() },
            Overlay::Detail(1),
            Overlay::Filter,
        ];
        for ov in overlays {
            app.open_form_create(); // give Form a state
            app.overlay = ov.clone();
            for (w, h) in [(120u16, 40u16), (80, 24), (40, 12)] {
                let backend = TestBackend::new(w, h);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("{ov:?} at {w}x{h}: {e}"));
            }
        }
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
    fn message_bar_prefers_the_latest_status() {
        let mut app = app_with_tunnels(vec![sample(1, "a", 8080)]);
        app.set_status(crate::model::StatusMsg::error("保存失败: 磁盘只读"));
        let text = render_to_text(&mut app, 120, 24);
        assert!(text.contains("保存失败"), "status message missing:\n{text}");
    }

    #[test]
    fn no_color_mode_drops_every_colour() {
        // Accessibility: with NO_COLOR the UI must still convey state via
        // symbols and words, and must not emit colour codes.
        let p = palette_with(true);
        assert_eq!(p.up, Color::Reset);
        assert_eq!(p.failed, Color::Reset);
        assert_eq!(p.text, Color::Reset);
        assert_eq!(p.selection_bg, Color::Reset);
        assert_eq!(p.accent, Color::Reset);
        // Coloured mode really is coloured.
        let c = palette_with(false);
        assert_ne!(c.up, Color::Reset);
        assert_ne!(c.failed, Color::Reset);
        assert_ne!(c.up, c.failed, "up and failed must be distinguishable");
    }

    #[test]
    fn truncate_and_pad_agree_on_display_width() {
        // CJK takes two cells; helpers must measure display width, not bytes.
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
        let t = truncate_to_width("abcdef", 3);
        assert_eq!(display_width(&t), 3);
        assert!(t.ends_with('…'), "truncation should be visible: {t:?}");
        // CJK: the ellipsis must not push the result past the budget.
        let t = truncate_to_width("中文字", 4);
        assert!(display_width(&t) <= 4, "truncated {t:?} exceeds width 4");
        assert!(t.ends_with('…'));
        // Fitting strings are returned untouched.
        assert_eq!(truncate_to_width("abc", 5), "abc");
        assert_eq!(truncate_to_width("", 3), "");
        assert_eq!(truncate_to_width("abc", 0), "");
        assert_eq!(display_width(&pad_to_width("ab", 5)), 5);
    }

    #[test]
    fn form_shows_the_two_stage_editing_hints() {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();

        // Navigating: arrows pick a field, enter starts editing.
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("enter 编辑"), "navigating hint missing:\n{text}");
        assert!(!text.contains("编辑中"), "must not look like it is editing:\n{text}");

        // Editing: the hint switches and the field is highlighted for input.
        app.form.as_mut().unwrap().field = FormField::Name;
        app.form.as_mut().unwrap().begin_edit();
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("编辑中"), "editing hint missing:\n{text}");
        assert!(text.contains("enter/esc 结束编辑"), "exit-editing hint missing:\n{text}");
    }

    #[test]
    fn form_renders_fields_in_the_new_order() {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        let text = render_to_text(&mut app, 120, 40);
        let pos = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle} missing:\n{text}"));
        // Remote target before the local landing port.
        assert!(pos("远端端口") < pos("本地端口"), "remote port must come before local port:\n{text}");
        assert!(pos("远端地址") < pos("远端端口"), "remote host before remote port:\n{text}");
    }

    #[test]
    fn auto_suggested_local_port_is_labelled_as_editable() {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.remote_port.set("18077");
        }
        app.refresh_local_port_suggestion();
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("可编辑"), "suggestion must say it can be edited:\n{text}");
    }

    #[test]
    fn chosen_host_is_visible_and_not_overwritten_by_the_hint() {
        // Regression: appending "(N台可选)" unconditionally pushed the picked
        // alias out of the field, so the user could not confirm the choice.
        let mut app = app_with_tunnels(vec![]);
        app.hosts = (0..12)
            .map(|i| crate::ssh::SshHost {
                alias: format!("host-{i:02}"),
                hostname: "1.1.1.1".into(),
                user: None,
                port: 22,
                proxy_jump: Vec::new(),
                identity_file: None,
            })
            .collect();
        app.open_form_create();

        // Empty field: advertise the list.
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("12台可选"), "empty field should hint at the list:\n{text}");

        // Chosen host: the alias must win over the hint.
        app.open_host_picker();
        app.picker_accept();
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("host-00"), "picked alias missing:\n{text}");
        assert!(!text.contains("12台可选"), "hint must not crowd out the value:\n{text}");
    }

    #[test]
    fn renders_form_errors_and_dynamic_kind() {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        if let Some(form) = app.form.as_mut() {
            form.kind_idx = 2; // Dynamic
            form.errors = vec!["名称不能为空".to_string()];
            form.field = FormField::RemoteHost;
        }
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("动态转发"), "dynamic kind missing:\n{text}");
        assert!(text.contains("SOCKS5"), "dynamic placeholder missing:\n{text}");
        assert!(text.contains("名称不能为空"), "form error missing:\n{text}");
    }

    #[test]
    fn renders_help_overlay_and_scrolls() {
        let mut app = app_with_tunnels(vec![]);
        app.open_help();
        let top = render_to_text(&mut app, 120, 35);
        assert!(top.contains("帮助"), "help title missing:\n{top}");
        assert!(top.contains("向下移动选择"), "first help row missing:\n{top}");

        // Scrolling hides earlier rows: the very first item must be gone while
        // a mid-list section stays visible. The offset is clamped, not dropped.
        app.help_scroll = 2;
        let scrolled = render_to_text(&mut app, 120, 35);
        assert!(app.help_scroll > 0, "help_scroll must survive the clamp");
        assert!(!scrolled.contains("向下移动选择"), "scroll had no effect:\n{scrolled}");
        assert!(scrolled.contains("新建转发"), "scrolled body missing:\n{scrolled}");
    }

    #[test]
    fn renders_detail_overlay_with_command_and_logs() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.tunnels[0].status = TunnelStatus::Failed {
            reason: FailureInfo::with_short(
                FailureKind::PortInUse,
                "本地端口被占用",
                "bind 127.0.0.1:8080: Address already in use",
            ),
        };
        app.tunnels[0].log.push(LogLine::new(LogLevel::Error, "bind 127.0.0.1:8080 failed"));
        app.open_detail();
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("详情"), "detail title missing:\n{text}");
        assert!(text.contains("ssh -N -L"), "equivalent command missing:\n{text}");
        assert!(text.contains("Address already in use"), "raw failure missing:\n{text}");
        assert!(text.contains("日志"), "log section missing:\n{text}");
        assert!(text.contains("api-dev"), "tunnel name missing:\n{text}");
    }

    #[test]
    fn renders_host_dropdown_and_confirm() {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        app.hosts = vec![
            crate::ssh::SshHost {
                alias: "bastion-prod".into(),
                hostname: "10.0.4.9".into(),
                user: Some("deploy".into()),
                port: 22,
                proxy_jump: Vec::new(),
                identity_file: None,
            },
            crate::ssh::SshHost {
                alias: "dev-box".into(),
                hostname: "192.168.1.7".into(),
                user: None,
                port: 2222,
                proxy_jump: Vec::new(),
                identity_file: None,
            },
        ];

        // Collapsed: the field advertises that a list exists.
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("2台可选"), "host field should hint at the list:\n{text}");
        assert!(text.contains('▾'), "collapsed field should show a ▾ marker:\n{text}");

        // Expanded: the list is inline, under the SSH HOST row.
        app.open_host_picker();
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("从 ~/.ssh/config 选择主机"), "dropdown title missing:\n{text}");
        assert!(text.contains("bastion-prod"), "host alias missing:\n{text}");
        assert!(text.contains("deploy@10.0.4.9:22"), "host endpoint missing:\n{text}");
        assert!(text.contains("enter 确定"), "dropdown hint missing:\n{text}");
        assert!(text.contains('▴'), "expanded field should show a ▴ marker:\n{text}");

        // Typing filters the list.
        app.picker_query_char('d');
        app.picker_query_char('e');
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("dev-box"), "filtered host missing:\n{text}");
        assert!(!text.contains("bastion-prod"), "filter should drop non-matches:\n{text}");

        // Accepting fills the field and collapses the list.
        app.picker_accept();
        assert_eq!(app.form.as_ref().unwrap().ssh_host.value(), "dev-box");
        assert!(!app.host_picker_open(), "accept should collapse the dropdown");
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("dev-box"));

        app.close_overlay();
        app.overlay = Overlay::Confirm {
            action: ConfirmAction::Delete(1),
            prompt: "确认删除「api-dev」？(y/N)".to_string(),
        };
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("确认删除"), "confirm prompt missing:\n{text}");
        assert!(text.contains("y "), "confirm key hint missing:\n{text}");
    }

    #[test]
    fn renders_filter_line_and_narrow_terminals_without_panicking() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db-prod", 5432)]);
        app.filter = "api".into();
        app.overlay = Overlay::Filter;
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("过滤:"), "filter prompt missing:\n{text}");
        assert!(text.contains("api"), "filter text missing:\n{text}");
        // Only the matching row is rendered.
        assert!(text.contains("api-dev"));
        assert!(!text.contains("db-prod"), "filter did not narrow the table:\n{text}");

        // Every screen size from tiny to huge must render without panicking.
        for (w, h) in [(20u16, 6u16), (40, 10), (60, 14), (80, 20), (100, 30), (200, 60)] {
            app.overlay = Overlay::None;
            let _ = render_to_text(&mut app, w, h);
            app.open_form_create();
            let _ = render_to_text(&mut app, w, h);
            app.open_help();
            let _ = render_to_text(&mut app, w, h);
            app.close_overlay();
            app.open_detail();
            let _ = render_to_text(&mut app, w, h);
            app.close_overlay();
        }
    }

    #[test]
    fn scroll_offsets_are_clamped_during_draw() {
        let mut app = app_with_tunnels(vec![]);
        app.overlay = Overlay::Help;
        app.help_scroll = 9999;
        let _ = render_to_text(&mut app, 120, 35);
        assert!(app.help_scroll < 9999, "help_scroll must be clamped");

        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.open_detail();
        app.detail_scroll = 9999;
        let _ = render_to_text(&mut app, 120, 35);
        assert!(app.detail_scroll < 9999, "detail_scroll must be clamped");
    }

    #[test]
    fn key_hints_swap_instead_of_lying_in_selection_mode() {
        // Selection mode RE-BINDS space and unbinds r/d/n/e. Showing the normal
        // hints while it is active would advertise keys that do something else
        // or nothing at all.
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db", 8081)]);
        let normal = render_to_text(&mut app, 140, 24);
        assert!(normal.contains("启停"), "normal hints must describe space:\n{normal}");
        assert!(normal.contains("重启"), "normal hints must mention r:\n{normal}");

        app.selected = 0;
        app.toggle_select_mode();
        let selecting = render_to_text(&mut app, 140, 24);
        assert!(!selecting.contains("启停"), "space does not start/stop in selection mode:\n{selecting}");
        assert!(selecting.contains("勾选"), "selection hints must describe space:\n{selecting}");
        assert!(selecting.contains("批量操作"), "selection hints must point at the batch keys:\n{selecting}");
        assert!(selecting.contains("已选"), "the marked count must be on screen:\n{selecting}");
    }

    #[test]
    fn selection_mode_gutter_marks_only_marked_rows() {
        let mut app = app_with_tunnels(vec![sample(1, "first", 8080), sample(2, "second", 8081)]);
        app.selected = 0;
        app.toggle_select_mode();
        // Nothing marked yet: the bar says 0 and no row carries a mark.
        let text = render_to_text(&mut app, 140, 26);
        assert!(text.contains("已选 0 条"), "initial state must be stated:\n{text}");
        app.toggle_mark_selected();
        let text = render_to_text(&mut app, 140, 26);
        assert!(text.contains("已选 1 条"), "marking must be visible:\n{text}");
        assert!(text.contains('✓'), "the marked row needs its own gutter symbol:\n{text}");
    }

    #[test]
    fn the_import_pane_is_reachable_only_through_the_form() {
        // The pane must never leak into the normal table view.
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        let text = render_to_text(&mut app, 140, 26);
        assert!(!text.contains("导入"), "the pane is a form sub-pane, not table chrome:\n{text}");
    }

    #[test]
    fn status_always_carries_symbol_and_word() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("● 已连通"), "status symbol+word missing:\n{text}");
    }

    #[test]
    fn form_field_buf_matches_form_state() {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        let form = app.form.as_ref().unwrap();
        assert_eq!(form_field_buf(form, FormField::Name).map(InputBuf::value), Some(""));
        assert!(form_field_buf(form, FormField::Kind).is_none());
        assert!(form_field_buf(form, FormField::AutoRestart).is_none());
        assert_eq!(form_field_buf(form, FormField::LocalAddr).map(InputBuf::value), Some("127.0.0.1"));
    }

    // -- import pane --------------------------------------------------------

    fn fwd(kind: ForwardKind, listen: u16, target: &str, target_port: u16, raw: &str) -> SshForward {
        SshForward {
            kind,
            listen_addr: String::new(),
            listen_port: listen,
            target_host: target.to_string(),
            target_port,
            host_alias: "dev-box".to_string(),
            line: 12,
            raw: raw.to_string(),
        }
    }

    /// An app with a create form whose SSH HOST is set and whose import table
    /// holds `rows`, pane open.
    fn app_with_import(rows: Vec<SshForward>) -> App {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        app.hosts = vec![crate::ssh::SshHost {
            alias: "dev-box".into(),
            hostname: "10.0.0.9".into(),
            user: None,
            port: 22,
            proxy_jump: Vec::new(),
            identity_file: None,
        }];
        if let Some(f) = app.form.as_mut() {
            f.ssh_host.set("dev-box");
            f.set_import_table("dev-box", ImportTable::new(rows));
            f.import_open = true;
        }
        app
    }

    fn sample_forwards() -> Vec<SshForward> {
        vec![
            fwd(ForwardKind::Local, 8080, "127.0.0.1", 80, "LocalForward 8080 127.0.0.1:80"),
            fwd(ForwardKind::Remote, 9000, "127.0.0.1", 9001, "RemoteForward 9000 127.0.0.1:9001"),
            // Un-importable: listen port could not be parsed.
            fwd(ForwardKind::Local, 0, "127.0.0.1", 5432, "LocalForward abc 127.0.0.1:5432"),
        ]
    }

    #[test]
    fn import_pane_renders_rows_checkboxes_and_summary() {
        let mut app = app_with_import(sample_forwards());
        let text = render_to_text(&mut app, 120, 45);
        assert!(text.contains("从 ~/.ssh/config 导入转发"), "pane title missing:\n{text}");
        assert!(text.contains("localhost:8080"), "listen cell missing:\n{text}");
        assert!(text.contains("(SOCKS5)") || text.contains("127.0.0.1:9001"), "target cell missing:\n{text}");
        // The 原始指令 column is the elastic one, so a long directive is shown
        // truncated with an ellipsis rather than being allowed to overflow.
        assert!(text.contains("LocalForward 8080"), "raw directive missing:\n{text}");
        // Two importable rows are pre-ticked, the un-importable one is not.
        assert!(text.contains("[x]"), "checked box missing:\n{text}");
        assert!(text.contains("[–]"), "un-importable box should be the striped form:\n{text}");
        assert!(text.contains("2/2 条勾选"), "summary must count importable rows only:\n{text}");
        assert!(text.contains("space 勾选"), "pane footer hints missing:\n{text}");
    }

    #[test]
    fn unimportable_import_row_shows_its_reason_and_cannot_be_ticked() {
        let mut app =
            app_with_import(vec![fwd(ForwardKind::Local, 0, "127.0.0.1", 5432, "LocalForward abc 127.0.0.1:5432")]);
        let text = render_to_text(&mut app, 120, 45);
        let row =
            text.lines().find(|l| l.contains("端口无法解析")).unwrap_or_else(|| panic!("reason missing:\n{text}"));
        assert!(!row.contains("[x]"), "a broken row must not look ticked: {row:?}");
        assert!(row.contains("[–]"), "it should look untickable, not merely unticked: {row:?}");
        assert!(text.contains("0/0 条勾选"), "summary should show nothing importable:\n{text}");
        // Ticking it is a no-op, at the model level too.
        let form = app.form.as_mut().unwrap();
        form.import.toggle_cursor();
        assert_eq!(form.import.checked_count(), 0, "un-importable rows must not become checked");
    }

    #[test]
    fn import_pane_shows_filtered_header_chips_and_buttons() {
        let mut app = app_with_import(sample_forwards());
        if let Some(f) = app.form.as_mut() {
            f.import.set_filter(ImportColumn::Kind, "R");
        }
        let text = render_to_text(&mut app, 120, 45);
        // The filtered column header is marked distinctly (`▾`) ...
        assert!(text.contains("类型▾"), "filtered header marker missing:\n{text}");
        // ... and the chip line names the active filter with a delete affordance.
        assert!(text.contains("筛选: 类型=R ×"), "filter chip missing:\n{text}");
        assert!(text.contains("清除全部筛选"), "clear-all affordance missing:\n{text}");
        assert!(text.contains("撤销筛选"), "undo affordance missing:\n{text}");
        assert!(text.contains("全选"), "select-all button missing:\n{text}");
        // Only the R row survives the filter.
        assert!(text.contains("RemoteForward 900"), "filtered row missing:\n{text}");
        assert!(!text.contains("LocalForward 8080"), "filter did not narrow:\n{text}");
    }

    #[test]
    fn import_pane_shows_the_editing_column_with_a_cursor() {
        let mut app = app_with_import(sample_forwards());
        if let Some(f) = app.form.as_mut() {
            f.import.begin_filter(ImportColumn::Listen);
            f.import.filter_char(ImportColumn::Listen, '8');
        }
        let text = render_to_text(&mut app, 120, 45);
        assert!(text.contains("监听:8"), "the column being typed into must echo the input:\n{text}");
    }

    #[test]
    fn a_terminal_too_short_for_the_pane_says_the_batch_will_still_be_created() {
        // The pane shrinks away on a short terminal, but `import_open` stays true
        // and `submit_form` keys off it: `ctrl+s` creates the ticked rows even
        // though not one of them is on screen. Silence there means creating
        // tunnels the user cannot see, so the form has to say it.
        let mut app = app_with_import(sample_forwards());
        let text = render_to_text(&mut app, 100, 14);
        assert!(!text.contains("从 ~/.ssh/config 导入转发"), "precondition: the pane does not fit:\n{text}");
        assert!(text.contains("导入面板未显示"), "the invisible-batch state must be announced:\n{text}");
        assert!(text.contains("ctrl+s"), "and it must say what ctrl+s will do:\n{text}");

        // Collapsing the pane (which is what the message suggests) makes the
        // warning moot: the form is back in charge and nothing is hidden.
        app.close_import_pane();
        let text = render_to_text(&mut app, 100, 14);
        assert!(!text.contains("导入面板未显示"), "a collapsed pane has nothing hidden:\n{text}");
    }

    #[test]
    fn import_pane_renders_at_small_sizes_without_panicking() {
        // The pane must degrade, never panic: it is opened by a key press, and a
        // panic would leave the terminal in raw mode.
        for rows in [0usize, 1, 3, 40] {
            let mut app = app_with_import(
                (0..rows)
                    .map(|i| fwd(ForwardKind::Local, 8000 + i as u16, "127.0.0.1", 80, "LocalForward x"))
                    .collect(),
            );
            if rows == 0 {
                app.form.as_mut().unwrap().import_open = true;
            }
            for (w, h) in [(1u16, 1u16), (8, 6), (20, 8), (40, 12), (74, 24), (120, 45)] {
                let backend = TestBackend::new(w, h);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("import pane at {w}x{h}: {e}"));
            }
        }
    }

    #[test]
    fn import_pane_empty_state_says_there_are_no_forwards() {
        // `import_open` on an empty table with no error: reachable when a host
        // switch empties the list. It must not collapse silently — the pane says
        // why it is empty, and the manual form fields all stay on screen.
        let mut app = app_with_import(vec![]);
        app.form.as_mut().unwrap().import_open = true;
        app.import_error = None;
        let text = render_to_text(&mut app, 120, 45);
        assert!(text.contains("没有端口转发配置"), "empty state missing:\n{text}");
        assert!(text.contains("从 ~/.ssh/config 导入转发"), "pane title missing:\n{text}");
        for field in FormField::ORDER {
            assert!(text.contains(field.label()), "form field {field:?} pushed out by the pane:\n{text}");
        }
    }

    #[test]
    fn import_pane_empty_state_surfaces_a_discovery_error() {
        let mut app = app_with_import(vec![]);
        app.form.as_mut().unwrap().import_open = true;
        app.import_error = Some("读取 ~/.ssh/config 转发配置失败: permission denied".to_string());
        let text = render_to_text(&mut app, 120, 45);
        assert!(text.contains("permission denied"), "the reason must be readable:\n{text}");
        assert!(!text.contains("没有端口转发配置"), "a failure is not the same as 'no forwards':\n{text}");
    }

    #[test]
    fn hits_are_not_pushed_for_rows_outside_the_list_area() {
        // 30 tunnels into a 14-row terminal: most rows are below the table area,
        // and a click down there must not resolve to a row that is not drawn.
        let tunnels: Vec<TunnelConfig> = (1..=30).map(|i| sample(i, &format!("t-{i:02}"), 8000 + i as u16)).collect();
        let mut app = app_with_tunnels(tunnels);
        let _ = render_to_text(&mut app, 100, 14);

        let list = app.hits.rect_of(&HitTarget::TunnelList).expect("table area must be recorded");
        let pushed: Vec<u16> =
            (1..=30u64).filter_map(|i| app.hits.rect_of(&HitTarget::TunnelRow(i)).map(|r| r.y)).collect();
        assert!(!pushed.is_empty(), "some rows must be clickable");
        assert!(pushed.iter().all(|y| *y < list.bottom()), "a row hit escaped the table area: {pushed:?}");
        assert!(pushed.len() < 30, "off-screen rows must not be clickable: {pushed:?}");
    }

    #[test]
    fn import_pane_hits_resolve_to_the_checkbox_and_row() {
        let mut app = app_with_import(sample_forwards());
        let _ = render_to_text(&mut app, 120, 45);

        // The first row's checkbox cell: the hit map must resolve to the *raw
        // row index*, not the visible position.
        let cb = app.hits.rect_of(&HitTarget::ImportCheckbox(0)).expect("first checkbox must be clickable");
        assert_eq!(app.hits.at(cb.x, cb.y), Some(&HitTarget::ImportCheckbox(0)));

        // Somewhere else on the same row: selects rather than toggles.
        let row = app.hits.rect_of(&HitTarget::ImportRow(0)).expect("first row must be clickable");
        let far_x = row.right().saturating_sub(2).max(row.x);
        assert_eq!(app.hits.at(far_x, row.y), Some(&HitTarget::ImportRow(0)), "wide part of the row selects");

        // Column headers and the action buttons are recorded too.
        assert!(app.hits.rect_of(&HitTarget::ImportColumnHeader(ImportColumn::Listen)).is_some());
        assert!(app.hits.rect_of(&HitTarget::ImportCheckAll).is_some());
        assert!(app.hits.rect_of(&HitTarget::ImportInvert).is_some());
        assert!(app.hits.rect_of(&HitTarget::ImportUncheckAll).is_some());
        assert!(app.hits.rect_of(&HitTarget::ImportList).is_some());
    }

    #[test]
    fn import_pane_hits_are_rebuilt_every_frame() {
        let mut app = app_with_import(sample_forwards());
        let _ = render_to_text(&mut app, 120, 45);
        let first = app.hits.len();
        assert!(first > 0);
        // Closing the pane must not leave its rects behind, or a click would hit
        // a widget that is no longer drawn.
        app.form.as_mut().unwrap().import_open = false;
        let _ = render_to_text(&mut app, 120, 45);
        assert!(app.hits.rect_of(&HitTarget::ImportCheckbox(0)).is_none(), "stale import hits survived");
    }

    // -- selection mode -----------------------------------------------------

    #[test]
    fn selection_mode_shows_the_mark_gutter_and_action_bar() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db-prod", 5432)]);
        app.selected = 0;
        app.toggle_select_mode();
        // Entering the mode starts with nothing marked; mark explicitly so the
        // gutter has something to show.
        app.toggle_mark_selected();
        let text = render_to_text(&mut app, 120, 30);
        assert!(text.contains("✓"), "a marked row needs the ✓ gutter:\n{text}");
        assert!(text.contains("已选 1 条"), "marked count missing:\n{text}");
        assert!(text.contains("s 启用"), "start action missing:\n{text}");
        assert!(text.contains("x 停用"), "stop action missing:\n{text}");
        assert!(text.contains("R 重启"), "restart action missing:\n{text}");
        assert!(text.contains("D 删除"), "delete action missing:\n{text}");
        assert!(text.contains("esc 退出"), "exit hint missing:\n{text}");
        // The cursor marker and the mark gutter coexist.
        assert!(text.contains("▶"), "cursor marker missing:\n{text}");
    }

    #[test]
    fn empty_selection_is_stated_plainly() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.toggle_select_mode();
        app.mark_none();
        let text = render_to_text(&mut app, 120, 30);
        assert!(text.contains("已选 0 条"), "an empty selection must be stated:\n{text}");
        // The gutter itself still renders (with `·`), so the column layout does
        // not jump when the first mark appears; only the ✓ marks are absent.
        let rows: Vec<&str> = text.lines().filter(|l| l.contains("api-dev")).collect();
        assert!(!rows.is_empty(), "tunnel row missing:\n{text}");
        assert!(rows.iter().all(|r| !r.contains('✓')), "nothing is marked, so no ✓ should appear: {rows:?}");
    }

    #[test]
    fn selection_mode_hits_resolve_to_rows_and_actions() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db-prod", 5432)]);
        app.toggle_select_mode();
        let _ = render_to_text(&mut app, 120, 30);

        let row1 = app.hits.rect_of(&HitTarget::TunnelRow(1)).expect("row must be clickable");
        assert_eq!(app.hits.at(row1.x + 1, row1.y), Some(&HitTarget::TunnelRow(1)));
        assert!(app.hits.rect_of(&HitTarget::TunnelList).is_some());

        let start = app.hits.rect_of(&HitTarget::BatchAction(BatchAction::Start)).expect("start button missing");
        assert_eq!(app.hits.at(start.x, start.y), Some(&HitTarget::BatchAction(BatchAction::Start)));
        assert!(app.hits.rect_of(&HitTarget::BatchAction(BatchAction::Delete)).is_some());
        assert!(app.hits.rect_of(&HitTarget::BatchSelectAll).is_some());
        assert!(app.hits.rect_of(&HitTarget::BatchExit).is_some());
    }

    #[test]
    fn normal_mode_has_no_mark_gutter_or_action_bar() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        let text = render_to_text(&mut app, 120, 30);
        assert!(!text.contains("已选 "), "the action bar must only exist in selection mode:\n{text}");
        assert!(!text.contains("R 重启"), "batch hints must not leak into normal mode:\n{text}");
        assert!(app.hits.rect_of(&HitTarget::BatchAction(BatchAction::Start)).is_none());
    }

    #[test]
    fn filter_overlay_records_its_input_and_clear_affordances() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.overlay = Overlay::Filter;
        app.filter = "api".into();
        let text = render_to_text(&mut app, 120, 30);
        assert!(text.contains("× 清除"), "clear affordance missing:\n{text}");
        assert!(app.hits.rect_of(&HitTarget::FilterBar).is_some(), "filter input must be clickable");
        assert!(app.hits.rect_of(&HitTarget::FilterClear).is_some(), "filter clear must be clickable");
    }
}

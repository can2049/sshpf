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
//! Every string the user reads is bilingual, Chinese first and English second
//! (`中文 English`), so no screen is a dead end for either reader. Narrow cells
//! degrade through [`crate::help::fit`] rather than overflowing.
//!
//! Layout of the main screen (matches `docs/research/prior-art.md` §7):
//!
//! ```text
//! row 0          title bar      sshpf 0.1.0 · ssh-config N hosts · cfg PATH   HH:MM:SS
//! row 1          counters       ● 4 已连通/Up  ◐ 2 连接中/Starting  …
//! rows 2..h-5    tunnel table   名称/Name | T | 本地/Local | 远端/Remote | …
//! row h-5        message bar    app.status, or the selected tunnel's full error
//! row h-4        selection bar  only while selecting (`v`)
//! rows h-3..h    key hints      three lines, every main-screen key
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
use layout::{COL_SPACING, DETAIL_LOG_TAIL, form_label_w, form_value_x};
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
// Bilingual text helpers
// ---------------------------------------------------------------------------
//
// Two shapes cover the whole UI:
//
// * prose and hint lines: `中文 / English` — the slash keeps the two languages
//   apart on one line, Chinese first so a truncated line still reads and so the
//   existing `contains("中文")` assertions keep meaning something;
// * narrow cells (a column header, a status word, a button): `中文/English`
//   without the spaces, because a table cell has no room for prose.
//
// Neither may overflow: `fit_pair` is the progressive-disclosure ladder that
// [`crate::help::fit`] implements for the help catalog — both languages, then
// Chinese only, then a truncation — with the compact `/` join this module uses.

/// `中文 / English`, for hint lines and dialog prose.
fn pair(zh: &str, en: &str) -> String {
    format!("{zh} / {en}")
}

/// `中文/English` fitted to `width` cells; Chinese only when the pair is too
/// wide, a truncation when even that does not fit.
fn fit_pair(zh: &str, en: &str, width: usize) -> String {
    let both = format!("{zh}/{en}");
    if display_width(&both) <= width {
        return both;
    }
    if display_width(zh) <= width {
        return zh.to_string();
    }
    truncate_to_width(zh, width)
}

/// `中文 / English` fitted to `width` cells, with the same ladder as
/// [`fit_pair`].
fn fit_pair_spaced(zh: &str, en: &str, width: usize) -> String {
    let both = pair(zh, en);
    if display_width(&both) <= width {
        return both;
    }
    if display_width(zh) <= width {
        return zh.to_string();
    }
    truncate_to_width(zh, width)
}

/// Greedy wrap to `width` cells.
///
/// A CJK sentence has no spaces to break on, so the fallback is a hard break at
/// a character boundary; Latin text prefers the last space so words stay whole.
/// Overflow is impossible in practice because every produced line is truncated
/// to `width` at render time, and the *count* of lines is what the scroll clamp
/// measures.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = crate::text::char_width(ch);
        if used + w > width && !cur.is_empty() {
            if let Some(pos) = cur.rfind(' ') {
                let rest = cur[pos + 1..].to_string();
                cur.truncate(pos);
                out.push(std::mem::take(&mut cur));
                cur = rest;
                used = display_width(&cur);
            } else {
                out.push(std::mem::take(&mut cur));
                used = 0;
            }
        }
        cur.push(ch);
        used += w;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
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
        Constraint::Length(3),        // key hints (three lines: every key, both languages)
    ])
    .split(area);

    draw_title(frame, app_ref, rows[0]);
    draw_counters(frame, app_ref, rows[1]);
    draw_table(frame, app_ref, rows[2], &mut hits);
    draw_message(frame, app_ref, rows[3]);
    if select_h > 0 {
        draw_selection_bar(frame, app_ref, rows[4], &mut hits);
    }
    draw_keys(frame, app_ref, rows[5]);

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
///
/// The help overlay's length is *measured*, not assumed: [`help_lines`] is the
/// very function the renderer uses, so the clamp and the drawing can never
/// disagree about how many rows the wrapped catalog actually occupies.
fn clamp_scrolls(app: &mut App, area: Rect) {
    let help_max =
        help_lines(help_inner_width(area) as usize, palette()).len().saturating_sub(help_inner_height(area) as usize);
    app.help_scroll = app.help_scroll.min(help_max.min(u16::MAX as usize) as u16);

    if let Overlay::Detail(id) = app.overlay {
        let max = match app.tunnel(id) {
            Some(view) => detail_lines(view, id, palette()).len().saturating_sub(detail_inner_height(area) as usize),
            None => 0,
        };
        app.detail_scroll = app.detail_scroll.min(max.min(u16::MAX as usize) as u16);
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
    let left = truncate_to_width(&left, chunks[0].width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(left, p.fg(p.text).add_modifier(Modifier::BOLD)))),
        chunks[0],
    );

    let clock = truncate_to_width(&now_hms(), chunks[1].width as usize);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(clock, p.fg(p.dim)))), chunks[1]);
}

fn draw_counters(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let c = app.counts();

    // Full bilingual labels on a wide terminal, the compact `●2 ◐0 ◌1 ✗1 ○2`
    // form when narrow — otherwise the counters are clipped mid-word. Colour is
    // never the only signal: every counter keeps its glyph.
    let compact = area.width < 96;
    let mut spans = vec![Span::raw(" ")];
    let label = |zh: &str, en: &str| if compact { String::new() } else { format!(" {zh}/{en}") };
    let items: [(Color, String); 5] = [
        (p.up, format!("● {}{}", c.up, label("已连通", "Up"))),
        (p.pending, format!("◐ {}{}", c.starting, label("连接中", "Starting"))),
        (p.pending, format!("◌ {}{}", c.retrying, label("重试", "Retrying"))),
        (p.failed, format!("✗ {}{}", c.failed, label("失败", "Failed"))),
        (p.off, format!("○ {}{}", c.off, label("未启用", "Off"))),
    ];
    let gap = if compact { "  " } else { "   " };
    for (i, (color, text)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(gap));
        }
        spans.push(Span::styled(text.clone(), p.fg(*color)));
    }

    let right_w = if app.filter.is_empty() { 0 } else { (display_width(&app.filter) as u16 + 16).min(area.width / 2) };
    let chunks = Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).split(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);

    if right_w > 0 {
        let text = truncate_to_width(&format!("过滤/Filter: {} ", app.filter), chunks[1].width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, p.fg(p.accent))).alignment(Alignment::Right)),
            chunks[1],
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
        let tail = fit_pair_spaced("enter 查看详情", "enter details", area.width as usize);
        let text = format!(" ✗ {}   ({tail})", reason.detail);
        Line::from(Span::styled(truncate_to_width(&text, area.width as usize), p.fg(p.failed)))
    } else {
        let idle = fit_pair_spaced(
            "就绪 · n 新建转发 · ? 帮助 · q 退出",
            "Ready · n new · ? help · q quit",
            area.width as usize,
        );
        Line::from(Span::styled(format!(" {idle}"), p.fg(p.dim)))
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Three lines of key hints, exactly matching the keys `main` dispatches.
///
/// The hints change in selection mode, because the mode *re-binds* keys rather
/// than adding to them: `space` marks instead of starting, and `r`/`d`/`n`/`e`
/// are not bound at all there. Leaving the normal hints on screen would
/// advertise keys that do something else or nothing — the exact way a user
/// discovers "the app is broken" while it is working as designed.
///
/// Every main-screen binding appears here, in both languages, and the list is
/// *flowed* across the three rows rather than sliced into fixed groups: a
/// footer is the one place where dropping a key silently is the whole bug, so
/// the layout gives each hint its own width and wraps instead of clipping.
fn draw_keys(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let hints: &[Hint] = if app.select_mode.active { SELECT_HINTS } else { MAIN_HINTS };
    let lines = flow_hints(hints, area.width as usize, area.height as usize, p);
    frame.render_widget(Paragraph::new(lines), area);
}

/// One `(key, Chinese, English)` triple of the hint lines.
type Hint = (&'static str, &'static str, &'static str);

/// Every binding of the normal main screen, complete: no key is left off.
///
/// Order is "how often you reach for it", not "what the match arm order is", so
/// that when a very narrow terminal cannot fit all three rows the least-used
/// hints are the ones that fall off the end.
const MAIN_HINTS: &[Hint] = &[
    ("j/k ↑↓", "移动", "Move"),
    ("g/G", "首尾", "First·Last"),
    ("space", "启停", "Start·Stop"),
    ("r", "重启", "Restart"),
    ("n", "新建", "New"),
    ("e/E", "编辑", "Edit"),
    ("d", "删除", "Delete"),
    ("enter/l", "详情", "Detail"),
    ("/", "过滤", "Filter"),
    ("a", "全启", "Start all"),
    ("x", "全停", "Stop all"),
    ("v/V", "多选", "Select"),
    ("?", "帮助", "Help"),
    ("q", "退出", "Quit"),
    ("esc", "清过滤", "Clear filter"),
];

/// The selection-mode footer. It must not advertise `r`/`d`/`n`/`e`, which are
/// unbound there, nor `space` as start/stop, which it re-binds to "mark".
const SELECT_HINTS: &[Hint] = &[
    ("j/k ↑↓", "移动", "Move"),
    ("space", "勾选", "Mark"),
    ("J/K ⇧↑↓", "扩选", "Extend"),
    ("g/G", "首尾", "First·Last"),
    ("a", "全选", "Select all"),
    ("s/x/R/D", "批量", "Batch"),
    ("v/esc", "退出", "Exit"),
    ("?", "帮助", "Help"),
];

/// Flow `hints` into at most `max_lines` rows of `width` cells.
///
/// Greedy and lossless up to `max_lines`: a hint that would cross the right edge
/// starts a new row, so the only thing that can drop is a hint that does not fit
/// on the last row of a genuinely tiny terminal.
fn flow_hints(hints: &[Hint], width: usize, max_lines: usize, p: Palette) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    let mut used = 1usize;

    for (key, zh, en) in hints {
        // `key 中文/English`, degrading to Chinese when the pair is too wide.
        let desc = fit_pair(zh, en, width);
        let w = display_width(key) + 1 + display_width(&desc);
        let sep = if used > 1 { 2 } else { 0 };
        if used + sep + w > width && used > 1 {
            lines.push(Line::from(std::mem::take(&mut spans)));
            spans.push(Span::raw(" "));
            used = 1;
        }
        if lines.len() + 1 >= max_lines && used + w > width {
            // No row left for this hint: stop rather than overflow the frame.
            break;
        }
        if used > 1 {
            spans.push(Span::raw("  "));
            used += 2;
        }
        spans.push(Span::styled((*key).to_string(), p.fg(p.accent)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(desc, p.fg(p.dim)));
        used += w;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines.truncate(max_lines);
    lines
}

// ---------------------------------------------------------------------------
// Main table
// ---------------------------------------------------------------------------

/// `(symbol, Chinese word, English word, palette slot)` for a status.
///
/// The symbol *and* both words are returned so a `NO_COLOR` run keeps the whole
/// meaning; colour is looked up through the palette and is never the only carrier.
fn status_parts(status: &TunnelStatus) -> (&'static str, &'static str, &'static str, fn(&Palette) -> Color) {
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
fn status_text(status: &TunnelStatus, fitting: usize) -> String {
    let (sym, zh, en, _) = status_parts(status);
    let word = fit_pair(zh, en, fitting.saturating_sub(display_width(sym) + 1));
    match status {
        TunnelStatus::Retrying { attempt, .. } => format!("{sym} {word}{attempt}"),
        _ => format!("{sym} {word}"),
    }
}

/// Text for the 时长 column.
fn uptime_text(status: &TunnelStatus, fitting: usize) -> String {
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
        // `中文/English` where the column is wide enough, Chinese where it is
        // not: a header that overflowed would push the column boundaries off the
        // cells they describe, which is worse than losing the English.
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
            "还没有转发条目，按 n 新建 / No tunnels yet, press n"
        } else {
            "没有匹配的条目，按 esc 清除过滤 / No match, esc clears the filter"
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
        let y = area.y + 2 + i as u16;
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

    // The count is emphasised because it is the mode's whole state. It carries
    // the English only on a terminal wide enough to keep every action button on
    // the bar: dropping a button to pay for the translation would be a bad trade
    // (the buttons are clickable, the number is not).
    let count = if area.width >= 140 { format!(" 已选 {n} 条 / {n} marked ") } else { format!(" 已选 {n} 条 ") };
    let count_w = display_width(&count) as u16;
    let count_style = if n == 0 { p.fg(p.pending) } else { p.fg(p.up) }.add_modifier(Modifier::BOLD);
    spans.push(Span::styled(count, count_style));
    x = x.saturating_add(count_w);

    // Layout, not content: the count plus the actions still fits narrow
    // terminals because each label is dropped whole once it stops fitting.
    // The label comes from `BatchAction` itself, so the bar and the confirmation
    // prompt can never drift apart.
    let buttons: [(BatchAction, &'static str); 4] =
        [(BatchAction::Start, "s"), (BatchAction::Stop, "x"), (BatchAction::Restart, "R"), (BatchAction::Delete, "D")];
    for (action, key) in buttons {
        let label = format!("{key} {}", fit_pair(action.label(), action.label_en(), 20));
        let sep = " · ";
        let sep_w = display_width(sep) as u16;
        let label_w = display_width(&label) as u16;
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

    for (label, target) in [
        (format!("a {}", fit_pair("全选", "All", 14)), HitTarget::BatchSelectAll),
        (format!("v/esc {}", fit_pair("退出", "Exit", 12)), HitTarget::BatchExit),
    ] {
        let sep = " · ";
        let sep_w = display_width(sep) as u16;
        let label_w = display_width(&label) as u16;
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
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
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

/// Narrowest key column: below this the two-cell indent plus a binding no
/// longer reads as a column, and descriptions are better off full width.
const HELP_KEY_MIN: usize = 10;

/// Widest key column. The catalog contains long alternatives
/// (`J / K / shift+↓ / shift+↑`), and a column sized for the worst case would
/// starve every description on a normal terminal — those few rows are allowed to
/// put their keys on their own line instead (see [`item_lines`]).
const HELP_KEY_MAX: usize = 22;

/// Cells of left indent for a prose note.
const HELP_NOTE_INDENT: usize = 2;

/// Width of the key column, measured from the catalog.
///
/// It must be **measured, not guessed**: a hard-coded 12 was narrower than
/// `g / G / home / end` (18 cells) and `J / K / shift+↓ / shift+↑` (25 cells,
/// Measured with [`display_width`], never counted by hand: eyeballing a binding
/// gets it wrong (this doc first claimed the longest one was 21 cells; it is 25,
/// and every arrow glyph is single width, so the error was pure miscounting, not
/// a width subtlety). Since `pad_to_width` pads but never truncates, those rows
/// ran straight into their description with no gap — `g / G / home /
/// end跳到首行`. Measuring the catalog keeps every row aligned and can never
/// collide with the next column, and the [`HELP_KEY_MAX`] / half-the-row caps
/// keep a single long binding from eating the entire line.
fn help_key_col(width: usize) -> usize {
    if width < 4 {
        return 0;
    }
    let widest = crate::help::SECTIONS
        .iter()
        .flat_map(|s| s.items.iter())
        .filter(|i| !i.is_note())
        .map(|i| display_width(i.keys) + 2)
        .max()
        .unwrap_or(HELP_KEY_MIN);
    let want = widest.clamp(HELP_KEY_MIN, HELP_KEY_MAX);
    // Never let the key column starve the description: at most half the row.
    let cap = (width / 2).max(HELP_KEY_MIN).min(width - 1);
    want.min(cap)
}

fn help_inner_width(area: Rect) -> u16 {
    // `popup_block` pads one cell on each side and draws a border on each side.
    centered_pct(area, 78, 84).width.saturating_sub(4)
}

fn help_inner_height(area: Rect) -> u16 {
    centered_pct(area, 78, 84).height.saturating_sub(2)
}

/// Lay the whole [`crate::help::SECTIONS`] catalog out into the lines the overlay
/// draws, wrapped to `width` cells.
///
/// This is the single source of truth for *both* the renderer and the scroll
/// clamp (`clamp_scrolls`): the maximum scroll offset must be the height of the
/// wrapped result minus the viewport, not the number of catalog entries. Wrapping
/// is width-dependent — a 40-cell popup needs far more rows than a 120-cell one —
/// so counting entries would either lie ("more content below" that is not there)
/// or strand the last rows off screen.
///
/// Row shapes:
///
/// * section header — `中文 · English`, accent + bold;
/// * key row — fixed key column, then `中文 English`; when the pair does not fit
///   on the remainder, the English moves to a continuation row indented past the
///   key column, so both languages are always readable;
/// * note row (`keys` empty) — dim prose, indented.
///
/// A pure function of `(width, palette)` on purpose: it is unit-tested directly,
/// including the invariant that every produced line fits `width`.
fn help_lines(width: usize, p: Palette) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut out: Vec<Line<'static>> = Vec::new();

    for section in crate::help::SECTIONS {
        // Header. `中文 · English` is the section title format the spec fixes.
        let title = format!(" {} · {} ", section.title_zh, section.title_en);
        if !out.is_empty() {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled(
            truncate_to_width(&title, width),
            p.fg(p.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));

        for item in section.items {
            if item.is_note() {
                // Prose: no key column, wrapped as its own paragraph so a long
                // safety rule does not get cut to one row.
                out.extend(note_lines(item.zh, item.en, width, p));
                continue;
            }
            out.extend(item_lines(item.keys, item.zh, item.en, width, p));
        }
    }
    out
}

/// One binding, as one or more rendered rows.
///
/// `key  中文 English` when it fits. When the pair does not fit, the text wraps:
/// Chinese first, English beneath it, both aligned to the description column —
/// never truncated, because a half sentence in either language is the failure
/// this feature exists to avoid.
///
/// Two rules make the column safe, and both were bugs before they were rules:
///
/// * a row whose key would leave **no gap** before the description (because the
///   binding is wider than the measured column) puts the key on its own line and
///   starts the description on the next one, rather than gluing the two together;
/// * the description is always wrapped, never cut, so no language can be lost to
///   a narrow popup.
fn item_lines(keys: &str, zh: &str, en: &str, width: usize, p: Palette) -> Vec<Line<'static>> {
    let key_col = help_key_col(width);
    let text_w = width.saturating_sub(key_col).max(1);
    let key_style = p.fg(p.accent).add_modifier(Modifier::BOLD);
    let zh_style = p.fg(p.text);
    let en_style = p.fg(p.dim);
    let key_text = format!("  {keys}");
    let indent = " ".repeat(key_col);
    let mut rows: Vec<Line<'static>> = Vec::new();

    // The binding must be followed by at least one space before the description.
    if display_width(&key_text) < key_col {
        let key_span = Span::styled(pad_to_width(&key_text, key_col), key_style);
        // Both languages on one row?
        if display_width(zh) + 1 + display_width(en) <= text_w {
            return vec![Line::from(vec![
                key_span,
                Span::styled(zh.to_string(), zh_style),
                Span::styled(" ", en_style),
                Span::styled(en.to_string(), en_style),
            ])];
        }
        let mut first = Some(key_span);
        for part in wrap_text(zh, text_w) {
            let lead = first.take().unwrap_or_else(|| Span::raw(indent.clone()));
            rows.push(Line::from(vec![lead, Span::styled(truncate_to_width(&part, text_w), zh_style)]));
        }
        for part in wrap_text(en, text_w) {
            rows.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(truncate_to_width(&part, text_w), en_style),
            ]));
        }
        return rows;
    }

    // The binding is wider than its column: give it a row of its own so it can
    // never run into the description, then indent the text to the key column.
    rows.push(Line::from(Span::styled(truncate_to_width(&key_text, width), key_style)));
    let indent_n = key_col.min(width.saturating_sub(1));
    let text_w = width.saturating_sub(indent_n).max(1);
    let indent = " ".repeat(indent_n);
    for part in wrap_text(zh, text_w) {
        rows.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(truncate_to_width(&part, text_w), zh_style),
        ]));
    }
    for part in wrap_text(en, text_w) {
        rows.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(truncate_to_width(&part, text_w), en_style),
        ]));
    }
    rows
}

/// A prose note: Chinese, then its English translation beneath it.
fn note_lines(zh: &str, en: &str, width: usize, p: Palette) -> Vec<Line<'static>> {
    let indent = HELP_NOTE_INDENT.min(width);
    let text_w = width.saturating_sub(indent);
    let mut rows: Vec<Line<'static>> = Vec::new();
    for part in wrap_text(zh, text_w) {
        rows.push(Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(truncate_to_width(&part, text_w), p.fg(p.text)),
        ]));
    }
    for part in wrap_text(en, text_w) {
        rows.push(Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(truncate_to_width(&part, text_w), p.fg(p.dim)),
        ]));
    }
    rows
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let p = palette();
    let popup = centered_pct(area, 78, 84);
    // Modals are drawn after the table, so they must record themselves: without
    // this, a click on the dialog would still hit the table row behind it.
    hits.push(popup, HitTarget::Popup);
    // Title and footer both bilingual; the footer is the only place the user
    // learns how to scroll this thing, so it stays truthful to `handle_help_key`
    // (`j/k`, `↑↓`, `pgup/pgdn`, and `? / esc / q / enter` all close).
    let block = popup_block("帮助 · Help", p).title_bottom(Line::from(Span::styled(
        " j/k ↑↓ 滚动 · pgup/pgdn 翻页 · ? esc 关闭 / Scroll · Page · Close ",
        p.fg(p.dim),
    )));
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let lines = help_lines(inner.width as usize, p);
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
    // Both languages in the title: this is the first thing that tells a user
    // which of the two form modes they are in.
    let title = if form.is_modifying() {
        format!("编辑转发 · Edit · {}", form.name.value())
    } else {
        "新建转发 · New tunnel".to_string()
    };
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

    let label_w = form_label_w(inner.width);
    let value_w = inner.width.saturating_sub(form_value_x(inner.width) as u16) as usize;
    let current_idx = FormField::ORDER.iter().position(|f| *f == form.field).unwrap_or(0);

    let lines = form_field_lines(app, form, p, label_w, value_w, current_idx);
    let extra = form_notes(form, p, inner.width as usize);

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
    label_w: usize,
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
        // `名称 Name` on a wide popup, `名称` when the column is narrow: the
        // ladder is [`crate::help::fit`]'s, with the compact `/` join this file
        // uses for table cells.
        let label =
            Span::styled(pad_to_width(&fit_pair(field.label(), field.label_en(), label_w), label_w), label_style);

        if *field == FormField::SshHost {
            lines.push(host_field_line(app, form, p, value_w, editing_here, label));
            continue;
        }
        let value = match field {
            FormField::Kind => Span::styled(
                pad_to_width(
                    &format!("< {} >", fit_pair_spaced(kind.label(), kind.label_en(), value_w.saturating_sub(4))),
                    value_w,
                ),
                if editing_here { p.focused() } else { p.fg(p.accent) },
            ),
            FormField::AutoRestart => {
                let mark = if form.auto_restart { "[x]" } else { "[ ]" };
                let text = fit_pair("网络类失败时自动退避重试", "Retry network failures", value_w.saturating_sub(4));
                Span::styled(pad_to_width(&format!("{mark} {text}"), value_w), value_style)
            }
            FormField::RemoteHost | FormField::RemotePort if kind == ForwardKind::Dynamic => {
                let text = fit_pair("(SOCKS5 不需要)", "(not for SOCKS5)", value_w);
                Span::styled(pad_to_width(&text, value_w), p.fg(p.dim))
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
    let marker_w = display_width(marker);
    let avail = value_w.saturating_sub(marker_w + 1);
    let hint = if !typed.trim().is_empty() {
        String::new()
    } else if !app.hosts.is_empty() {
        fit_pair_spaced(&format!("({}台可选)", app.hosts.len()), &format!("({} available)", app.hosts.len()), avail)
    } else if app.hosts_error.is_some() {
        fit_pair_spaced("(读取失败)", "(read failed)", avail)
    } else {
        fit_pair_spaced("(无主机)", "(no hosts)", avail)
    };
    let shown = truncate_to_width(&format!(" {typed}{hint}"), avail);
    let value_style = if editing_here { p.focused() } else { p.fg(p.text) };
    Line::from(vec![
        label,
        Span::raw(" "),
        Span::styled(pad_to_width(&shown, avail), value_style),
        Span::styled(marker, drop_style.add_modifier(Modifier::BOLD)),
    ])
}

/// Validation errors, the auto-suggested port note, and the key hint.
///
/// The hint line is the form's own legend: both stages are spelled out, because
/// `enter`/`esc` mean different things in them and a table can only show one.
/// `ctrl+s`/`f10` save from either stage (the handler treats them identically),
/// so the hint does not have to change with it.
fn form_notes(form: &crate::app::FormState, p: Palette, width: usize) -> Vec<Line<'static>> {
    let mut extra = Vec::new();
    for err in form.errors.iter().take(3) {
        extra.push(Line::from(Span::styled(truncate_to_width(&format!("✗ {err}"), width), p.fg(p.failed))));
    }
    if let Some(note) = &form.local_port_note {
        // The auto-derived local port is a *suggestion*; say so plainly.
        let text = format!("· {}（可编辑） / (editable)", note);
        extra.push(Line::from(Span::styled(truncate_to_width(&text, width), p.fg(p.pending))));
    }
    if let Some(hint) = &form.port_hint {
        // The Chinese keyword check is deliberate: `port_hint` is built in
        // `src/app/form.rs` and the Chinese prefix is part of that contract.
        let style = if hint.contains("占用") { p.fg(p.pending) } else { p.fg(p.up) };
        extra.push(Line::from(Span::styled(truncate_to_width(&format!("· {hint}"), width), style)));
    }
    extra.push(Line::from(""));
    // The legend flows like the main footer instead of being one long string:
    // the form popup is only ~70 cells wide, and a single line would be cut
    // mid-token (`esc 取…`), which is worse than useless for the keys that teach
    // the two-stage editing model. Flowing drops a whole hint instead, and on a
    // normal terminal every hint fits in both languages.
    let hints: &[Hint] = if form.is_editing_field() {
        // Say which stage the form is in, in words: the highlight colour is the
        // other cue, and colour alone is not allowed to carry meaning.
        extra.push(Line::from(Span::styled("编辑中 / Editing:", p.fg(p.pending))));
        &[
            ("字符", "输入", "Type"),
            ("enter/esc", "结束编辑", "Done"),
            ("tab", "下一字段", "Next"),
            ("ctrl+s", "保存", "Save"),
        ]
    } else {
        &[
            ("↑↓", "选字段", "Field"),
            ("enter", "编辑", "Edit"),
            ("tab", "切换", "Next"),
            ("ctrl+s", "保存", "Save"),
            ("f10", "保存", "Save"),
            ("esc", "取消", "Cancel"),
        ]
    };
    extra.extend(flow_hints(hints, width, 3, p));
    extra
}

/// Show a text cursor only while a field is actively being edited.
fn place_form_cursor(frame: &mut Frame, form: &crate::app::FormState, inner: Rect, popup: Rect, current_idx: usize) {
    if !form.is_editing_field() {
        return;
    }
    let Some(buf) = form_field_buf(form, form.field) else { return };
    let prefix: String = buf.text.chars().take(buf.cursor).collect();
    let x = inner.x + form_value_x(inner.width) as u16 + display_width(&prefix) as u16;
    let y = inner.y + current_idx as u16;
    if x < popup.right().saturating_sub(1) && y < inner.bottom() {
        frame.set_cursor_position((x, y));
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
            Line::from(Span::styled(
                " 从 ~/.ssh/config 选择主机 / Pick host ",
                p.fg(p.accent).add_modifier(Modifier::BOLD),
            )),
        );
    let body = block.inner(area);
    frame.render_widget(block, area);
    if body.height == 0 {
        return;
    }

    let width = body.width as usize;
    let list_h = body.height.saturating_sub(2) as usize;

    if app.hosts.is_empty() {
        let msg = app
            .hosts_error
            .clone()
            .unwrap_or_else(|| "未在 ~/.ssh/config 中找到主机 / No hosts in ~/.ssh/config".to_string());
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncate_to_width(&msg, width), p.fg(p.failed)))),
            Rect { height: 1, ..body },
        );
        return;
    }

    // Row 0: filter echo.
    let filter = if picker.query.is_empty() {
        Span::styled(fit_pair("输入以过滤", "Type to filter", width.saturating_sub(10)), p.fg(p.dim))
    } else {
        Span::styled(truncate_to_width(&picker.query, width.saturating_sub(10)), p.fg(p.text))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("过滤/Filter: ", p.fg(p.dim)), filter])),
        Rect { y: body.y, height: 1, ..body },
    );

    let list_area = Rect { y: body.y + 1, height: list_h as u16, ..body };
    if picker.matches.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("没有匹配的主机 / No matching host", p.fg(p.dim)))),
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
            truncate_to_width(
                "↑↓ k/j 选择 · enter 确定 · esc 收起 · tab 下一字段 / Select · Confirm · Close · Next",
                width,
            ),
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
    let min = 40u16.min(area.width);
    let max = 90u16.min(area.width);
    let wanted = (display_width(prompt) as u16 + 8).clamp(min, max.max(min));
    // Fit the dialog to its content: prompt may wrap onto two lines, plus a
    // blank line and the key row, plus the two border rows.
    let prompt_rows = display_width(prompt).div_ceil(wanted.saturating_sub(4) as usize) as u16;
    let popup = centered(area, wanted, (prompt_rows.max(1) + 4).min(area.height));
    // The riskiest click-through to allow: the dialog asks before a destructive
    // action, so a click meant for the dialog must never reach a table row.
    hits.push(popup, HitTarget::Popup);
    let block = popup_block("确认 · Confirm", p);
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 {
        return;
    }

    // `y`/`Y`/`enter` accept and `n`/`N`/`esc` cancel in the real handler; both
    // spellings are advertised so nobody has to guess whether shift matters.
    let lines = vec![
        Line::from(vec![
            Span::styled("⚠ ", p.fg(p.failed).add_modifier(Modifier::BOLD)),
            Span::styled(truncate_to_width(prompt, inner.width.saturating_sub(3) as usize), p.fg(p.text)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" y/Y / enter ", p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("确认 / Confirm    ", p.fg(p.text)),
            Span::styled("n/N / esc ", p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("取消 / Cancel", p.fg(p.text)),
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
    if v { "是/Yes" } else { "否/No" }
}

/// Build the scrollable body of the detail overlay: fields, the full failure
/// text (never truncated), the equivalent ssh command and the log tail.
fn detail_lines(view: &TunnelView, id: TunnelId, p: Palette) -> Vec<Line<'static>> {
    let cfg = &view.cfg;
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 26 = widest bilingual label (`启用/自动重连 Enabled/Auto-retry` = 24 cells)
    // rounded up, so every value in the detail panel starts at the same column.
    let kv = |k: &str, v: String| {
        Line::from(vec![Span::styled(pad_to_width(&format!("{k}:"), 27), p.fg(p.dim)), Span::styled(v, p.fg(p.text))])
    };

    lines.push(kv("名称/Name", cfg.name.clone()));
    lines.push(kv("ID", id.to_string()));
    lines.push(kv("类型/Kind", fit_pair(cfg.kind.label(), cfg.kind.label_en(), 40)));
    lines.push(kv("SSH 主机/Host", cfg.ssh_host.clone()));
    lines.push(kv("本地/Local", cfg.local_display()));
    lines.push(kv("远端/Remote", cfg.remote_display()));
    lines.push(kv("转发参数/Spec", cfg.forward_spec()));
    lines.push(kv(
        "启用/自动重连 Enabled/Auto-retry",
        format!("{} / {}", yes_no(cfg.enabled), yes_no(cfg.auto_restart)),
    ));
    lines.push(kv("创建时间/Created", fmt_unix(cfg.created_at)));
    lines.push(kv(
        "状态/Status",
        format!("{}（重试次数 {} / retries {}）", view.status.key(), view.retries, view.retries),
    ));
    if !cfg.note.is_empty() {
        lines.push(kv("备注/Note", cfg.note.clone()));
    }
    if !cfg.ssh_options.is_empty() {
        lines.push(kv("ssh 选项/Options", cfg.ssh_options.join(" ")));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "失败原因（原始 stderr，完整保留）/ Failure (raw stderr, kept verbatim)",
        p.fg(p.dim).add_modifier(Modifier::BOLD),
    )));
    match view.status.failure() {
        Some(reason) => {
            lines.push(Line::from(Span::styled(
                format!("  {} / {}", reason.short, reason.kind.short_en()),
                p.fg(p.failed).add_modifier(Modifier::BOLD),
            )));
            for raw in reason.detail.lines() {
                lines.push(Line::from(Span::styled(format!("  {raw}"), p.fg(p.text))));
            }
        }
        None => lines.push(Line::from(Span::styled("  无 / None", p.fg(p.dim)))),
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "等价 ssh 命令（可复制）/ Equivalent ssh command (copyable)",
        p.fg(p.dim).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(format!("  {}", tunnel_command(cfg)), p.fg(p.accent))));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("日志（最近 {DETAIL_LOG_TAIL} 行）/ Log (last {DETAIL_LOG_TAIL} lines)"),
        p.fg(p.dim).add_modifier(Modifier::BOLD),
    )));
    let tail = view.log.tail(DETAIL_LOG_TAIL);
    if tail.is_empty() {
        lines.push(Line::from(Span::styled("  暂无日志 / No log yet", p.fg(p.dim))));
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
    let title = format!("详情 · Detail · {} · j/k ↑↓ 滚动 · q/esc/enter/l 返回", view.cfg.name);
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

    // Both languages where they fit, Chinese only on a narrow terminal: the
    // affordances on this line are clickable, so they must never be clipped into
    // a half-word that no longer says what it does.
    let prefix = fit_pair_spaced(" 过滤:", " Filter:", row.width as usize / 3);
    let prefix = format!("{prefix} ");
    let prefix_w = display_width(&prefix) as u16;
    let clear = format!("  × {} ", fit_pair("清除", "Clear", 12));
    let clear_w = display_width(&clear) as u16;
    // Keep the clear affordance on screen even when the typed text is long: it
    // sits on the right, the input echoes in whatever is left of it.
    let hint = format!("   {}", fit_pair_spaced("enter 应用 esc 清除", "enter apply · esc clear", 30));
    let hint_w = display_width(&hint) as u16;
    let value_w = row.width.saturating_sub(prefix_w + clear_w + hint_w);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prefix, p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled(truncate_to_width(&app.filter, value_w as usize), p.fg(p.text)),
            Span::styled(truncate_to_width(&clear, clear_w as usize), p.fg(p.accent)),
            Span::styled(truncate_to_width(&hint, hint_w as usize), p.fg(p.dim)),
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

    let x = row.x + prefix_w + display_width(&app.filter) as u16;
    if x < row.x + bar_w {
        frame.set_cursor_position((x, row.y));
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

    // -- help overlay: rendering completeness ------------------------------

    /// Flatten help lines to text, one row per line.
    fn help_text(width: usize) -> String {
        help_lines(width, palette_with(false)).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn help_overlay_keeps_both_languages_of_every_item() {
        // The rendering counterpart of `tests/help_coverage.rs`: that test proves
        // every dispatched key is *documented*, this one proves every documented
        // row actually *reaches the screen* in both languages. The bug it was
        // written for: a binding wider than the key column ran into its own
        // description (`g / G / home / end跳到首行`), and a squeezed description
        // could lose the English half entirely.
        //
        // Only the first few cells of each half are compared: wrapping splits
        // phrases across rows, so a whole-string `contains` would fail on a
        // correct render. The prefixes are what a reader needs to recognise the
        // row.
        // Whitespace is normalised away on both sides: wrapping legitimately
        // moves a space to a line break (`等同 q…` -> `等同` / `q…`), and this
        // test is about *content*, not spacing — the no-glue rule has its own
        // test below.
        let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        for width in [40usize, 60, 78, 110] {
            let text = squash(&help_text(width));
            for section in crate::help::SECTIONS {
                // Section titles are bilingual too.
                assert!(
                    text.contains(&squash(section.title_zh)),
                    "width {width}: section title {:?} missing",
                    section.title_zh
                );
                assert!(
                    text.contains(&squash(section.title_en)),
                    "width {width}: section title {:?} missing",
                    section.title_en
                );
                for item in section.items {
                    let zh_prefix: String = item.zh.chars().take(4).collect();
                    let en_prefix: String = item.en.chars().take(6).collect();
                    assert!(
                        text.contains(&squash(&zh_prefix)),
                        "width {width}: {} item {:?} lost its Chinese ({zh_prefix:?})",
                        section.id.key(),
                        item.keys
                    );
                    assert!(
                        text.contains(&squash(&en_prefix)),
                        "width {width}: {} item {:?} lost its English ({en_prefix:?})",
                        section.id.key(),
                        item.keys
                    );
                }
            }
        }
    }

    #[test]
    fn help_overlay_never_glues_a_binding_to_its_description() {
        // Regression: `pad_to_width` pads but never truncates, so a key column
        // narrower than the binding produced `g / G / home / end跳到首行` with no
        // separating space at all.
        for width in [40usize, 60, 78, 110] {
            for line in help_lines(width, palette_with(false)) {
                let text = line.to_string();
                // No CJK may directly follow an ASCII word character anywhere in
                // the key column region: that is exactly the glue signature.
                let glued = text.chars().collect::<Vec<_>>().windows(2).any(|w| {
                    let (a, b) = (w[0], w[1]);
                    a.is_ascii_alphanumeric() && (('\u{4e00}'..='\u{9fff}').contains(&b))
                });
                assert!(!glued, "width {width}: binding glued to description: {text:?}");
            }
        }
    }

    #[test]
    fn help_lines_fit_the_width_at_every_popup_size() {
        // Every produced row must fit, or the overlay bleeds past its border.
        for width in [8usize, 12, 20, 24, 30, 40, 50, 60, 78, 110, 200] {
            for line in help_lines(width, palette_with(false)) {
                let w = display_width(&line.to_string());
                assert!(w <= width, "width {width}: row is {w} cells: {line:?}");
            }
        }
    }

    #[test]
    fn help_key_column_is_measured_from_the_catalog() {
        // Long alternatives exist in the catalog, so a fixed 12-cell column was
        // structurally too narrow; it must be derived from the data.
        let widest = crate::help::SECTIONS
            .iter()
            .flat_map(|s| s.items.iter())
            .filter(|i| !i.is_note())
            .map(|i| display_width(i.keys) + 2)
            .max()
            .unwrap();
        assert!(
            help_key_col(120) >= widest.min(HELP_KEY_MAX),
            "key column {} does not fit the widest binding {widest}",
            help_key_col(120)
        );
        // ... but it may never eat the line on a narrow popup.
        for width in [10usize, 16, 24, 40] {
            assert!(help_key_col(width) <= width / 2 + HELP_KEY_MIN, "width {width} key column too wide");
        }
    }

    #[test]
    fn help_scroll_limit_is_measured_from_wrapped_rows() {
        // The clamp must count wrapped rows, not catalog entries: wrapping makes
        // the row count width-dependent, and a clamp that used `item_count()`
        // would either stop short of the end or report content that is not there.
        let p = palette_with(false);
        let narrow = help_lines(60, p).len();
        let wide = help_lines(160, p).len();
        assert!(narrow > wide, "narrow render must wrap to more rows ({narrow} vs {wide})");
        assert!(wide >= crate::help::item_count(), "eleven rows per section cannot be fewer than the items");

        let mut app = app_with_tunnels(vec![]);
        app.open_help();
        // Tall enough that the whole catalog fits: there must be nothing to scroll.
        let backend = TestBackend::new(200, 70);
        let mut term = Terminal::new(backend).unwrap();
        let _ = term.draw(|f| draw(f, &mut app));
        assert_eq!(app.help_scroll, 0);

        // Short terminal: the clamp equals rows minus viewport, not a guess.
        app.help_scroll = 9999;
        let _ = render_to_text(&mut app, 120, 35);
        let expected = help_lines(help_inner_width(Rect::new(0, 0, 120, 35)) as usize, palette())
            .len()
            .saturating_sub(help_inner_height(Rect::new(0, 0, 120, 35)) as usize);
        assert_eq!(app.help_scroll as usize, expected, "clamp must be rows - viewport");
        assert!(app.help_scroll > 0, "the catalog is longer than a 35-row terminal");
    }

    #[test]
    fn renders_empty_state() {
        let mut app = app_with_tunnels(vec![]);
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("sshpf"), "title bar missing:\n{text}");
        assert!(text.contains("cfg:"), "config path missing:\n{text}");
        assert!(text.contains("还没有转发条目"), "empty hint missing:\n{text}");
        assert!(text.contains("No tunnels yet"), "empty hint needs its English half:\n{text}");
        assert!(text.contains("新建"), "key hint missing:\n{text}");
        // Counters state every status in both languages on a wide terminal...
        assert!(text.contains("0 已连通/Up"), "counters missing:\n{text}");
        assert!(text.contains("0 未启用/Off"), "counters missing:\n{text}");
        // ...and degrade to glyph + number when narrow, never to a clipped word.
        let narrow = render_to_text(&mut app, 70, 20);
        assert!(narrow.contains("● 0"), "compact counters missing:\n{narrow}");
        assert!(!narrow.contains("Starting"), "narrow counters must drop the words:\n{narrow}");
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

        // Navigating: arrows pick a field, enter starts editing. The legend names
        // both stages' keys, and it must not claim to be editing.
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("enter"), "navigating hint missing:\n{text}");
        assert!(text.contains("编辑"), "navigating hint must mention editing:\n{text}");
        assert!(text.contains("ctrl+s"), "save key must be advertised:\n{text}");
        assert!(!text.contains("编辑中"), "must not look like it is editing:\n{text}");

        // Editing: an explicit worded stage marker appears (never colour alone),
        // and the legend switches to the editing-stage keys.
        app.form.as_mut().unwrap().field = FormField::Name;
        app.form.as_mut().unwrap().begin_edit();
        let text = render_to_text(&mut app, 120, 40);
        assert!(text.contains("编辑中"), "editing hint missing:\n{text}");
        assert!(text.contains("Editing"), "editing hint needs its English:\n{text}");
        assert!(text.contains("结束编辑"), "exit-editing hint missing:\n{text}");
        assert!(text.contains("enter/esc"), "the exit-editing binding must be spelled out:\n{text}");
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
        assert!(top.contains("帮助 · Help"), "bilingual help title missing:\n{top}");
        // The overlay is sectioned now, and each section title is bilingual.
        assert!(top.contains("主界面 · Main screen"), "first section header missing:\n{top}");
        // The first rows of that section, in both languages on the same row.
        assert!(top.contains("向下移动光标"), "first help row missing:\n{top}");
        assert!(top.contains("Move the cursor down"), "first help row needs its English:\n{top}");
        // The footer teaches this overlay's own keys, also bilingual.
        assert!(top.contains("滚动"), "scroll hint missing:\n{top}");
        assert!(top.contains("Scroll"), "scroll hint needs its English:\n{top}");

        // Scrolling hides the top of the catalog and lets later content in. The
        // offset is clamped against the *wrapped* row count, not the item count.
        let first_row = "Move the cursor down";
        assert!(top.contains(first_row), "precondition: first row is visible before scrolling");
        app.help_scroll = 40;
        let scrolled = render_to_text(&mut app, 120, 35);
        assert!(app.help_scroll > 0, "help_scroll must survive the clamp");
        assert!(!scrolled.contains(first_row), "scroll had no effect:\n{scrolled}");
        assert_ne!(top, scrolled, "scrolling must change what is on screen");

        // And the clamp is exact: rows minus viewport, both measured by the same
        // function the renderer uses.
        app.help_scroll = u16::MAX;
        let _ = render_to_text(&mut app, 120, 35);
        let area = Rect::new(0, 0, 120, 35);
        let expected = help_lines(help_inner_width(area) as usize, palette())
            .len()
            .saturating_sub(help_inner_height(area) as usize);
        assert_eq!(app.help_scroll as usize, expected, "clamp must be wrapped rows - viewport");
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
        // The batch keys are advertised as the concrete bindings, not as a vague
        // "batch operations" label: the mode is where they actually work.
        assert!(selecting.contains("s/x/R/D"), "selection hints must list the batch keys:\n{selecting}");
        assert!(selecting.contains("批量"), "selection hints must say what those keys do:\n{selecting}");
        assert!(selecting.contains("已选"), "the marked count must be on screen:\n{selecting}");
        // Selection mode must not advertise the normal-mode-only bindings. (The
        // action bar legitimately offers `R 重启`, the *batch* restart — what
        // must not appear is the single-row `n 新建` and the `r` spelling that
        // would suggest lowercase still restarts one tunnel.)
        assert!(!selecting.contains("新建/New"), "n is unbound in selection mode:\n{selecting}");
        assert!(!selecting.contains("r 重启"), "lowercase r is unbound in selection mode:\n{selecting}");
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

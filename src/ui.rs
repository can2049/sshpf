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
    FormField, ForwardKind, HostPicker, LogLevel, LogLine, TunnelConfig, TunnelId, TunnelStatus, TunnelView, now_hms,
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

pub mod layout;

use crate::text::{display_width, pad_to_width, truncate_to_width};
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

    // --- immutable phase: render ------------------------------------------
    let app: &App = app;

    let rows = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Length(1), // counters
        Constraint::Min(1),    // table
        Constraint::Length(1), // message bar
        Constraint::Length(2), // key hints
    ])
    .split(area);

    draw_title(frame, app, rows[0]);
    draw_counters(frame, app, rows[1]);
    draw_table(frame, app, rows[2]);
    draw_message(frame, app, rows[3]);
    draw_keys(frame, rows[4]);

    match &app.overlay {
        Overlay::None => {}
        Overlay::Help => draw_help(frame, app, area),
        Overlay::Form => draw_form(frame, app, area),
        Overlay::Confirm { prompt, .. } => draw_confirm(frame, prompt, area),
        Overlay::Detail(id) => draw_detail(frame, app, *id, area),
        Overlay::Filter => draw_filter(frame, app, area),
    }
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

    let right_w = if app.filter.is_empty() { 0 } else { (display_width(&app.filter) as u16 + 10).min(area.width / 2) };
    let chunks = Layout::horizontal([Constraint::Min(0), Constraint::Length(right_w)]).split(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), chunks[0]);

    if right_w > 0 {
        let text = truncate_to_width(&format!("filter: {} ", app.filter), chunks[1].width as usize);
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
        let text = truncate_to_width(&format!(" ✗ {}   (enter 查看详情)", reason.detail), area.width as usize);
        Line::from(Span::styled(text, p.fg(p.failed)))
    } else {
        Line::from(Span::styled(" 就绪 · n 新建转发 · ? 帮助 · q 退出", p.fg(p.dim)))
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Two lines of key hints, exactly matching the keys `main` dispatches.
fn draw_keys(frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let lines = vec![
        key_line(
            &[
                ("j/k ↑↓", "移动"),
                ("space", "启停"),
                ("r", "重启"),
                ("n", "新建"),
                ("e", "编辑"),
                ("d", "删除"),
                ("enter", "详情"),
                ("l", "日志"),
            ],
            p,
        ),
        key_line(
            &[
                ("/", "过滤"),
                ("a", "全启"),
                ("x", "全停"),
                ("g/G", "首尾"),
                ("?", "帮助"),
                ("esc", "关闭弹窗"),
                ("q", "退出"),
            ],
            p,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn key_line(items: &[(&str, &str)], p: Palette) -> Line<'static> {
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

fn cell(text: impl Into<String>, style: Style) -> Cell<'static> {
    Cell::new(Text::from(Line::from(text.into()))).style(style)
}

fn draw_table(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let layout = column_layout(area.width);

    let header_style = p.fg(p.dim).add_modifier(Modifier::BOLD);
    let headers: Vec<Cell<'static>> = layout
        .iter()
        .map(|(col, w)| {
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
        })
        .collect();
    let header = Row::new(headers);

    let visible = app.visible_indices();
    let mut rows: Vec<Row<'static>> = Vec::with_capacity(visible.len().max(1));
    for idx in &visible {
        if let Some(view) = app.tunnels.get(*idx) {
            rows.push(table_row(view, &layout, p));
        }
    }

    if rows.is_empty() {
        let text = if app.tunnels.is_empty() {
            "还没有转发条目，按 n 新建"
        } else {
            "没有匹配的条目，按 esc 清除过滤"
        };
        rows.push(Row::new(vec![cell(text, p.fg(p.dim)).column_span(layout.len().max(1) as u16)]));
    }

    let widths: Vec<Constraint> = layout.iter().map(|(_, w)| Constraint::Length(*w)).collect();
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
}

fn table_row(view: &TunnelView, layout: &[(Col, u16)], p: Palette) -> Row<'static> {
    let cfg = &view.cfg;
    let mut cells: Vec<Cell<'static>> = Vec::with_capacity(layout.len());
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
    if matches!(view.status, TunnelStatus::Stopped) { row.style(p.fg(p.dim)) } else { row }
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
    ("enter/↓", "在 SSH 主机字段展开 ~/.ssh/config 下拉"),
    ("enter", "校验并保存（保存后立即连接）"),
    ("esc", "取消编辑（有未保存内容会确认）"),
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
];

fn help_line_count() -> usize {
    HELP_ITEMS.len()
}

fn help_inner_height(area: Rect) -> u16 {
    centered_pct(area, 78, 84).height.saturating_sub(2)
}

fn draw_help(frame: &mut Frame, app: &App, area: Rect) {
    let p = palette();
    let popup = centered_pct(area, 78, 84);
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

fn draw_form(frame: &mut Frame, app: &App, area: Rect) {
    let Some(form) = app.form.as_ref() else { return };
    let p = palette();
    let title =
        if form.is_modifying() { format!("编辑转发 · {}", form.name.value()) } else { "新建转发".to_string() };
    // Tall enough to hold the fields *and* the expanded host dropdown.
    // Rows: title border (2) + one line per field + errors/port hint (2)
    //       + blank (1) + key hint (1) + dropdown frame (2) + dropdown rows.
    let fields_h = FormField::ORDER.len() as u16;
    let wanted_h = fields_h + 4 + 2 + crate::app::HOST_DROPDOWN_ROWS as u16 + 2;
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
    let extra = form_notes(form, p, inner.width as usize);

    let used = lines.len() as u16;
    if inner.height > used {
        let rest = Rect { x: inner.x, y: inner.y + used, width: inner.width, height: inner.height - used };
        frame.render_widget(Paragraph::new(extra), rest);
    }
    frame.render_widget(Paragraph::new(lines), inner);

    // Expand the dropdown *inside* the form, anchored under the SSH HOST row.
    if let Some(picker) = app.picker.as_ref().filter(|pk| pk.open) {
        draw_host_dropdown(frame, app, picker, inner, current_idx as u16);
    }

    place_form_cursor(frame, form, inner, popup, current_idx);
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
    let x = inner.x + FORM_VALUE_X as u16 + display_width(&prefix) as u16;
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

fn draw_confirm(frame: &mut Frame, prompt: &str, area: Rect) {
    let p = palette();
    let min = 40u16.min(area.width);
    let max = 90u16.min(area.width);
    let wanted = (display_width(prompt) as u16 + 8).clamp(min, max.max(min));
    // Fit the dialog to its content: prompt may wrap onto two lines, plus a
    // blank line and the key row, plus the two border rows.
    let prompt_rows = display_width(prompt).div_ceil(wanted.saturating_sub(4) as usize) as u16;
    let popup = centered(area, wanted, (prompt_rows.max(1) + 4).min(area.height));
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

fn draw_detail(frame: &mut Frame, app: &App, id: TunnelId, area: Rect) {
    let p = palette();
    let Some(view) = app.tunnel(id) else { return };
    let popup = centered_pct(area, 88, 86);
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

fn draw_filter(frame: &mut Frame, app: &App, area: Rect) {
    let p = palette();
    // Occupies the very last screen row: a one-line, non-modal input.
    let row = Rect { x: area.x, y: area.bottom().saturating_sub(1), width: area.width, height: 1 };
    if row.height == 0 || row.width == 0 {
        return;
    }
    frame.render_widget(Clear, row);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" 过滤: ", p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled(truncate_to_width(&app.filter, row.width.saturating_sub(24) as usize), p.fg(p.text)),
            Span::styled("    enter 应用   esc 清除", p.fg(p.dim)),
        ]))
        .style(p.bg(p.selection_bg)),
        row,
    );
    let x = row.x + display_width(" 过滤: ") as u16 + display_width(&app.filter) as u16;
    if x < row.right() {
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
    use crate::model::{FailureInfo, FailureKind, InputBuf, TunnelStatus};
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
                proxy_jump: None,
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
                proxy_jump: None,
                identity_file: None,
            },
            crate::ssh::SshHost {
                alias: "dev-box".into(),
                hostname: "192.168.1.7".into(),
                user: None,
                port: 2222,
                proxy_jump: None,
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
}

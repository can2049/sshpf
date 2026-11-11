//! The detail/log overlay: everything about one tunnel, scrollable.
//!
//! Two things deliberately do not degrade here: the **full failure text** is kept
//! verbatim (a classified short phrase is what the table shows; this panel is
//! where the unrestricted text lives), and the equivalent `ssh` command is
//! printed in full so it can be copied. The log tail is the tunnel's own ring
//! buffer, not the global log file.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::App;
use crate::model::{ForwardKind, LogLevel, LogLine, TunnelConfig, TunnelId, TunnelView};
use crate::text::pad_to_width;
use crate::ui::frame::{centered_pct, clear_and_block, popup_block};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::layout::DETAIL_LOG_TAIL;
use crate::ui::palette::{Palette, fmt_unix, palette};
use crate::ui::text::{fit_pair, pair};

// ---------------------------------------------------------------------------
// Overlay: Detail
// ---------------------------------------------------------------------------

/// The equivalent ssh command line, shown in the detail panel for copy/paste.
pub(crate) fn tunnel_command(cfg: &TunnelConfig) -> String {
    let flag = match cfg.kind {
        ForwardKind::Local => "-L",
        ForwardKind::Remote => "-R",
        ForwardKind::Dynamic => "-D",
    };
    format!("ssh -N {} {} {}", flag, cfg.forward_spec(), cfg.ssh_host)
}

pub(crate) fn level_tag(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "DBG",
        LogLevel::Info => "INF",
        LogLevel::Warn => "WRN",
        LogLevel::Error => "ERR",
    }
}

pub(crate) fn level_color(level: LogLevel, p: Palette) -> Color {
    match level {
        LogLevel::Debug => p.dim,
        LogLevel::Info => p.text,
        LogLevel::Warn => p.pending,
        LogLevel::Error => p.failed,
    }
}

pub(crate) fn detail_inner_height(area: Rect) -> u16 {
    centered_pct(area, 88, 86).height.saturating_sub(2)
}

pub(crate) fn yes_no(v: bool) -> &'static str {
    if v { "是/Yes" } else { "否/No" }
}

/// Build the scrollable body of the detail overlay: fields, the full failure
/// text (never truncated), the equivalent ssh command and the log tail.
pub(crate) fn detail_lines(view: &TunnelView, id: TunnelId, p: Palette) -> Vec<Line<'static>> {
    let cfg = &view.cfg;
    let mut lines = field_lines(view, id, p);
    lines.push(Line::from(""));
    lines.extend(failure_lines(view, p));
    lines.push(Line::from(""));
    lines.extend(command_lines(cfg, p));
    lines.push(Line::from(""));
    lines.extend(log_lines(view, p));
    lines
}

/// `label: value` with the value column lined up across the whole panel.
fn kv(k: &str, v: String, p: Palette) -> Line<'static> {
    // 26 = widest bilingual label (`启用/自动重连 Enabled/Auto-retry` = 24 cells)
    // rounded up, so every value in the detail panel starts at the same column.
    Line::from(vec![Span::styled(pad_to_width(&format!("{k}:"), 27), p.fg(p.dim)), Span::styled(v, p.fg(p.text))])
}

/// The tunnel's own fields.
fn field_lines(view: &TunnelView, id: TunnelId, p: Palette) -> Vec<Line<'static>> {
    let cfg = &view.cfg;
    let mut lines = vec![
        kv("名称/Name", cfg.name.clone(), p),
        kv("ID", id.to_string(), p),
        kv("类型/Kind", fit_pair(cfg.kind.label(), cfg.kind.label_en(), 40), p),
        kv("SSH 主机/Host", cfg.ssh_host.clone(), p),
        kv("本地/Local", cfg.local_display(), p),
        kv("远端/Remote", cfg.remote_display(), p),
        kv("转发参数/Spec", cfg.forward_spec(), p),
        kv("启用/自动重连 Enabled/Auto-retry", pair(yes_no(cfg.enabled), yes_no(cfg.auto_restart)), p),
        kv("创建时间/Created", fmt_unix(cfg.created_at), p),
        kv("状态/Status", format!("{}（重试次数 {} / retries {}）", view.status.key(), view.retries, view.retries), p),
    ];
    if !cfg.note.is_empty() {
        lines.push(kv("备注/Note", cfg.note.clone(), p));
    }
    if !cfg.ssh_options.is_empty() {
        lines.push(kv("ssh 选项/Options", cfg.ssh_options.join(" "), p));
    }
    lines
}

/// The failure section: the classified phrase, then the full error text
/// **verbatim**.
///
/// This is the one place the unrestricted text is shown — the table has room for
/// a short phrase only — so it is never wrapped or truncated here.
fn failure_lines(view: &TunnelView, p: Palette) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "失败原因（原始 stderr，完整保留）/ Failure (raw stderr, kept verbatim)",
        p.fg(p.dim).add_modifier(Modifier::BOLD),
    ))];
    match view.status.failure() {
        Some(reason) => {
            lines.push(Line::from(Span::styled(
                format!("  {}", pair(&reason.short, reason.kind.short_en())),
                p.fg(p.failed).add_modifier(Modifier::BOLD),
            )));
            for raw in reason.detail.lines() {
                lines.push(Line::from(Span::styled(format!("  {raw}"), p.fg(p.text))));
            }
        }
        None => lines.push(Line::from(Span::styled("  无 / None", p.fg(p.dim)))),
    }
    lines
}

/// The copy-pasteable equivalent of what the tunnel runs.
fn command_lines(cfg: &TunnelConfig, p: Palette) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "等价 ssh 命令（可复制）/ Equivalent ssh command (copyable)",
            p.fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(format!("  {}", tunnel_command(cfg)), p.fg(p.accent))),
    ]
}

/// The tail of the tunnel's own log ring.
fn log_lines(view: &TunnelView, p: Palette) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!("日志（最近 {DETAIL_LOG_TAIL} 行）/ Log (last {DETAIL_LOG_TAIL} lines)"),
        p.fg(p.dim).add_modifier(Modifier::BOLD),
    ))];
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

pub(crate) fn draw_detail(frame: &mut Frame, app: &App, id: TunnelId, area: Rect, hits: &mut HitMap) {
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FailureInfo, FailureKind, TunnelStatus};

    use crate::ui::test_support::*;

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
}

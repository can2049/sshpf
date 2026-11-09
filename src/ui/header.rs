//! The main screen's chrome: the title bar, the status counters and the message
//! bar.
//!
//! Three one-line bands, drawn above the table on every frame:
//!
//! * [`draw_title`] — `sshpf <version> · ssh-config N hosts · cfg PATH`, with the
//!   clock pinned right;
//! * [`draw_counters`] — one glyph + count per status, bilingual while the
//!   terminal is wide enough and glyph-only when it is not;
//! * [`draw_message`] — the latest `App::status`, else the selected tunnel's
//!   failure reason, else an idle hint.
//!
//! All three degrade by *dropping a clause* rather than by clipping a word: a
//! half-rendered label reads as a bug.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::config;
use crate::help;
use crate::model::{LogLevel, now_hms};
use crate::text::{display_width, truncate_to_width};
use crate::ui::frame::band;
use crate::ui::palette::palette;
use crate::ui::text::{fit_pair_spaced, warn_once};

// ---------------------------------------------------------------------------
// Chrome: title / counters / message / keys
// ---------------------------------------------------------------------------

pub(crate) fn draw_title(frame: &mut Frame, app: &App, area: Rect) {
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
    // Which process owns the state, and how many windows are sharing it. This is
    // not decoration: closing the last client stops every forward, so the user
    // has to be able to see that they are one of several — or the only one.
    let session = session_label(app);
    // Progressive disclosure: the absolute config path is the first thing to go
    // when it would collide with the clock, then the host count.
    let left = if area.width >= 84 {
        format!(
            " sshpf {}  ·  {}  ·  {}{session}  ·  cfg: {} ",
            env!("CARGO_PKG_VERSION"),
            ssh_hosts,
            "",
            config::display_path(&app.paths.config),
        )
    } else if area.width >= 46 {
        format!(" sshpf {}  ·  {}  ·  {session}", env!("CARGO_PKG_VERSION"), ssh_hosts)
    } else {
        format!(" sshpf {}  ·  {session}", env!("CARGO_PKG_VERSION"))
    };
    let left = truncate_to_width(&left, left_area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(left, p.fg(p.text).add_modifier(Modifier::BOLD)))),
        left_area,
    );

    let clock = truncate_to_width(&now_hms(), clock_area.width as usize);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(clock, p.fg(p.dim)))), clock_area);
}

/// `server` when this process owns the tunnels, `client 2/3` when it is one of
/// several windows on a shared server.
///
/// The count is the server's, not a local guess, so two windows always agree
/// about how many are open. A client whose link has dropped says so here too:
/// a table that quietly stops updating is worse than one that admits it is stale.
fn session_label(app: &App) -> String {
    if !app.is_mirror() {
        return help::bi("服务端/server", "server");
    }
    match app.conn_state() {
        Some(crate::app::remote::ConnState::Lost(_)) => help::bi("已断开/detached", "detached"),
        _ => {
            let n = app.remote_client_count().unwrap_or(1);
            help::bi(&format!("客户端 {n}/client {n}"), &format!("client {n}"))
        }
    }
}

pub(crate) fn draw_counters(frame: &mut Frame, app: &App, area: Rect) {
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

    // The filter is typed text with no length cap, so its display width can
    // exceed `u16::MAX`; the sum is taken wide and clamped, never cast first.
    let right_w = if app.filter.is_empty() {
        0
    } else {
        // Enough for the bilingual prefix (`过滤/Filter: ` is 13 cells) plus the
        // trailing space, so a short filter is never squeezed into an ellipsis.
        let wanted = (display_width(&app.filter) as u32).saturating_add(FILTER_CHIP_OVERHEAD);
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
        let text = truncate_to_width(&format!("过滤/Filter: {} ", app.filter), area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, p.fg(p.accent))).alignment(Alignment::Right)),
            area,
        );
    }
}

/// The non-modal one-line message bar: `app.status`, else the selected tunnel's
/// full failure text, else a hint.
pub(crate) fn draw_message(frame: &mut Frame, app: &App, area: Rect) {
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

/// Cells the filter chip spends on everything except the typed text: the
/// bilingual `过滤/Filter: ` prefix (13 cells) plus the trailing space, rounded up.
///
/// Named rather than inlined so the budget in `draw_counters` and the label
/// drawn below it cannot drift apart — a prefix longer than the budget would
/// truncate the user's own filter text into an ellipsis.
const FILTER_CHIP_OVERHEAD: u32 = 16;
#[cfg(test)]
mod tests {
    use crate::app::Overlay;
    use crate::ui::draw;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::ui::test_support::*;

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

    #[test]
    fn message_bar_prefers_the_latest_status() {
        let mut app = app_with_tunnels(vec![sample(1, "a", 8080)]);
        app.set_status(crate::model::StatusMsg::error("保存失败: 磁盘只读"));
        let text = render_to_text(&mut app, 120, 24);
        assert!(text.contains("保存失败"), "status message missing:\n{text}");
    }
}

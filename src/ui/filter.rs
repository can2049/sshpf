//! The `/` filter input: one line at the very bottom of the screen.
//!
//! Non-modal in feel (the table keeps rendering above it) but modal in effect: it
//! takes the last screen row and swallows clicks aimed at it. Two clickable
//! affordances are recorded — the input bar and the `× 清除` label — and the text
//! cursor is placed from the *display* width of what has been typed, so a CJK
//! filter parks the cursor where the user sees it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::App;
use crate::text::{display_width, truncate_to_width};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::palette::palette;
use crate::ui::text::{fit_pair, fit_pair_spaced, warn_once};

// ---------------------------------------------------------------------------
// Overlay: Filter
// ---------------------------------------------------------------------------

pub(crate) fn draw_filter(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let p = palette();
    // Occupies the very last screen row: a one-line, non-modal input.
    let row = Rect { x: area.x, y: area.bottom().saturating_sub(1), width: area.width, height: 1 };
    if row.height == 0 || row.width == 0 {
        return;
    }
    frame.render_widget(Clear, row);

    let parts = filter_parts(row.width);
    let value_w = row.width.saturating_sub(parts.prefix_w + parts.clear_w + parts.hint_w);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(parts.prefix, p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled(truncate_to_width(&app.filter, value_w as usize), p.fg(p.text)),
            Span::styled(truncate_to_width(&parts.clear, parts.clear_w as usize), p.fg(p.accent)),
            Span::styled(truncate_to_width(&parts.hint, parts.hint_w as usize), p.fg(p.dim)),
        ]))
        .style(p.bg(p.selection_bg)),
        row,
    );

    // The input line itself: a click parks the cursor there (the overlay is
    // already modal-ish, so it only needs to keep the click from falling
    // through to the table underneath).
    let bar_w = parts.prefix_w + value_w;
    hits.push(Rect { x: row.x, y: row.y, width: bar_w, height: 1 }, HitTarget::FilterBar);
    if row.width >= parts.clear_w + parts.hint_w {
        hits.push(
            Rect { x: row.x + row.width - parts.hint_w - parts.clear_w, y: row.y, width: parts.clear_w, height: 1 },
            HitTarget::FilterClear,
        );
    }
    place_filter_cursor(frame, app, row, parts.prefix_w, bar_w);
}

/// The three fixed pieces of the line and their widths.
struct FilterParts {
    prefix: String,
    prefix_w: u16,
    clear: String,
    clear_w: u16,
    hint: String,
    hint_w: u16,
}

/// Build the line's fixed text.
///
/// Both languages where they fit, Chinese only on a narrow terminal: the
/// affordances on this line are clickable, so they must never be clipped into a
/// half-word that no longer says what it does. The clear affordance sits on the
/// right and the input echoes in whatever is left of it, so a long filter still
/// leaves the `×` on screen.
fn filter_parts(width: u16) -> FilterParts {
    let prefix = format!("{} ", fit_pair_spaced(" 过滤:", " Filter:", width as usize / 3));
    let prefix_w = display_width(&prefix) as u16;
    let clear = format!("  × {} ", fit_pair("清除", "Clear", 12));
    let clear_w = display_width(&clear) as u16;
    let hint = format!("   {}", fit_pair_spaced("enter 应用 esc 清除", "enter apply · esc clear", 30));
    let hint_w = display_width(&hint) as u16;
    FilterParts { prefix, prefix_w, clear, clear_w, hint, hint_w }
}

/// Park the terminal cursor after the typed text.
///
/// The filter has no length cap, so its cell width is not bounded by `u16`:
/// compute wide and let an off-screen cursor simply be skipped.
fn place_filter_cursor(frame: &mut Frame, app: &App, row: Rect, prefix_w: u16, bar_w: u16) {
    let x = usize::from(row.x) + usize::from(prefix_w) + display_width(&app.filter);
    let bar_right = usize::from(row.x) + usize::from(bar_w);
    if x >= bar_right {
        return;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app::Overlay;
    use crate::ui::test_support::*;

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

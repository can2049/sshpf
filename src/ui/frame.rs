//! The frame's shared geometry: band lookup and the modal scaffolding.
//!
//! Every panel that draws a popup gets its chrome from here, so all modals are
//! bordered, padded and cleared identically:
//!
//! * [`band`] — one row of a `Layout::split` result, degrading to an empty rect
//!   instead of panicking when the solver contract is violated;
//! * [`centered`] / [`centered_pct`] — popup rectangles;
//! * [`popup_block`] / [`clear_and_block`] — the shared modal frame.
//!
//! The top-level `draw()` lives in [`crate::ui`] itself; this module is what it
//! and the overlays build on.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding};

use crate::ui::palette::Palette;
use crate::ui::text::warn_once;

/// One band of a `Layout::split` result, degrading to an empty rect.
///
/// ratatui hands back exactly one rect per constraint, so the index is an
/// invariant of the solver rather than user data. It is still fetched instead of
/// indexed: a violated contract would otherwise take the whole TUI down
/// mid-frame (the terminal is left in raw mode with no UI), whereas a blank band
/// renders, logs, and recovers on the next frame. Every `draw_*` below treats a
/// zero-sized area as "nothing to draw".
pub(crate) fn band(rows: &[Rect], index: usize, what: &'static str) -> Rect {
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
// Overlay scaffolding
// ---------------------------------------------------------------------------

/// Centered popup of an exact size, clamped to the frame.
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
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
pub(crate) fn centered_pct(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (area.width as u32 * pct_w as u32 / 100) as u16;
    let h = (area.height as u32 * pct_h as u32 / 100) as u16;
    centered(area, w.max(24).min(area.width), h.max(6).min(area.height))
}

pub(crate) fn popup_block(title: &str, p: Palette) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(p.fg(p.accent))
        .title(Line::from(Span::styled(format!(" {title} "), p.fg(p.accent).add_modifier(Modifier::BOLD))))
        .padding(Padding::new(1, 1, 0, 0))
}

/// Clear the cells under the popup, then draw its frame. Without `Clear` the
/// table would show through the modal.
pub(crate) fn clear_and_block(frame: &mut Frame, area: Rect, block: Block<'static>) {
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

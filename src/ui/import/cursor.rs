//! A one-line span builder that keeps drawn text and recorded hit rects in sync.
//!
//! The chips row and the buttons row both draw clickable labels whose width
//! depends on user-typed filter text (which has no length cap). Recording the hit
//! rect and advancing the x cursor in two places is exactly how a click ends up
//! one label off, so both happen here, together, and the fit test runs in `usize`
//! — a `u16` sum of `x + w` would wrap and let an oversized label be drawn on top
//! of the ones after it.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::text::display_width;
use crate::ui::hit::{HitMap, HitTarget};

// ---------------------------------------------------------------------------

/// Appends clickable labels to one line while keeping the drawn x position in
/// sync with the recorded hit rects — the one place in this pane where the two
/// could drift apart, so they are computed together.
pub(super) struct LineCursor<'a> {
    spans: Vec<Span<'static>>,
    hits: &'a mut HitMap,
    x: u16,
    y: u16,
    right: u16,
}

impl<'a> LineCursor<'a> {
    pub(super) fn new(x: u16, y: u16, right: u16, hits: &'a mut HitMap) -> Self {
        LineCursor { spans: Vec::new(), hits, x, y, right }
    }

    /// Plain text, no hit rect.
    ///
    /// Chip and button labels embed user-typed filter text, which has no length
    /// cap, so the label's cell width is measured wide and the fit test is done
    /// in `usize` — a u16 sum of `x + w` would wrap and let an oversized label
    /// be placed on top of the ones after it.
    pub(super) fn text(&mut self, text: &str, style: Style) {
        let Some(w) = self.fit(text) else { return };
        self.spans.push(Span::styled(text.to_string(), style));
        self.x += w;
    }

    /// A clickable label. Returns false when it does not fit, so callers stop
    /// instead of drawing a half-clipped button.
    pub(super) fn button(&mut self, label: &str, style: Style, target: HitTarget) -> bool {
        let Some(w) = self.fit(label) else { return false };
        self.hits.push(Rect { x: self.x, y: self.y, width: w, height: 1 }, target);
        self.spans.push(Span::styled(label.to_string(), style));
        self.x += w;
        true
    }

    /// Blank separator cells (never clickable).
    pub(super) fn gap(&mut self, width: u16) {
        let room = self.right.saturating_sub(self.x).min(width);
        if room == 0 {
            return;
        }
        self.spans.push(Span::raw(" ".repeat(room as usize)));
        self.x = self.x.saturating_add(room);
    }

    /// The label's width in cells if it still fits on the line.
    pub(super) fn fit(&self, label: &str) -> Option<u16> {
        let w = display_width(label);
        let end = usize::from(self.x) + w;
        if end > usize::from(self.right) {
            return None;
        }
        // `end <= right <= u16::MAX`, so the cast cannot truncate.
        u16::try_from(w).ok()
    }

    /// `中文/English` when it still fits the row, otherwise `中文`.
    ///
    /// Buttons are the case where the bilingual form can genuinely push a
    /// *later* button off the row, and an unreachable button is worse than an
    /// untranslated one, so the fallback is decided per label against the room
    /// actually left rather than against the pane's full width.
    pub(super) fn fit_label(&self, zh: &str, en: &str) -> String {
        let room = self.right.saturating_sub(self.x) as usize;
        let both = format!("{zh}/{en}");
        if display_width(&both) <= room { both } else { zh.to_string() }
    }

    pub(super) fn finish(self) -> Line<'static> {
        Line::from(self.spans)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    use crate::ui::hit::HitTarget;

    /// Chip/button labels embed user-typed filter text; `LineCursor` must refuse
    /// to place one that does not fit rather than wrapping its x coordinate.
    #[test]
    fn line_cursor_refuses_a_label_wider_than_the_line() {
        let mut hits = HitMap::new();
        let mut line = LineCursor::new(0, 0, 10, &mut hits);
        assert!(line.button("abc", Style::default(), HitTarget::ImportCheckAll));
        assert!(!line.button(&"x".repeat(65_535), Style::default(), HitTarget::ImportInvert), "must not fit");
        assert!(!line.button("abcdefghij", Style::default(), HitTarget::ImportUndoFilter), "must not fit");
        // Only the first button was recorded, and it is where it was drawn.
        assert_eq!(hits.rect_of(&HitTarget::ImportCheckAll), Some(Rect { x: 0, y: 0, width: 3, height: 1 }));
        assert!(hits.rect_of(&HitTarget::ImportInvert).is_none());
    }
}

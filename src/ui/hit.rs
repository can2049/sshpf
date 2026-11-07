//! Mouse hit-testing: a semantic map of what is on screen.
//!
//! The renderer already knows where every row, header and button landed, so it
//! records that geometry as it draws ([`HitMap::push`]); the mouse handler then
//! asks "what is at (x, y)?" instead of duplicating the layout arithmetic. This
//! is deliberately a *map of intents*, not of rectangles: a click on a column
//! header resolves to [`HitTarget::ImportColumnHeader`], not to "x=23".
//!
//! Two rules keep this honest:
//!
//! * Only the renderer writes it, once per frame (`HitMap::clear` at the top of
//!   `draw`). A stale map would send clicks to last frame's layout.
//! * Drawing order decides stacking: later pushes win, so a popup automatically
//!   shadows whatever it covers without any z-index bookkeeping.

use ratatui::layout::Rect;

use crate::model::{BatchAction, ImportColumn, TunnelId};

/// A clickable/scrollable thing on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTarget {
    /// A row of the main tunnel table, by tunnel id (not row index, which moves).
    TunnelRow(TunnelId),
    /// The main table's list area (for wheel scrolling with no row under the
    /// pointer).
    TunnelList,
    /// A checkbox of an import row, by row index into `ImportTable::rows`.
    ImportCheckbox(usize),
    /// A row of the import table (click selects, click on the box toggles).
    ImportRow(usize),
    /// A column header of the import table: clicking starts filtering it.
    ImportColumnHeader(ImportColumn),
    /// The `筛选: 列=值 ×` chip that deletes one column filter.
    ImportFilterChip(ImportColumn),
    /// `清除全部筛选` in the import footer.
    ImportClearFilters,
    /// `撤销筛选` (undo) in the import footer.
    ImportUndoFilter,
    /// `全选` / `反选` / `全不选` on the import table.
    ImportCheckAll,
    ImportInvert,
    ImportUncheckAll,
    /// The import list area, for wheel scrolling.
    ImportList,
    /// A footer button of the selection-mode action bar.
    BatchAction(BatchAction),
    /// `退出选择模式`.
    BatchExit,
    /// `全选` inside selection mode.
    BatchSelectAll,
    /// The main filter input line (`/` overlay): click puts the cursor there.
    FilterBar,
    /// The 过滤 line's `清除` affordance.
    FilterClear,
    /// A modal surface that must swallow clicks aimed at what it covers.
    ///
    /// Modals are drawn *after* the table, so without an entry of their own the
    /// table's rects stay on top of the hit stack and a click on a popup lands on
    /// the row behind it — the click-through bug this variant exists to prevent.
    /// The handler does nothing for it: swallowing is the whole job.
    Popup,
}

/// The geometry recorded for one frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HitMap {
    entries: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        HitMap::default()
    }

    /// Drop last frame's entries. Called once at the top of `draw`.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record `target` at `rect`. Zero-area rects are ignored: they cannot be
    /// clicked, and keeping them would let a hidden widget steal a hit.
    pub fn push(&mut self, rect: Rect, target: HitTarget) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.entries.push((rect, target));
    }

    /// The target at a cell, if any. Later pushes win (popups shadow the table).
    pub fn at(&self, x: u16, y: u16) -> Option<&HitTarget> {
        self.entries.iter().rev().find(|(r, _)| contains(*r, x, y)).map(|(_, t)| t)
    }

    /// Number of recorded entries (used by tests to prove the map is rebuilt).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The rect of the first (bottom-most) target equal to `target`, for tests
    /// and for the dispatcher's "click went outside this panel" checks.
    pub fn rect_of(&self, target: &HitTarget) -> Option<Rect> {
        self.entries.iter().find(|(_, t)| t == target).map(|(r, _)| *r)
    }
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, width: w, height: h }
    }

    #[test]
    fn hits_resolve_inside_their_rect_only() {
        let mut m = HitMap::new();
        m.push(rect(0, 2, 10, 3), HitTarget::TunnelRow(7));
        assert_eq!(m.at(0, 2), Some(&HitTarget::TunnelRow(7)));
        assert_eq!(m.at(9, 4), Some(&HitTarget::TunnelRow(7)));
        // Right/bottom edges are exclusive, top/left inclusive.
        assert_eq!(m.at(10, 2), None);
        assert_eq!(m.at(0, 5), None);
        assert_eq!(m.at(0, 1), None);
    }

    #[test]
    fn later_pushes_shadow_earlier_ones() {
        let mut m = HitMap::new();
        m.push(rect(0, 0, 20, 20), HitTarget::TunnelList);
        m.push(rect(5, 5, 4, 4), HitTarget::ImportClearFilters);
        assert_eq!(m.at(6, 6), Some(&HitTarget::ImportClearFilters), "popup must win");
        assert_eq!(m.at(1, 1), Some(&HitTarget::TunnelList));
    }

    #[test]
    fn empty_rects_are_not_clickable() {
        let mut m = HitMap::new();
        m.push(rect(3, 3, 0, 5), HitTarget::FilterBar);
        m.push(rect(3, 3, 5, 0), HitTarget::FilterClear);
        assert!(m.is_empty());
        assert_eq!(m.at(3, 3), None);
    }

    #[test]
    fn clear_drops_the_previous_frame() {
        let mut m = HitMap::new();
        m.push(rect(0, 0, 5, 5), HitTarget::TunnelList);
        assert_eq!(m.len(), 1);
        m.clear();
        assert!(m.is_empty());
        assert_eq!(m.at(1, 1), None);
    }

    #[test]
    fn rect_of_finds_the_first_occurrence() {
        let mut m = HitMap::new();
        m.push(rect(0, 0, 5, 5), HitTarget::ImportColumnHeader(ImportColumn::Listen));
        m.push(rect(0, 5, 5, 5), HitTarget::ImportColumnHeader(ImportColumn::Listen));
        assert_eq!(m.rect_of(&HitTarget::ImportColumnHeader(ImportColumn::Listen)), Some(rect(0, 0, 5, 5)));
        assert_eq!(m.rect_of(&HitTarget::BatchExit), None);
    }
}

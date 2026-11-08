//! `HostPicker`: the SSH-HOST dropdown of the create/edit form.
//!
//! Rendered as an inline list attached to the SSH HOST field (not a separate
//! modal), so choosing a host from `~/.ssh/config` is the obvious path rather
//! than a hidden shortcut. `open` controls whether the list is expanded; the
//! match list itself is supplied by the caller.
//!
//! Belongs here: dropdown state — query, matches, selection, scroll window.
//! Does not belong: how matches are computed (`app::picker` + `fuzzy_match`).

/// The SSH-HOST dropdown: the query typed, the matching list indices, which one
/// is highlighted, and the scroll window over a `visible`-row viewport.
#[derive(Debug, Clone, Default)]
pub struct HostPicker {
    pub open: bool,
    pub query: String,
    /// Indices into the caller-supplied host list, best match first.
    pub matches: Vec<usize>,
    pub selected: usize,
    /// First visible row in the dropdown (scrolling window).
    pub scroll: u16,
}

impl HostPicker {
    pub fn new() -> Self {
        HostPicker::default()
    }

    /// `DOWN` arrow marker shown inside the field when the list is collapsed.
    pub fn marker(&self) -> &'static str {
        if self.open { "▴" } else { "▾" }
    }

    pub fn query_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn query_backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.scroll = 0;
    }

    /// Move down, keeping the selection inside a `visible`-row window.
    pub fn down(&mut self, visible: usize) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
            self.ensure_visible(visible);
        }
    }

    /// Move up, keeping the selection inside a `visible`-row window.
    pub fn up(&mut self, visible: usize) {
        self.selected = self.selected.saturating_sub(1);
        self.ensure_visible(visible);
    }

    pub fn home(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn end(&mut self, visible: usize) {
        self.selected = self.matches.len().saturating_sub(1);
        self.ensure_visible(visible);
    }

    /// Clip `scroll` so the selected row is inside the viewport.
    ///
    /// `selected` is clamped to the match list first: it is a public field and
    /// the query can shrink the list at any time, which would otherwise let the
    /// `u16` arithmetic below overflow.
    pub fn ensure_visible(&mut self, visible: usize) {
        let last = self.matches.len().saturating_sub(1);
        let sel = self.selected.min(last);
        self.selected = sel;
        if visible == 0 {
            self.scroll = sel.min(u16::MAX as usize) as u16;
            return;
        }
        let scroll = self.scroll as usize;
        if sel < scroll {
            self.scroll = sel.min(u16::MAX as usize) as u16;
        } else if sel >= scroll.saturating_add(visible) {
            self.scroll = (sel + 1 - visible).min(u16::MAX as usize) as u16;
        }
    }

    pub fn current(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_picker_scroll_window() {
        let mut p = HostPicker::new();
        p.matches = (0..20).collect();
        assert!(!p.open);
        assert_eq!(p.marker(), "▾");
        p.open = true;
        assert_eq!(p.marker(), "▴");

        // Walking down past the window pushes the viewport along.
        for _ in 0..10 {
            p.down(5);
        }
        assert_eq!(p.selected, 10);
        assert!(p.scroll <= 10);
        assert!(10 < p.scroll as usize + 5, "selection must stay visible");

        p.home();
        assert_eq!((p.selected, p.scroll), (0, 0));
        p.end(5);
        assert_eq!(p.selected, 19);
        assert_eq!(p.current(), Some(19));

        // A shrinking list must re-clamp the viewport.
        p.matches.truncate(3);
        p.ensure_visible(5);
        assert!(p.scroll as usize <= p.selected);
    }

    #[test]
    fn host_picker_queries_reset_the_cursor() {
        let mut p = HostPicker::new();
        p.matches = (0..5).collect();
        p.selected = 3;
        p.scroll = 2;
        p.query_char('a');
        assert_eq!((p.selected, p.scroll), (0, 0));
        p.selected = 2;
        p.query_backspace();
        assert_eq!((p.selected, p.scroll), (0, 0));
        assert_eq!(p.query, "");
    }

    #[test]
    fn picker_ensure_visible_survives_a_scroll_left_at_u16_max() {
        // Same `u16` overflow as the import table: `scroll + visible`.
        let mut p = HostPicker::new();
        p.matches = (0..20).collect();
        p.scroll = u16::MAX;
        p.selected = 0;
        p.ensure_visible(5);
        assert_eq!(p.scroll, 0);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn picker_selection_is_clamped_to_the_match_list() {
        // `selected` is public and the query can shrink `matches` at any time;
        // an out-of-range selection must degrade to the last row, not panic.
        let mut p = HostPicker::new();
        p.matches = (0..3).collect();
        p.selected = 99;
        p.ensure_visible(5);
        assert_eq!(p.selected, 2);
        assert_eq!(p.current(), Some(2));

        p.matches.clear();
        p.selected = 5;
        p.ensure_visible(5);
        assert_eq!(p.selected, 0);
        assert_eq!(p.scroll, 0);
        assert_eq!(p.current(), None);
    }

    #[test]
    fn picker_end_keeps_the_last_row_inside_the_window() {
        // `end` used to move the selection without moving the viewport, leaving
        // the highlighted row scrolled out of sight.
        let mut p = HostPicker::new();
        p.matches = (0..20).collect();
        p.end(5);
        assert_eq!(p.selected, 19);
        assert!(p.selected < p.scroll as usize + 5, "the last row must be visible: scroll={}", p.scroll);
    }
}

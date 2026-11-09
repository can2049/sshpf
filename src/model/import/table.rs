//! `ImportTable` — the candidate rows, the cursor and the tick marks.
//!
//! Deliberately a *pure* state machine: it owns the rows, the cursor/scroll
//! window and the selection, and knows nothing about `App`, SSH or rendering.
//! That is what makes the navigation rules unit-testable without a terminal.
//!
//! The per-column filters and their undo stack live in [`super::filter`], but
//! they are the same `ImportTable`: both files contribute an `impl` block.
//!
//! Belongs here: row storage, cursor/scroll invariants, selection, the `touched`
//! flag that tells a *default* tick apart from a user instruction.
//! Does not belong: filtering (see `super::filter`), rendering, key handling.

use super::row::{ImportColumn, ImportRow};
use super::{SshForward, col_match};

/// The "forwards found for this host" table shown when creating a tunnel.
///
/// Default policy (requirement: "默认勾选该 host 在 ~/.ssh/config 中的所有现有
/// 端口转发"): every importable row starts checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportTable {
    pub rows: Vec<ImportRow>,
    /// Index into [`ImportTable::visible`], not into `rows`.
    pub cursor: usize,
    /// First visible row (scroll window).
    pub scroll: u16,
    /// One filter per [`ImportColumn`], indexed by `ImportColumn::index()`.
    pub filters: [String; ImportColumn::COUNT],
    /// The column filter currently being typed into, if any.
    pub editing: Option<ImportColumn>,
    /// Filter snapshots for undo (`z`). Newest last. `pub(super)` because the
    /// filter half of this type lives in `super::filter`.
    pub(super) history: Vec<[String; ImportColumn::COUNT]>,
    /// Whether the user has expressed an intention *about these rows*.
    ///
    /// `false` means the tick marks are only the default the table arrived with
    /// (every importable row ticked, see [`ImportTable::new`]). `true` means the
    /// user ticked, unticked, re-ticked, inverted, or changed a filter — i.e. the
    /// selection is their instruction rather than the offered default.
    ///
    /// The submit path needs this distinction: an untouched pane is a
    /// *proposal*, so a complete hand-filled form may outrank it; once touched,
    /// the pane's ticks are what the user means by "save". Nothing in the table
    /// reads it — it is a message from the interaction layer to the router — and
    /// cursor movement deliberately does not set it: looking at rows is not
    /// choosing them.
    pub touched: bool,
}

impl ImportTable {
    /// Build a table from the discovered forwards: everything importable and not
    /// already saved is checked by default.
    pub fn new(forwards: Vec<SshForward>) -> Self {
        ImportTable::with_existing(forwards, Vec::new())
    }

    /// Build a table that knows which rows are already saved tunnels.
    ///
    /// The caller (`app::import_pane`) owns the config, so it answers the "already
    /// saved?" question for every row and passes the answers in. `existing` is
    /// aligned with `forwards` by position: entry `i` is the name of
    /// the saved tunnel that row `i` already is, if any. Positional (rather than a
    /// callback) so the caller can answer from one snapshot of the config — a
    /// callback that re-read the config per row could give two rows of one table
    /// two different answers for the same forward.
    ///
    /// The two things that follow from a row being saved, and both matter:
    ///
    /// * it starts **unticked**, because the default offer is "these are the
    ///   forwards you can create" and this one already exists;
    /// * it can never *become* ticked (`toggle_*` / `check_all_*` skip it), because
    ///   a keyboard route to a known duplicate would undo the whole point.
    ///
    /// The row stays visible and keeps its name, so the pane can say "already
    /// exists as X" instead of appearing to have lost a directive the user wrote.
    pub fn with_existing(forwards: Vec<SshForward>, existing: Vec<Option<String>>) -> Self {
        let mut answers = existing.into_iter();
        let rows = forwards
            .into_iter()
            .map(|forward| {
                // Only an *importable* row can already exist: a directive we
                // cannot express as a `TunnelConfig` has no identity to compare,
                // and asking would compare a nonsense string.
                let importable = forward.unsupported_reason().is_none();
                let existing = answers.next().flatten().filter(|_| importable);
                let checked = importable && existing.is_none();
                ImportRow { forward, checked, existing }
            })
            .collect();
        ImportTable {
            rows,
            cursor: 0,
            scroll: 0,
            filters: std::array::from_fn(|_| String::new()),
            editing: None,
            history: Vec::new(),
            // Freshly discovered rows are the *default offer*; only a user action
            // turns them into an instruction.
            touched: false,
        }
    }

    /// An empty table (`~/.ssh/config` has nothing for this host).
    pub fn empty() -> Self {
        ImportTable::new(Vec::new())
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Row indices that survive every column filter, in file order.
    pub fn visible(&self) -> Vec<usize> {
        let active: Vec<ImportColumn> =
            ImportColumn::ALL.into_iter().filter(|c| !self.filter(*c).trim().is_empty()).collect();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| active.iter().all(|col| col_match(self.filter(*col), &col.cell(&row.forward))))
            .map(|(i, _)| i)
            .collect()
    }

    /// The row under the cursor.
    pub fn cursor_row(&self) -> Option<&ImportRow> {
        self.visible().get(self.cursor).and_then(|i| self.rows.get(*i))
    }

    pub fn cursor_forward(&self) -> Option<SshForward> {
        self.cursor_row().map(|r| r.forward.clone())
    }

    /// Keep `cursor` inside the filtered view. Called after any filter or row
    /// change, because a shrinking view must never leave a dangling cursor.
    pub fn clamp_cursor(&mut self) {
        let n = self.visible().len();
        self.cursor = if n == 0 { 0 } else { self.cursor.min(n - 1) };
    }

    pub fn move_cursor(&mut self, delta: i32) {
        self.clamp_cursor();
        let n = self.visible().len();
        if n == 0 {
            return;
        }
        let next = self.cursor as i64 + delta as i64;
        self.cursor = next.clamp(0, n as i64 - 1) as usize;
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.visible().len().saturating_sub(1);
    }

    /// Clip `scroll` so the cursor row is inside a `height`-row viewport.
    ///
    /// `scroll`/`height` are `u16` and `scroll` can be written directly, so the
    /// comparisons use `usize` arithmetic: `self.scroll + height` in `u16` would
    /// overflow (and panic in a debug build) for a scroll left at `u16::MAX`.
    pub fn ensure_visible(&mut self, height: usize) {
        let sel = self.cursor;
        if height == 0 {
            self.scroll = sel.min(u16::MAX as usize) as u16;
            return;
        }
        let scroll = self.scroll as usize;
        if sel < scroll {
            self.scroll = sel.min(u16::MAX as usize) as u16;
        } else if sel >= scroll.saturating_add(height) {
            self.scroll = (sel + 1 - height).min(u16::MAX as usize) as u16;
        }
    }

    // -- selection ---------------------------------------------------------

    /// Record that the user has expressed an intention about these rows.
    ///
    /// Called by every selection/filter mutator that a *key press or click* can
    /// reach. Deliberately not called from navigation (`move_cursor`, `home`,
    /// `end`, `clamp_cursor`, `ensure_visible`) or construction: those only move
    /// a highlight or establish the default.
    pub(super) fn touch(&mut self) {
        self.touched = true;
    }

    pub fn toggle_cursor(&mut self) {
        self.touch();
        if let Some(&idx) = self.visible().get(self.cursor)
            && let Some(row) = self.rows.get_mut(idx)
            && row.creatable()
        {
            row.checked = !row.checked;
        }
    }

    /// Tick every importable row in the *filtered* view.
    ///
    /// Filtered, not all: with a filter active the user is looking at a subset,
    /// and a "全选" that silently checked hidden rows would be a footgun.
    pub fn check_all_visible(&mut self) {
        self.touch();
        for idx in self.visible() {
            if let Some(row) = self.rows.get_mut(idx)
                && row.creatable()
            {
                row.checked = true;
            }
        }
    }

    pub fn uncheck_all_visible(&mut self) {
        self.touch();
        for idx in self.visible() {
            if let Some(row) = self.rows.get_mut(idx) {
                row.checked = false;
            }
        }
    }

    pub fn invert_visible(&mut self) {
        self.touch();
        for idx in self.visible() {
            if let Some(row) = self.rows.get_mut(idx)
                && row.creatable()
            {
                row.checked = !row.checked;
            }
        }
    }

    /// Rows that will become a tunnel if the user saves now.
    ///
    /// Counts only `creatable()` rows: an already-saved forward is listed but
    /// cannot be created, so counting it would promise a tunnel the save does not
    /// produce.
    pub fn checked_count(&self) -> usize {
        self.rows.iter().filter(|r| r.checked && r.creatable()).count()
    }

    /// Rows the user *could* create — the denominator of `n/m 条勾选`.
    ///
    /// Not `len()`: un-importable rows (unix sockets, a `-R` with no
    /// destination) and already-saved rows are listed but never creatable, and a
    /// denominator that included them would make "all of them are ticked" look
    /// false.
    pub fn importable_count(&self) -> usize {
        self.rows.iter().filter(|r| r.creatable()).count()
    }

    /// Rows listed because a saved tunnel already is this forward.
    ///
    /// Reported in the pane footer so "why is this line dimmed?" has an answer on
    /// screen, and so a host whose every directive is already managed says so
    /// instead of looking broken.
    pub fn existing_count(&self) -> usize {
        self.rows.iter().filter(|r| r.importable() && r.existing.is_some()).count()
    }

    pub fn any_checked(&self) -> bool {
        self.checked_count() > 0
    }

    /// The checked forwards, in file order — what the form turns into tunnels.
    pub fn checked_forwards(&self) -> Vec<SshForward> {
        self.rows.iter().filter(|r| r.checked && r.creatable()).map(|r| r.forward.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardKind;
    use crate::model::fixtures::{fwd, sample_table};

    #[test]
    fn a_fresh_table_is_untouched_and_navigation_cannot_change_that() {
        // `touched` answers "did the user express an intention about these
        // rows?". Looking at them is not an intention: the submit router lets a
        // complete hand-filled form outrank an untouched pane, so a cursor move
        // must never silently hand the pane the save key.
        let mut t = sample_table();
        assert!(!t.touched, "discovered rows are a default offer");
        t.move_cursor(2);
        t.home();
        t.end();
        t.ensure_visible(2);
        t.clamp_cursor();
        assert!(!t.touched, "navigation must not look like a decision");
    }

    #[test]
    fn every_selection_change_marks_the_table_touched() {
        // One test per mutator the key handler can reach: each is a deliberate
        // act on the ticks, so each must set the flag.
        for mutate in [
            ImportTable::toggle_cursor as fn(&mut ImportTable),
            ImportTable::uncheck_all_visible,
            ImportTable::check_all_visible,
            ImportTable::invert_visible,
        ] {
            let mut t = sample_table();
            mutate(&mut t);
            assert!(t.touched, "{} must mark the table touched", std::any::type_name_of_val(&mutate));
        }
    }

    #[test]
    fn every_importable_row_starts_checked() {
        // Requirement: the host's existing forwards are the default, so creating
        // a tunnel should already carry them without any clicking.
        let t = sample_table();
        assert_eq!(t.checked_count(), 4);
        assert_eq!(t.importable_count(), 4);
        assert!(t.rows.iter().all(|r| r.checked));
        assert_eq!(t.checked_forwards().len(), 4);
    }

    #[test]
    fn unimportable_rows_are_never_checked_by_default() {
        let mut bad = fwd(ForwardKind::Local, 0, ("", 0));
        bad.listen_port = 0;
        let t = ImportTable::new(vec![bad, fwd(ForwardKind::Local, 8080, ("10.0.0.1", 80))]);
        assert_eq!(t.len(), 2);
        assert_eq!(t.importable_count(), 1, "the broken row is listed but not importable");
        assert!(!t.rows[0].checked);
        assert!(t.rows[1].checked);
        assert_eq!(t.checked_forwards().len(), 1);
    }

    #[test]
    fn toggling_ignores_unimportable_rows() {
        let mut bad = fwd(ForwardKind::Local, 0, ("", 0));
        bad.listen_port = 0;
        let mut t = ImportTable::new(vec![bad]);
        t.toggle_cursor();
        assert!(!t.rows[0].checked, "a row that cannot be imported must not become checked");
        t.check_all_visible();
        assert!(!t.rows[0].checked, "select-all must skip it too");
    }

    #[test]
    fn select_all_only_touches_the_filtered_view() {
        let mut t = sample_table();
        t.uncheck_all_visible();
        assert_eq!(t.checked_count(), 0);
        t.set_filter(ImportColumn::Kind, "D");
        t.check_all_visible();
        assert_eq!(t.visible(), vec![3]);
        assert_eq!(t.checked_count(), 1, "only the visible row may be ticked");
        // Clearing the filter shows the others are still unticked.
        t.clear_all_filters();
        assert_eq!(t.checked_count(), 1);
    }

    #[test]
    fn invert_flips_only_the_visible_importable_rows() {
        let mut t = sample_table();
        t.set_filter(ImportColumn::Kind, "L");
        t.invert_visible();
        assert_eq!(t.checked_count(), 2, "the two Local rows flipped on... to off; the others stay on");
        // Initially 4 checked; flipping 2 visible ones leaves 2 checked.
        assert!(!t.rows[0].checked && !t.rows[1].checked);
        assert!(t.rows[2].checked && t.rows[3].checked, "hidden rows must not change");
    }

    #[test]
    fn checked_forwards_keep_file_order() {
        let t = sample_table();
        let ports: Vec<u16> = t.checked_forwards().iter().map(|f| f.listen_port).collect();
        assert_eq!(ports, vec![8080, 9090, 7000, 1080]);
    }

    #[test]
    fn cursor_movement_clamps_instead_of_wrapping() {
        let mut t = sample_table();
        t.move_cursor(99);
        assert_eq!(t.cursor, 3, "clamps at the last row");
        t.move_cursor(-99);
        assert_eq!(t.cursor, 0);
        t.end();
        assert_eq!(t.cursor, 3);
        t.home();
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn cursor_stays_inside_a_scrolled_window() {
        let mut t = sample_table();
        t.ensure_visible(2);
        t.move_cursor(3);
        t.ensure_visible(2);
        // cursor 3 in a 2-row window must put the window at rows 2..=3.
        assert_eq!(t.scroll, 2);
        t.home();
        t.ensure_visible(2);
        assert_eq!(t.scroll, 0, "scrolling back up must follow the cursor");
    }

    #[test]
    fn import_ensure_visible_survives_a_scroll_left_at_u16_max() {
        // `scroll` is a public `u16` field. The old code compared `cursor` and
        // `scroll + height` in `u16`, so a scroll near 65535 overflowed (panic in
        // a debug build — which is what CI runs).
        let mut table = sample_table();
        table.scroll = u16::MAX;
        table.cursor = 0;
        table.ensure_visible(5); // must not overflow
        assert_eq!(table.scroll, 0, "the window must follow the cursor back up");

        // The actual overflowing pair: both ends at the top of the `u16` range.
        table.scroll = u16::MAX - 1;
        table.cursor = u16::MAX as usize;
        table.ensure_visible(2); // must not overflow either
        assert!(table.scroll as usize <= table.cursor);
    }

    #[test]
    fn import_ensure_visible_handles_a_zero_height_viewport() {
        // A pane squeezed to nothing must not divide or subtract by zero.
        let mut table = sample_table();
        table.cursor = 2;
        table.ensure_visible(0);
        assert_eq!(table.scroll, 2);
    }

    #[test]
    fn import_ensure_visible_with_a_cursor_past_the_end_is_clamped() {
        let mut table = sample_table();
        table.cursor = 99;
        table.ensure_visible(2);
        assert!(table.scroll as usize <= table.cursor);
    }
}

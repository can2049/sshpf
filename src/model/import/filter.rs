//! The per-column filters of `ImportTable`, and the undo stack behind `z`.
//!
//! A column filter is a *scope narrowing* tool, so [`col_match`](super::col_match) uses a
//! case-insensitive **substring** match rather than the fuzzy subsequence
//! matcher used by the main table: fuzzy matching on a port would happily return
//! unrelated rows.
//!
//! Every mutation snapshots the filter set first ([`FILTER_HISTORY_MAX`] deep),
//! which is what makes `z` undo one keystroke at a time. A change that would not
//! change anything (clearing an already-empty filter) deliberately does *not*
//! push a snapshot, so undo never looks broken.
//!
//! Belongs here: filter text, matching, undo.
//! Does not belong: cursor/selection (`super::table`), rendering.

use super::row::ImportColumn;
use super::table::ImportTable;

/// How many filter snapshots the undo stack keeps.
const FILTER_HISTORY_MAX: usize = 64;

impl ImportTable {
    // -- filters -----------------------------------------------------------

    pub fn has_filters(&self) -> bool {
        self.filters.iter().any(|f| !f.trim().is_empty())
    }

    pub fn filter(&self, col: ImportColumn) -> &str {
        // The array is sized `ImportColumn::COUNT` and indexed by
        // `ImportColumn::index()`; `get` makes the coupling explicit instead of
        // relying on the two staying in step.
        self.filters.get(col.index()).map(String::as_str).unwrap_or("")
    }

    fn snapshot(&mut self) {
        self.history.push(self.filters.clone());
        if self.history.len() > FILTER_HISTORY_MAX {
            self.history.remove(0);
        }
    }

    /// Replace one column's filter, recording an undo point.
    pub fn set_filter(&mut self, col: ImportColumn, text: impl Into<String>) {
        self.snapshot();
        if let Some(slot) = self.filters.get_mut(col.index()) {
            *slot = text.into();
        }
        self.after_filter_change();
    }

    pub fn filter_char(&mut self, col: ImportColumn, c: char) {
        self.snapshot();
        if let Some(slot) = self.filters.get_mut(col.index()) {
            slot.push(c);
        }
        self.after_filter_change();
    }

    pub fn filter_backspace(&mut self, col: ImportColumn) {
        let Some(index) = self.filter_index(col) else {
            return;
        };
        if self.filters[index].is_empty() {
            return;
        }
        self.snapshot();
        self.filters[index].pop();
        self.after_filter_change();
    }

    /// Delete one column's filter (undoable).
    pub fn clear_filter(&mut self, col: ImportColumn) {
        let Some(index) = self.filter_index(col) else {
            return;
        };
        if self.filters[index].is_empty() {
            return;
        }
        self.snapshot();
        self.filters[index].clear();
        self.after_filter_change();
    }

    /// `col`'s slot in `filters`, or `None` when the array and the column list
    /// disagree about how many columns exist.
    fn filter_index(&self, col: ImportColumn) -> Option<usize> {
        let index = col.index();
        (index < self.filters.len()).then_some(index)
    }

    /// Delete every filter at once (undoable).
    pub fn clear_all_filters(&mut self) {
        if !self.has_filters() {
            return;
        }
        self.snapshot();
        self.filters = std::array::from_fn(|_| String::new());
        self.after_filter_change();
    }

    /// Roll back the most recent filter change. Returns true when something was
    /// undone, so the caller can say so instead of failing silently.
    pub fn undo_filter(&mut self) -> bool {
        match self.history.pop() {
            Some(prev) => {
                self.filters = prev;
                self.after_filter_change();
                true
            }
            None => false,
        }
    }

    pub fn can_undo_filter(&self) -> bool {
        !self.history.is_empty()
    }

    fn after_filter_change(&mut self) {
        // The filtered view changed size: re-anchor the cursor instead of
        // leaving it pointing at a row that is no longer shown.
        self.cursor = 0;
        self.scroll = 0;
        self.clamp_cursor();
    }

    /// Begin typing into a column filter.
    pub fn begin_filter(&mut self, col: ImportColumn) {
        self.editing = Some(col);
    }

    pub fn end_filter(&mut self) {
        self.editing = None;
    }

    /// The status line: `3/5 条勾选 · 筛选: 类型=R, 监听=8080`.
    ///
    /// Chinese only on purpose: §5.2 of the i18n contract keeps the model layer
    /// language-neutral and lets the UI compose the English half from the
    /// `*_en()` accessors (`src/ui/import.rs`).
    pub fn summary(&self) -> String {
        let mut s = format!("{}/{} 条勾选", self.checked_count(), self.importable_count());
        if self.has_filters() {
            let parts: Vec<String> = ImportColumn::ALL
                .into_iter()
                .map(|c| (c, self.filter(c)))
                .filter(|(_, f)| !f.trim().is_empty())
                .map(|(c, f)| format!("{}={f}", c.label()))
                .collect();
            s.push_str(&format!(" · 筛选: {}", parts.join(", ")));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardKind;
    use crate::model::ImportTable;
    use crate::model::SshForward;
    use crate::model::col_match;
    use crate::model::fixtures::sample_table;

    #[test]
    fn column_filters_are_anded_and_case_insensitive() {
        let mut t = sample_table();
        t.set_filter(ImportColumn::Kind, "L");
        assert_eq!(t.visible(), vec![0, 1], "only Local rows");
        t.set_filter(ImportColumn::Listen, "9090");
        assert_eq!(t.visible(), vec![1], "both filters must hold");
        // Substring, not prefix: "0.0." only appears in the middle of the
        // rendered listen address.
        t.set_filter(ImportColumn::Listen, "0.1:9");
        assert_eq!(t.visible(), vec![1], "a substring match is enough");
        t.set_filter(ImportColumn::Listen, "127.0.0.1");
        assert_eq!(t.visible(), vec![0, 1], "every Local row listens on loopback");
        t.set_filter(ImportColumn::Target, "10.0.0.2");
        assert_eq!(t.visible(), vec![1]);
        t.clear_all_filters();
        assert_eq!(t.visible().len(), 4);
    }

    #[test]
    fn filters_match_case_insensitively_and_ignore_padding() {
        let t = ImportTable::new(vec![SshForward {
            kind: ForwardKind::Local,
            listen_addr: String::new(),
            listen_port: 8080,
            target_host: "LocalHost".into(),
            target_port: 80,
            host_alias: "h".into(),
            line: 1,
            raw: "LocalForward 8080 LocalHost:80".into(),
        }]);
        assert!(col_match("localhost", &ImportColumn::Target.cell(&t.rows[0].forward)));
        assert!(col_match("  LOCAL  ", "localhost"), "surrounding spaces must not break a filter");
        assert!(col_match("", "anything"), "an empty filter matches everything");
    }

    #[test]
    fn filter_undo_rolls_back_one_step_at_a_time() {
        let mut t = sample_table();
        assert!(!t.can_undo_filter());
        t.set_filter(ImportColumn::Kind, "L");
        t.set_filter(ImportColumn::Listen, "8080");
        assert_eq!(t.visible(), vec![0]);
        assert!(t.undo_filter());
        assert_eq!(t.filter(ImportColumn::Listen), "", "the newest change is undone first");
        assert_eq!(t.visible(), vec![0, 1]);
        assert!(t.undo_filter());
        assert!(!t.has_filters());
        assert!(!t.undo_filter(), "nothing left to undo must report false, not panic");
    }

    #[test]
    fn clearing_an_already_empty_filter_is_not_an_undo_step() {
        // Otherwise `F` on an unfiltered column would push a snapshot and make
        // `z` look broken (it would appear to do nothing).
        let mut t = sample_table();
        t.clear_filter(ImportColumn::Kind);
        t.clear_all_filters();
        assert!(!t.can_undo_filter());
    }

    #[test]
    fn filter_history_is_bounded() {
        let mut t = sample_table();
        for i in 0..(FILTER_HISTORY_MAX + 20) {
            t.set_filter(ImportColumn::Kind, i.to_string());
        }
        let mut undoable = 0;
        while t.undo_filter() {
            undoable += 1;
        }
        assert_eq!(undoable, FILTER_HISTORY_MAX, "the undo stack must not grow without bound");
    }

    #[test]
    fn changing_a_filter_reanchors_a_dangling_cursor() {
        // A filter that hides everything must not leave the cursor pointing at
        // row 5 of a one-row view.
        let mut t = sample_table();
        t.cursor = 3;
        t.set_filter(ImportColumn::Listen, "8080");
        assert_eq!(t.cursor, 0);
        assert!(t.cursor_row().is_some());
        t.set_filter(ImportColumn::Listen, "zzz-no-match");
        assert_eq!(t.visible(), Vec::<usize>::new());
        assert!(t.cursor_row().is_none(), "no match means no cursor row, not a panic");
        t.move_cursor(1);
        assert_eq!(t.cursor, 0, "moving in an empty view is a no-op");
    }

    #[test]
    fn summary_reports_counts_and_active_filters() {
        let mut t = sample_table();
        assert_eq!(t.summary(), "4/4 条勾选");
        t.uncheck_all_visible();
        t.set_filter(ImportColumn::Kind, "L");
        let s = t.summary();
        assert!(s.contains("0/4"), "{s}");
        assert!(s.contains("类型=L"), "{s}");
    }

    #[test]
    fn filter_accessors_tolerate_a_null_scroll_target() {
        // `filter()` returning "" for an unknown slot is what lets `visible()` /
        // `summary()` use it without a bounds check that could be stale.
        let mut table = sample_table();
        for col in ImportColumn::ALL {
            assert!(table.filter(col).is_empty());
            table.set_filter(col, "x");
            assert_eq!(table.filter(col), "x");
            table.clear_filter(col);
            assert!(table.filter(col).is_empty());
        }
    }
}

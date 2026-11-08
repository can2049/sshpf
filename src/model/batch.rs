//! Multi-selection of table rows, and the batch verbs that operate on it.
//!
//! Selection is by [`TunnelId`], not by row index: rows move when the filter
//! changes or a tunnel is deleted, and a marked set that silently retargeted
//! another tunnel would be dangerous for a batch delete.
//!
//! Belongs here: which rows are marked and what a batch can do to them.
//! Does not belong: executing the action (`app::select`), rendering the bar.

use super::TunnelId;

/// One or more rows of the main table marked for a batch operation.
///
/// Selection is by `TunnelId`, not by row index: rows move when the filter
/// changes or a tunnel is deleted, and a marked set that silently retargeted
/// another tunnel would be dangerous for a batch delete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultiSelect {
    /// Whether the table is in selection mode at all.
    pub active: bool,
    /// Marked tunnel ids, ascending for stable prompts.
    pub marked: std::collections::BTreeSet<TunnelId>,
    /// Row the mark was last extended from (an index into `visible_indices`),
    /// for shift+click / shift+j range marking.
    pub anchor: Option<usize>,
}

impl MultiSelect {
    pub fn new() -> Self {
        MultiSelect::default()
    }

    /// Enter selection mode with nothing marked.
    ///
    /// Deliberately empty rather than pre-marking the cursor row: `space` must
    /// mean the same thing before and after entering (toggle the row under the
    /// cursor). If entering already marked it, the first `space` would *unmark*
    /// the row the user just pointed at, which reads as a bug.
    pub fn enter(&mut self) {
        self.active = true;
        self.marked.clear();
        self.anchor = None;
    }

    pub fn exit(&mut self) {
        self.active = false;
        self.marked.clear();
        self.anchor = None;
    }

    pub fn is_marked(&self, id: TunnelId) -> bool {
        self.marked.contains(&id)
    }

    pub fn len(&self) -> usize {
        self.marked.len()
    }

    pub fn is_empty(&self) -> bool {
        self.marked.is_empty()
    }

    pub fn toggle(&mut self, id: TunnelId) {
        if !self.marked.remove(&id) {
            self.marked.insert(id);
        }
    }

    pub fn set(&mut self, id: TunnelId, on: bool) {
        if on {
            self.marked.insert(id);
        } else {
            self.marked.remove(&id);
        }
    }

    pub fn clear(&mut self) {
        self.marked.clear();
        self.anchor = None;
    }

    /// Mark every id in `ids` (used by 全选 on the filtered view).
    pub fn mark_all(&mut self, ids: &[TunnelId]) {
        self.marked.extend(ids.iter().copied());
    }

    /// Mark the inclusive range `from..=to` in `ids`.
    pub fn mark_range(&mut self, ids: &[TunnelId], from: usize, to: usize) {
        // `lo`/`hi` are caller-supplied indices; clamp both to the view before
        // slicing, and use `get` so an empty view stays a no-op rather than a
        // `len() - 1` underflow.
        let Some(last) = ids.len().checked_sub(1) else {
            return;
        };
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        let lo = lo.min(last);
        let hi = hi.min(last);
        if let Some(range) = ids.get(lo..=hi) {
            self.marked.extend(range.iter().copied());
        }
        self.anchor = Some(hi);
    }

    /// Drop marks for tunnels that no longer exist (after a batch delete).
    pub fn retain_existing(&mut self, exists: impl Fn(TunnelId) -> bool) {
        self.marked.retain(|id| exists(*id));
        if self.marked.is_empty() {
            self.anchor = None;
        }
    }

    /// Marked ids as a `Vec`, for prompts and dispatch.
    pub fn ids(&self) -> Vec<TunnelId> {
        self.marked.iter().copied().collect()
    }
}

/// An operation a multi-selection can perform at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAction {
    Start,
    Stop,
    Restart,
    Delete,
}

impl BatchAction {
    pub const ALL: [BatchAction; 4] =
        [BatchAction::Start, BatchAction::Stop, BatchAction::Restart, BatchAction::Delete];

    pub fn label(self) -> &'static str {
        match self {
            BatchAction::Start => "启用",
            BatchAction::Stop => "停用",
            BatchAction::Restart => "重启",
            BatchAction::Delete => "删除",
        }
    }

    /// English half of [`BatchAction::label`], for the bilingual UI.
    pub fn label_en(self) -> &'static str {
        match self {
            BatchAction::Start => "Start",
            BatchAction::Stop => "Stop",
            BatchAction::Restart => "Restart",
            BatchAction::Delete => "Delete",
        }
    }

    /// ASCII name, used for log fields (and therefore stable across UI wording).
    pub fn name(self) -> &'static str {
        match self {
            BatchAction::Start => "start",
            BatchAction::Stop => "stop",
            BatchAction::Restart => "restart",
            BatchAction::Delete => "delete",
        }
    }

    /// Every batch operation goes through the confirmation gate: even "enable"
    /// opens N listeners at once, which is exactly what the single-row flow
    /// already warns about.
    /// Bilingual: the Chinese sentence stays first and unchanged (tests and the
    /// UI key off it), the English sentence follows after a space.
    pub fn prompt(self, n: usize) -> String {
        match self {
            BatchAction::Start => crate::text::bi(
                &format!("确认启用选中的 {n} 条转发？将新开 {n} 个监听端口。"),
                &format!("Start the {n} selected forwards? {n} new listening ports will open."),
            ),
            BatchAction::Stop => crate::text::bi(
                &format!("确认停用选中的 {n} 条转发？将断开它们的连接。"),
                &format!("Stop the {n} selected forwards? Their connections will drop."),
            ),
            BatchAction::Restart => crate::text::bi(
                &format!("确认重启选中的 {n} 条转发？连接会短暂中断。"),
                &format!("Restart the {n} selected forwards? The connections will briefly drop."),
            ),
            BatchAction::Delete => crate::text::bi(
                &format!("确认删除选中的 {n} 条转发？此操作不可撤销。"),
                &format!("Delete the {n} selected forwards? This cannot be undone."),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TunnelId;

    #[test]
    fn multi_select_enter_marks_the_cursor_and_exit_clears() {
        let mut ms = MultiSelect::new();
        assert!(!ms.active);
        ms.enter();
        assert!(ms.active);
        assert!(ms.is_empty(), "entering must not pre-mark anything");
        ms.mark_all(&[7]);
        assert_eq!(ms.ids(), vec![7]);
        ms.exit();
        assert!(!ms.active);
        assert!(ms.is_empty());
        assert_eq!(ms.anchor, None);
    }

    #[test]
    fn multi_select_toggle_and_set() {
        let mut ms = MultiSelect::new();
        ms.enter();
        ms.toggle(1);
        ms.toggle(2);
        assert_eq!(ms.len(), 2);
        ms.toggle(1);
        assert_eq!(ms.ids(), vec![2], "a second toggle removes");
        ms.set(2, false);
        assert!(ms.is_empty());
        ms.set(3, true);
        assert_eq!(ms.ids(), vec![3]);
    }

    #[test]
    fn multi_select_range_is_inclusive_and_order_insensitive() {
        let ids: Vec<TunnelId> = (1..=6).collect();
        let mut ms = MultiSelect::new();
        ms.mark_range(&ids, 1, 3);
        assert_eq!(ms.ids(), vec![2, 3, 4]);
        // Dragging backwards marks the same set.
        ms.enter();
        ms.mark_range(&ids, 4, 2);
        assert_eq!(ms.ids(), vec![3, 4, 5]);
        // Out-of-range indices are clamped, not a panic.
        ms.enter();
        ms.mark_range(&ids, 0, 99);
        assert_eq!(ms.len(), 6);
        ms.enter();
        ms.mark_range(&[], 0, 2);
        assert!(ms.is_empty(), "an empty view marks nothing");
    }

    #[test]
    fn retain_existing_drops_marks_for_deleted_tunnels() {
        let mut ms = MultiSelect::new();
        ms.enter();
        ms.mark_all(&[1, 2, 3]);
        ms.retain_existing(|id| id != 2);
        assert_eq!(ms.ids(), vec![1, 3]);
        ms.retain_existing(|_| false);
        assert!(ms.is_empty());
        assert_eq!(ms.anchor, None, "an empty selection must not keep an anchor");
    }

    #[test]
    fn marked_ids_are_sorted_for_stable_prompts() {
        let mut ms = MultiSelect::new();
        ms.enter();
        for id in [9, 3, 5] {
            ms.mark_all(&[id]);
        }
        assert_eq!(ms.ids(), vec![3, 5, 9]);
    }

    #[test]
    fn every_batch_action_has_a_label_and_a_counted_prompt() {
        for action in BatchAction::ALL {
            assert!(!action.label().is_empty());
            let prompt = action.prompt(3);
            assert!(prompt.contains('3'), "{action:?} prompt must state the count: {prompt}");
            assert!(!prompt.contains('\n'), "prompts must stay single-line");
        }
        assert_eq!(BatchAction::ALL.len(), 4);
    }

    #[test]
    fn mark_range_with_extreme_indices_marks_a_clamped_range() {
        // `from`/`to` come from a cursor index; `usize::MAX` must clamp instead
        // of slicing out of bounds (and the empty case must not underflow
        // `len() - 1`).
        let ids: Vec<TunnelId> = (1..=3).collect();
        let mut ms = MultiSelect::new();
        ms.enter();
        ms.mark_range(&ids, usize::MAX, usize::MAX);
        assert_eq!(ms.ids(), vec![3], "both ends clamp to the last row");
        ms.enter();
        ms.mark_range(&ids, 0, usize::MAX);
        assert_eq!(ms.ids(), vec![1, 2, 3]);
        ms.enter();
        ms.mark_range(&[], usize::MAX, 0);
        assert!(ms.is_empty());
        assert_eq!(ms.anchor, None, "an empty view must not record an anchor");
    }
}

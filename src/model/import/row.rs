//! One row of the import table: a discovered forward, its tick mark, and the
//! four columns a user can filter on.
//!
//! The checkbox column is selection state, not data, so it is deliberately not
//! an [`ImportColumn`] — a filter on it would be a second way to express "select
//! all", which is what the row toggles are for.
//!
//! Belongs here: how a forward is presented as a row, and what a column reads.
//! Does not belong: where the rows are stored or filtered (`table`, `filter`).

use super::SshForward;

/// A column of the import table: what a column filter can scope the view by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportColumn {
    Kind,
    Listen,
    Target,
    Raw,
}

impl ImportColumn {
    pub const COUNT: usize = 4;
    /// Columns that accept a filter, left to right.
    pub const ALL: [ImportColumn; ImportColumn::COUNT] =
        [ImportColumn::Kind, ImportColumn::Listen, ImportColumn::Target, ImportColumn::Raw];

    pub fn label(self) -> &'static str {
        match self {
            ImportColumn::Kind => "类型",
            ImportColumn::Listen => "监听",
            ImportColumn::Target => "目标",
            ImportColumn::Raw => "原始指令",
        }
    }

    /// English half of [`ImportColumn::label`], for the bilingual UI.
    pub fn label_en(self) -> &'static str {
        match self {
            ImportColumn::Kind => "Kind",
            ImportColumn::Listen => "Listen",
            ImportColumn::Target => "Target",
            ImportColumn::Raw => "Directive",
        }
    }

    /// Position in [`ImportColumn::ALL`], i.e. the index into
    /// [`ImportTable::filters`](crate::model::ImportTable::filters).
    ///
    /// Derived from `ALL` rather than hand-numbered: the filter array is sized
    /// `COUNT`, so a variant missing from `ALL` must not silently get a
    /// hand-written index that is out of range.
    pub fn index(self) -> usize {
        ImportColumn::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }

    /// `value` as the filter sees it, so the table and the matcher cannot
    /// disagree about what a column contains.
    pub fn cell(self, f: &SshForward) -> String {
        match self {
            ImportColumn::Kind => f.kind.tag().to_string(),
            ImportColumn::Listen => f.listen_display(),
            ImportColumn::Target => f.target_display(),
            ImportColumn::Raw => f.raw.clone(),
        }
    }

    pub fn next(self) -> ImportColumn {
        let i = self.index();
        ImportColumn::ALL.get(i + 1).or_else(|| ImportColumn::ALL.first()).copied().unwrap_or(self)
    }

    pub fn prev(self) -> ImportColumn {
        match self.index() {
            0 => ImportColumn::ALL.last().copied().unwrap_or(self),
            i => ImportColumn::ALL.get(i - 1).copied().unwrap_or(self),
        }
    }
}

/// One candidate row: a forward found in `~/.ssh/config`, plus whether the user
/// wants it created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRow {
    pub forward: SshForward,
    pub checked: bool,
    /// Name of the saved tunnel that already *is* this forward, if any.
    ///
    /// A row with `Some(name)` is still listed — hiding it would make the pane
    /// silently disagree with `~/.ssh/config`, and the user would have no way to
    /// see why a forward they wrote is "missing". It is rendered dimmed with
    /// 「已存在」 and can never be ticked: creating it would be the duplicate this
    /// very marker exists to prevent. See [`ImportRow::existing`] for the flag
    /// checks and `docs/dedup.md` for the identity rule.
    pub existing: Option<String>,
}

impl ImportRow {
    /// `false` for rows we can list but cannot import (unix sockets, a `-R`
    /// with no destination): the table renders them dimmed with the reason.
    pub fn importable(&self) -> bool {
        self.forward.unsupported_reason().is_none()
    }

    /// Whether this row can still be *created*: importable and not already saved.
    ///
    /// This — not [`ImportRow::importable`] — is what the tick marks and the
    /// checked counters use. A row that already exists is perfectly importable as
    /// data; what it cannot be is a new tunnel. Keeping the two predicates apart
    /// is what lets the pane still explain itself ("this forward is already
    /// there") instead of just dropping the line.
    pub fn creatable(&self) -> bool {
        self.importable() && self.existing.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardKind;
    use crate::model::ImportTable;
    use crate::model::fixtures::fwd;

    #[test]
    fn import_columns_cycle_and_expose_their_cells() {
        let mut col = ImportColumn::Kind;
        for _ in 0..ImportColumn::COUNT {
            assert!(!col.label().is_empty());
            col = col.next();
        }
        assert_eq!(col, ImportColumn::Kind, "cycling wraps");
        assert_eq!(ImportColumn::Kind.prev(), ImportColumn::Raw);

        let f = fwd(ForwardKind::Dynamic, 1080, ("", 0));
        assert_eq!(ImportColumn::Kind.cell(&f), "D");
        assert_eq!(ImportColumn::Target.cell(&f), "(SOCKS5)", "a SOCKS row has no target");
    }

    #[test]
    fn every_import_column_index_is_a_valid_filter_slot() {
        // `ImportTable::filters` is `[String; ImportColumn::COUNT]` and is
        // indexed by `ImportColumn::index()`. The two used to be hand-numbered,
        // so a new variant could describe a slot the array does not have.
        let table = ImportTable::empty();
        for col in ImportColumn::ALL {
            let index = col.index();
            assert!(index < ImportColumn::COUNT, "{col:?} maps to {index}, past COUNT");
            assert!(index < table.filters.len(), "{col:?} would index outside the filter array");
        }
    }

    #[test]
    fn import_column_cycling_visits_every_column_from_every_start() {
        // `next`/`prev` used `% COUNT` on a hand-written index; verify the wrap
        // holds for every starting column, in both directions.
        for start in ImportColumn::ALL {
            let mut seen = Vec::new();
            let mut col = start;
            for _ in 0..ImportColumn::COUNT {
                seen.push(col);
                col = col.next();
            }
            assert_eq!(col, start, "next must come back to {start:?}");
            assert_eq!(seen.len(), ImportColumn::COUNT);
            let mut seen = Vec::new();
            let mut col = start;
            for _ in 0..ImportColumn::COUNT {
                seen.push(col);
                col = col.prev();
            }
            assert_eq!(col, start, "prev must come back to {start:?}");
            assert_eq!(seen.len(), ImportColumn::COUNT);
        }
    }
}

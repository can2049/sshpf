//! Bilingual (Chinese + English) help catalog for the whole interface.
//!
//! Every operation the TUI offers — every key in every context, every mouse
//! gesture, every destructive-action gate — is described here **twice**: once in
//! Chinese and once in English. The help overlay renders this data (it is pure
//! data, no rendering lives in this module), and the on-screen hint lines use
//! the same wording so a user never has to guess whether the footer and `?`
//! agree.
//!
//! # Where to add a row
//!
//! The catalog is split by **context**, one file per kind of screen, so "add a
//! key to the form" has an obvious answer. This file is the map and the shared
//! types; only [`SECTIONS`] (the display order) has to be touched here.
//!
//! | file | sections | add a row when… |
//! |---|---|---|
//! | [`main`] | `Main`, `Selection` | the key works on the dashboard or in `v` mode |
//! | [`form`] | `Form`, `FormEditing`, `HostPicker` | the key works in the create/edit form or its host dropdown |
//! | [`import`] | `ImportPane`, `ImportFilter` | the key works in the `~/.ssh/config` import pane |
//! | [`ports`] | `PortPicker` | the key works in the peer's listening-port list (`p`) |
//! | [`dialogs`] | `Confirm`, `Detail`, `FilterBar`, `HelpOverlay` | the key works in a modal |
//! | [`notes`] | `Mouse`, `Status`, `Safety` | it is prose about gestures, glyphs or the safety rules |
//!
//! Why a data module instead of prose inside the renderer:
//!
//! * It can be **tested**. `tests/help_coverage.rs` scans the real key dispatch
//!   in `src/keys.rs` and fails when a dispatched key is not documented here, so
//!   "add a key, forget the help" is a test failure rather than a support ticket.
//! * It can be **counted**. [`SECTIONS`] is the single source of truth for the
//!   overlay's content; the renderer only decides layout, wrap and scroll.
//! * Both languages live next to each other in the source, which is the only
//!   practical way to keep them from drifting apart.
//!
//! # Conventions used by every entry
//!
//! * `keys` is the display form of a binding (`"j / ↓"`, `"ctrl+s"`). An empty
//!   `keys` means the row is prose (a note), rendered indented without a key
//!   column. A row whose `keys` is non-empty must have a *short* `zh`/`en` that
//!   fits one line of the overlay.
//! * `zh` is Chinese, `en` is English, both required. Neither may be empty.
//! * Every destructive action states in both languages that it asks first.

pub mod dialogs;
pub mod form;
pub mod import;
pub mod main;
pub mod notes;
pub mod ports;

/// navigable and lets the coverage test map a dispatch function to exactly one
/// section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionId {
    /// Main dashboard: the tunnel table, no overlay open.
    Main,
    /// Selection mode (`v`): keys are *re-bound*, not added to.
    Selection,
    /// Create/edit form, navigating stage.
    Form,
    /// Create/edit form, text-editing stage.
    FormEditing,
    /// The `~/.ssh/config` host dropdown inside the form.
    HostPicker,
    /// The "import the host's forwards" pane inside the form.
    ImportPane,
    /// A column filter of the import pane being typed into.
    ImportFilter,
    /// The peer's listening-port list (`p` on the 远端端口 field).
    PortPicker,
    /// Confirmation dialog.
    Confirm,
    /// Detail/log overlay.
    Detail,
    /// The `/` filter input on the main table.
    FilterBar,
    /// This overlay's own navigation.
    HelpOverlay,
    /// Mouse gestures (click / shift-click / wheel).
    Mouse,
    /// What the status glyphs and words mean.
    Status,
    /// Cross-cutting rules: the confirmation gate, colour, logs.
    Safety,
}

impl SectionId {
    /// Sections in display order inside the overlay.
    pub const ALL: [SectionId; 15] = [
        SectionId::Main,
        SectionId::Selection,
        SectionId::Form,
        SectionId::FormEditing,
        SectionId::HostPicker,
        SectionId::PortPicker,
        SectionId::ImportPane,
        SectionId::ImportFilter,
        SectionId::Confirm,
        SectionId::Detail,
        SectionId::FilterBar,
        SectionId::HelpOverlay,
        SectionId::Mouse,
        SectionId::Status,
        SectionId::Safety,
    ];

    /// Stable identifier used by the coverage test and the docs.
    pub fn key(self) -> &'static str {
        match self {
            SectionId::Main => "main",
            SectionId::Selection => "selection",
            SectionId::Form => "form",
            SectionId::FormEditing => "form_editing",
            SectionId::HostPicker => "host_picker",
            SectionId::ImportPane => "import_pane",
            SectionId::ImportFilter => "import_filter",
            SectionId::PortPicker => "port_picker",
            SectionId::Confirm => "confirm",
            SectionId::Detail => "detail",
            SectionId::FilterBar => "filter_bar",
            SectionId::HelpOverlay => "help_overlay",
            SectionId::Mouse => "mouse",
            SectionId::Status => "status",
            SectionId::Safety => "safety",
        }
    }
}

/// One documented row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpItem {
    /// Display form of the binding, e.g. `"j / ↓"`, `"ctrl+s"`, `"space"`.
    /// Empty for a prose note.
    pub keys: &'static str,
    /// Chinese description (required).
    pub zh: &'static str,
    /// English description (required).
    pub en: &'static str,
}

impl HelpItem {
    /// A key row.
    pub const fn new(keys: &'static str, zh: &'static str, en: &'static str) -> Self {
        HelpItem { keys, zh, en }
    }

    /// A prose note (no key column).
    pub const fn note(zh: &'static str, en: &'static str) -> Self {
        HelpItem { keys: "", zh, en }
    }

    /// Whether this row is prose rather than a binding.
    pub fn is_note(&self) -> bool {
        self.keys.is_empty()
    }
}

/// One section of the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpSection {
    pub id: SectionId,
    pub title_zh: &'static str,
    pub title_en: &'static str,
    pub items: &'static [HelpItem],
}

/// The whole catalog, in display order.
///
/// The order is fixed by [`SectionId::ALL`], and a unit test asserts the two
/// agree: the overlay draws `SECTIONS` top to bottom, and the coverage test maps
/// each dispatch handler to exactly one section, so a reordering here would
/// silently repoint every "where do I read about this key" answer.
pub const SECTIONS: &[HelpSection] = &[
    main::MAIN,
    main::SELECTION,
    form::FORM,
    form::FORM_EDITING,
    form::HOST_PICKER,
    ports::PORT_PICKER,
    import::IMPORT_PANE,
    import::IMPORT_FILTER,
    dialogs::CONFIRM,
    dialogs::DETAIL,
    dialogs::FILTER_BAR,
    dialogs::HELP_OVERLAY,
    notes::MOUSE,
    notes::STATUS,
    notes::SAFETY,
];

/// Look one section up by id.
///
/// Returns the section whose id is `id`. The catalog is a `const` built from
/// [`SectionId::ALL`]'s order and a unit test asserts every id appears exactly
/// once, so the fallback below is unreachable in a tested build — but it is a
/// fallback and not an `expect`, because the alternative is a panic inside a
/// render loop that would leave the terminal in raw mode with no UI at all. A
/// wrong-but-rendered section is recoverable; a dead process is not.
pub fn section(id: SectionId) -> &'static HelpSection {
    SECTIONS.iter().find(|s| s.id == id).unwrap_or(&SECTIONS[0])
}

/// `中文 English`, the form used by every inline hint line and dialog label.
///
/// A re-export of [`crate::text::bi`]: the crate has exactly one bilingual join,
/// and this alias exists because the call sites in `app`, `keys` and `model`
/// read better as `help::bi` (they are all quoting help-catalog wording).
pub use crate::text::bi;

/// Bilingual text fitted to `width` cells.
///
/// Progressive disclosure for the narrow columns and hint lines: show both
/// languages when they fit, otherwise Chinese only, otherwise a truncation. The
/// English half is never cut mid-word and left dangling — a half English phrase
/// is worse than none — hence the two fallbacks rather than one truncation.
///
/// A thin wrapper over [`crate::text::fit`] with this module's `" "` separator;
/// it stays because `tests/help_coverage.rs` and the existing help tests depend
/// on its exact behaviour, and because a caller in `ui` should not have to know
/// which separator the catalog uses.
pub fn fit(zh: &str, en: &str, width: usize) -> String {
    crate::text::fit(zh, en, " ", width)
}

/// Total number of documented rows across all sections (used by tests and the
/// overlay's scroll clamp).
pub fn item_count() -> usize {
    SECTIONS.iter().map(|s| s.items.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_has_a_title_in_both_languages() {
        for s in SECTIONS {
            assert!(!s.title_zh.is_empty(), "{}: missing Chinese title", s.id.key());
            assert!(!s.title_en.is_empty(), "{}: missing English title", s.id.key());
            assert!(
                s.title_zh.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{}: title_zh has no CJK",
                s.id.key()
            );
        }
    }

    #[test]
    fn every_item_documents_both_languages() {
        for s in SECTIONS {
            for item in s.items {
                assert!(!item.zh.is_empty(), "{}: empty Chinese description for {:?}", s.id.key(), item.keys);
                assert!(!item.en.is_empty(), "{}: empty English description for {:?}", s.id.key(), item.keys);
                assert!(
                    !item.en.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                    "{}: English description contains CJK for {:?}",
                    s.id.key(),
                    item.keys
                );
            }
        }
    }

    /// The catalog's order *is* the overlay's order, and every id must be
    /// reachable exactly once. This is the assertion that lets [`section`] fall
    /// back instead of panicking: with 15 unique ids and a 15-row table, the
    /// lookup can never miss.
    #[test]
    fn sections_are_listed_in_order_and_each_appears_once() {
        assert_eq!(SECTIONS.len(), SectionId::ALL.len());
        for (i, id) in SectionId::ALL.iter().enumerate() {
            assert_eq!(SECTIONS[i].id, *id, "section {i} is out of order");
            let hits = SECTIONS.iter().filter(|s| s.id == *id).count();
            assert_eq!(hits, 1, "{} appears {hits} times", id.key());
        }
    }

    /// [`section`] agrees with the table it looks up, and never panics — the
    /// whole reason the `expect` is gone.
    #[test]
    fn section_lookup_finds_every_id_without_panicking() {
        for id in SectionId::ALL {
            let found = section(id);
            assert_eq!(found.id, id, "section({:?}) returned {:?}", id, found.id);
            assert!(!found.items.is_empty(), "{}: documented but empty", id.key());
        }
        assert_eq!(item_count(), SECTIONS.iter().map(|s| s.items.len()).sum::<usize>());
    }

    #[test]
    fn fit_prefers_both_languages_then_chinese_only() {
        assert_eq!(fit("移动", "Move", 20), "移动 Move");
        // Too narrow for both, wide enough for Chinese.
        assert_eq!(fit("移动", "Move", 5), "移动");
        // Too narrow even for Chinese: truncate.
        let narrow = fit("移动选择", "Move selection", 3);
        assert!(crate::text::display_width(&narrow) <= 3);
    }

    #[test]
    fn notes_have_no_key_column() {
        let note = HelpItem::note("提示", "Note");
        assert!(note.is_note());
        assert!(!HelpItem::new("q", "退出", "Quit").is_note());
    }
}

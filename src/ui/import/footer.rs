//! The pane footer: the selection summary, then the wrapped key hints.
//!
//! The hints are deliberately *not* one clipped line: the pane has a dozen
//! bindings and hiding half of them behind `…` is how a help line stops being
//! help. [`pack_hints`] tries the bilingual form first and falls back to
//! Chinese-only (roughly 40 % narrower) before it will clip anything, so on any
//! realistic pane every key stays on screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::model::{ImportColumn, ImportTable};
use crate::text::{display_width, truncate_to_width};
use crate::ui::palette::Palette;

/// Rows the footer gets: the state summary plus the bilingual key hints. Six is
/// what the twelve bindings need at the form's default width; when the pane is
/// shorter the hints first drop to Chinese-only (denser, so nothing is lost) and
/// only then clip.
pub(super) const FOOTER_H: u16 = 6;

/// Separator between two hints on the same footer row.
const HINT_SEP: &str = " · ";

/// One footer key hint: the binding, its Chinese wording, its English wording.
///
/// Mirrors `keys.rs` exactly; see the module docs for the rules that matter:
/// `a` is a toggle (not "全不选"), there is no `c`/`A`, `space` ticks a row once
/// the pane is open (so the pane closes with `i`/`esc`), and a literal `f`
/// always means "next column" — it can never be typed into a filter.
pub(super) struct Hint {
    pub(super) keys: &'static str,
    pub(super) zh: &'static str,
    pub(super) en: &'static str,
}

impl Hint {
    /// `space 勾选` — used when the bilingual form does not fit one row.
    fn zh_text(&self) -> String {
        format!("{} {}", self.keys, self.zh)
    }

    /// `space 勾选 / Select`
    fn bi_text(&self) -> String {
        format!("{} {} / {}", self.keys, self.zh, self.en)
    }
}

/// Footer key hints, in reading order: the tick and filter keys first (the
/// pane's main job), then movement, then how to leave.
pub(super) const HINTS: &[Hint] = &[
    Hint { keys: "space", zh: "勾选", en: "Select" },
    Hint { keys: "f/←→", zh: "选列", en: "Column" },
    Hint { keys: "F", zh: "清该列", en: "Clear" },
    Hint { keys: "ctrl+r", zh: "清全部", en: "All" },
    Hint { keys: "z", zh: "撤销", en: "Undo" },
    Hint { keys: "a/U/I", zh: "全选/全不选/反选", en: "All/None/Invert" },
    Hint { keys: "j/k ↑↓", zh: "移动", en: "Move" },
    Hint { keys: "g/G", zh: "首尾", en: "Top/End" },
    Hint { keys: "pgup/pgdn", zh: "翻页", en: "Page" },
    Hint { keys: "enter", zh: "结束筛选", en: "Done" },
    Hint { keys: "esc/i", zh: "收起", en: "Close" },
    Hint { keys: "ctrl+s", zh: "保存", en: "Save" },
];

/// The footer: the state summary, then the key hints wrapped to as many rows as
/// the pane can spare.
///
/// The hints are deliberately *not* one clipped line any more: the pane has a
/// dozen bindings and hiding half of them behind `…` is how a help line stops
/// being help. `hints` has already been wrapped by [`pack_hints`], so all this
/// has to do is draw them and clip to the room left above the border.
pub(super) fn draw_footer(
    frame: &mut Frame,
    table: &ImportTable,
    inner: Rect,
    y: u16,
    bottom: u16,
    hints: &[String],
    p: Palette,
) {
    let width = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(1 + hints.len());
    lines.push(Line::from(Span::styled(
        truncate_to_width(&footer_summary(table), width),
        p.fg(p.accent).add_modifier(Modifier::BOLD),
    )));
    for hint in hints {
        lines.push(Line::from(Span::styled(truncate_to_width(hint, width), p.fg(p.dim))));
    }
    let height = bottom.saturating_sub(y).min(lines.len() as u16);
    lines.truncate(height as usize);
    frame.render_widget(Paragraph::new(lines), Rect { x: inner.x, y, width: inner.width, height });
}

/// The footer's first line: `2/3 条勾选 / 2/3 checked · 筛选: 类型=R / filter: 类型=R`.
///
/// Counts come from the table's public API rather than reusing
/// [`ImportTable::summary`], so the English half is composed here next to the
/// Chinese one and the two cannot drift apart.
pub(super) fn footer_summary(table: &ImportTable) -> String {
    let checked = table.checked_count();
    let importable = table.importable_count();
    let mut s = format!("{checked}/{importable} 条勾选 / {checked}/{importable} checked");
    if table.has_filters() {
        let active = ImportColumn::ALL.into_iter().filter(|c| !table.filter(*c).trim().is_empty());
        let zh: Vec<String> = active.clone().map(|c| format!("{}={}", c.label(), table.filter(c))).collect();
        let en: Vec<String> = active.map(|c| format!("{}={}", c.label_en(), table.filter(c))).collect();
        s.push_str(&format!(" · 筛选: {} / filter: {}", zh.join(", "), en.join(", ")));
    }
    s
}

/// Wrap the [`HINTS`] into at most `max_lines` rows of `width` cells.
///
/// Two passes, because the pane must never *lose* a binding:
///
/// 1. bilingual (`space 勾选 / Select`) — the normal case, and the only one on a
///    wide pane;
/// 2. Chinese only (`space 勾选`), which is roughly 40 % narrower. On a pane too
///    narrow for the bilingual form this is what keeps every key on screen
///    instead of dropping the tail behind `…`.
///
/// Only if even the Chinese pass needs more rows than exist does this clip, and
/// then the *whole* footer is at its narrowest, where a truncated last row is
/// the honest limit rather than a silently unreachable key.
pub(super) fn pack_hints(width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    if let Some(full) = wrap_hints(width, max_lines, false) {
        return full;
    }
    // Chinese-only is much denser; retry before giving up on any binding.
    wrap_hints(width, max_lines, true).unwrap_or_else(|| {
        let lines = wrap_hints(width, usize::MAX, true).unwrap_or_default();
        lines.into_iter().take(max_lines).collect()
    })
}

/// One wrapping pass. `chinese_only` picks the compact form every hint uses.
///
/// Returns `None` when the hints need more than `max_lines` rows, so the caller
/// can retry in a denser form rather than silently truncating the list.
pub(super) fn wrap_hints(width: usize, max_lines: usize, chinese_only: bool) -> Option<Vec<String>> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for hint in HINTS {
        let mut text = if chinese_only { hint.zh_text() } else { hint.bi_text() };
        if display_width(&text) > width {
            // Not even this form fits one row: the compact Chinese one might, and
            // otherwise the binding is truncated but still named.
            text = hint.zh_text();
            if display_width(&text) > width {
                text = truncate_to_width(&text, width);
            }
        }
        let w = display_width(&text);
        let sep_w = if cur.is_empty() { 0 } else { display_width(HINT_SEP) };
        if !cur.is_empty() && cur_w + sep_w + w > width {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
            if lines.len() > max_lines {
                return None;
            }
        }
        if !cur.is_empty() {
            cur.push_str(HINT_SEP);
            cur_w += sep_w;
        }
        cur.push_str(&text);
        cur_w += w;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    (lines.len() <= max_lines).then_some(lines)
}
#[cfg(test)]
mod tests {
    use super::*;

    use crate::model::{ForwardKind, SshForward};

    fn forward(kind: ForwardKind, listen_port: u16, raw: &str) -> SshForward {
        SshForward {
            kind,
            listen_addr: String::new(),
            listen_port,
            target_host: "127.0.0.1".to_string(),
            target_port: 3000,
            host_alias: "dev-box".to_string(),
            line: 7,
            raw: raw.to_string(),
        }
    }

    #[test]
    fn footer_hints_cover_every_binding_once() {
        // The pane's whole point is that no key is a secret; a dropped hint is a
        // regression, so every binding the module documents is asserted here.
        // The token set is also cross-checked against `HINTS` itself, so adding a
        // binding without covering it here is a test failure too.
        let expected =
            ["space", "f/←→", "F", "ctrl+r", "z", "a/U/I", "j/k ↑↓", "g/G", "pgup/pgdn", "enter", "esc/i", "ctrl+s"];
        assert_eq!(expected.len(), HINTS.len(), "a hint was added to HINTS without a coverage entry");
        let packed = pack_hints(120, 4).join(" ");
        for token in expected {
            assert!(packed.contains(token), "footer hint {token:?} missing from {packed:?}");
        }
        for hint in HINTS {
            assert!(packed.contains(hint.keys), "binding {:?} lost: {packed:?}", hint.keys);
            assert!(packed.contains(hint.zh), "Chinese half of {:?} lost: {packed:?}", hint.keys);
            assert!(packed.contains(hint.en), "English half of {:?} lost: {packed:?}", hint.keys);
        }
    }

    #[test]
    fn every_binding_survives_a_narrow_or_short_pane() {
        // The real failure mode this guards: clipping the footer's tail at a
        // realistic size, which would make the last few keys undiscoverable.
        // Chinese-only wrapping is what keeps all twelve on screen.
        for (width, rows) in [(120usize, 4usize), (100, 4), (80, 4), (60, 5), (60, 4)] {
            let packed = pack_hints(width, rows).join(" ");
            for hint in HINTS {
                assert!(packed.contains(hint.keys), "{width}x{rows} lost {:?}: {packed:?}", hint.keys);
            }
            assert!(pack_hints(width, rows).len() <= rows, "{width}: more rows than allowed");
        }
    }

    #[test]
    fn footer_hints_wrap_within_the_width_and_degrade_to_chinese() {
        for width in [12usize, 24, 40, 60, 120] {
            for line in pack_hints(width, 6) {
                assert!(display_width(&line) <= width, "hint line overflows {width}: {line:?}");
            }
        }
        // Every line is non-empty: an empty row would waste the footer budget.
        assert!(pack_hints(80, 4).iter().all(|l| !l.is_empty()));
        // A narrow pane keeps the key tokens (Chinese only) rather than dropping
        // the binding entirely.
        let narrow = pack_hints(20, 8).join(" ");
        assert!(narrow.contains("space"), "{narrow:?}");
        // Zero width/rows must not panic or produce content.
        assert!(pack_hints(0, 4).is_empty());
        assert!(pack_hints(80, 0).is_empty());
    }

    #[test]
    fn footer_summary_is_bilingual_and_counts_importable_rows() {
        let table = ImportTable::new(vec![
            forward(ForwardKind::Local, 8080, "LocalForward 8080 127.0.0.1:3000"),
            // Un-importable: it must not inflate the denominator.
            forward(ForwardKind::Local, 0, "LocalForward abc 127.0.0.1:5432"),
        ]);
        let s = footer_summary(&table);
        assert!(s.contains("1/1 条勾选"), "{s}");
        assert!(s.contains("1/1 checked"), "{s}");

        let mut filtered = ImportTable::new(vec![forward(ForwardKind::Local, 8080, "LocalForward 8080 x:1")]);
        filtered.set_filter(ImportColumn::Kind, "L");
        let s = footer_summary(&filtered);
        assert!(s.contains("筛选: 类型=L"), "{s}");
        assert!(s.contains("filter: Kind=L"), "{s}");
    }
}

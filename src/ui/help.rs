//! The `?` overlay: the whole bilingual catalog, laid out and scrolled.
//!
//! The catalog itself is [`crate::help::SECTIONS`] (pure data); this file only
//! decides **layout**: a measured key column, wrapping that keeps both languages
//! readable, and indentation for prose notes.
//!
//! [`help_lines`] is deliberately the single source of truth for *both* the
//! renderer and the scroll clamp in [`crate::ui::frame`]: the maximum scroll
//! offset must be the height of the wrapped result minus the viewport, and
//! wrapping is width-dependent, so counting catalog entries would either lie
//! about "more content below" or strand the last rows off screen.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::text::{display_width, pad_to_width, truncate_to_width};
use crate::ui::frame::{centered_pct, clear_and_block, popup_block};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::palette::{Palette, palette};
use crate::ui::text::wrap_text;

// ---------------------------------------------------------------------------
// Overlay: Help
// ---------------------------------------------------------------------------

/// Narrowest key column: below this the two-cell indent plus a binding no
/// longer reads as a column, and descriptions are better off full width.
const HELP_KEY_MIN: usize = 10;

/// Widest key column. The catalog contains long alternatives
/// (`J / K / shift+↓ / shift+↑`), and a column sized for the worst case would
/// starve every description on a normal terminal — those few rows are allowed to
/// put their keys on their own line instead (see [`item_lines`]).
const HELP_KEY_MAX: usize = 22;

/// Cells of left indent for a prose note.
const HELP_NOTE_INDENT: usize = 2;

/// Width of the key column, measured from the catalog.
///
/// It must be **measured, not guessed**: a hard-coded 12 was narrower than
/// `g / G / home / end` (18 cells) and `J / K / shift+↓ / shift+↑` (25 cells).
/// Since `pad_to_width` pads but never truncates, those rows ran straight into
/// their description with no gap — `g / G / home / end跳到首行`.
///
/// Both widths come from [`display_width`], never from counting by hand: this
/// doc first claimed the longest binding was 21 cells when it is 25. Every arrow
/// glyph is single width, so that was pure miscounting rather than a width
/// subtlety — and a wrong *number* in a comment is harmless once measured, while
/// a wrong *explanation* passes silently into the next reader's assumptions.
///
/// Measuring the catalog keeps every row aligned and can never collide with the
/// next column, and the [`HELP_KEY_MAX`] / half-the-row caps keep a single long
/// binding from eating the entire line.
pub(crate) fn help_key_col(width: usize) -> usize {
    if width < 4 {
        return 0;
    }
    let widest = crate::help::SECTIONS
        .iter()
        .flat_map(|s| s.items.iter())
        .filter(|i| !i.is_note())
        .map(|i| display_width(i.keys) + 2)
        .max()
        .unwrap_or(HELP_KEY_MIN);
    let want = widest.clamp(HELP_KEY_MIN, HELP_KEY_MAX);
    // Never let the key column starve the description: at most half the row.
    let cap = (width / 2).max(HELP_KEY_MIN).min(width - 1);
    want.min(cap)
}

pub(crate) fn help_inner_width(area: Rect) -> u16 {
    // `popup_block` pads one cell on each side and draws a border on each side.
    centered_pct(area, 78, 84).width.saturating_sub(4)
}

pub(crate) fn help_inner_height(area: Rect) -> u16 {
    centered_pct(area, 78, 84).height.saturating_sub(2)
}

/// Lay the whole [`crate::help::SECTIONS`] catalog out into the lines the overlay
/// draws, wrapped to `width` cells.
///
/// This is the single source of truth for *both* the renderer and the scroll
/// clamp (`clamp_scrolls`): the maximum scroll offset must be the height of the
/// wrapped result minus the viewport, not the number of catalog entries. Wrapping
/// is width-dependent — a 40-cell popup needs far more rows than a 120-cell one —
/// so counting entries would either lie ("more content below" that is not there)
/// or strand the last rows off screen.
///
/// Row shapes:
///
/// * section header — `中文 · English`, accent + bold;
/// * key row — fixed key column, then `中文 English`; when the pair does not fit
///   on the remainder, the English moves to a continuation row indented past the
///   key column, so both languages are always readable;
/// * note row (`keys` empty) — dim prose, indented.
///
/// A pure function of `(width, palette)` on purpose: it is unit-tested directly,
/// including the invariant that every produced line fits `width`.
pub(crate) fn help_lines(width: usize, p: Palette) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut out: Vec<Line<'static>> = Vec::new();

    for section in crate::help::SECTIONS {
        // Header. `中文 · English` is the section title format the spec fixes.
        let title = format!(" {} · {} ", section.title_zh, section.title_en);
        if !out.is_empty() {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled(
            truncate_to_width(&title, width),
            p.fg(p.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));

        for item in section.items {
            if item.is_note() {
                // Prose: no key column, wrapped as its own paragraph so a long
                // safety rule does not get cut to one row.
                out.extend(note_lines(item.zh, item.en, width, p));
                continue;
            }
            out.extend(item_lines(item.keys, item.zh, item.en, width, p));
        }
    }
    out
}

/// One binding, as one or more rendered rows.
///
/// `key  中文 English` when it fits. When the pair does not fit, the text wraps:
/// Chinese first, English beneath it, both aligned to the description column —
/// never truncated, because a half sentence in either language is the failure
/// this feature exists to avoid.
///
/// Two rules make the column safe, and both were bugs before they were rules:
///
/// * a row whose key would leave **no gap** before the description (because the
///   binding is wider than the measured column) puts the key on its own line and
///   starts the description on the next one, rather than gluing the two together;
/// * the description is always wrapped, never cut, so no language can be lost to
///   a narrow popup.
pub(crate) fn item_lines(keys: &str, zh: &str, en: &str, width: usize, p: Palette) -> Vec<Line<'static>> {
    let key_col = help_key_col(width);
    let text_w = width.saturating_sub(key_col).max(1);
    let key_style = p.fg(p.accent).add_modifier(Modifier::BOLD);
    let zh_style = p.fg(p.text);
    let en_style = p.fg(p.dim);
    let key_text = format!("  {keys}");
    let indent = " ".repeat(key_col);
    let mut rows: Vec<Line<'static>> = Vec::new();

    // The binding must be followed by at least one space before the description.
    if display_width(&key_text) < key_col {
        let key_span = Span::styled(pad_to_width(&key_text, key_col), key_style);
        // Both languages on one row?
        if display_width(zh) + 1 + display_width(en) <= text_w {
            return vec![Line::from(vec![
                key_span,
                Span::styled(zh.to_string(), zh_style),
                Span::styled(" ", en_style),
                Span::styled(en.to_string(), en_style),
            ])];
        }
        let mut first = Some(key_span);
        for part in wrap_text(zh, text_w) {
            let lead = first.take().unwrap_or_else(|| Span::raw(indent.clone()));
            rows.push(Line::from(vec![lead, Span::styled(truncate_to_width(&part, text_w), zh_style)]));
        }
        for part in wrap_text(en, text_w) {
            rows.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(truncate_to_width(&part, text_w), en_style),
            ]));
        }
        return rows;
    }

    // The binding is wider than its column: give it a row of its own so it can
    // never run into the description, then indent the text to the key column.
    rows.push(Line::from(Span::styled(truncate_to_width(&key_text, width), key_style)));
    let indent_n = key_col.min(width.saturating_sub(1));
    let text_w = width.saturating_sub(indent_n).max(1);
    let indent = " ".repeat(indent_n);
    for part in wrap_text(zh, text_w) {
        rows.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(truncate_to_width(&part, text_w), zh_style),
        ]));
    }
    for part in wrap_text(en, text_w) {
        rows.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(truncate_to_width(&part, text_w), en_style),
        ]));
    }
    rows
}

/// A prose note: Chinese, then its English translation beneath it.
pub(crate) fn note_lines(zh: &str, en: &str, width: usize, p: Palette) -> Vec<Line<'static>> {
    let indent = HELP_NOTE_INDENT.min(width);
    let text_w = width.saturating_sub(indent);
    let mut rows: Vec<Line<'static>> = Vec::new();
    for part in wrap_text(zh, text_w) {
        rows.push(Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(truncate_to_width(&part, text_w), p.fg(p.text)),
        ]));
    }
    for part in wrap_text(en, text_w) {
        rows.push(Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(truncate_to_width(&part, text_w), p.fg(p.dim)),
        ]));
    }
    rows
}

pub(crate) fn draw_help(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let p = palette();
    let popup = centered_pct(area, 78, 84);
    // Modals are drawn after the table, so they must record themselves: without
    // this, a click on the dialog would still hit the table row behind it.
    hits.push(popup, HitTarget::Popup);
    // Title and footer both bilingual; the footer is the only place the user
    // learns how to scroll this thing, so it stays truthful to `handle_help_key`
    // (`j/k`, `↑↓`, `pgup/pgdn`, and `? / esc / q / enter` all close).
    let block = popup_block("帮助 · Help", p).title_bottom(Line::from(Span::styled(
        " j/k ↑↓ 滚动 · pgup/pgdn 翻页 · ? esc 关闭 / Scroll · Page · Close ",
        p.fg(p.dim),
    )));
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let lines = help_lines(inner.width as usize, p);
    frame.render_widget(Paragraph::new(lines).scroll((app.help_scroll, 0)), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::draw;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::ui::palette::palette_with;
    use crate::ui::test_support::*;

    /// Flatten help lines to text, one row per line.
    fn help_text(width: usize) -> String {
        help_lines(width, palette_with(false)).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn help_overlay_keeps_both_languages_of_every_item() {
        // The rendering counterpart of `tests/help_coverage.rs`: that test proves
        // every dispatched key is *documented*, this one proves every documented
        // row actually *reaches the screen* in both languages. The bug it was
        // written for: a binding wider than the key column ran into its own
        // description (`g / G / home / end跳到首行`), and a squeezed description
        // could lose the English half entirely.
        //
        // Only the first few cells of each half are compared: wrapping splits
        // phrases across rows, so a whole-string `contains` would fail on a
        // correct render. The prefixes are what a reader needs to recognise the
        // row.
        // Whitespace is normalised away on both sides: wrapping legitimately
        // moves a space to a line break (`等同 q…` -> `等同` / `q…`), and this
        // test is about *content*, not spacing — the no-glue rule has its own
        // test below.
        let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        for width in [40usize, 60, 78, 110] {
            let text = squash(&help_text(width));
            for section in crate::help::SECTIONS {
                // Section titles are bilingual too.
                assert!(
                    text.contains(&squash(section.title_zh)),
                    "width {width}: section title {:?} missing",
                    section.title_zh
                );
                assert!(
                    text.contains(&squash(section.title_en)),
                    "width {width}: section title {:?} missing",
                    section.title_en
                );
                for item in section.items {
                    let zh_prefix: String = item.zh.chars().take(4).collect();
                    let en_prefix: String = item.en.chars().take(6).collect();
                    assert!(
                        text.contains(&squash(&zh_prefix)),
                        "width {width}: {} item {:?} lost its Chinese ({zh_prefix:?})",
                        section.id.key(),
                        item.keys
                    );
                    assert!(
                        text.contains(&squash(&en_prefix)),
                        "width {width}: {} item {:?} lost its English ({en_prefix:?})",
                        section.id.key(),
                        item.keys
                    );
                }
            }
        }
    }

    #[test]
    fn help_overlay_never_glues_a_binding_to_its_description() {
        // Regression: `pad_to_width` pads but never truncates, so a key column
        // narrower than the binding produced `g / G / home / end跳到首行` with no
        // separating space at all.
        for width in [40usize, 60, 78, 110] {
            for line in help_lines(width, palette_with(false)) {
                let text = line.to_string();
                // No CJK may directly follow an ASCII word character anywhere in
                // the key column region: that is exactly the glue signature.
                let glued = text.chars().collect::<Vec<_>>().windows(2).any(|w| {
                    let (a, b) = (w[0], w[1]);
                    a.is_ascii_alphanumeric() && (('\u{4e00}'..='\u{9fff}').contains(&b))
                });
                assert!(!glued, "width {width}: binding glued to description: {text:?}");
            }
        }
    }

    #[test]
    fn help_lines_fit_the_width_at_every_popup_size() {
        // Every produced row must fit, or the overlay bleeds past its border.
        for width in [8usize, 12, 20, 24, 30, 40, 50, 60, 78, 110, 200] {
            for line in help_lines(width, palette_with(false)) {
                let w = display_width(&line.to_string());
                assert!(w <= width, "width {width}: row is {w} cells: {line:?}");
            }
        }
    }

    #[test]
    fn help_key_column_is_measured_from_the_catalog() {
        // Long alternatives exist in the catalog, so a fixed 12-cell column was
        // structurally too narrow; it must be derived from the data.
        let widest = crate::help::SECTIONS
            .iter()
            .flat_map(|s| s.items.iter())
            .filter(|i| !i.is_note())
            .map(|i| display_width(i.keys) + 2)
            .max()
            .unwrap();
        assert!(
            help_key_col(120) >= widest.min(HELP_KEY_MAX),
            "key column {} does not fit the widest binding {widest}",
            help_key_col(120)
        );
        // ... but it may never eat the line on a narrow popup.
        for width in [10usize, 16, 24, 40] {
            assert!(help_key_col(width) <= width / 2 + HELP_KEY_MIN, "width {width} key column too wide");
        }
    }

    #[test]
    fn help_scroll_limit_is_measured_from_wrapped_rows() {
        // The clamp must count wrapped rows, not catalog entries: wrapping makes
        // the row count width-dependent, and a clamp that used `item_count()`
        // would either stop short of the end or report content that is not there.
        let p = palette_with(false);
        let narrow = help_lines(60, p).len();
        let wide = help_lines(160, p).len();
        assert!(narrow > wide, "narrow render must wrap to more rows ({narrow} vs {wide})");
        assert!(wide >= crate::help::item_count(), "eleven rows per section cannot be fewer than the items");

        let mut app = app_with_tunnels(vec![]);
        app.open_help();
        // Tall enough that the whole catalog fits: there must be nothing to scroll.
        let backend = TestBackend::new(200, 70);
        let mut term = Terminal::new(backend).unwrap();
        let _ = term.draw(|f| draw(f, &mut app));
        assert_eq!(app.help_scroll, 0);

        // Short terminal: the clamp equals rows minus viewport, not a guess.
        app.help_scroll = 9999;
        let _ = render_to_text(&mut app, 120, 35);
        let expected = help_lines(help_inner_width(Rect::new(0, 0, 120, 35)) as usize, palette())
            .len()
            .saturating_sub(help_inner_height(Rect::new(0, 0, 120, 35)) as usize);
        assert_eq!(app.help_scroll as usize, expected, "clamp must be rows - viewport");
        assert!(app.help_scroll > 0, "the catalog is longer than a 35-row terminal");
    }

    #[test]
    fn renders_help_overlay_and_scrolls() {
        let mut app = app_with_tunnels(vec![]);
        app.open_help();
        let top = render_to_text(&mut app, 120, 35);
        assert!(top.contains("帮助 · Help"), "bilingual help title missing:\n{top}");
        // The overlay is sectioned now, and each section title is bilingual.
        assert!(top.contains("主界面 · Main screen"), "first section header missing:\n{top}");
        // The first rows of that section, in both languages on the same row.
        assert!(top.contains("向下移动光标"), "first help row missing:\n{top}");
        assert!(top.contains("Move the cursor down"), "first help row needs its English:\n{top}");
        // The footer teaches this overlay's own keys, also bilingual.
        assert!(top.contains("滚动"), "scroll hint missing:\n{top}");
        assert!(top.contains("Scroll"), "scroll hint needs its English:\n{top}");

        // Scrolling hides the top of the catalog and lets later content in. The
        // offset is clamped against the *wrapped* row count, not the item count.
        let first_row = "Move the cursor down";
        assert!(top.contains(first_row), "precondition: first row is visible before scrolling");
        app.help_scroll = 40;
        let scrolled = render_to_text(&mut app, 120, 35);
        assert!(app.help_scroll > 0, "help_scroll must survive the clamp");
        assert!(!scrolled.contains(first_row), "scroll had no effect:\n{scrolled}");
        assert_ne!(top, scrolled, "scrolling must change what is on screen");

        // And the clamp is exact: rows minus viewport, both measured by the same
        // function the renderer uses.
        app.help_scroll = u16::MAX;
        let _ = render_to_text(&mut app, 120, 35);
        let area = Rect::new(0, 0, 120, 35);
        let expected = help_lines(help_inner_width(area) as usize, palette())
            .len()
            .saturating_sub(help_inner_height(area) as usize);
        assert_eq!(app.help_scroll as usize, expected, "clamp must be wrapped rows - viewport");
    }
}

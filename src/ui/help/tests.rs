//! Unit tests for `help.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

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
    let expected =
        help_lines(help_inner_width(area) as usize, palette()).len().saturating_sub(help_inner_height(area) as usize);
    assert_eq!(app.help_scroll as usize, expected, "clamp must be wrapped rows - viewport");
}

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
mod tests;

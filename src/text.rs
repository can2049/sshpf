//! The crate's single string layer: widths, truncation, bilingual joins, wrapping
//! and glob matching.
//!
//! # Why one module
//!
//! Every one of these used to exist twice — `ui` had its own width table and its
//! own bilingual "fit" ladder, `ssh` had a second `glob_match`, the help catalog
//! had a third copy of the fit ladder. The copies drifted (the renderer knew
//! about emoji, the classifier did not), which is exactly the class of bug a
//! single implementation prevents.
//!
//! **If you need a string operation, it belongs here** — not in `ui`, not in
//! `help`, not in `ssh`. This module has no I/O, no `ratatui`, no knowledge of
//! the domain model.
//!
//! # Index
//!
//! | need | function |
//! |---|---|
//! | terminal cells of one char / a string | [`char_width`] / [`display_width`] |
//! | cut to fit a cell budget | [`truncate_to_width`] |
//! | pad to a cell budget | [`pad_to_width`] |
//! | `中文 English` (the default bilingual join) | [`bi`] |
//! | bilingual text that must fit a cell budget | [`fit`] |
//! | greedy wrap to a cell budget | [`wrap`] |
//! | `*` / `?` glob match | [`glob_match`] |
//!
//! Widths come from `unicode-width` (UAX #11). Two rules follow from that and
//! are load-bearing:
//!
//! * **[`display_width`] measures the whole string, never the sum of its
//!   chars.** Zero-width joiners and combining marks make the two differ: the
//!   ZWJ family emoji `👨‍👩‍👧` is one 2-cell glyph, while adding up per-char
//!   widths gives 6–8. Measuring the string is what the terminal does, so it is
//!   what the renderer must agree with.
//! * **A character may be zero cells wide**, so every loop that walks chars to
//!   build up a line checks the *budget*, not `used + w`. See
//!   [`truncate_to_width`] and [`wrap`].
//!
//! `unicode-width` was deliberately avoided before this, on the belief that the
//! hand-written table was enough. It was not: the old table could not return 0,
//! so combining marks were counted as a cell and every ZWJ sequence was
//! over-measured. See [`docs/design.md`](../../docs/design.md) D37.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Width of a single character in terminal cells (0, 1 or 2).
///
/// `0` for combining marks, zero-width joiners, variation selectors and
/// Hangul medial/final jamo. Control characters report `1` (they are not
/// rendered as a glyph but they do occupy the cursor position they were written
/// at, and treating them as 0 would let a control byte hide inside a cell
/// budget); `unicode-width` returns `None` for them, so they are mapped
/// explicitly rather than falling into the zero case.
pub fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1)
}

/// Terminal cell width of a string — measured as a string, not summed per char.
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate to `width` cells, appending `…` when content is dropped.
///
/// The cut point is found by measuring each prefix as a string instead of
/// accumulating per-char widths: a char can be zero cells wide, and a ZWJ
/// sequence only knows its width once the sequence is complete, so an
/// accumulate-then-compare loop can overshoot the budget or split a grapheme.
pub fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(s) <= width {
        return s.to_string();
    }
    let budget = width.saturating_sub(1); // room for the ellipsis
    let mut end = 0;
    for (idx, _) in s.char_indices().skip(1) {
        if display_width(&s[..idx]) > budget {
            break;
        }
        end = idx;
    }
    format!("{}…", &s[..end])
}

/// Pad `s` with spaces to `width` cells (never truncates).
pub fn pad_to_width(s: &str, width: usize) -> String {
    let cur = display_width(s);
    if cur >= width {
        return s.to_string();
    }
    let mut out = s.to_string();
    out.push_str(&" ".repeat(width - cur));
    out
}

// ---------------------------------------------------------------------------
// bilingual joins
// ---------------------------------------------------------------------------

/// `中文 English` — the default bilingual join used by prose, hint lines and
/// dialog labels.
///
/// Chinese first is a hard convention: a truncated line still reads, and every
/// `contains("中文")` assertion in the tests keeps meaning something.
pub fn bi(zh: &str, en: &str) -> String {
    format!("{zh} {en}")
}

/// Bilingual text fitted to a `width`-cell budget, with an explicit separator.
///
/// Progressive disclosure, in order:
///
/// 1. both languages (`zh{sep}en`) when they fit;
/// 2. Chinese only when the pair is too wide;
/// 3. a truncation of the Chinese when even that does not fit.
///
/// The English half is never cut mid-word and left dangling — half an English
/// phrase is worse than none, hence the ladder rather than a single truncation.
///
/// `sep` lets the callers keep their two shapes: `" "` for prose and hint lines
/// (`中文 English`), `"/"` for table cells and buttons (`中文/English`), which
/// have no room for spaces.
pub fn fit(zh: &str, en: &str, sep: &str, width: usize) -> String {
    let both = format!("{zh}{sep}{en}");
    if display_width(&both) <= width {
        return both;
    }
    if display_width(zh) <= width {
        return zh.to_string();
    }
    truncate_to_width(zh, width)
}

// ---------------------------------------------------------------------------
// wrapping
// ---------------------------------------------------------------------------

/// Greedy wrap to `width` cells.
///
/// A CJK sentence has no spaces to break on, so the fallback is a hard break at
/// a character boundary; Latin text prefers the last space so words stay whole.
/// Overflow is impossible in practice because every produced line is truncated
/// to `width` at render time, and the *count* of lines is what the scroll clamps
/// measure.
///
/// Width is re-measured on the candidate line rather than accumulated, for the
/// same reason as [`truncate_to_width`]: chars can be zero cells wide, and a ZWJ
/// sequence's width is not the sum of its parts.
///
/// Never returns an empty vector: empty input yields one empty line, so callers
/// can assume `wrap(..).len() >= 1`.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    // Reused across iterations: finding out whether `ch` overflows means building
    // the candidate line, and a fresh `clone()` per char would allocate once per
    // character of every wrapped line.
    let mut candidate = String::new();
    for ch in text.chars() {
        // Would adding `ch` overflow the line? Measure the candidate, do not
        // accumulate: `char_width` alone cannot see that the joiner just made a
        // grapheme *narrower* than the two emoji it sits between.
        candidate.clear();
        candidate.push_str(&cur);
        candidate.push(ch);
        if display_width(&candidate) > width && !cur.is_empty() {
            if let Some(pos) = cur.rfind(' ') {
                let rest = cur[pos + 1..].to_string();
                cur.truncate(pos);
                out.push(std::mem::take(&mut cur));
                cur = rest;
            } else {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

// ---------------------------------------------------------------------------
// glob matching
// ---------------------------------------------------------------------------

/// `*` / `?` glob match, iterative and allocation-light.
///
/// Used by `~/.ssh/config` `Host` patterns (case-insensitive, per OpenSSH) and by
/// `known_hosts` host patterns (case-sensitive, per the file format). One
/// implementation with an `ignore_case` flag instead of two copies that drifted.
///
/// `*` matches any run of characters, `?` exactly one. No bracket classes: real
/// ssh configs use `*`/`?` and a half-implemented `[...]` would be worse than
/// none.
pub fn glob_match(pattern: &str, text: &str, ignore_case: bool) -> bool {
    let fold = |c: char| if ignore_case { c.to_lowercase().next().unwrap_or(c) } else { c };
    let p: Vec<char> = pattern.chars().map(fold).collect();
    let t: Vec<char> = text.chars().map(fold).collect();
    // Iterative backtracking match: linear in practice, O(n*m) worst case, no
    // recursion (a hostile pattern cannot blow the stack).
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_cover_cjk_emoji_and_ascii() {
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
        // Emoji count as wide: the UI and the classifier must agree here.
        assert_eq!(display_width("🚀"), 2);
        assert_eq!(display_width("✓"), 1);
    }

    /// A zero-width joiner sequence is ONE 2-cell glyph, not the sum of its
    /// parts. This is the case the old hand-written table got wrong: it summed
    /// per-char widths, so `👨‍👩‍👧` measured 8 cells and every row containing one
    /// was pushed out of alignment.
    #[test]
    fn a_zwj_sequence_is_one_glyph_not_a_sum_of_its_parts() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"; // 👨‍👩‍👧
        assert_eq!(display_width(family), 2, "one 2-cell glyph");
        assert_eq!(family.chars().count(), 5, "five code points make it up");
        // The naive sum is much larger; guard the distinction explicitly so a
        // future "optimisation" back to per-char summing fails here.
        let summed: usize = family.chars().map(char_width).sum();
        assert!(summed > 2, "per-char summing must NOT agree: {summed}");

        // An emoji + modifier sequence is likewise one glyph.
        let thumbs = "\u{1F44D}\u{1F3FD}"; // 👍🏽
        assert_eq!(display_width(thumbs), 2);
    }

    /// A combining mark occupies no cell of its own.
    #[test]
    fn a_combining_mark_adds_no_width() {
        assert_eq!(char_width('\u{0301}'), 0, "combining acute accent");
        assert_eq!(char_width('\u{200D}'), 0, "zero-width joiner");
        assert_eq!(char_width('\u{FE0F}'), 0, "variation selector-16");
        assert_eq!(display_width("e\u{0301}"), 1, "e + combining acute is one cell");
        assert_eq!(display_width("e\u{0301}"), display_width("é"));
    }

    /// Truncation must not split a grapheme, and must stay within budget even
    /// when the text is made of multi-cell glyphs.
    #[test]
    fn truncate_keeps_a_zwj_sequence_whole() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"; // 👨‍👩‍👧
        let text = format!("a{family}b");
        assert_eq!(display_width(&text), 4, "1 + 2 + 1");
        // Budget 3 leaves room for 'a', the glyph, and the ellipsis.
        let cut = truncate_to_width(&text, 3);
        assert!(display_width(&cut) <= 3, "budget respected: {cut:?}");
        assert!(cut.ends_with('…'), "{cut:?}");
        // The joiner must not survive without the emoji that follow it: a cut
        // inside the sequence would produce a lone ZWJ and a broken glyph.
        assert!(!cut.trim_end_matches('…').ends_with('\u{200D}'), "truncation split the ZWJ sequence: {cut:?}");
    }

    /// Wrapping measures the candidate line, so a zero-width char never forces
    /// a spurious break and a wide glyph never overflows.
    #[test]
    fn wrap_measures_the_candidate_line() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"; // 👨‍👩‍👧
        // Two glyphs are 4 cells; at width 4 they fit on one line.
        let two = format!("{family}{family}");
        assert_eq!(display_width(&two), 4);
        assert_eq!(wrap(&two, 4).len(), 1, "two 2-cell glyphs fit in 4 cells");

        // Text with combining marks wraps on the plain chars, not on the marks.
        let lines = wrap("e\u{0301}e\u{0301}e\u{0301}", 2);
        assert!(lines.iter().all(|l| display_width(l) <= 2), "{lines:?}");
        assert_eq!(lines.concat(), "e\u{0301}e\u{0301}e\u{0301}", "no characters lost");
    }

    #[test]
    fn truncate_never_exceeds_the_budget() {
        assert_eq!(truncate_to_width("abc", 5), "abc");
        assert_eq!(truncate_to_width("abc", 0), "");
        assert_eq!(display_width(&truncate_to_width("abcdef", 3)), 3);
        let t = truncate_to_width("中文字", 4);
        assert!(display_width(&t) <= 4, "{t:?}");
        assert!(t.ends_with('…'));
    }

    /// The budget must hold for *every* width against *every* shape of text,
    /// including the cases where a char is zero cells wide or a glyph is two.
    /// A single accumulate-then-compare loop used to satisfy the easy rows and
    /// miss these.
    #[test]
    fn truncate_respects_the_budget_for_any_width_and_text_shape() {
        let cases = [
            "",
            "abc",
            "中文字",
            "a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}b", // a👨‍👩‍👧b
            "e\u{0301}e\u{0301}e\u{0301}",                   // combining marks
            "\u{1F680}\u{1F680}\u{1F680}",                   // three 2-cell glyphs
        ];
        for text in cases {
            for width in 0..12usize {
                let out = truncate_to_width(text, width);
                if width == 0 {
                    assert!(out.is_empty(), "width 0 must yield nothing, got {out:?}");
                    continue;
                }
                assert!(
                    display_width(&out) <= width,
                    "{text:?} at width {width} overflowed: {out:?} is {} cells",
                    display_width(&out)
                );
                if display_width(text) > width {
                    assert!(
                        out.ends_with('…'),
                        "{text:?} at width {width} dropped content without an ellipsis: {out:?}"
                    );
                    assert!(
                        !out.trim_end_matches('…').ends_with('\u{200D}'),
                        "{text:?} at width {width} left a dangling joiner: {out:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn pad_reaches_exactly_the_target() {
        assert_eq!(display_width(&pad_to_width("ab", 5)), 5);
        // Already wider: returned untouched rather than negative-padded.
        assert_eq!(pad_to_width("abcdef", 3), "abcdef");
        assert_eq!(pad_to_width("中文", 4), "中文");
    }

    // -- bilingual joins ----------------------------------------------------

    #[test]
    fn bi_is_chinese_first_with_a_space() {
        assert_eq!(bi("退出", "Quit"), "退出 Quit");
        // Empty halves still round-trip; callers rely on this for optional text.
        assert_eq!(bi("退出", ""), "退出 ");
    }

    #[test]
    fn fit_prefers_both_languages_then_chinese_only() {
        assert_eq!(fit("移动", "Move", " ", 20), "移动 Move");
        // The compact join keeps a table cell readable.
        assert_eq!(fit("移动", "Move", "/", 20), "移动/Move");
        // Too narrow for both, wide enough for Chinese.
        assert_eq!(fit("移动", "Move", " ", 5), "移动");
        // Too narrow even for Chinese: truncate, never overflow.
        let narrow = fit("移动选择", "Move selection", " ", 3);
        assert!(display_width(&narrow) <= 3, "{narrow:?}");
        // Width 0 must not panic and must not return content.
        assert_eq!(fit("移动", "Move", " ", 0), "");
    }

    // -- wrapping -----------------------------------------------------------

    #[test]
    fn wrap_breaks_cjk_hard_and_latin_on_spaces() {
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
        // Latin prefers a word boundary rather than a mid-word cut.
        let lines = wrap("hello world", 8);
        assert_eq!(lines[0], "hello");
        assert!(display_width(&lines[0]) <= 8);
        // CJK has no spaces, so it is a hard break at a character boundary.
        let cjk = wrap("中文字", 4);
        assert!(cjk.iter().all(|l| display_width(l) <= 4), "{cjk:?}");
        assert_eq!(cjk.concat(), "中文字", "wrapping must not lose characters");
    }

    #[test]
    fn wrap_never_returns_an_empty_vector() {
        // The help overlay's scroll clamp counts lines, so `len() >= 1` matters.
        assert_eq!(wrap("", 10), vec![String::new()]);
        assert_eq!(wrap("abc", 0), vec!["abc".to_string()]);
    }

    // -- glob matching ------------------------------------------------------

    #[test]
    fn glob_matches_stars_and_questions() {
        assert!(glob_match("*", "anything", false));
        assert!(glob_match("web*", "web1", false));
        assert!(glob_match("web?", "web1", false));
        assert!(!glob_match("web?", "web12", false));
        assert!(glob_match("a*b*c", "axxbyyc", false));
        assert!(glob_match("*", "", false), "a lone star matches the empty string");
        assert!(!glob_match("a", "", false));
    }

    #[test]
    fn glob_case_sensitivity_is_the_callers_choice() {
        // ssh `Host` patterns are case-insensitive; `known_hosts` entries are not.
        assert!(glob_match("WEB*", "web1", true));
        assert!(!glob_match("WEB*", "web1", false));
    }

    #[test]
    fn glob_backtracks_without_recursion() {
        // A pattern that forces repeated backtracking: `star` must be revisited.
        assert!(glob_match("*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaaaab", false));
        assert!(!glob_match("*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaaaac", false));
    }
}

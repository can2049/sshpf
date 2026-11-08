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
//! `unicode-width` is deliberately not a dependency — see
//! [`docs/design.md`](../../docs/design.md).

/// Width of a single character in terminal cells (1 or 2).
///
/// East-Asian wide/fullwidth ranges and emoji render in two cells; everything
/// else is assumed to be one.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    let wide = matches!(
        cp,
        0x1100..=0x115F      // Hangul Jamo
        | 0x2E80..=0x303E    // CJK radicals, Kangxi, CJK symbols & punctuation
        | 0x3041..=0x33FF    // Hiragana .. CJK compatibility
        | 0x3400..=0x4DBF    // CJK ext A
        | 0x4E00..=0x9FFF    // CJK unified ideographs
        | 0xA000..=0xA4CF    // Yi
        | 0xAC00..=0xD7A3    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK compatibility ideographs
        | 0xFE30..=0xFE6F    // CJK compatibility forms
        | 0xFF00..=0xFF60    // full-width forms
        | 0xFFE0..=0xFFE6
        // Emoji: the blocks are scattered, so cover the whole pictograph area
        // rather than two disjoint ranges (a gap here misaligns the whole row).
        | 0x1F300..=0x1F64F  // misc symbols & pictographs, emoticons
        | 0x1F680..=0x1F6FF  // transport & map symbols
        | 0x1F900..=0x1F9FF  // supplemental symbols & pictographs
        | 0x1FA70..=0x1FAFF  // symbols & pictographs extended-A
        | 0x20000..=0x3FFFD  // CJK ext B+
    );
    if wide { 2 } else { 1 }
}

/// Terminal cell width of a string.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Truncate to `width` cells, appending `…` when content is dropped.
pub fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(s) <= width {
        return s.to_string();
    }
    let budget = width.saturating_sub(1); // room for the ellipsis
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = char_width(ch);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
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
/// Never returns an empty vector: empty input yields one empty line, so callers
/// can assume `wrap(..).len() >= 1`.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if used + w > width && !cur.is_empty() {
            if let Some(pos) = cur.rfind(' ') {
                let rest = cur[pos + 1..].to_string();
                cur.truncate(pos);
                out.push(std::mem::take(&mut cur));
                cur = rest;
                used = display_width(&cur);
            } else {
                out.push(std::mem::take(&mut cur));
                used = 0;
            }
        }
        cur.push(ch);
        used += w;
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

    #[test]
    fn truncate_never_exceeds_the_budget() {
        assert_eq!(truncate_to_width("abc", 5), "abc");
        assert_eq!(truncate_to_width("abc", 0), "");
        assert_eq!(display_width(&truncate_to_width("abcdef", 3)), 3);
        let t = truncate_to_width("中文字", 4);
        assert!(display_width(&t) <= 4, "{t:?}");
        assert!(t.ends_with('…'));
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

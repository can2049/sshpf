//! Display-width helpers shared by the renderer and the failure classifier.
//!
//! Both used to keep their own copy and they had drifted apart (the UI version
//! knew about emoji, the ssh version did not), which is exactly the kind of bug
//! a shared implementation prevents.
//!
//! `unicode-width` is deliberately not a dependency — see `docs/design.md`.

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
}

//! The UI layer's string wrappers, and the once-per-process warning latch.
//!
//! Every ladder/width/wrap implementation lives in [`crate::text`]; this file only
//! fixes the two separators the TUI uses and keeps the old call sites readable:
//!
//! * [`pair`] / [`fit_pair_spaced`] — prose join `中文 / English` (spaces around
//!   the slash), used by the message bar, hints and dialogue prose;
//! * [`fit_pair`] — the compact `中文/English` join, used by table cells and
//!   buttons where two extra cells matter.
//!
//! [`warn_once`] also lives here because it is the renderer's only side effect:
//! a degradation that survives a resize must be logged once per process, not once
//! per frame (at ~30 fps the log would rotate the interesting history out).

use crate::text::fit;

/// Separator of the prose join: one cell of air on each side of the slash.
pub(crate) const PAIR_SEP: &str = " / ";

/// `中文 / English`, for hint lines and dialog prose.
pub(crate) fn pair(zh: &str, en: &str) -> String {
    // The same ladder with an unbounded budget: the two-language form is always
    // returned, so this cannot drift from the fitted variants.
    fit(zh, en, PAIR_SEP, usize::MAX)
}

/// `中文/English` fitted to `width` cells; Chinese only when the pair is too
/// wide, a truncation when even that does not fit.
pub(crate) fn fit_pair(zh: &str, en: &str, width: usize) -> String {
    fit(zh, en, "/", width)
}

/// `中文 / English` fitted to `width` cells, with the same ladder as
/// [`fit_pair`].
pub(crate) fn fit_pair_spaced(zh: &str, en: &str, width: usize) -> String {
    fit(zh, en, PAIR_SEP, width)
}

/// Greedy wrap to `width` cells — [`crate::text::wrap`] under the name this layer
/// has always used.
pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    crate::text::wrap(text, width)
}

/// Log a degradation once per process.
///
/// The renderer runs at ~30 frames/s, so a condition that survives a resize
/// would otherwise write a log line per frame until the 2 MiB rotation kicks in
/// and pushes the *interesting* history out. One line per process is the useful
/// amount: it says "this happened", and the frame that triggered it is the only
/// one that differed.
///
/// Returns `true` the first time it fires (so callers can log inline in tests).
pub(crate) fn warn_once(
    seen: &std::sync::atomic::AtomicBool,
    event: &str,
    fields: &[(&str, &dyn std::fmt::Display)],
) -> bool {
    use std::sync::atomic::Ordering;
    if seen.swap(true, Ordering::Relaxed) {
        return false;
    }
    crate::logging::warn(event, fields);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::text::display_width;

    #[test]
    fn display_width_counts_cjk_as_two() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width(""), 0);
        assert_eq!(display_width("a中b"), 4);
        // Status glyphs are single width; the Chinese word is double.
        assert_eq!(display_width("● 已连通"), 1 + 1 + 6);
        assert_eq!(display_width("✗ 失败"), 1 + 1 + 4);
        // Full-width forms are double width.
        assert_eq!(display_width("Ａ"), 2);
    }

    #[test]
    fn pad_and_truncate_respect_display_width() {
        assert_eq!(crate::text::pad_to_width("中", 4), "中  ");
        assert_eq!(display_width(&crate::text::pad_to_width("abc", 5)), 5);
        assert_eq!(crate::text::truncate_to_width("abc", 5), "abc");
        let cut = crate::text::truncate_to_width("中文字符串", 5);
        assert!(display_width(&cut) <= 5, "{cut:?} too wide");
        assert!(cut.ends_with('…'), "{cut:?}");
        assert_eq!(crate::text::truncate_to_width("anything", 0), "");
    }

    #[test]
    fn truncate_and_pad_agree_on_display_width() {
        // CJK takes two cells; helpers must measure display width, not bytes.
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
        let t = crate::text::truncate_to_width("abcdef", 3);
        assert_eq!(display_width(&t), 3);
        assert!(t.ends_with('…'), "truncation should be visible: {t:?}");
        // CJK: the ellipsis must not push the result past the budget.
        let t = crate::text::truncate_to_width("中文字", 4);
        assert!(display_width(&t) <= 4, "truncated {t:?} exceeds width 4");
        assert!(t.ends_with('…'));
        // Fitting strings are returned untouched.
        assert_eq!(crate::text::truncate_to_width("abc", 5), "abc");
        assert_eq!(crate::text::truncate_to_width("", 3), "");
        assert_eq!(crate::text::truncate_to_width("abc", 0), "");
        assert_eq!(display_width(&crate::text::pad_to_width("ab", 5)), 5);
    }

    /// The two joins are the same ladder with different separators, and the
    /// spaced one must keep the exact `中文 / English` wording the screens show.
    #[test]
    fn the_two_joins_share_one_ladder() {
        assert_eq!(pair("退出", "Quit"), "退出 / Quit");
        assert_eq!(fit_pair_spaced("退出", "Quit", 40), "退出 / Quit");
        assert_eq!(fit_pair("退出", "Quit", 40), "退出/Quit");
        // Too narrow for both: Chinese only, then a truncation. Never a dangling
        // English half.
        assert_eq!(fit_pair_spaced("退出", "Quit", 5), "退出");
        assert_eq!(fit_pair("退出", "Quit", 4), "退出");
        let narrow = fit_pair_spaced("退出重试", "Quit and retry", 3);
        assert!(display_width(&narrow) <= 3, "{narrow:?}");
    }

    /// `warn_once` must fire exactly once per process, not once per frame.
    ///
    /// The log call itself goes through `crate::logging::warn` (already covered by
    /// the logging tests) — what is new here is the latch.
    #[test]
    fn warn_once_latches_so_a_per_frame_degradation_logs_once() {
        let seen = std::sync::atomic::AtomicBool::new(false);
        assert!(warn_once(&seen, "ui.render_degraded", &[("what", &"test")]), "first call must report");
        for _ in 0..100 {
            assert!(!warn_once(&seen, "ui.render_degraded", &[("what", &"test")]), "later calls must stay quiet");
        }
        // A fresh latch is per-site, so a second site still gets its own line.
        let other = std::sync::atomic::AtomicBool::new(false);
        assert!(warn_once(&other, "ui.render_degraded", &[("what", &"other")]));
    }
}

//! The main screen's key hints: three flowed rows of `key 中文/English`.
//!
//! The hint set changes with the mode (normal vs selection) because selection
//! *re-binds* keys rather than adding to them, and because a footer that
//! advertises an unbound key is how a user concludes the app is broken. The
//! hints are *flowed* into the available rows instead of sliced into fixed
//! groups, so the only thing a tiny terminal can drop is the least-used tail.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::text::display_width;
use crate::ui::palette::{Palette, palette};
use crate::ui::text::fit_pair;

/// Three lines of key hints, exactly matching the keys `main` dispatches.
///
/// The hints change in selection mode, because the mode *re-binds* keys rather
/// than adding to them: `space` marks instead of starting, and `r`/`d`/`n`/`e`
/// are not bound at all there. Leaving the normal hints on screen would
/// advertise keys that do something else or nothing — the exact way a user
/// discovers "the app is broken" while it is working as designed.
///
/// Every main-screen binding appears here, in both languages, and the list is
/// *flowed* across the three rows rather than sliced into fixed groups: a
/// footer is the one place where dropping a key silently is the whole bug, so
/// the layout gives each hint its own width and wraps instead of clipping.
pub(crate) fn draw_keys(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let hints: &[Hint] = if app.select_mode.active { SELECT_HINTS } else { MAIN_HINTS };
    let lines = flow_hints(hints, area.width as usize, area.height as usize, p);
    frame.render_widget(Paragraph::new(lines), area);
}

/// One `(key, Chinese, English)` triple of the hint lines.
pub(crate) type Hint = (&'static str, &'static str, &'static str);

/// Every binding of the normal main screen, complete: no key is left off.
///
/// Order is "how often you reach for it", not "what the match arm order is", so
/// that when a very narrow terminal cannot fit all three rows the least-used
/// hints are the ones that fall off the end.
const MAIN_HINTS: &[Hint] = &[
    ("j/k ↑↓", "移动", "Move"),
    ("g/G", "首尾", "First·Last"),
    ("space", "启停", "Start·Stop"),
    ("r", "重启", "Restart"),
    ("n", "新建", "New"),
    ("e/E", "编辑", "Edit"),
    ("d", "删除", "Delete"),
    ("enter/l", "详情", "Detail"),
    ("/", "过滤", "Filter"),
    ("a", "全启", "Start all"),
    ("x", "全停", "Stop all"),
    ("v/V", "多选", "Select"),
    ("?", "帮助", "Help"),
    ("q", "退出", "Quit"),
    ("esc", "清过滤", "Clear filter"),
];

/// The selection-mode footer. It must not advertise `r`/`d`/`n`/`e`, which are
/// unbound there, nor `space` as start/stop, which it re-binds to "mark".
const SELECT_HINTS: &[Hint] = &[
    ("j/k ↑↓", "移动", "Move"),
    ("space", "勾选", "Mark"),
    ("J/K ⇧↑↓", "扩选", "Extend"),
    ("g/G", "首尾", "First·Last"),
    ("a", "全选", "Select all"),
    ("s/x/R/D", "批量", "Batch"),
    ("v/esc", "退出", "Exit"),
    ("?", "帮助", "Help"),
];

/// Flow `hints` into at most `max_lines` rows of `width` cells.
///
/// Greedy and lossless up to `max_lines`: a hint that would cross the right edge
/// starts a new row, so the only thing that can drop is a hint that does not fit
/// on the last row of a genuinely tiny terminal.
pub(crate) fn flow_hints(hints: &[Hint], width: usize, max_lines: usize, p: Palette) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    let mut used = 1usize;

    for (key, zh, en) in hints {
        // `key 中文/English`, degrading to Chinese when the pair is too wide.
        let desc = fit_pair(zh, en, width);
        let w = display_width(key) + 1 + display_width(&desc);
        let sep = if used > 1 { 2 } else { 0 };
        if used + sep + w > width && used > 1 {
            lines.push(Line::from(std::mem::take(&mut spans)));
            spans.push(Span::raw(" "));
            used = 1;
        }
        if lines.len() + 1 >= max_lines && used + w > width {
            // No row left for this hint: stop rather than overflow the frame.
            break;
        }
        if used > 1 {
            spans.push(Span::raw("  "));
            used += 2;
        }
        spans.push(Span::styled((*key).to_string(), p.fg(p.accent)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(desc, p.fg(p.dim)));
        used += w;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines.truncate(max_lines);
    lines
}
#[cfg(test)]
mod tests {

    use crate::ui::test_support::*;

    #[test]
    fn key_hints_spell_out_space() {
        // The hint used to be abbreviated "spc"; spell it out so it matches the
        // key a user actually presses.
        let mut app = app_with_tunnels(vec![]);
        let text = render_to_text(&mut app, 120, 20);
        assert!(text.contains("space"), "space hint missing:\n{text}");
        assert!(text.contains("启停"), "space description missing:\n{text}");
        assert!(!text.contains("spc"), "abbreviation should be gone:\n{text}");
    }

    #[test]
    fn key_hints_swap_instead_of_lying_in_selection_mode() {
        // Selection mode RE-BINDS space and unbinds r/d/n/e. Showing the normal
        // hints while it is active would advertise keys that do something else
        // or nothing at all.
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db", 8081)]);
        let normal = render_to_text(&mut app, 140, 24);
        assert!(normal.contains("启停"), "normal hints must describe space:\n{normal}");
        assert!(normal.contains("重启"), "normal hints must mention r:\n{normal}");

        app.selected = 0;
        app.toggle_select_mode();
        let selecting = render_to_text(&mut app, 140, 24);
        assert!(!selecting.contains("启停"), "space does not start/stop in selection mode:\n{selecting}");
        assert!(selecting.contains("勾选"), "selection hints must describe space:\n{selecting}");
        // The batch keys are advertised as the concrete bindings, not as a vague
        // "batch operations" label: the mode is where they actually work.
        assert!(selecting.contains("s/x/R/D"), "selection hints must list the batch keys:\n{selecting}");
        assert!(selecting.contains("批量"), "selection hints must say what those keys do:\n{selecting}");
        assert!(selecting.contains("已选"), "the marked count must be on screen:\n{selecting}");
        // Selection mode must not advertise the normal-mode-only bindings. (The
        // action bar legitimately offers `R 重启`, the *batch* restart — what
        // must not appear is the single-row `n 新建` and the `r` spelling that
        // would suggest lowercase still restarts one tunnel.)
        assert!(!selecting.contains("新建/New"), "n is unbound in selection mode:\n{selecting}");
        assert!(!selecting.contains("r 重启"), "lowercase r is unbound in selection mode:\n{selecting}");
    }
}

//! The selection-mode action bar: how many rows are marked, and what can be done.
//!
//! Drawn in place of a table row only while `v` mode is active (see
//! [`crate::ui::frame::draw`]), because the mode *re-binds* keys rather than
//! adding to them: `space` marks instead of starting, so a normal footer would
//! advertise bindings that do something else there.
//!
//! Every action on the bar is a clickable label recorded in the
//! [`crate::ui::hit::HitMap`], so the mouse reaches exactly the handlers the keys
//! do. Buttons are dropped *whole* when they stop fitting — an unreachable
//! button is worse than an untranslated one.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::App;
use crate::model::BatchAction;
use crate::text::display_width;
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::palette::{Palette, palette};
use crate::ui::text::fit_pair;

// ---------------------------------------------------------------------------
// Selection mode: action bar
// ---------------------------------------------------------------------------

/// The bottom bar of selection mode: how many are marked, and what can be done.
///
/// Every action is a clickable label recorded in the hit map, so the mouse
/// reaches exactly the same handlers as the keys. The count leads the line
/// because "what is my selection right now" is the question the mode exists to
/// answer; it is stated even when zero rather than being omitted.
pub(crate) fn draw_selection_bar(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let n = app.select_mode.len();
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new("").style(p.bg(p.selection_bg)), area);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let x = push_count(&mut spans, n, area.width, p);
    // Layout, not content: the count plus the actions still fits narrow
    // terminals because each label is dropped whole once it stops fitting.
    let x = push_batch_buttons(&mut spans, x, area, hits, p);
    let _ = push_mode_buttons(&mut spans, x, area, hits, p);
    frame.render_widget(Paragraph::new(Line::from(spans)).style(p.bg(p.selection_bg)), area);
}

/// The leading `已选 N 条 / N marked` label; returns the x it ends at.
///
/// The count is emphasised because it is the mode's whole state. It carries the
/// English only on a terminal wide enough to keep every action button on the
/// bar: dropping a button to pay for the translation would be a bad trade (the
/// buttons are clickable, the number is not).
fn push_count(spans: &mut Vec<Span<'static>>, n: usize, width: u16, p: Palette) -> u16 {
    let count = if width >= 140 { format!(" 已选 {n} 条 / {n} marked ") } else { format!(" 已选 {n} 条 ") };
    let count_w = display_width(&count) as u16;
    let count_style = if n == 0 { p.fg(p.pending) } else { p.fg(p.up) }.add_modifier(Modifier::BOLD);
    spans.push(Span::styled(count, count_style));
    count_w
}

/// `s 启用 · x 停用 · R 重启 · D 删除`, each a clickable rect.
///
/// The label comes from `BatchAction` itself, so the bar and the confirmation
/// prompt can never drift apart.
fn push_batch_buttons(spans: &mut Vec<Span<'static>>, start_x: u16, area: Rect, hits: &mut HitMap, p: Palette) -> u16 {
    let mut x = start_x;
    let buttons: [(BatchAction, &'static str); 4] =
        [(BatchAction::Start, "s"), (BatchAction::Stop, "x"), (BatchAction::Restart, "R"), (BatchAction::Delete, "D")];
    for (action, key) in buttons {
        let label = format!("{key} {}", fit_pair(action.label(), action.label_en(), 20));
        match push_button(spans, x, area, hits, p, &label, HitTarget::BatchAction(action)) {
            Some(next) => x = next,
            None => break,
        }
    }
    x
}

/// `a 全选 · v/esc 退出`, each a clickable rect.
fn push_mode_buttons(spans: &mut Vec<Span<'static>>, start_x: u16, area: Rect, hits: &mut HitMap, p: Palette) -> u16 {
    let mut x = start_x;
    for (label, target) in [
        (format!("a {}", fit_pair("全选", "All", 14)), HitTarget::BatchSelectAll),
        (format!("v/esc {}", fit_pair("退出", "Exit", 12)), HitTarget::BatchExit),
    ] {
        match push_button(spans, x, area, hits, p, &label, target) {
            Some(next) => x = next,
            None => break,
        }
    }
    x
}

/// Append ` · label` and record its rect; `None` when it would not fit whole.
fn push_button(
    spans: &mut Vec<Span<'static>>,
    x: u16,
    area: Rect,
    hits: &mut HitMap,
    p: Palette,
    label: &str,
    target: HitTarget,
) -> Option<u16> {
    let sep = " · ";
    let sep_w = display_width(sep) as u16;
    let label_w = display_width(label) as u16;
    if x + sep_w + label_w > area.right() {
        return None;
    }
    spans.push(Span::styled(sep, p.fg(p.dim)));
    spans.push(Span::styled(label.to_string(), p.fg(p.accent)));
    let x = x + sep_w;
    hits.push(Rect { x, y: area.y, width: label_w, height: 1 }, target);
    Some(x + label_w)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ui::test_support::*;

    #[test]
    fn selection_mode_shows_the_mark_gutter_and_action_bar() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db-prod", 5432)]);
        app.selected = 0;
        app.toggle_select_mode();
        // Entering the mode starts with nothing marked; mark explicitly so the
        // gutter has something to show.
        app.toggle_mark_selected();
        let text = render_to_text(&mut app, 120, 30);
        assert!(text.contains("✓"), "a marked row needs the ✓ gutter:\n{text}");
        assert!(text.contains("已选 1 条"), "marked count missing:\n{text}");
        assert!(text.contains("s 启用"), "start action missing:\n{text}");
        assert!(text.contains("x 停用"), "stop action missing:\n{text}");
        assert!(text.contains("R 重启"), "restart action missing:\n{text}");
        assert!(text.contains("D 删除"), "delete action missing:\n{text}");
        assert!(text.contains("esc 退出"), "exit hint missing:\n{text}");
        // The cursor marker and the mark gutter coexist.
        assert!(text.contains("▶"), "cursor marker missing:\n{text}");
    }

    #[test]
    fn empty_selection_is_stated_plainly() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.toggle_select_mode();
        app.mark_none();
        let text = render_to_text(&mut app, 120, 30);
        assert!(text.contains("已选 0 条"), "an empty selection must be stated:\n{text}");
        // The gutter itself still renders (with `·`), so the column layout does
        // not jump when the first mark appears; only the ✓ marks are absent.
        let rows: Vec<&str> = text.lines().filter(|l| l.contains("api-dev")).collect();
        assert!(!rows.is_empty(), "tunnel row missing:\n{text}");
        assert!(rows.iter().all(|r| !r.contains('✓')), "nothing is marked, so no ✓ should appear: {rows:?}");
    }

    #[test]
    fn selection_mode_gutter_marks_only_marked_rows() {
        let mut app = app_with_tunnels(vec![sample(1, "first", 8080), sample(2, "second", 8081)]);
        app.selected = 0;
        app.toggle_select_mode();
        // Nothing marked yet: the bar says 0 and no row carries a mark.
        let text = render_to_text(&mut app, 140, 26);
        assert!(text.contains("已选 0 条"), "initial state must be stated:\n{text}");
        app.toggle_mark_selected();
        let text = render_to_text(&mut app, 140, 26);
        assert!(text.contains("已选 1 条"), "marking must be visible:\n{text}");
        assert!(text.contains('✓'), "the marked row needs its own gutter symbol:\n{text}");
    }

    #[test]
    fn selection_mode_hits_resolve_to_rows_and_actions() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db-prod", 5432)]);
        app.toggle_select_mode();
        let _ = render_to_text(&mut app, 120, 30);

        let row1 = app.hits.rect_of(&HitTarget::TunnelRow(1)).expect("row must be clickable");
        assert_eq!(app.hits.at(row1.x + 1, row1.y), Some(&HitTarget::TunnelRow(1)));
        assert!(app.hits.rect_of(&HitTarget::TunnelList).is_some());

        let start = app.hits.rect_of(&HitTarget::BatchAction(BatchAction::Start)).expect("start button missing");
        assert_eq!(app.hits.at(start.x, start.y), Some(&HitTarget::BatchAction(BatchAction::Start)));
        assert!(app.hits.rect_of(&HitTarget::BatchAction(BatchAction::Delete)).is_some());
        assert!(app.hits.rect_of(&HitTarget::BatchSelectAll).is_some());
        assert!(app.hits.rect_of(&HitTarget::BatchExit).is_some());
    }

    #[test]
    fn normal_mode_has_no_mark_gutter_or_action_bar() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        let text = render_to_text(&mut app, 120, 30);
        assert!(!text.contains("已选 "), "the action bar must only exist in selection mode:\n{text}");
        assert!(!text.contains("R 重启"), "batch hints must not leak into normal mode:\n{text}");
        assert!(app.hits.rect_of(&HitTarget::BatchAction(BatchAction::Start)).is_none());
    }
}

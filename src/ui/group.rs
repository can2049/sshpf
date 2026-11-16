//! The group bar and the group-alias popup.
//!
//! The bar is the only place that says *which* group is being browsed and which
//! one (if any) is working. Two glyphs carry that without colour: `▶` marks the
//! browsed group, `●` marks the working group. Clicking a chip only browses it;
//! working is a deliberate `w`, because it may drop live connections.
//!
//! The alias popup is one text field with the same cursor rules as the filter
//! line: the cursor is parked from the *display* width, so CJK aliases land where
//! the user sees the caret.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, GroupNameMode};
use crate::text::{display_width, truncate_to_width};
use crate::ui::frame::{centered, clear_and_block, popup_block};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::palette::palette;
use crate::ui::text::{fit_pair, fit_pair_spaced};

/// One compact row above the table.
pub(crate) fn draw_group_bar(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let p = palette();
    let budget = area.width as usize;
    let prefix = format!(" {} ", fit_pair("分组", "Groups", budget / 3));
    let prefix_w = display_width(&prefix);
    let mut spans = vec![Span::raw(prefix)];
    let mut used = prefix_w;
    let mut first = true;

    for group in &app.cfg.groups {
        let browsed = group.id == app.browsed_group;
        let working = app.active_group == Some(group.id);
        let marker = if browsed { "▶ " } else { "  " };
        let suffix = if working { " ●" } else { "" };
        let text = format!("{marker}{}{suffix}", group.alias());
        let w = display_width(&text);
        let sep = if first { 0 } else { 2 };
        if used + sep + w > budget {
            break;
        }
        let style = if working {
            p.fg(p.up).add_modifier(Modifier::BOLD)
        } else if browsed {
            p.fg(p.accent)
        } else {
            p.fg(p.dim)
        };
        if sep > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(text, style));
        let x = area.x.saturating_add(u16::try_from(used + sep).unwrap_or(u16::MAX));
        let width = u16::try_from(w).unwrap_or(u16::MAX);
        hits.push(Rect { x, y: area.y, width, height: 1 }, HitTarget::GroupTab(group.id));
        used += sep + w;
        first = false;
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Centered alias input for `N` (create) and `R` (rename).
pub(crate) fn draw_group_name(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(prompt) = app.group_prompt.as_ref() else {
        return;
    };
    let p = palette();
    let width = 60u16.min(area.width);
    let height = 5u16.min(area.height);
    if width == 0 || height == 0 {
        return;
    }
    let popup = centered(area, width, height);
    hits.push(popup, HitTarget::Popup);
    let (title, label) = match prompt.mode {
        GroupNameMode::Create => ("新建分组 · New group", "别名 / Alias: "),
        GroupNameMode::Rename(_) => ("重命名分组 · Rename group", "别名 / Alias: "),
    };
    let block = popup_block(title, p);
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let value_w = (inner.width as usize).saturating_sub(display_width(label));
    let value = truncate_to_width(prompt.input.value(), value_w);
    let line = Line::from(vec![
        Span::styled(label, p.fg(p.dim)),
        Span::styled(value, p.fg(p.text).add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(line), Rect { height: 1, ..inner });

    if inner.height >= 2 {
        let hint = fit_pair_spaced("enter 保存 · esc 取消", "enter save · esc cancel", inner.width as usize);
        let row = Rect { y: inner.y + 1, height: 1, ..inner };
        frame
            .render_widget(Paragraph::new(Line::from(Span::styled(hint, p.fg(p.dim)))).wrap(Wrap { trim: false }), row);
    }

    // Caret after the visible prefix + typed text, only when it is on screen.
    let x =
        usize::from(inner.x).saturating_add(display_width(label)).saturating_add(display_width(prompt.input.value()));
    if x < usize::from(inner.right())
        && let Ok(x) = u16::try_from(x)
    {
        frame.set_cursor_position((x, inner.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GroupConfig;
    use crate::ui::test_support::*;

    fn two_group_app() -> crate::app::App {
        let mut app = app_with_tunnels(vec![sample(1, "work-forward", 8080)]);
        app.cfg.groups.push(GroupConfig::new(2, "home"));
        app.active_group = Some(1);
        app.cfg.active_group = Some(1);
        app.browsed_group = 2;
        app
    }

    #[test]
    fn the_bar_names_both_groups_and_marks_browse_vs_work() {
        let mut app = two_group_app();
        let text = render_to_text(&mut app, 120, 24);
        assert!(text.contains("#1"), "unnamed default group must show its id:\n{text}");
        assert!(text.contains("home"), "names must be on the bar:\n{text}");
        assert!(text.contains('▶'), "the browsed group needs its marker:\n{text}");
        assert!(text.contains('●'), "the working group needs its marker:\n{text}");
        assert!(app.hits.rect_of(&HitTarget::GroupTab(2)).is_some(), "group chips must be clickable");
    }

    #[test]
    fn clicking_a_group_chip_browses_it_without_working_it() {
        let mut app = two_group_app();
        app.browsed_group = 1;
        let _ = render_to_text(&mut app, 120, 24);
        let rect = app.hits.rect_of(&HitTarget::GroupTab(2)).expect("chip rect");
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.browsed_group, 2);
        assert_eq!(app.active_group, Some(1), "a click never makes the group work");
    }

    #[test]
    fn the_alias_popup_renders_at_pathological_sizes() {
        let mut app = two_group_app();
        app.open_group_create();
        for w in [1u16, 2, 3, 5, 10, 20, 40, 80, 120] {
            for h in [1u16, 2, 3, 4, 5, 10, 20] {
                let backend = ratatui::backend::TestBackend::new(w, h);
                let mut term = ratatui::Terminal::new(backend).unwrap();
                term.draw(|f| crate::ui::draw(f, &mut app)).unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
            }
        }
    }
}

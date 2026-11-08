//! The confirmation dialog for destructive actions.
//!
//! One popup, sized to its prompt (a prompt may wrap onto two lines), with the
//! accept and cancel bindings both spelled out: `y`/`Y`/`enter` and
//! `n`/`N`/`esc`. It is the last gate before a delete, so the popup records
//! itself in the hit map — a click aimed at the dialog must never fall through
//! to the row behind it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::text::{display_width, truncate_to_width};
use crate::ui::frame::{centered, clear_and_block, popup_block};
use crate::ui::hit::{HitMap, HitTarget};
use crate::ui::palette::palette;

// ---------------------------------------------------------------------------
// Overlay: Confirm
// ---------------------------------------------------------------------------

pub(crate) fn draw_confirm(frame: &mut Frame, prompt: &str, area: Rect, hits: &mut HitMap) {
    let p = palette();
    // Width of the prompt in cells, kept wide: a pasted name is unbounded, so
    // `as u16` on the raw width could wrap a huge prompt into a tiny dialog.
    let prompt_w = display_width(prompt) as u32;
    let min = 40u16.min(area.width);
    let max = 90u16.min(area.width);
    let wanted = prompt_w.saturating_add(8).min(u32::from(u16::MAX)) as u16;
    let wanted = wanted.clamp(min, max.max(min));
    // Fit the dialog to its content: prompt may wrap onto two lines, plus a
    // blank line and the key row, plus the two border rows.
    //
    // The wrap budget drops below 1 on a terminal narrower than 5 columns
    // (`wanted` is then `area.width` itself, and 4 cells go to the borders and
    // the `⚠ ` marker) — `div_ceil` by that is a divide-by-zero panic, so it is
    // floored to one cell and the row count is capped instead.
    let wrap_w = wanted.saturating_sub(4).max(1) as u32;
    let prompt_rows = prompt_w.div_ceil(wrap_w).min(u32::from(u16::MAX)) as u16;
    let popup = centered(area, wanted, (prompt_rows.max(1) + 4).min(area.height));
    // The riskiest click-through to allow: the dialog asks before a destructive
    // action, so a click meant for the dialog must never reach a table row.
    hits.push(popup, HitTarget::Popup);
    let block = popup_block("确认 · Confirm", p);
    let inner = block.inner(popup);
    clear_and_block(frame, popup, block);
    if inner.height == 0 {
        return;
    }

    // `y`/`Y`/`enter` accept and `n`/`N`/`esc` cancel in the real handler; both
    // spellings are advertised so nobody has to guess whether shift matters.
    let lines = vec![
        Line::from(vec![
            Span::styled("⚠ ", p.fg(p.failed).add_modifier(Modifier::BOLD)),
            Span::styled(truncate_to_width(prompt, inner.width.saturating_sub(3) as usize), p.fg(p.text)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" y/Y / enter ", p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("确认 / Confirm    ", p.fg(p.text)),
            Span::styled("n/N / esc ", p.fg(p.accent).add_modifier(Modifier::BOLD)),
            Span::styled("取消 / Cancel", p.fg(p.text)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
#[cfg(test)]
mod tests {
    use crate::ui::draw;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{ConfirmAction, Overlay};
    use crate::ui::test_support::*;

    /// Every overlay at a width the dialog cannot fit into.
    ///
    /// `draw_confirm` sized its content area from `wanted - 4`, and on a terminal
    /// narrower than 5 columns `wanted == area.width` makes that budget 0 — so
    /// the row count divided by zero and the TUI died mid-frame, leaving the
    /// terminal in raw mode. Widths 1..=10 cover the whole broken band plus the
    /// first widths that merely clip.
    #[test]
    fn confirm_overlay_renders_when_the_terminal_is_narrower_than_the_dialog() {
        // One `App` for the whole matrix: building it loads a config file and
        // allocates a temp dir, which dominates the runtime otherwise. Drawing
        // only touches `hits` and the scroll offsets.
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.overlay =
            Overlay::Confirm { action: ConfirmAction::Delete(1), prompt: "确认删除「api-dev」？(y/N)".into() };
        for w in 1u16..=10 {
            for h in [1u16, 2, 3, 4, 5, 6, 7, 9, 12, 20, 45] {
                let backend = TestBackend::new(w, h);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("confirm at {w}x{h}: {e}"));
            }
        }
    }
}

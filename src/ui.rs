//! Pure rendering layer (ratatui 0.30).
//!
//! Rules of this module:
//!
//! * No I/O, no process spawning, no touching of tunnel state. The only mutation
//!   allowed is clamping the ratatui scroll offsets (`help_scroll`,
//!   `detail_scroll`) so that a shrunk terminal does not leave the view past the
//!   end of the content.
//! * Everything the user needs must survive `NO_COLOR`: every status carries a
//!   symbol *and* a word, never colour alone.
//! * Column widths are computed from `frame.area().width`; all Chinese text is
//!   measured with [`crate::text::display_width`] (CJK counts as 2 columns), never
//!   `str::len()`.
//!
//! Every string the user reads is bilingual, Chinese first and English second
//! (`中文 English`), so no screen is a dead end for either reader. Narrow cells
//! degrade through [`crate::text::fit`] rather than overflowing.
//!
//! Layout of the main screen (matches `docs/research/prior-art.md` §7):
//!
//! ```text
//! row 0          title bar      sshpf 0.1.0 · ssh-config N hosts · cfg PATH   HH:MM:SS
//! row 1          counters       ● 4 已连通/Up  ◐ 2 连接中/Starting  …
//! rows 2..h-5    tunnel table   名称/Name | T | 本地/Local | 远端/Remote | …
//! row h-5        message bar    app.status, or the selected tunnel's full error
//! row h-4        selection bar  only while selecting (`v`)
//! rows h-3..h    key hints      three lines, every main-screen key
//! ```
//!
//! # Module map — where to add what
//!
//! [`draw`] in [`frame`] is the only entry point; it lays out six bands and calls
//! one sibling per band. Every other module draws exactly one region and owns
//! that region's geometry, so "change the table" and "change the help overlay"
//! never touch the same file.
//!
//! | file | draws | open it when… |
//! |---|---|---|
//! | [`frame`] | `draw()`, the band split, popup scaffolding | you add a whole new screen band or a modal frame |
//! | [`palette`] | colours, `fmt_uptime`/`fmt_unix`, `cell` | you need a colour or a column format |
//! | [`text`] | `pair`/`fit_pair`/`wrap_text`, `warn_once` | you need bilingual text or a once-per-process log |
//! | [`header`] | title bar, counters, message bar | you change the top chrome |
//! | [`hints`] | the three footer hint rows | you add a main-screen key |
//! | [`table`] | the tunnel table | you change a row/column |
//! | [`selection`] | the `v`-mode action bar | you add a batch action |
//! | [`help`] | the `?` overlay | you change help layout |
//! | [`form`] | the create/edit form | you add a field |
//! | [`dropdown`] | the inline host list | you change host picking |
//! | [`confirm`] | the confirmation dialog | you change the delete gate |
//! | [`detail`] | the detail/log overlay | you change what a tunnel reports |
//! | [`filter`] | the `/` filter line | you change filtering |
//! | [`hit`] | the mouse hit map | you add a clickable thing |
//! | [`layout`] | table column widths (pure arithmetic) | you change column degradation |
//! | [`import`] | the import pane and its four parts | you change importing |
//!
//! New keys are **not** here: they are dispatched in `crate::keys` and documented
//! in `crate::help`.

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Render one frame. `main` calls this inside `terminal.draw(|f| ui::draw(f, &mut app))`.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    // --- mutable phase: clamp scroll offsets ------------------------------
    clamp_scrolls(app, area);

    // --- hit map: rebuilt from scratch, every frame -----------------------
    // A stale map would send a click to last frame's geometry, so it is cleared
    // first. It is *taken out* of `App` rather than borrowed: the renderers
    // below only get `&App` (that is what keeps this module read-only), yet they
    // must still record where they drew things. Written back at the end.
    app.hits.clear();
    let mut hits = std::mem::take(&mut app.hits);

    // --- immutable phase: render ------------------------------------------
    let app_ref: &App = app;

    // The action bar only exists while selecting, and it takes its row from the
    // table instead of leaving a blank strip in normal mode.
    let select_h = u16::from(app_ref.select_mode.active);
    let rows = Layout::vertical([
        Constraint::Length(1),        // title
        Constraint::Length(1),        // counters
        Constraint::Min(1),           // table
        Constraint::Length(1),        // message bar
        Constraint::Length(select_h), // selection-mode action bar
        Constraint::Length(3),        // key hints (three lines: every key, both languages)
    ])
    .split(area);

    draw_title(frame, app_ref, band(&rows, 0, "title"));
    draw_counters(frame, app_ref, band(&rows, 1, "counters"));
    draw_table(frame, app_ref, band(&rows, 2, "table"), &mut hits);
    draw_message(frame, app_ref, band(&rows, 3, "message"));
    if select_h > 0 {
        draw_selection_bar(frame, app_ref, band(&rows, 4, "selection_bar"), &mut hits);
    }
    draw_keys(frame, app_ref, band(&rows, 5, "keys"));

    match &app_ref.overlay {
        Overlay::None => {}
        Overlay::Help => draw_help(frame, app_ref, area, &mut hits),
        Overlay::Form => draw_form(frame, app_ref, area, &mut hits),
        Overlay::Confirm { prompt, .. } => draw_confirm(frame, prompt, area, &mut hits),
        Overlay::Detail(id) => draw_detail(frame, app_ref, *id, area, &mut hits),
        Overlay::Filter => draw_filter(frame, app_ref, area, &mut hits),
    }

    app.hits = hits;
}
// ---------------------------------------------------------------------------
// Degradation helpers
// ---------------------------------------------------------------------------

/// Clamp the ratatui scroll offsets to the current content length so that a
/// resize (or a long list scrolled to the end) never leaves a blank view.
///
/// The help overlay's length is *measured*, not assumed: [`help_lines`] is the
/// very function the renderer uses, so the clamp and the drawing can never
/// disagree about how many rows the wrapped catalog actually occupies.
fn clamp_scrolls(app: &mut App, area: Rect) {
    let help_max =
        help_lines(help_inner_width(area) as usize, palette()).len().saturating_sub(help_inner_height(area) as usize);
    app.help_scroll = app.help_scroll.min(help_max.min(u16::MAX as usize) as u16);

    if let Overlay::Detail(id) = app.overlay {
        let max = match app.tunnel(id) {
            Some(view) => detail_lines(view, id, palette()).len().saturating_sub(detail_inner_height(area) as usize),
            None => 0,
        };
        app.detail_scroll = app.detail_scroll.min(max.min(u16::MAX as usize) as u16);
    }
}

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};

use crate::app::{App, Overlay};
use crate::ui::confirm::draw_confirm;
use crate::ui::detail::{detail_inner_height, detail_lines, draw_detail};
use crate::ui::filter::draw_filter;
use crate::ui::form::draw_form;
use crate::ui::frame::band;
use crate::ui::header::{draw_counters, draw_message, draw_title};
use crate::ui::help::{draw_help, help_inner_height, help_inner_width, help_lines};
use crate::ui::hints::draw_keys;
use crate::ui::selection::draw_selection_bar;
use crate::ui::table::draw_table;

pub mod confirm;
pub mod detail;
pub mod dropdown;
pub mod filter;
pub mod form;
pub mod frame;
pub mod header;
pub mod help;
pub mod hints;
pub mod hit;
pub mod import;
pub mod layout;
pub mod palette;
pub mod selection;
pub mod table;
pub mod text;

pub use layout::{Col, column_layout};
pub use palette::palette;
pub use palette::{Palette, palette_with};

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ConfirmAction;
    use crate::model::TunnelStatus;
    use crate::ui::test_support::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Instant;

    #[test]
    fn renders_empty_state() {
        let mut app = app_with_tunnels(vec![]);
        let text = render_to_text(&mut app, 120, 35);
        assert!(text.contains("sshpf"), "title bar missing:\n{text}");
        assert!(text.contains("cfg:"), "config path missing:\n{text}");
        assert!(text.contains("还没有转发条目"), "empty hint missing:\n{text}");
        assert!(text.contains("No tunnels yet"), "empty hint needs its English half:\n{text}");
        assert!(text.contains("新建"), "key hint missing:\n{text}");
        // Counters state every status in both languages on a wide terminal...
        assert!(text.contains("0 已连通/Up"), "counters missing:\n{text}");
        assert!(text.contains("0 未启用/Off"), "counters missing:\n{text}");
        // ...and degrade to glyph + number when narrow, never to a clipped word.
        let narrow = render_to_text(&mut app, 70, 20);
        assert!(narrow.contains("● 0"), "compact counters missing:\n{narrow}");
        assert!(!narrow.contains("Starting"), "narrow counters must drop the words:\n{narrow}");
    }

    /// One frame per overlay across a sampled matrix of terminal sizes.
    ///
    /// The matrix is what turns "no size may panic" from a claim into a check:
    /// every overlay is a different composition of `Layout::split`, cell
    /// arithmetic and wrapping, and each has its own small-size cliff (a popup
    /// taller than the frame, a zero-width inner area, a one-row body). Widths
    /// 1..=12 are exhaustive because that is where the popups collapse, and the
    /// wider samples straddle every column-selection threshold (64/88/110) plus
    /// the title's progressive-disclosure cuts (46/84).
    #[test]
    fn every_overlay_renders_across_a_sampled_size_matrix() {
        let widths: Vec<u16> =
            (1u16..=12).chain([16, 20, 24, 30, 40, 46, 50, 60, 64, 70, 80, 84, 88, 100, 110, 120, 130, 140]).collect();
        let heights = [1u16, 2, 3, 4, 5, 6, 7, 8, 10, 12, 16, 20, 24, 30, 36, 45];
        let overlays = [
            Overlay::None,
            Overlay::Help,
            Overlay::Form,
            Overlay::Confirm { action: ConfirmAction::Delete(1), prompt: "确认删除「api-dev」？".into() },
            Overlay::Detail(1),
            Overlay::Filter,
        ];
        for ov in overlays {
            let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080), sample(2, "db", 5432)]);
            app.tunnels[0].status = TunnelStatus::Up { since: Instant::now() };
            app.open_form_create(); // `Form` needs a state, and Detail needs a tunnel
            for &w in &widths {
                for &h in &heights {
                    // Re-assigned every frame: drawing clamps scrolls and may
                    // close nothing, but the overlay must be the one under test.
                    app.overlay = ov.clone();
                    let backend = TestBackend::new(w, h);
                    let mut term = Terminal::new(backend).unwrap();
                    term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("{ov:?} at {w}x{h}: {e}"));
                }
            }
        }
    }

    #[test]
    fn renders_at_pathological_terminal_sizes() {
        // No size may panic: users resize terminals, and a panic here would
        // leave the terminal in raw mode.
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        for w in [1u16, 2, 3, 5, 10, 20, 40, 60, 80] {
            for h in [1u16, 2, 3, 4, 10, 24] {
                let backend = TestBackend::new(w, h);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| draw(f, &mut app)).expect("draw must not fail");
            }
        }
    }

    #[test]
    fn renders_every_overlay_without_panicking() {
        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.selected = 0;
        let overlays = [
            Overlay::Help,
            Overlay::Form,
            Overlay::Confirm { action: ConfirmAction::Delete(1), prompt: "确认删除「x」？".into() },
            Overlay::Detail(1),
            Overlay::Filter,
        ];
        for ov in overlays {
            app.open_form_create(); // give Form a state
            app.overlay = ov.clone();
            for (w, h) in [(120u16, 40u16), (80, 24), (40, 12)] {
                let backend = TestBackend::new(w, h);
                let mut term = Terminal::new(backend).unwrap();
                term.draw(|f| draw(f, &mut app)).unwrap_or_else(|e| panic!("{ov:?} at {w}x{h}: {e}"));
            }
        }
    }

    #[test]
    fn scroll_offsets_are_clamped_during_draw() {
        let mut app = app_with_tunnels(vec![]);
        app.overlay = Overlay::Help;
        app.help_scroll = 9999;
        let _ = render_to_text(&mut app, 120, 35);
        assert!(app.help_scroll < 9999, "help_scroll must be clamped");

        let mut app = app_with_tunnels(vec![sample(1, "api-dev", 8080)]);
        app.open_detail();
        app.detail_scroll = 9999;
        let _ = render_to_text(&mut app, 120, 35);
        assert!(app.detail_scroll < 9999, "detail_scroll must be clamped");
    }
}

//! Colour vocabulary and the column formatters shared by every renderer.
//!
//! [`Palette`] is the frame's colour set: one struct, one lookup per frame, and
//! `NO_COLOR` turns every slot into [`Color::Reset`] so the glyphs and words have
//! to carry the status on their own. Nothing in this file draws a widget.
//!
//! Also here, because they are the two "format a value for a narrow column"
//! helpers the table and the detail panel share: [`fmt_uptime`] (durations) and
//! [`fmt_unix`] (timestamps). [`cell`] is the `Text`→`Cell` adapter used by the
//! table, and lives here so a table cell and a popup value cannot pick different
//! text-shaping rules.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::Cell;

/// Colour configuration for one frame. Derived from [`palette`] and disabled
/// wholesale when `NO_COLOR` is present in the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Tunnel is forwarding (`●`).
    pub up: Color,
    /// `◐ starting` / `◌ retrying`.
    pub pending: Color,
    /// `✗ failed`.
    pub failed: Color,
    /// `○ off`.
    pub off: Color,
    /// Accent for keys, borders and the type column.
    pub accent: Color,
    /// Default body text.
    pub text: Color,
    /// Secondary text (labels, hints, separators).
    pub dim: Color,
    /// Background of the selected table row / current form field.
    pub selection_bg: Color,
    /// Foreground on top of `selection_bg`.
    pub selection_fg: Color,
    /// `true` when every colour is [`Color::Reset`].
    pub no_color: bool,
}

impl Palette {
    /// Style for a plain foreground colour.
    pub(crate) fn fg(self, color: Color) -> Style {
        Style::default().fg(color)
    }

    pub(crate) fn bg(self, color: Color) -> Style {
        Style::default().bg(color)
    }

    /// Style for the currently focused widget.
    pub(crate) fn focused(self) -> Style {
        Style::default().fg(self.selection_fg).bg(self.selection_bg)
    }
}

/// Palette for the current process, honouring `NO_COLOR`.
pub fn palette() -> Palette {
    palette_with(std::env::var_os("NO_COLOR").is_some())
}

/// Palette with an injected `NO_COLOR` flag (keeps the unit test parallel-safe).
///
/// When `no_color` is set every colour becomes [`Color::Reset`], which makes the
/// glyphs (`● ◐ ◌ ✗ ○`) the only carrier of the status — the accessibility
/// requirement from the spec.
pub fn palette_with(no_color: bool) -> Palette {
    if no_color {
        return Palette {
            up: Color::Reset,
            pending: Color::Reset,
            failed: Color::Reset,
            off: Color::Reset,
            accent: Color::Reset,
            text: Color::Reset,
            dim: Color::Reset,
            selection_bg: Color::Reset,
            selection_fg: Color::Reset,
            no_color: true,
        };
    }
    Palette {
        up: Color::Green,
        pending: Color::Yellow,
        failed: Color::Red,
        off: Color::DarkGray,
        accent: Color::Cyan,
        text: Color::Reset,
        dim: Color::DarkGray,
        selection_bg: Color::DarkGray,
        selection_fg: Color::White,
        no_color: false,
    }
}
/// Human duration used by the 时长 column.
///
/// * `< 1h`   → `mm:ss`  (`00:00`, `01:30`)
/// * `< 24h`  → `1h06m`
/// * else     → `3d11h`
pub fn fmt_uptime(secs: u64) -> String {
    if secs < 60 * 60 {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    } else if secs < 24 * 60 * 60 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{:02}h", secs / 86400, (secs % 86400) / 3600)
    }
}

/// Format a Unix timestamp as local `YYYY-MM-DD HH:MM:SS`.
pub(crate) fn fmt_unix(ts: i64) -> String {
    match chrono::TimeZone::timestamp_opt(&chrono::Local, ts, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "—".to_string(),
    }
}
pub(crate) fn cell(text: impl Into<String>, style: Style) -> Cell<'static> {
    Cell::new(Text::from(Line::from(text.into()))).style(style)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_formats_scale() {
        assert_eq!(fmt_uptime(0), "00:00");
        assert_eq!(fmt_uptime(90), "01:30");
        assert_eq!(fmt_uptime(4000), "1h06m");
        assert_eq!(fmt_uptime(300_000), "3d11h");
        // Boundaries.
        assert_eq!(fmt_uptime(59), "00:59");
        assert_eq!(fmt_uptime(3600), "1h00m");
        assert_eq!(fmt_uptime(86_400), "1d00h");
    }

    #[test]
    fn palette_degrades_with_no_color() {
        let plain = palette_with(true);
        assert!(plain.no_color);
        for c in [
            plain.up,
            plain.pending,
            plain.failed,
            plain.off,
            plain.accent,
            plain.text,
            plain.dim,
            plain.selection_bg,
            plain.selection_fg,
        ] {
            assert_eq!(c, Color::Reset, "NO_COLOR palette must not emit colours");
        }

        let colored = palette_with(false);
        assert!(!colored.no_color);
        assert_eq!(colored.up, Color::Green);
        assert_eq!(colored.pending, Color::Yellow);
        assert_eq!(colored.failed, Color::Red);
        assert_eq!(colored.off, Color::DarkGray);
        assert_ne!(colored.up, colored.failed);
    }

    // -- column layout ------------------------------------------------------

    // -- rendering ----------------------------------------------------------

    #[test]
    fn no_color_mode_drops_every_colour() {
        // Accessibility: with NO_COLOR the UI must still convey state via
        // symbols and words, and must not emit colour codes.
        let p = palette_with(true);
        assert_eq!(p.up, Color::Reset);
        assert_eq!(p.failed, Color::Reset);
        assert_eq!(p.text, Color::Reset);
        assert_eq!(p.selection_bg, Color::Reset);
        assert_eq!(p.accent, Color::Reset);
        // Coloured mode really is coloured.
        let c = palette_with(false);
        assert_ne!(c.up, Color::Reset);
        assert_ne!(c.failed, Color::Reset);
        assert_ne!(c.up, c.failed, "up and failed must be distinguishable");
    }
}

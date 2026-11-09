//! Which sub-pane the form shows under its field rows, and how the body splits
//! for it.
//!
//! Two panes can occupy that slot: the `~/.ssh/config` import table
//! ([`crate::ui::import`]) and the peer's listening ports ([`crate::ui::ports`]).
//! Both hang off the form, both are added to the popup's height rather than
//! borrowed from the fields (a pane that squeezed the fields would hide the values
//! the user is checking against the rows they tick), and both disappear on a
//! terminal too short for them.
//!
//! They never share a frame: opening one closes the other. This module is the one
//! place that decides *which* pane claims the slot, so the popup's height
//! arithmetic and the pane actually drawn cannot disagree — the failure mode there
//! is a popup grown for a pane it does not draw, or a pane drawn into rows it does
//! not own.
//!
//! Belongs here: the pane choice, the popup height, and the body split.
//! Does not belong: what either pane draws (each pane's own module), or the form's
//! fields ([`super`]).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use crate::app::App;
use crate::model::FormField;
use crate::text::truncate_to_width;
use crate::ui::hit::HitMap;
use crate::ui::palette::Palette;
use crate::ui::{import, ports};

/// The sub-pane the form shows under its fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pane {
    /// The forwards `~/.ssh/config` already declares for this host.
    Import,
    /// The ports the peer is listening on.
    Ports,
}

impl Pane {
    /// Height this pane would like; the form grows by exactly this much.
    fn pref_height(self) -> u16 {
        match self {
            Pane::Import => import::PREF_HEIGHT,
            Pane::Ports => ports::PREF_HEIGHT,
        }
    }

    /// Shortest this pane is still worth drawing.
    fn min_height(self) -> u16 {
        match self {
            Pane::Import => import::MIN_HEIGHT,
            Pane::Ports => ports::MIN_HEIGHT,
        }
    }

    /// Draw the pane into `area`.
    pub(super) fn draw(self, frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
        match self {
            Pane::Import => import::draw_pane(frame, app, area, hits),
            Pane::Ports => ports::draw_port_pane(frame, app, area, hits),
        }
    }
}

/// Which sub-pane is shown, if any.
///
/// Visibility goes through the `App` predicates so the renderer and the key
/// handler cannot disagree. Two extra cases are the renderer's to own, because
/// `import_pane_open` is deliberately just "is there a table worth showing":
///
/// * discovery *failed* → the table is empty, but the reason is exactly what the
///   user needs, so the pane opens to show it;
/// * `import_open` set on an empty table → the state is reachable (a filter or a
///   host switch can empty it), and silently collapsing would look like the key
///   did nothing. `import::draw_empty` has a branch for each.
///
/// (`import_error` on its own deliberately does *not* force the pane open:
/// `App::toggle_import_pane` reports the failure on the status line and leaves
/// the pane closed, and two places claiming to show the same reason would be
/// worse than one.)
///
/// The port list is checked first and wins when both are somehow set: the app
/// closes one when opening the other, but the port list is what the submit path
/// keys off (`App::port_batch_intent`), so it is the one that must be visible.
pub(super) fn pane(app: &App) -> Option<Pane> {
    if app.port_picker_visible() {
        return Some(Pane::Ports);
    }
    (app.import_pane_open() || app.form.as_ref().is_some_and(|f| f.import_open)).then_some(Pane::Import)
}

/// Height the popup asks for.
///
/// Tall enough to hold the fields *and* the expanded host dropdown: title border
/// (2) + one line per field + errors/port hint (2) + blank (1) + key hint (1) +
/// dropdown frame (2) + dropdown rows. The sub-pane is *added* on top rather than
/// borrowed from the form, so a pane never squeezes the fields whose values the
/// user is checking against the rows they tick.
pub(super) fn height(pane: Option<Pane>) -> u16 {
    let fields_h = FormField::ORDER.len() as u16;
    let base_h = fields_h + 4 + 2 + crate::app::HOST_DROPDOWN_ROWS as u16 + 2;
    base_h + pane.map_or(0, Pane::pref_height)
}

/// Split the popup body between the field rows and the sub-pane.
///
/// The pane is reserved out of the bottom, but only after the fields and their
/// notes have their rows: on a short terminal the pane is what shrinks (and
/// eventually disappears) so the form stays usable. When it cannot fit at all,
/// the warning goes into `extra` — see [`invisible_note`].
pub(super) fn split(
    app: &App,
    inner: Rect,
    pane: Option<Pane>,
    needed: u16,
    extra: &mut Vec<Line<'static>>,
    p: Palette,
) -> (Rect, Option<Rect>) {
    let Some(pane) = pane else { return (inner, None) };
    let room = inner.height.saturating_sub(needed).min(pane.pref_height());
    if room >= pane.min_height() {
        let body = Rect { height: inner.height - room, ..inner };
        return (body, Some(Rect { x: inner.x, y: inner.y + body.height, width: inner.width, height: room }));
    }
    extra.extend(invisible_note(app, inner.width as usize, p, pane));
    (inner, None)
}

/// Draw the chosen pane into its reserved rect.
pub(super) fn draw(frame: &mut Frame, app: &App, area: Rect, pane: Option<Pane>, hits: &mut HitMap) {
    if let Some(pane) = pane {
        // Drawn last, and outside the field body: a pane owns its own cursor, which
        // must win over the form field cursor.
        pane.draw(frame, app, area, hits);
    }
}

/// The warning for the one genuinely dangerous state: the pane is hidden but its
/// selection is still what the save key would act on.
///
/// `ctrl+s` would create the ticked rows while the user cannot see a single one
/// of them. Say so instead of silently acting on an invisible list — and point at
/// the key that gets them out of that state.
fn invisible_note(app: &App, width: usize, p: Palette, pane: Pane) -> Option<Line<'static>> {
    let form = app.form.as_ref()?;
    let msg = match pane {
        Pane::Import => {
            if !form.import_open || form.import.is_empty() {
                return None;
            }
            format!(
                "⚠ 终端太矮，导入面板未显示；ctrl+s 仍会按已勾选的 {} 条创建。i 收起面板可改用手填",
                form.import.checked_count()
            )
        }
        Pane::Ports => {
            let pick = app.ports.as_ref().filter(|pick| pick.open && pick.checked_count() > 0)?;
            format!(
                "⚠ 终端太矮，端口面板未显示；ctrl+s 仍会按已勾选的 {} 个端口创建。esc 关闭面板可改用手填",
                pick.checked_count()
            )
        }
    };
    Some(Line::from(Span::styled(truncate_to_width(&msg, width), p.fg(p.pending))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PortPick, RemotePort};
    use crate::ui::palette::palette;
    use crate::ui::test_support::*;

    /// A create form with an expanded peer-port list holding `ports`.
    fn app_with_ports(ports: &[u16]) -> App {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        let mut pick = PortPick::scanning("dev-box", true, 1);
        let rows = ports.iter().map(|p| RemotePort::new(*p, "127.0.0.1")).collect();
        assert!(pick.install(1, rows, 0), "the current generation installs");
        // Ticked: that is the state where hiding the pane is *dangerous* (save
        // would create rows nobody can see), and the only one that needs the note.
        pick.check_all();
        app.ports = Some(pick);
        app
    }

    #[test]
    fn the_pane_choice_follows_the_app_state() {
        let mut app = app_with_tunnels(vec![]);
        app.open_form_create();
        assert_eq!(pane(&app), None, "a bare form has no sub-pane");

        app.ports = Some(PortPick::scanning("dev-box", true, 1));
        assert_eq!(pane(&app), Some(Pane::Ports), "a scan in flight is worth showing");

        // A default (collapsed, idle) picker is not: drawing it would add height
        // for nothing.
        app.ports = Some(PortPick::default());
        assert_eq!(pane(&app), None);

        let mut import = app_with_import(sample_forwards());
        assert_eq!(pane(&import), Some(Pane::Import), "the import table still claims the slot");
        // Both somehow set: the port list wins, because it is what the save path
        // keys off.
        import.ports = Some(PortPick::scanning("dev-box", true, 1));
        assert_eq!(pane(&import), Some(Pane::Ports));
    }

    #[test]
    fn the_popup_grows_by_exactly_the_pane_it_shows() {
        let bare = height(None);
        assert_eq!(height(Some(Pane::Import)), bare + import::PREF_HEIGHT);
        assert_eq!(height(Some(Pane::Ports)), bare + ports::PREF_HEIGHT);
        // The port pane is sized for the shared row window, so its preferred
        // height is what the key handler's scroll window assumes.
        assert_eq!(ports::PREF_HEIGHT, 2 + 1 + crate::model::PORT_ROWS_VISIBLE as u16 + 1);
    }

    #[test]
    fn a_short_popup_keeps_the_fields_and_drops_the_pane() {
        let p = palette();
        let app = app_with_ports(&[8080]);
        let mut extra: Vec<Line<'static>> = Vec::new();
        // Room for the fields but not for the pane: the pane disappears and the
        // warning takes its place.
        let inner = Rect { x: 0, y: 0, width: 100, height: 20 };
        let (body, area) = split(&app, inner, Some(Pane::Ports), 18, &mut extra, p);
        assert_eq!(area, None, "no room: the pane must not be squeezed in");
        assert_eq!(body, inner, "the fields keep every row");
        assert!(extra.iter().any(|l| format!("{l:?}").contains("端口面板未显示")), "the hidden batch is announced");

        // A tall popup reserves the pane at the bottom, below the fields.
        let tall = Rect { x: 0, y: 0, width: 100, height: 60 };
        let mut extra = Vec::new();
        let (body, area) = split(&app, tall, Some(Pane::Ports), 18, &mut extra, p);
        let area = area.expect("a tall popup must show the pane");
        assert_eq!(area.height, ports::PREF_HEIGHT, "the pane asks for exactly its preferred height");
        assert_eq!(area.y, body.bottom(), "and it starts where the field body ends");
        assert_eq!(body.height, tall.height - ports::PREF_HEIGHT);
        assert!(extra.is_empty(), "nothing is hidden, so there is nothing to announce");
    }

    #[test]
    fn the_split_never_draws_outside_the_popup() {
        let p = palette();
        let app = app_with_ports(&[8080]);
        for h in 0u16..=70 {
            for w in [12u16, 40, 100] {
                let inner = Rect { x: 3, y: 2, width: w, height: h };
                let mut extra = Vec::new();
                let (body, area) = split(&app, inner, Some(Pane::Ports), 18, &mut extra, p);
                assert!(body.bottom() <= inner.bottom(), "{w}x{h}: the field body escaped the popup");
                if let Some(a) = area {
                    assert!(a.bottom() <= inner.bottom(), "{w}x{h}: the pane escaped the popup");
                    assert!(a.y >= body.bottom(), "{w}x{h}: the pane overlaps the fields");
                    assert!(a.height >= ports::MIN_HEIGHT, "{w}x{h}: a pane below its minimum was drawn");
                }
            }
        }
    }
}

//! Mouse dispatch: resolve a click/scroll through the frame's [`HitMap`] and
//! perform the same state changes the keyboard would.
//!
//! The handler never computes geometry. `ui::draw` records where it actually put
//! every row, checkbox, header and button (`HitMap::push`); this module asks
//! "what is at (x, y)?" and dispatches. That has two consequences worth keeping:
//!
//! * A layout change cannot break the mouse, because there is only one place
//!   that knows the layout.
//! * The mouse inherits the keyboard's safety rules for free: a clicked batch
//!   button still goes through the confirmation gate, because it calls the same
//!   `request_batch` the key does.
//!
//! Clicks on empty space deliberately do nothing rather than being forwarded to
//! whatever is underneath: a mis-click next to a button must not start tunnels.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::app::App;
use crate::ui::hit::HitTarget;

impl App {
    /// Resolve one mouse event. Returns true when it was consumed.
    pub fn handle_mouse(&mut self, m: MouseEvent) -> bool {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.on_left_click(m.column, m.row, m.modifiers),
            MouseEventKind::ScrollUp => self.on_scroll(m.column, m.row, -3),
            MouseEventKind::ScrollDown => self.on_scroll(m.column, m.row, 3),
            _ => false,
        }
    }

    fn on_left_click(&mut self, x: u16, y: u16, modifiers: crossterm::event::KeyModifiers) -> bool {
        let Some(target) = self.hits.at(x, y).cloned() else {
            return false;
        };
        let shift = modifiers.contains(crossterm::event::KeyModifiers::SHIFT);
        match target {
            HitTarget::TunnelRow(id) => {
                self.click_tunnel_row(id, shift);
                true
            }
            HitTarget::TunnelList => true,
            HitTarget::ImportCheckbox(row) => {
                self.import_toggle_at(row);
                true
            }
            HitTarget::ImportRow(row) => {
                self.import_select_at(row);
                true
            }
            HitTarget::ImportColumnHeader(col) => {
                self.begin_import_filter(col);
                true
            }
            HitTarget::ImportFilterChip(col) => {
                self.clear_import_filter(col);
                true
            }
            HitTarget::ImportClearFilters => {
                self.clear_all_import_filters();
                true
            }
            HitTarget::ImportUndoFilter => {
                self.undo_import_filter();
                true
            }
            HitTarget::ImportCheckAll => {
                self.import_check_all();
                true
            }
            HitTarget::ImportInvert => {
                self.import_invert();
                true
            }
            HitTarget::ImportUncheckAll => {
                self.import_uncheck_all();
                true
            }
            HitTarget::ImportList => true,
            HitTarget::BatchAction(action) => {
                self.request_batch(action);
                true
            }
            HitTarget::BatchExit => {
                self.exit_select_mode();
                true
            }
            HitTarget::BatchSelectAll => {
                self.mark_all_toggle();
                true
            }
            // A modal swallowed the click: consume it, do nothing. Letting it
            // fall through would mark/select a row that is painted *under* the
            // dialog, which is both invisible and destructive in selection mode.
            HitTarget::Popup => true,
            HitTarget::FilterBar => true,
            HitTarget::FilterClear => {
                self.filter.clear();
                self.selected = 0;
                true
            }
        }
    }

    /// Click on a main-table row: move the cursor, and in selection mode also
    /// act on the mark (plain click toggles, shift+click extends the range).
    fn click_tunnel_row(&mut self, id: crate::model::TunnelId, shift: bool) {
        let ids = self.visible_ids();
        let Some(pos) = ids.iter().position(|candidate| *candidate == id) else {
            return;
        };
        self.selected = pos;
        if !self.select_mode.active {
            return;
        }
        if shift {
            self.extend_mark_to_cursor();
        } else {
            self.select_mode.toggle(id);
            self.select_mode.anchor = Some(pos);
            let n = self.select_mode.len();
            self.set_status(crate::model::StatusMsg::info(if n == 0 {
                "已选 0 条".to_string()
            } else {
                format!("已选 {n} 条")
            }));
        }
    }

    /// Wheel over a list scrolls it. `delta` is negative for up.
    fn on_scroll(&mut self, x: u16, y: u16, delta: i32) -> bool {
        let Some(target) = self.hits.at(x, y).cloned() else {
            return false;
        };
        match target {
            // Scrolling the table moves the cursor: this is what every TUI list
            // does, and it keeps the wheel useful in a table with no separate
            // scroll offset.
            HitTarget::TunnelRow(_) | HitTarget::TunnelList | HitTarget::BatchAction(_) => {
                self.scroll_selection(delta);
                true
            }
            HitTarget::ImportRow(_)
            | HitTarget::ImportCheckbox(_)
            | HitTarget::ImportList
            | HitTarget::ImportColumnHeader(_)
            | HitTarget::ImportFilterChip(_) => {
                self.import_move(delta);
                true
            }
            _ => false,
        }
    }

    /// Move the main-table cursor `delta` rows (negative = up).
    ///
    /// Stops as soon as the cursor stops moving: both `select_next` and
    /// `select_prev` clamp at the ends, so walking `delta` times is wasted work
    /// at best and an unbounded loop for a huge `delta` at worst.
    fn scroll_selection(&mut self, delta: i32) {
        for _ in 0..delta.unsigned_abs() {
            let before = self.selected;
            if delta > 0 {
                self.select_next();
            } else {
                self.select_prev();
            }
            if self.selected == before {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{ForwardKind, TunnelConfig};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    fn tunnel(id: crate::model::TunnelId, name: &str, port: u16) -> TunnelConfig {
        TunnelConfig {
            id,
            name: name.into(),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: port,
            remote_host: "127.0.0.1".into(),
            remote_port: 22,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    fn app_with(tunnels: Vec<TunnelConfig>) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
        crate::config::save(&paths.config, &cfg).unwrap();
        (dir, App::new(paths))
    }

    fn scroll(app: &mut App, kind: MouseEventKind) -> bool {
        app.handle_mouse(crossterm::event::MouseEvent { kind, column: 5, row: 5, modifiers: KeyModifiers::NONE })
    }

    #[test]
    fn a_click_on_empty_space_does_nothing() {
        // The hit map is empty, so nothing is under the click; it must not be
        // forwarded to whatever is conceptually beneath.
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        let consumed = app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!consumed);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn scrolling_a_list_is_bounded_even_with_no_rows() {
        // A wheel event with a large delta must not loop on an empty table:
        // `select_next` is a no-op there, and the loop stops as soon as the
        // cursor stops moving.
        let (_d, mut app) = app_with(vec![]);
        app.hits.push(Rect::new(0, 0, 40, 10), HitTarget::TunnelList);
        assert!(scroll(&mut app, MouseEventKind::ScrollDown));
        assert!(scroll(&mut app, MouseEventKind::ScrollUp));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn scrolling_the_table_moves_and_clamps_the_cursor() {
        let (_d, mut app) = app_with((1..=3).map(|i| tunnel(i, &format!("t{i}"), 18000 + i as u16)).collect());
        app.hits.push(Rect::new(0, 0, 40, 10), HitTarget::TunnelList);
        scroll(&mut app, MouseEventKind::ScrollDown);
        assert_eq!(app.selected, 2, "one wheel notch is three rows, clamped to the end");
        scroll(&mut app, MouseEventKind::ScrollUp);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn a_click_on_the_import_checkbox_of_a_stale_row_does_not_tick_anything() {
        // The hit map carries raw row indices; after a re-filter the index can
        // point at a row that is no longer visible. Clicking must not tick a
        // different row.
        let (_d, mut app) = app_with(vec![]);
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            let forwards: Vec<crate::model::SshForward> = (0..3)
                .map(|i| crate::model::SshForward {
                    kind: ForwardKind::Local,
                    listen_addr: "127.0.0.1".into(),
                    listen_port: 18001 + i,
                    target_host: "10.0.0.1".into(),
                    target_port: 80,
                    host_alias: "dev-box".into(),
                    line: i as usize + 1,
                    raw: String::new(),
                })
                .collect();
            f.ssh_host.set("dev-box");
            f.set_import_table("dev-box", crate::model::ImportTable::new(forwards));
            f.import_open = true;
        }
        let checked_before = app.form.as_ref().unwrap().import.checked_count();
        app.hits.push(Rect::new(0, 0, 40, 10), HitTarget::ImportCheckbox(999));
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.form.as_ref().unwrap().import.checked_count(),
            checked_before,
            "a stale checkbox hit must not toggle a row"
        );
    }

    #[test]
    fn a_click_on_a_modal_background_does_not_reach_the_row_underneath() {
        // `HitTarget::Popup` is consumed so a dialog cannot leak clicks to the
        // table behind it (destructive in selection mode).
        let (_d, mut app) = app_with(vec![tunnel(1, "a", 18001)]);
        app.toggle_select_mode();
        app.hits.push(Rect::new(0, 0, 40, 10), HitTarget::Popup);
        let consumed = app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert!(consumed);
        assert!(app.select_mode.is_empty(), "nothing may be marked through the dialog");
    }

    #[test]
    fn a_shift_click_on_a_row_extends_the_range_without_panicking() {
        let (_d, mut app) = app_with((1..=3).map(|i| tunnel(i, &format!("t{i}"), 18000 + i as u16)).collect());
        app.toggle_select_mode();
        app.hits.push(Rect::new(0, 0, 40, 10), HitTarget::TunnelRow(3));
        app.handle_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: KeyModifiers::SHIFT,
        });
        assert!(app.select_mode.is_marked(3));
    }
}

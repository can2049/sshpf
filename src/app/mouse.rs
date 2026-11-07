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
                for _ in 0..delta.unsigned_abs() {
                    if delta > 0 {
                        self.select_next();
                    } else {
                        self.select_prev();
                    }
                }
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
}

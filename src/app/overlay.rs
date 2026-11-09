//! Opening and closing overlays: help, the form, the detail view, confirmations.
//!
//! An overlay is the only thing that captures keys, so exactly one may be open
//! (see [`Overlay`]). `open_*` sets it and resets the relevant scroll position;
//! `close_overlay` is the single place that tears one down — including the form
//! state and the host dropdown that live *inside* the form overlay.
//!
//! The panel contents are rendered by [`crate::ui`] from the same enum, so a new
//! overlay cannot be added here without the renderer noticing.

use crate::app::FormState;

use super::{App, Overlay};

impl App {
    // -- overlays ----------------------------------------------------------

    pub fn open_help(&mut self) {
        self.overlay = Overlay::Help;
        self.help_scroll = 0;
    }

    pub fn open_form_create(&mut self) {
        let bind = self.cfg.defaults.bind.clone();
        let mut form = FormState::create(&bind);
        form.auto_restart = self.cfg.defaults.auto_restart;
        self.form = Some(form);
        self.overlay = Overlay::Form;
    }

    pub fn open_form_edit(&mut self) {
        let Some(view) = self.selected_tunnel() else {
            return;
        };
        self.form = Some(FormState::edit(&view.cfg));
        self.overlay = Overlay::Form;
    }

    pub fn open_detail(&mut self) {
        if let Some(id) = self.selected_id() {
            self.overlay = Overlay::Detail(id);
            self.detail_scroll = 0;
        }
    }

    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
        self.form = None;
        self.picker = None;
        // The port list belongs to the form: leaving it behind would let a later
        // form (for another host) show stale rows.
        self.ports = None;
        self.port_rx = None;
    }
}

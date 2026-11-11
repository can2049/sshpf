//! The overlay vocabulary and the code that opens and closes overlays: help, the
//! form, the detail view, confirmations, the filter.
//!
//! An overlay is the only thing that captures keys, so exactly one may be open at a
//! time (see [`Overlay`]). `open_*` sets it and resets the relevant scroll
//! position; [`App::close_overlay`] is the single place that tears one down —
//! including the form state and the host dropdown that live *inside* the form
//! overlay.
//!
//! The two enums live here rather than in `app.rs` because they are the units this
//! module acts on: a new overlay is added next to the code that opens it, and the
//! renderer builds its panels from the same enum, so the two cannot drift.
//!
//! Belongs here: `Overlay`, `ConfirmAction`, and the open/close transitions.
//! Does not belong: what a confirmed action *does* ([`crate::app::confirm`]) or the
//! prompt wording ([`crate::app::confirm`]).

use crate::model::{BatchAction, TunnelId};

use super::{App, FormState};

/// Which overlay (if any) currently captures keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    /// Create/edit form. The SSH-host dropdown lives *inside* this overlay
    /// (`App::picker`), so it does not need its own variant.
    Form,
    /// Destructive-action confirmation.
    Confirm {
        action: ConfirmAction,
        prompt: String,
    },
    /// Scrollable full detail for one tunnel.
    Detail(TunnelId),
    /// Inline filter editing on the main table.
    Filter,
}

/// A destructive or otherwise consequential action awaiting confirmation.
///
/// Anything that (a) loses data, (b) drops live connections, or (c) is hard to
/// notice afterwards goes through this gate. Cheap, reversible, in-place actions
/// (moving the cursor, filtering) deliberately do not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    /// Remove one tunnel definition (loses the config entry).
    Delete(TunnelId),
    /// Stop one running tunnel (drops its live connections).
    Stop(TunnelId),
    /// Stop every running tunnel.
    StopAll,
    /// Restart one tunnel (briefly drops its connections).
    Restart(TunnelId),
    /// Start every stopped tunnel (opens many listeners at once).
    StartAll,
    /// Save edits to an existing tunnel (replaces its running process).
    ApplyEdit(TunnelId),
    /// Discard unsaved edits (loses typed input).
    DiscardForm,
    /// Apply one operation to every marked tunnel in selection mode.
    Batch { action: BatchAction, count: usize },
    /// Quit the program (stops every tunnel).
    Quit,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, Paths};

    fn app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crateless_save(&paths.config);
        (dir, App::new(paths))
    }

    /// Write an empty config so `App::new` has a file to load.
    fn crateless_save(path: &std::path::Path) {
        crate::config::save(path, &ConfigFile::default()).unwrap();
    }

    #[test]
    fn opening_an_overlay_replaces_the_previous_one() {
        // Exactly one overlay captures keys, so this is a rule and not a detail.
        let (_d, mut app) = app();
        app.open_help();
        assert_eq!(app.overlay, Overlay::Help);
        app.open_form_create();
        assert_eq!(app.overlay, Overlay::Form);
        assert!(app.form.is_some());
    }

    #[test]
    fn closing_tears_down_the_form_and_its_substates() {
        let (_d, mut app) = app();
        app.open_form_create();
        app.close_overlay();
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.form.is_none(), "the form is part of the overlay");
        assert!(app.picker.is_none(), "the host dropdown lives inside the form");
        assert!(app.ports.is_none(), "a stale port list must not outlive its form");
    }

    #[test]
    fn opening_help_resets_its_scroll() {
        let (_d, mut app) = app();
        app.help_scroll = 12;
        app.open_help();
        assert_eq!(app.help_scroll, 0);
    }
}

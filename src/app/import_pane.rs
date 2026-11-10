//! The import pane: discovering a host's forwards and driving the pane's widget.
//!
//! This is the *interface* half of importing — everything the user does to the
//! table (open/collapse the pane, filter columns, move the cursor, tick rows).
//! The *commit* half (turning ticked rows into tunnels) lives in
//! [`super::import`]; keeping them apart means the pane can be tested without any
//! persistence and the batch can be tested without any UI state.
//!
//! Two design points worth keeping:
//!
//! * The pane **opens itself** as soon as a host with importable forwards is
//!   committed. "Default all ticked" is only a real default if the rows are on
//!   screen without a second key press — an offer the user has to discover is not
//!   a default. It only auto-opens when there is at least one *importable* row.
//! * Discovery is **not repeated** for the same host: re-running it would throw
//!   away the user's tick marks and filters on every keystroke.
//! * The auto-open is an *offer*, not an instruction. Everything the user does to
//!   the pane deliberately — opening it by hand, focusing a column filter, ticking
//!   or unticking rows — marks the table
//!   `touched`, and only a touched table *with at least one row still ticked*
//!   outranks a complete hand-filled form on `ctrl+s` (unticking every row is an
//!   instruction to create none of them, which leaves the pane nothing to carry
//!   out). See `form_submit`'s module docs for the routing rule.

use crate::app::{App, FormMode};
use crate::eventlog;
use crate::help;
use crate::model::{ImportColumn, ImportTable, SshForward, StatusMsg};
use crate::{keys, ssh};

impl App {
    /// Read the forwards declared for `host_alias` and load them into the open
    /// form's import table.
    ///
    /// Called when the SSH HOST field changes (and when the picker accepts a
    /// host), so the table always describes the host currently in the field.
    /// Re-running for the same host is a no-op, which keeps typing cheap.
    pub fn refresh_import_table(&mut self, host_alias: &str) {
        let alias = host_alias.trim().to_string();
        let Some(form) = self.form.as_ref() else {
            return;
        };
        // Editing an existing tunnel must not silently rewrite it into a batch:
        // the import table only exists for new tunnels.
        if matches!(form.mode, FormMode::Edit(_)) {
            return;
        }
        // Same host: keep the table *and* the user's tick marks / collapsed
        // state. Re-discovery here would throw away choices they already made.
        if form.import_host_name() == alias {
            return;
        }
        // Empty field means "no host chosen yet". Do not scan for it: `Host *`
        // matches the empty string, which would list the global forwards under
        // an alias the user never picked.
        if alias.is_empty() {
            self.install_import_rows(String::new(), Vec::new());
            self.import_error = None;
            return;
        }

        let (forwards, error) = discover_forwards(&alias);
        self.apply_import_discovery(alias, forwards, error);
    }

    /// Install what discovery found for `host` and record any error to show.
    ///
    /// Split from [`Self::refresh_import_table`] so the "rows arrived, now what"
    /// half can be tested without touching the filesystem or the process
    /// environment (which would need `unsafe` under Rust 2024).
    pub(crate) fn apply_import_discovery(&mut self, host: String, forwards: Vec<SshForward>, error: Option<String>) {
        self.install_import_rows(host, forwards);
        self.import_error = error;
    }

    /// Install the rows discovered for `host` and decide whether the pane shows.
    ///
    /// This is the whole "default, not a hidden offer" rule, kept separate from
    /// discovery so it can be unit-tested without a real `~/.ssh/config`:
    ///
    /// * at least one **importable** row → open, every importable row already
    ///   ticked (`ImportTable::new` does the ticking) and the user unticks what
    ///   they do not want;
    /// * no importable row (either nothing at all, or only rows that cannot be
    ///   mapped onto a `TunnelConfig`) → stay closed, so the form is not covered
    ///   by a pane with nothing to tick. `space`/`i` still opens it, which is how
    ///   the un-importable rows explain themselves.
    ///
    /// Only a *changed* host reaches this (see `refresh_import_table`), so
    /// re-committing the same alias cannot undo a deliberate collapse.
    pub(crate) fn install_import_rows(&mut self, host: impl Into<String>, forwards: Vec<SshForward>) {
        // Rows whose forward is already a saved tunnel start unticked and stay
        // that way: see `ImportTable::with_existing`. This is the fix for "every
        // new port also re-adds cloud_pc's two forwards" — the pane used to offer
        // them as ticked defaults on every open, so `ctrl+s` re-created them.
        let existing: Vec<Option<String>> = forwards.iter().map(|f| self.saved_forward_name(f)).collect();
        let table = ImportTable::with_existing(forwards, existing);
        // Auto-open still keys off rows that can be *created*: a pane whose every
        // row is already saved has no default to offer, so opening it
        // automatically would cover the fields the user just reached for. It stays
        // one key away (`space`/`i`), where it explains the dimmed rows.
        let creatable = table.importable_count();
        if let Some(form) = self.form.as_mut() {
            form.set_import_table(host, table);
            form.import_open = creatable > 0;
        }
    }

    /// Re-run discovery for whatever is in the SSH HOST field.
    pub fn refresh_import_for_current_host(&mut self) {
        let alias = match self.form.as_ref() {
            Some(f) => f.ssh_host.value().to_string(),
            None => return,
        };
        self.refresh_import_table(&alias);
    }

    /// `space`/`i` on the SSH HOST field: show or hide the import pane.
    ///
    /// Loads the table first if it is stale, so the pane can never display rows
    /// belonging to a different host.
    ///
    /// A *manual* open marks the table [`touched`](crate::model::ImportTable::touched):
    /// the user went out of their way to put these rows on screen, which is a
    /// stronger statement of intent than the auto-open default. Collapsing does
    /// not clear the flag — having moved the pane at all is what makes the pane
    /// outrank a hand-filled form.
    pub fn toggle_import_pane(&mut self) {
        self.refresh_import_for_current_host();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.import.is_empty() {
            self.import_error = self.import_error.clone().or_else(|| {
                Some(help::bi(
                    &format!("{} 在 ~/.ssh/config 中没有端口转发配置", form.ssh_host.value()),
                    &format!("{} has no port forwards in ~/.ssh/config", form.ssh_host.value()),
                ))
            });
            form.import_open = false;
            let msg = self.import_error.clone().unwrap_or_default();
            self.set_status(StatusMsg::warn(msg));
            return;
        }
        form.import_open = !form.import_open;
        if form.import_open {
            form.import.touched = true;
        }
        form.import.end_filter();
    }

    pub fn close_import_pane(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import_open = false;
            form.import.end_filter();
        }
    }

    pub fn import_pane_open(&self) -> bool {
        self.form.as_ref().is_some_and(|f| f.import_open && !f.import.is_empty())
    }

    // -- filter editing ----------------------------------------------------

    /// Start typing into one column's filter (bound to the header click /
    /// `f` cycling, and to mouse clicks on a header).
    ///
    /// Leaves the text field's editing stage if it was active. Two cursors cannot
    /// both own the keyboard, and after this change the loser is decided by
    /// `is_editing_field()` rather than by draw order — so a mouse click on a
    /// header while a field was being edited would otherwise show the filter
    /// cursor in the pane while every keystroke still went into the field.
    ///
    /// Marks the table touched: focusing a column filter is the user working the
    /// pane, not merely looking at the default offer.
    pub fn begin_import_filter(&mut self, col: ImportColumn) {
        if let Some(form) = self.form.as_mut() {
            form.end_edit();
            form.import.begin_filter(col);
            form.import.touched = true;
            form.import_open = true;
        }
    }

    pub fn end_import_filter(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.end_filter();
        }
    }

    /// Route a character into the filter of the column being edited.
    pub fn import_filter_char(&mut self, c: char) {
        if let Some(form) = self.form.as_mut()
            && let Some(col) = form.import.editing
        {
            form.import.filter_char(col, c);
        }
    }

    pub fn import_filter_backspace(&mut self) {
        if let Some(form) = self.form.as_mut()
            && let Some(col) = form.import.editing
        {
            form.import.filter_backspace(col);
        }
    }

    /// Delete the filter on `col` (chip `×`, or `F` on a column).
    pub fn clear_import_filter(&mut self, col: ImportColumn) {
        if let Some(form) = self.form.as_mut() {
            form.import.clear_filter(col);
        }
    }

    pub fn clear_all_import_filters(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.clear_all_filters();
        }
    }

    /// Undo the last filter change. Reports when there is nothing to undo rather
    /// than appearing to do nothing.
    pub fn undo_import_filter(&mut self) {
        let undone = self.form.as_mut().is_some_and(|f| f.import.undo_filter());
        if undone {
            self.set_status(StatusMsg::info(help::bi("已撤销筛选", "Filter change undone")));
        } else {
            self.set_status(StatusMsg::info(help::bi("没有可撤销的筛选", "No filter change to undo")));
        }
    }

    // -- row selection -----------------------------------------------------

    pub fn import_move(&mut self, delta: i32) {
        let rows = keys::IMPORT_ROWS_VISIBLE;
        if let Some(form) = self.form.as_mut() {
            form.import.move_cursor(delta);
            form.import.ensure_visible(rows);
        }
    }

    pub fn import_home(&mut self) {
        let rows = keys::IMPORT_ROWS_VISIBLE;
        if let Some(form) = self.form.as_mut() {
            form.import.home();
            form.import.ensure_visible(rows);
        }
    }

    pub fn import_end(&mut self) {
        let rows = keys::IMPORT_ROWS_VISIBLE;
        if let Some(form) = self.form.as_mut() {
            form.import.end();
            form.import.ensure_visible(rows);
        }
    }

    pub fn import_toggle_cursor(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.toggle_cursor();
        }
    }

    /// Toggle the row at a raw index (mouse click on a checkbox).
    ///
    /// The index comes from the frame's hit map, which can be one frame stale:
    /// a filter change or a re-discovery between drawing and clicking leaves the
    /// click pointing at a row that is no longer visible. That must be a no-op —
    /// moving the cursor to "wherever it happens to be" and ticking *that* row
    /// would tick a row the user did not click.
    pub fn import_toggle_at(&mut self, row: usize) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(pos) = form.import.visible().iter().position(|i| *i == row) else {
            eventlog::import_click_stale(row);
            return;
        };
        // Move the cursor there too: the click both selects and toggles, so the
        // highlighted row always matches what the user just acted on.
        form.import.cursor = pos;
        form.import.toggle_cursor();
    }

    /// Put the cursor on a raw row index (mouse click elsewhere on a row).
    pub fn import_select_at(&mut self, row: usize) {
        let Some(form) = self.form.as_mut() else {
            return;
        };
        match form.import.visible().iter().position(|i| *i == row) {
            Some(pos) => form.import.cursor = pos,
            None => eventlog::import_click_stale(row),
        }
    }

    pub fn import_check_all(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.check_all_visible();
        }
    }

    pub fn import_uncheck_all(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.uncheck_all_visible();
        }
    }

    pub fn import_invert(&mut self) {
        if let Some(form) = self.form.as_mut() {
            form.import.invert_visible();
        }
    }
}

/// Read the forwards `alias` declares in `~/.ssh/config`, turning a read failure
/// into the bilingual sentence the pane shows.
///
/// Returns an empty row set *and* the message on failure: a discovery error is a
/// degraded-but-usable state (the pane can still be opened to explain itself),
/// not a reason to abort the form.
fn discover_forwards(alias: &str) -> (Vec<SshForward>, Option<String>) {
    match ssh::forwards::forwards_for_host(alias) {
        Ok(rows) => (rows, None),
        Err(e) => (
            Vec::new(),
            Some(help::bi(
                &format!("读取 ~/.ssh/config 转发配置失败: {e}"),
                &format!("Cannot read the forward rules in ~/.ssh/config: {e}"),
            )),
        ),
    }
}

#[cfg(test)]
mod tests;

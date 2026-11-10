//! Persistence: turning the live [`App`] state into the two files on disk.
//!
//! Two different things are persisted, and conflating them loses user intent:
//!
//! * **`config.toml`** — the tunnel *definitions* plus the `enabled` flag (should
//!   it come up next time?). Written by [`App::persist`].
//! * **`state.toml`** — the *runtime* status of every tunnel from the last run,
//!   used to hint what to restore. Written by [`App::save_state`].
//!
//! Both are best-effort by design: a failed write is logged and (for the config)
//! surfaced in the status bar, never fatal — the change is live in memory either
//! way, and a TUI that refuses to run because it cannot write a file is worse
//! than one that says so.

use crate::config;
use crate::eventlog;
use crate::help;
use crate::model::{StatusMsg, now_unix};

use super::App;

impl App {
    // -- persistence -------------------------------------------------------

    pub fn persist(&mut self) -> Result<(), String> {
        // Only the owner writes files. A mirror's `cfg` is a copy of what the
        // server last sent, so writing it back would race the server's own write
        // and could resurrect a tunnel the server just deleted.
        if self.is_mirror() {
            return Ok(());
        }
        for view in &self.tunnels {
            if let Some(slot) = self.cfg.find_mut(view.cfg.id) {
                slot.enabled = view.cfg.enabled;
                slot.name = view.cfg.name.clone();
            }
        }
        config::save(&self.paths.config, &self.cfg).map_err(|e| {
            eventlog::config_save_failed(&config::display_path(&self.paths.config), &e.to_string());
            e.to_string()
        })
    }

    pub(crate) fn persist_quietly(&mut self) {
        if let Err(e) = self.persist() {
            self.set_status(StatusMsg::error(help::bi(
                &format!("保存配置失败: {e}"),
                &format!("Cannot save the config: {e}"),
            )));
        }
    }

    pub fn save_state(&self) {
        // Same reason as `persist`: `state.toml` describes what *this* process
        // ran. A mirror ran nothing, and its table is a copy of the server's.
        if self.is_mirror() {
            return;
        }
        let mut st = config::StateFile { last_run: now_unix(), ..Default::default() };
        for view in &self.tunnels {
            st.last_state.insert(view.cfg.id.to_string(), view.status.key().to_string());
        }
        if let Err(e) = config::save_state(&self.paths.state, &st) {
            eventlog::state_save_failed(&config::display_path(&self.paths.state), &e.to_string());
        }
    }
}

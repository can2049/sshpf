//! Saving the form: validation, persistence and the supervisor, in that order.
//!
//! The [form state](super::form) is a pure state machine — it never spawns, never
//! persists and can be unit-tested alone. *Submitting* it is the opposite: it
//! touches three collaborators (the config file, the retry bookkeeping, the
//! tunnel supervisor), so it needs [`App`]. That asymmetry is why this file
//! exists next to `form.rs` rather than inside it.
//!
//! The one rule that matters here: a create is additive and saves straight away,
//! while an edit replaces a persisted definition **and bounces a live process**,
//! so it goes through the confirmation gate first.

use crate::app::{App, ConfirmAction, FormMode};
use crate::eventlog;
use crate::help;
use crate::model::{TunnelConfig, TunnelId, TunnelView};

// ---------------------------------------------------------------------------
// Form-driven App operations
// ---------------------------------------------------------------------------
//
// Saving the form touches three collaborators (validation, persistence, the
// process supervisor), so it needs `App`; but it is entirely about the form,
// hence it lives next to the form state rather than in the orchestration file.

impl App {
    /// Validate the open form and either save immediately (new tunnel) or ask
    /// for confirmation (editing an existing one, which replaces a running
    /// process and therefore drops live connections).
    pub fn submit_form(&mut self) {
        // The import pane owns the submit *while it is on screen*: it lists what
        // the host already declares, so those rows are the intent and the port
        // fields are only a scratch pad. Validating the scratch fields would
        // reject a perfectly good batch (they are typically empty once the pane is
        // in use) — and worse, unticking every row would fall through to the
        // single-field path and report "名称不能为空", which describes a field the
        // user is not even looking at.
        //
        // The pane is open by default now, so "has rows" alone would hijack the
        // submit for someone who collapsed it on purpose to type one forward by
        // hand. Visibility, not row count, is what says which of the two the user
        // is working with.
        if let Some(form) = self.form.as_ref()
            && !form.is_modifying()
            && form.import_open
            && !form.import.is_empty()
        {
            self.commit_form_create_from_import();
            return;
        }

        // The port list is the other batch source: while it is on screen with
        // rows ticked, saving means "one tunnel per ticked port". It is checked
        // after the import pane because the two cannot both be open (opening one
        // closes the other), and only while *visible* — a user who collapsed it
        // to type by hand must get the single tunnel they are looking at.
        if self.port_batch_intent() {
            let configs = self.validated_port_batch();
            match configs {
                Some(configs) if !configs.is_empty() => self.commit_port_batch(configs),
                Some(_) | None => {
                    if self.form.as_ref().is_some_and(|f| f.errors.is_empty())
                        && let Some(f) = self.form.as_mut()
                    {
                        f.errors = vec![help::bi("没有勾选任何端口", "No port is ticked")];
                    }
                }
            }
            return;
        }

        let Some(cfg) = self.validated_form_config() else {
            return;
        };
        match cfg.clone() {
            // Creating a tunnel is additive and easily undone by deleting it,
            // so it saves straight away and connects immediately.
            _ if matches!(self.form.as_ref().map(|f| f.mode), Some(FormMode::Create)) => {
                self.commit_form_create(cfg);
            }
            // Editing rewrites persisted config and bounces a live process.
            _ => {
                let id = cfg.id;
                self.pending_form = Some(cfg);
                self.ask_confirm(ConfirmAction::ApplyEdit(id));
            }
        }
    }

    /// Turn the current form into a validated `TunnelConfig`, reporting field
    /// errors into the form instead of acting when anything is wrong.
    fn validated_form_config(&mut self) -> Option<TunnelConfig> {
        let form = self.form.clone()?;
        let existing = match form.mode {
            FormMode::Edit(id) => self.cfg.find(id).cloned(),
            FormMode::Create => None,
        };
        let id = match form.mode {
            FormMode::Edit(id) => id,
            // A hand-edited `id = u64::MAX` leaves no allocatable id; report it
            // like any other validation problem instead of overflowing.
            FormMode::Create => match self.next_tunnel_id() {
                Some(id) => id,
                None => {
                    if let Some(f) = self.form.as_mut() {
                        f.errors = vec!["无法分配新的转发编号，请检查配置中的 id".to_string()];
                    }
                    return None;
                }
            },
        };
        let mut cfg = match form.to_config(id, existing.as_ref()) {
            Ok(c) => c,
            Err(errs) => {
                if let Some(f) = self.form.as_mut() {
                    f.errors = errs;
                }
                return None;
            }
        };
        // Cross-entry conflict: same listen endpoint as another enabled tunnel.
        if let Some(conflict) = self.listen_conflict(&cfg) {
            if let Some(f) = self.form.as_mut() {
                f.errors = vec![help::bi(
                    &format!("监听地址与「{conflict}」冲突"),
                    &format!("Listen endpoint collides with \"{conflict}\""),
                )];
            }
            return None;
        }
        cfg.name = self.cfg.unique_name(
            &cfg.name,
            match form.mode {
                FormMode::Edit(id) => Some(id),
                FormMode::Create => None,
            },
        );
        Some(cfg)
    }

    fn commit_form_create(&mut self, mut cfg: TunnelConfig) {
        cfg.enabled = true;
        eventlog::tunnel_created(&cfg, "form");
        self.cfg.tunnels.push(cfg.clone());
        // Remember where the row landed *before* pushing, rather than recovering
        // it as `len() - 1` afterwards: the arithmetic is the same today, but it
        // stops being an underflow trap if the push ever moves.
        let pos = self.tunnels.len();
        self.tunnels.push(TunnelView::new(cfg.clone()));
        let vis = self.visible_indices();
        self.selected = vis.iter().position(|i| *i == pos).unwrap_or(vis.len().saturating_sub(1));
        self.start_tunnel_with(cfg.id, "created");
        self.close_overlay();
        self.persist_quietly();
    }

    /// Apply the pending edit that the user confirmed.
    pub(crate) fn commit_form_edit(&mut self, eid: TunnelId) {
        let Some(cfg) = self.pending_form.take() else {
            return;
        };
        if let Some(slot) = self.cfg.find_mut(eid) {
            let keep_enabled = slot.enabled;
            let mut next = cfg.clone();
            next.id = eid;
            next.enabled = keep_enabled || next.enabled;
            *slot = next;
        }
        if let Some(v) = self.tunnels.iter_mut().find(|t| t.cfg.id == eid) {
            let enabled = v.cfg.enabled;
            v.cfg = cfg.clone();
            v.cfg.id = eid;
            v.cfg.enabled = enabled || v.cfg.enabled;
        }
        if let Some(view) = self.tunnels.iter().find(|t| t.cfg.id == eid) {
            eventlog::tunnel_updated(&view.cfg);
        }
        self.supervisor.stop(eid);
        self.forget_runtime_state(eid);
        self.start_tunnel_with(eid, "edited");
        self.close_overlay();
        self.persist_quietly();
    }

    /// Whether the open form has changes worth warning about when discarding.
    pub fn form_has_unsaved_edits(&self) -> bool {
        let Some(form) = self.form.as_ref() else {
            return false;
        };
        match form.mode {
            FormMode::Create => {
                !form.ssh_host.value().trim().is_empty()
                    || !form.name.value().trim().is_empty()
                    || !form.remote_port.value().trim().is_empty()
                    || !form.local_port.value().trim().is_empty()
            }
            // Editing: compare against the stored definition.
            FormMode::Edit(id) => match self.cfg.find(id) {
                Some(orig) => {
                    form.name.value() != orig.name
                        || form.ssh_host.value() != orig.ssh_host
                        || form.local_addr.value() != orig.local_addr
                        || form.local_port.value() != orig.local_port.to_string()
                        || form.remote_host.value() != orig.remote_host
                        || form.remote_port.value() != orig.remote_port.to_string()
                        || form.kind() != orig.kind
                        || form.auto_restart != orig.auto_restart
                }
                None => true,
            },
        }
    }

    /// Request closing the form, asking first if there is unsaved input.
    pub fn request_close_form(&mut self) {
        if self.form_has_unsaved_edits() {
            self.ask_confirm(ConfirmAction::DiscardForm);
        } else {
            self.close_overlay();
        }
    }
}
#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::config::{ConfigFile, Paths};
    use crate::model::ForwardKind;

    #[test]
    fn an_exhausted_id_space_is_reported_into_the_form() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let mut cfg = ConfigFile::default();
        cfg.tunnels.push(crate::model::TunnelConfig {
            id: u64::MAX,
            name: "huge".into(),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 18001,
            remote_host: "127.0.0.1".into(),
            remote_port: 22,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        });
        crate::config::save(&paths.config, &cfg).unwrap();
        let mut app = App::new(paths);
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("host");
            f.name.set("fresh");
            f.remote_port.set("22");
            f.local_port.set("18002");
        }
        app.submit_form();
        assert_eq!(app.tunnels.len(), 1, "no new definition without a free id");
        let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
        assert!(errors.iter().any(|e| e.contains("编号")), "{errors:?}");
    }

    #[test]
    fn a_new_tunnel_lands_on_the_row_it_was_added_at() {
        // `commit_form_create` used to recover the new row as `len() - 1`; the
        // cursor must still land on that row.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        let mut app = App::new(paths);
        app.open_form_create();
        {
            let f = app.form.as_mut().unwrap();
            f.ssh_host.set("host");
            f.name.set("fresh");
            f.remote_port.set("22");
            f.local_port.set("18003");
        }
        app.submit_form();
        assert_eq!(app.tunnels.len(), 1);
        assert_eq!(app.selected_tunnel().map(|t| t.cfg.name.as_str()), Some("fresh"));
    }
}

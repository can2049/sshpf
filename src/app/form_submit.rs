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
//!
//! # Which of the two creates the user means
//!
//! A new tunnel can be created two ways: from the port fields the user typed, or
//! from the `~/.ssh/config` rows in the import pane. The pane **auto-opens** and
//! **auto-ticks** the host's existing forwards (that default is the product
//! promise: "default all ticked" with zero extra keys). So "the pane is on
//! screen" cannot by itself mean "the pane is the instruction" — it is also the
//! state of someone who picked a host and then started typing one specific
//! forward by hand.
//!
//! The router therefore separates the two:
//!
//! * **Default offer** — an untouched pane (no tick was changed, no filter was
//!   touched, the user never opened it by hand) is only a suggestion. A form
//!   that is complete on its own is an explicit instruction and wins.
//! * **Explicit pane work** — once the user has touched the rows at all
//!   (`form.import.touched`), the pane is what they mean by "save", and the
//!   port fields are ignored exactly as before.
//! * **Incomplete form** — nothing was typed that could be a tunnel, so the
//!   untouched pane keeps winning. That preserves the headline workflow: pick a
//!   host, hit `ctrl+s`, import everything.
//! * **Emptied pane** — touching the rows can also mean *unticking all of
//!   them* ("create none of these"). That leaves the pane with nothing to
//!   create, so it cannot be the instruction: a complete typed form wins, and
//!   the pane keeps the key only to report "没有勾选任何转发" when nothing was
//!   typed either. Reporting that over a valid form was a bug: the user unticked
//!   the host's rows precisely because they wanted a different forward.
//!
//! When a complete form wins over a still-offering pane, the success notice says
//! so: the pane disappears without its rows being created, and silence there
//! would read as "the batch ran too".

use crate::app::{App, ConfirmAction, FormMode};
use crate::eventlog;
use crate::help;
use crate::model::{LogLevel, StatusMsg, TunnelConfig, TunnelId, TunnelView};

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
    ///
    /// For a create, the import pane only owns the submit while it is *on screen,
    /// the user has actually used it, and it still has something ticked* — or
    /// while the typed fields do not add up to a tunnel at all. See the module
    /// docs for the full rule; the short version is "a touched pane with rows to
    /// create, or an incomplete form ⇒ batch, else the typed one".
    pub fn submit_form(&mut self) {
        let intent = self.save_intent_snapshot();

        if self.import_pane_wins_submit() {
            intent.record("import");
            self.commit_form_create_from_import();
            return;
        }

        // The port list is the other batch source: while it is on screen with
        // rows ticked, saving means "one tunnel per ticked port". It is checked
        // after the import pane because the two cannot both be open (opening one
        // closes the other), and only while *visible* — a user who collapsed it
        // to type by hand must get the single tunnel they are looking at.
        //
        // Deliberately unchanged by the rule above: the port list starts with
        // every box *unticked*, so an untouched list can never silently outrank a
        // typed forward. It has no "default offer" to protect against.
        if self.port_batch_intent() {
            intent.record("port-batch");
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

        // What the untouched pane was still offering, captured while the form
        // (and therefore the count) exists: the create below closes the overlay.
        let offered = self.untouched_import_offer();
        let Some(cfg) = self.validated_form_config() else {
            intent.record("refused");
            return;
        };
        match cfg.clone() {
            // Creating a tunnel is additive and easily undone by deleting it,
            // so it saves straight away and connects immediately.
            _ if matches!(self.form.as_ref().map(|f| f.mode), Some(FormMode::Create)) => {
                intent.record("typed");
                self.commit_form_create(cfg);
                if let Some(n) = offered {
                    self.note_import_offer_not_taken(n);
                }
            }
            // Editing rewrites persisted config and bounces a live process.
            _ => {
                intent.record("edit");
                let id = cfg.id;
                self.pending_form = Some(cfg);
                self.ask_confirm(ConfirmAction::ApplyEdit(id));
            }
        }
    }

    /// Snapshot of what the two interfaces were offering when a save key arrived.
    ///
    /// Captured once at entry to [`Self::submit_form`], because the branches below
    /// mutate the form (a create closes the overlay), so reading the pane's state
    /// after the fact would describe a state the user never saw.
    fn save_intent_snapshot(&self) -> SaveIntent {
        let form = self.form.as_ref();
        SaveIntent {
            pane_open: form.is_some_and(|f| f.import_open && !f.import.is_empty()),
            rows: form.map(|f| f.import.len()).unwrap_or(0),
            checked: form.map(|f| f.import.checked_count()).unwrap_or(0),
            touched: form.is_some_and(|f| f.import.touched),
            complete: form.is_some_and(|f| f.is_complete()),
        }
    }

    /// Whether the import pane owns this submit rather than the typed fields.
    ///
    /// True only for a create with a drawable, non-empty pane, and then only when
    /// either the user has touched the rows or the form cannot stand on its own.
    /// Editing is excluded: an edit always stays one tunnel.
    ///
    /// An **emptied** pane is the one case where "touched" cannot decide on its
    /// own: unticking every row says "create none of these", which leaves the
    /// pane with no instruction to carry out at all. A complete typed form is
    /// then the only instruction with a body, so it wins.
    fn import_pane_wins_submit(&self) -> bool {
        let Some(form) = self.form.as_ref() else {
            return false;
        };
        if form.is_modifying() || !form.import_open || form.import.is_empty() {
            return false;
        }
        // "Ticked" is not the same as "has a body": every importable row being
        // unticked means the batch path would create nothing, so the pane can
        // only report "nothing is ticked". It keeps the key solely when that is
        // the useful answer — i.e. when the typed fields do not add up to a
        // tunnel either.
        if !form.import.any_checked() {
            return !form.is_complete();
        }
        form.import.touched || !form.is_complete()
    }

    /// Rows an untouched, still-offering pane holds, or `None` when it is not
    /// offering anything (closed, empty, or already touched by the user).
    fn untouched_import_offer(&self) -> Option<usize> {
        let form = self.form.as_ref()?;
        if form.is_modifying() || !form.import_open || form.import.is_empty() || form.import.touched {
            return None;
        }
        match form.import.checked_count() {
            0 => None,
            n => Some(n),
        }
    }

    /// Say that a submit created the typed forward and left the pane's rows
    /// uncreated, so a pane that vanishes is not mistaken for a batch that ran.
    fn note_import_offer_not_taken(&mut self, offered: usize) {
        // A failed persist or start has already put an error on the status bar;
        // that is more important than this notice, so leave it alone.
        if self.status.as_ref().is_some_and(|s| s.level == LogLevel::Error) {
            return;
        }
        self.set_status(StatusMsg::info(help::bi(
            &format!("已创建手填的 1 条转发；导入面板的 {offered} 条未创建"),
            &format!("Created the 1 typed forward; the {offered} import rows were not created"),
        )));
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
        if let Err(reason) = self.admit_form_config(&cfg) {
            if let Some(f) = self.form.as_mut() {
                f.errors = vec![reason];
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

    /// Refuse a config that would duplicate or collide, returning the message.
    ///
    /// Two separate rules, in this order:
    ///
    /// 1. **Already have this forward** — compared by
    ///    [`TunnelConfig::forward_identity`], i.e. by intent, not by ports as
    ///    typed. A re-typed duplicate also collides on its listen port, but "you
    ///    already have this" is what is going on; "the listen address collides"
    ///    only describes the symptom.
    /// 2. **Listen-endpoint conflict** — a different forward that wants the same
    ///    bind address (see `config::listen_endpoint`).
    ///
    /// The local listen port of an `-L` forward cannot take part in rule 1: it is
    /// the number `plan_local_port` may have rewritten *and persisted* on an
    /// earlier run, so the original request is not recoverable — see
    /// [`crate::app::duplicate`].
    fn admit_form_config(&self, cfg: &TunnelConfig) -> Result<(), String> {
        if let Some(existing) = self.duplicate_of(cfg) {
            eventlog::action_duplicate_skipped("form", &cfg.name, &existing);
            return Err(help::bi(
                &format!("已存在相同的转发「{existing}」，未重复创建"),
                &format!("The same forward already exists as \"{existing}\"; nothing was created"),
            ));
        }
        match self.listen_conflict(cfg) {
            Some(conflict) => Err(help::bi(
                &format!("监听地址与「{conflict}」冲突"),
                &format!("Listen endpoint collides with \"{conflict}\""),
            )),
            None => Ok(()),
        }
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
/// What a save key found on screen, frozen at the moment it was pressed.
///
/// The pair of records this produces (`form.save_attempted` here,
/// `form.save_key_swallowed` at the dispatch layer) is what makes a save that
/// produced nothing diagnosable: the tunnel count alone cannot tell "the key
/// never arrived" from "it arrived and was refused".
struct SaveIntent {
    pane_open: bool,
    rows: usize,
    checked: usize,
    touched: bool,
    complete: bool,
}

impl SaveIntent {
    /// Write the record for the branch that is about to run.
    fn record(&self, route: &str) {
        eventlog::save_attempted(route, self.pane_open, self.rows, self.checked, self.touched, self.complete);
    }
}

#[cfg(test)]
mod tests;

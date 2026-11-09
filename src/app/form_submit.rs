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
        if self.import_pane_wins_submit() {
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
            return;
        };
        match cfg.clone() {
            // Creating a tunnel is additive and easily undone by deleting it,
            // so it saves straight away and connects immediately.
            _ if matches!(self.form.as_ref().map(|f| f.mode), Some(FormMode::Create)) => {
                self.commit_form_create(cfg);
                if let Some(n) = offered {
                    self.note_import_offer_not_taken(n);
                }
            }
            // Editing rewrites persisted config and bounces a live process.
            _ => {
                let id = cfg.id;
                self.pending_form = Some(cfg);
                self.ask_confirm(ConfirmAction::ApplyEdit(id));
            }
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
#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::config::{ConfigFile, Paths};
    use crate::model::{ForwardKind, SshForward};

    fn app_fresh() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        crate::config::save(&paths.config, &ConfigFile::default()).unwrap();
        (dir, App::new(paths))
    }

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    /// One `~/.ssh/config` row. `target_port` is spelled out so two rows can be
    /// kept semantically distinct: the duplicate guard treats `-L` rows with the
    /// same target as the same forward regardless of the local listen port
    /// (that port is bumped by availability, not chosen by the user).
    fn show(listen_port: u16, target_port: u16) -> SshForward {
        SshForward {
            kind: ForwardKind::Local,
            listen_addr: "127.0.0.1".into(),
            listen_port,
            target_host: "10.0.0.1".into(),
            target_port,
            host_alias: "dev-box".into(),
            line: 1,
            raw: format!("LocalForward {listen_port} 10.0.0.1:{target_port}"),
        }
    }

    /// A create form for `dev-box` whose import pane holds `rows`, auto-opened
    /// exactly the way discovery opens it (`install_import_rows`) — so `touched`
    /// starts false, which is the whole point of these tests.
    fn app_with_default_pane(rows: Vec<SshForward>) -> (tempfile::TempDir, App) {
        let (dir, mut app) = app_fresh();
        app.open_form_create();
        app.form.as_mut().unwrap().ssh_host.set("dev-box");
        app.install_import_rows("dev-box", rows);
        (dir, app)
    }

    /// Fill the typed fields with one complete, valid forward.
    fn fill_hand_typed(app: &mut App, name: &str, remote_port: u16, local_port: u16) {
        let f = app.form.as_mut().unwrap();
        f.name.set(name);
        f.ssh_host.set("dev-box");
        f.remote_host.set("10.0.0.9");
        f.remote_port.set(remote_port.to_string());
        f.local_port.set(local_port.to_string());
    }

    /// True when this tunnel carries the importer's provenance note, i.e. it was
    /// created from a `~/.ssh/config` row rather than typed by hand.
    fn came_from_ssh_config(cfg: &crate::model::TunnelConfig) -> bool {
        cfg.note.contains("~/.ssh/config")
    }

    #[test]
    fn a_complete_hand_typed_form_outranks_the_untouched_default_pane() {
        // The user-reported regression: host discovered 2 forwards, the pane
        // auto-opened and auto-ticked them, the user filled the form by hand for
        // port 3082 and pressed ctrl+s — and only the 2 imported rows appeared.
        // An untouched pane is a *default offer*; a complete form is an explicit
        // instruction, so it must win.
        let (p1, p2) = (free_port(), free_port());
        let (typed_port, remote_port) = (free_port(), free_port());
        let (_d, mut app) = app_with_default_pane(vec![show(p1, 80), show(p2, 8080)]);
        assert!(!app.form.as_ref().unwrap().import.touched, "precondition: auto-open is not a user action");
        fill_hand_typed(&mut app, "cloud_pc-3082", remote_port, typed_port);

        app.submit_form();

        assert_eq!(app.tunnels.len(), 1, "only the typed forward may be created");
        assert_eq!(app.tunnels[0].cfg.name, "cloud_pc-3082");
        assert_eq!(app.tunnels[0].cfg.local_port, typed_port);
        assert_eq!(app.tunnels[0].cfg.remote_port, remote_port);
        assert!(!came_from_ssh_config(&app.tunnels[0].cfg), "the created tunnel must be the typed one");
        assert!(
            !app.tunnels.iter().any(|t| t.cfg.local_port == p1 || t.cfg.local_port == p2),
            "not one import row may be created"
        );
        // The pane vanishes without its rows being created; silence would read as
        // "the batch ran too".
        let msg = app.status.as_ref().map(|s| s.text.clone()).unwrap_or_default();
        assert!(msg.contains("未创建"), "the user must be told the pane's rows were skipped: {msg}");
        assert!(msg.contains('2'), "and how many: {msg}");
    }

    #[test]
    fn an_emptied_pane_hands_the_save_key_back_to_the_complete_typed_form() {
        // The reported regression: the pane auto-opened with the host's two
        // forwards, the user unticked *all* of them and typed the forward they
        // actually wanted (port 3081), then pressed ctrl+s. Unticking is an
        // instruction, but an emptied pane has nothing left to create — the
        // complete typed form is the only instruction with a body, so it must
        // win. The old rule read "touched ⇒ the pane decides" and answered
        // "没有勾选任何转发 / No forward is ticked" over a perfectly valid form.
        let (p1, p2) = (free_port(), free_port());
        let (typed_port, wanted_port) = (free_port(), 3081u16);
        let (_d, mut app) = app_with_default_pane(vec![show(p1, 80), show(p2, 8080)]);
        app.form.as_mut().unwrap().import.uncheck_all_visible();
        assert!(app.form.as_ref().unwrap().import.touched, "unticking every row is still a user instruction");
        assert_eq!(app.form.as_ref().unwrap().import.checked_count(), 0, "precondition: nothing is ticked");
        fill_hand_typed(&mut app, "cloud_pc-3081", wanted_port, typed_port);

        app.submit_form();

        assert_eq!(app.tunnels.len(), 1, "the typed forward is the only thing left to create");
        assert_eq!(app.tunnels[0].cfg.name, "cloud_pc-3081");
        assert_eq!(app.tunnels[0].cfg.remote_port, wanted_port);
        assert_eq!(app.tunnels[0].cfg.local_port, typed_port);
        assert!(app.form.is_none(), "the form closes: nothing was wrong with what the user typed");
        assert!(
            !app.tunnels.iter().any(|t| t.cfg.local_port == p1 || t.cfg.local_port == p2),
            "the unticked import rows must still not be created"
        );
    }

    #[test]
    fn an_emptied_pane_still_reports_nothing_ticked_when_no_forward_was_typed() {
        // The counterpart, so the fix cannot become "an empty pane never
        // decides": untick everything and type nothing, and the only sensible
        // answer is that there is nothing to save.
        let (_d, mut app) = app_with_default_pane(vec![show(free_port(), 80)]);
        app.form.as_mut().unwrap().import.uncheck_all_visible();

        app.submit_form();

        assert!(app.tunnels.is_empty(), "nothing ticked and nothing typed creates nothing");
        assert!(app.form.is_some(), "the form stays open to explain why");
        let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
        assert!(errors.iter().any(|e| e.contains("没有勾选任何转发")), "{errors:?}");
    }

    #[test]
    fn a_touched_pane_outranks_a_complete_hand_typed_form() {
        // The counterpart: once the user has unticked a row, the pane's ticks are
        // the instruction, and a filled-in form is just leftover scratch input.
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_default_pane(vec![show(p1, 80), show(p2, 8080)]);
        app.form.as_mut().unwrap().import.toggle_cursor(); // untick row 0 → touched
        assert!(app.form.as_ref().unwrap().import.touched);
        fill_hand_typed(&mut app, "ignored", free_port(), free_port());

        app.submit_form();

        assert_eq!(app.tunnels.len(), 1, "the batch creates one tunnel per still-ticked row");
        assert_eq!(app.tunnels[0].cfg.local_port, p2, "and it is the row that stayed ticked");
        assert!(came_from_ssh_config(&app.tunnels[0].cfg));
        assert!(app.tunnels.iter().all(|t| t.cfg.name != "ignored"), "the typed fields must be ignored");
    }

    #[test]
    fn an_incomplete_form_still_lets_the_default_pane_win() {
        // The headline workflow must survive: pick a host, press ctrl+s, import
        // everything. Nothing was typed that could be a tunnel, so the default
        // offer is the only instruction there is.
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_default_pane(vec![show(p1, 80), show(p2, 8080)]);
        assert!(!app.form.as_ref().unwrap().is_complete(), "precondition: an empty form is not a tunnel");

        app.submit_form();

        assert_eq!(app.tunnels.len(), 2, "both imported rows must be created");
        assert!(app.tunnels.iter().all(|t| came_from_ssh_config(&t.cfg)));
        assert!(app.form.is_none(), "the form closes on success");
    }

    #[test]
    fn only_choosing_a_row_counts_as_touching_the_pane() {
        // Clicking a row only moves the cursor/highlight — it expresses no
        // intention about the ticks, so it must not let the pane hijack ctrl+s.
        // Clicking the checkbox *does* change the selection, so it must.
        let (_d, mut app) = app_with_default_pane(vec![show(free_port(), 80), show(free_port(), 8080)]);
        app.import_select_at(0);
        assert!(!app.form.as_ref().unwrap().import.touched, "selecting a row is not an instruction");

        app.import_toggle_at(0);
        assert!(app.form.as_ref().unwrap().import.touched, "ticking a row is");
    }

    #[test]
    fn a_new_host_starts_with_a_fresh_untouched_offer() {
        // Switching host installs a different set of rows: the previous host's
        // "I ticked things here" intention cannot carry over to brand-new rows.
        let (p1, p2) = (free_port(), free_port());
        let (_d, mut app) = app_with_default_pane(vec![show(p1, 80)]);
        app.form.as_mut().unwrap().import.toggle_cursor();
        assert!(app.form.as_ref().unwrap().import.touched);

        app.install_import_rows("dev-box", vec![show(p2, 8080)]);
        assert!(!app.form.as_ref().unwrap().import.touched, "a new host's rows are a new default offer");

        // And the consequence: a complete hand-typed form wins over them.
        let typed_port = free_port();
        fill_hand_typed(&mut app, "typed", free_port(), typed_port);
        app.submit_form();
        assert_eq!(app.tunnels.len(), 1);
        assert_eq!(app.tunnels[0].cfg.local_port, typed_port);
        assert!(!came_from_ssh_config(&app.tunnels[0].cfg));
    }

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

    // -- the single-form create: the same duplicate guard --------------------

    /// A saved `-L` tunnel: `local_port` → `remote_host:remote_port` on `ssh_host`.
    fn saved_tunnel(local_port: u16, remote_port: u16, ssh_host: &str) -> crate::model::TunnelConfig {
        crate::model::TunnelConfig {
            id: 1,
            name: "already-there".into(),
            kind: ForwardKind::Local,
            ssh_host: ssh_host.into(),
            local_addr: "127.0.0.1".into(),
            local_port,
            remote_host: "127.0.0.1".into(),
            remote_port,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 1,
        }
    }

    /// An app whose config already holds one tunnel, with a fresh create form.
    fn app_with_saved(saved: crate::model::TunnelConfig) -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
        let cfg = ConfigFile { tunnels: vec![saved], ..ConfigFile::default() };
        crate::config::save(&paths.config, &cfg).unwrap();
        let mut app = App::new(paths);
        app.open_form_create();
        (dir, app)
    }

    #[test]
    fn typing_a_forward_that_already_exists_creates_nothing() {
        // The single-form half of the de-duplication rule: the user re-types a
        // forward they already have. Nothing is created, and the reason is a
        // field error naming the row that already holds it — not the listen-port
        // collision message, which would describe the symptom instead.
        let (_d, mut app) = app_with_saved(saved_tunnel(18789, 18789, "dev-box"));
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("typed-again");
            f.ssh_host.set("dev-box");
            f.remote_host.set("127.0.0.1");
            f.remote_port.set("18789");
            f.local_port.set("18099");
        }
        app.submit_form();
        assert_eq!(app.tunnels.len(), 1, "the duplicate must not be added");
        assert!(app.form.is_some(), "the form stays open to explain why");
        let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
        assert!(
            errors.iter().any(|e| e.contains("已存在相同的转发") && e.contains("already-there")),
            "the error must name the existing tunnel: {errors:?}"
        );
    }

    #[test]
    fn a_saved_tunnel_whose_local_port_was_bumped_is_still_a_duplicate() {
        // The shape the user actually hit: the saved entry was created earlier,
        // when 18789 was busy, so what got persisted is local 18790 → remote
        // 18789. Typing `18789 -> 127.0.0.1:18789` now asks for an intent that is
        // already satisfied; only the local port differs, and that port is a
        // derived value, not the user's intent.
        let (_d, mut app) = app_with_saved(saved_tunnel(18790, 18789, "dev-box"));
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("typed-again");
            f.ssh_host.set("dev-box");
            f.remote_host.set("127.0.0.1");
            f.remote_port.set("18789");
            f.local_port.set("18789");
        }
        app.submit_form();
        assert_eq!(app.tunnels.len(), 1, "a bumped persisted local port must not defeat the duplicate check");
        let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
        assert!(errors.iter().any(|e| e.contains("已存在相同的转发")), "{errors:?}");
    }

    #[test]
    fn a_different_target_or_host_is_not_a_duplicate() {
        // The guard must not swallow legitimate forwards. Each case changes one
        // identity field and also picks a free local port, so the *listen-port*
        // check (a separate rule) cannot be what refuses it: a refusal here would
        // have to come from the duplicate guard.
        for (remote_port, ssh_host) in [(9999u16, "dev-box"), (18789, "other-box")] {
            let (_d, mut app) = app_with_saved(saved_tunnel(18789, 18789, "dev-box"));
            let local_port = free_port();
            {
                let f = app.form.as_mut().unwrap();
                f.name.set("legit");
                f.ssh_host.set(ssh_host);
                f.remote_host.set("127.0.0.1");
                f.remote_port.set(remote_port.to_string());
                f.local_port.set(local_port.to_string());
            }
            app.submit_form();
            assert_eq!(app.tunnels.len(), 2, "remote_port={remote_port} ssh_host={ssh_host} is a new forward");
        }
    }

    #[test]
    fn the_same_forward_typed_with_a_taken_local_port_says_already_exists() {
        // Both rules apply to a re-typed duplicate (same intent *and* a listen
        // collision). The duplicate message is the one that describes what is
        // actually going on, so it must be the one the user reads.
        let (_d, mut app) = app_with_saved(saved_tunnel(18789, 18789, "dev-box"));
        {
            let f = app.form.as_mut().unwrap();
            f.name.set("typed-again");
            f.ssh_host.set("dev-box");
            f.remote_host.set("127.0.0.1");
            f.remote_port.set("18789");
            f.local_port.set("18789");
        }
        app.submit_form();
        let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
        assert!(errors.iter().any(|e| e.contains("已存在相同的转发")), "{errors:?}");
        assert!(
            !errors.iter().any(|e| e.contains("监听地址与")),
            "the symptom (listen collision) must not be reported instead: {errors:?}"
        );
    }

    #[test]
    fn editing_a_tunnel_does_not_collide_with_its_own_definition() {
        // The duplicate lookup skips the candidate's own id, so re-saving an edit
        // is not reported as "already exists".
        let (_d, mut app) = app_with_saved(saved_tunnel(18789, 18789, "dev-box"));
        app.selected = 0;
        app.open_form_edit();
        app.form.as_mut().unwrap().name.set("renamed");
        app.submit_form();
        // An edit goes through the confirmation gate; it must not be reported as
        // a duplicate in the form.
        let errors = app.form.as_ref().map(|f| f.errors.clone()).unwrap_or_default();
        assert!(errors.is_empty(), "editing a tunnel is not creating a duplicate: {errors:?}");
        assert!(matches!(app.overlay, crate::app::Overlay::Confirm { .. }), "the edit still asks first");
    }
}

//! User-action and interface-degradation event vocabulary.
//!
//! Two families live here because both answer "what did the person at the
//! keyboard do, and did the interface keep up?":
//!
//! * `action.*` / `import.*` — a confirmation gate was armed, passed or
//!   cancelled, a batch was applied, an import was committed or rejected.
//! * `ui.*` / `form.*` / `import.click_stale` — a degraded-but-surviving path
//!   (a slow frame, a terminal without mouse reporting, a click whose row moved,
//!   a stale kind index). These are `warn`/`debug`, never `error`: the TUI is
//!   still usable and the user was not blocked.
//!
//! Anything whose subject is a tunnel's connection belongs in
//! [`super::tunnel`]; config/app lifecycle in [`super::process`].

use crate::logging::{self, Level, Logger};

/// A destructive/consequential action was confirmed and applied.
pub fn action_applied(action: &str, detail: &str) {
    logging::info("action.applied", &[("action", &action), ("detail", &detail)]);
}

/// A risky action was requested and the confirmation gate is now open.
pub fn action_requested(action: &str, detail: &str) {
    logging::debug("action.requested", &[("action", &action), ("detail", &detail)]);
}

/// A risky action was cancelled at the gate.
pub fn action_cancelled(action: &str) {
    logging::info("action.cancelled", &[("action", &action)]);
}

/// A batch operation finished; `changed` is what actually changed (not what was
/// marked), which is the number the user was shown.
pub fn action_batch(action: &str, marked: usize, changed: usize) {
    logging::info("action.batch", &[("action", &action), ("marked", &marked), ("changed", &changed)]);
}

/// A batch import from `~/.ssh/config` produced `created` tunnels.
pub fn import_committed(host: &str, created: usize) {
    logging::info("import.committed", &[("host", &host), ("created", &created)]);
}

/// Import validation failed; nothing was created.
pub fn import_rejected(problems: &[String]) {
    logging::warn("import.rejected", &[("problems", &problems.len()), ("msg", &problems.join("; "))]);
}

/// A batch row was dropped because the same forward is already saved.
///
/// `source` says which path proposed it — `import` (`~/.ssh/config` batch),
/// `port-scan` (ticked remote ports) or `form` (the single hand-filled form);
/// `name` is the candidate's would-be name and `existing` the tunnel it
/// duplicates. `info`, not `warn`: skipping a redundant forwarding is the intended
/// outcome, and this line is what explains a submit that created fewer tunnels
/// than rows were ticked.
pub fn action_duplicate_skipped(source: &str, name: &str, existing: &str) {
    action_duplicate_skipped_on(logging::logger(), source, name, existing);
}

pub fn action_duplicate_skipped_on(logger: &Logger, source: &str, name: &str, existing: &str) {
    logger.event(
        Level::Info,
        "action.duplicate_skipped",
        &[("source", &source), ("name", &name), ("existing", &existing)],
    );
}

/// A `ctrl+s` / `f10` reached the form and a save was attempted.
///
/// Called by the **form router** (`App::submit_form`) exactly once per save key,
/// before any branch runs, so the file answers the two questions a missing
/// tunnel raises and nothing else on disk can:
///
/// * *did the key arrive at all?* — a save key that a sub-focus swallowed (a
///   column filter or the host dropdown treating `s` as text) leaves **no**
///   record here, which is what separates "never saved" from "saved and
///   refused";
/// * *which of the two interfaces was in charge?* — `route` is the branch that
///   ran, `pane` says whether the import pane was on screen, and
///   `rows`/`checked`/`touched` describe the offer it was making.
///
/// `route` is a stable ASCII value: `import` (the `~/.ssh/config` batch),
/// `port-batch` (ticked remote ports), `typed` (the hand-filled single tunnel)
/// and `none` (validation refused it / the form was not savable).
pub fn save_attempted(route: &str, pane_open: bool, rows: usize, checked: usize, touched: bool, complete: bool) {
    save_attempted_on(logging::logger(), route, pane_open, rows, checked, touched, complete);
}

pub fn save_attempted_on(
    logger: &Logger,
    route: &str,
    pane_open: bool,
    rows: usize,
    checked: usize,
    touched: bool,
    complete: bool,
) {
    logger.event(
        Level::Info,
        "form.save_attempted",
        &[
            ("route", &route),
            ("pane_open", &pane_open),
            ("rows", &rows),
            ("checked", &checked),
            ("touched", &touched),
            ("complete", &complete),
        ],
    );
}

/// A save key was pressed while a sub-focus owned the keyboard, so the form
/// never saw it.
///
/// `focus` is the owner that swallowed the key: `host_dropdown`,
/// `import_filter` or `port_picker`. This is the counterpart of
/// [`save_attempted`]: the pair tells "the user pressed save and a panel ate it"
/// apart from "the user never pressed save", which used to be indistinguishable
/// because both left the log empty.
///
/// `warn`, not `debug`: the user explicitly asked to save and got no save, and
/// no tunnel appeared. That is a defect to fix, not chatter.
pub fn save_key_swallowed(focus: &str) {
    save_key_swallowed_on(logging::logger(), focus);
}

pub fn save_key_swallowed_on(logger: &Logger, focus: &str) {
    logger.event(Level::Warn, "form.save_key_swallowed", &[("focus", &focus)]);
}

/// A confirmation was armed for a tunnel id that is no longer in the table.
///
/// The dialog still opens with a fallback sentence, so the user is not stuck;
/// the record is what explains why the prompt had no name in it.
pub fn action_target_missing(action: &str) {
    action_target_missing_on(logging::logger(), action);
}

pub fn action_target_missing_on(logger: &Logger, action: &str) {
    logger.event(Level::Warn, "action.target_missing", &[("action", &action)]);
}

/// A mouse click resolved to an import row that is no longer in the filtered view.
///
/// The hit map is drawn one frame ahead of the click, so a re-filter or a
/// re-discovery can invalidate it. The click is dropped (rather than being
/// applied to whatever row now occupies that slot) and recorded here.
pub fn import_click_stale(row: usize) {
    logging::debug("import.click_stale", &[("row", &row)]);
}

/// The form's `kind_idx` pointed outside `ForwardKind::all()`.
///
/// Only reachable if a caller assigns the index directly; the form falls back to
/// the default kind so the dialog stays usable, and this line says why the
/// highlighted kind may look reset.
pub fn form_kind_index_invalid(index: usize, len: usize) {
    form_kind_index_invalid_on(logging::logger(), index, len);
}

pub fn form_kind_index_invalid_on(logger: &Logger, index: usize, len: usize) {
    logger.event(Level::Warn, "form.kind_index_invalid", &[("index", &index), ("len", &len)]);
}

/// The UI frame took far longer than the 200 ms tick — the first thing to check
/// when the TUI feels stuck.
pub fn slow_frame(elapsed_ms: u128) {
    logging::warn("ui.slow_frame", &[("elapsed_ms", &elapsed_ms)]);
}

/// The user's terminal does not support mouse reporting; the UI keeps working.
pub fn mouse_capture_failed() {
    logging::debug("ui.no_mouse", &[]);
}

/// A port scan of `host` was requested (`multi` = ticking rows can batch-create).
///
/// `info`, not `debug`: it opens a *second* SSH connection to the host, so the
/// line explains a connection the user did not make from the table.
pub fn port_scan_started(host: &str, multi: bool) {
    port_scan_started_on(logging::logger(), host, multi);
}

pub fn port_scan_started_on(logger: &Logger, host: &str, multi: bool) {
    logger.event(Level::Info, "portscan.started", &[("host", &host), ("multi", &multi)]);
}

/// A port scan produced `found` offerable ports, hiding `excluded` that the
/// host's ssh config already declares.
pub fn port_scan_finished(host: &str, found: usize, excluded: usize) {
    port_scan_finished_on(logging::logger(), host, found, excluded);
}

pub fn port_scan_finished_on(logger: &Logger, host: &str, found: usize, excluded: usize) {
    logger.event(Level::Info, "portscan.finished", &[("host", &host), ("found", &found), ("excluded", &excluded)]);
}

/// A port scan failed; `msg` is the sentence shown in the pane.
pub fn port_scan_failed(host: &str, msg: &str) {
    port_scan_failed_on(logging::logger(), host, msg);
}

pub fn port_scan_failed_on(logger: &Logger, host: &str, msg: &str) {
    logger.event(Level::Warn, "portscan.failed", &[("host", &host), ("msg", &msg)]);
}

/// A scan answer arrived for a generation the picker no longer shows.
///
/// Normal when the user switches host or rescans while a probe is in flight; the
/// answer is dropped rather than applied to the wrong host's list.
pub fn port_scan_stale(seq: u64, current: u64) {
    port_scan_stale_on(logging::logger(), seq, current);
}

pub fn port_scan_stale_on(logger: &Logger, seq: u64, current: u64) {
    logger.event(Level::Debug, "portscan.stale", &[("seq", &seq), ("current", &current)]);
}

/// The picker was collapsed without being used.
pub fn port_scan_closed() {
    logging::debug("portscan.closed", &[]);
}

/// The list was dropped because the host or the forward kind changed under it.
pub fn port_picker_dropped(from: &str, to: &str) {
    port_picker_dropped_on(logging::logger(), from, to);
}

pub fn port_picker_dropped_on(logger: &Logger, from: &str, to: &str) {
    logger.event(Level::Debug, "portscan.dropped", &[("from", &from), ("to", &to)]);
}

/// Port validation failed; nothing was created.
pub fn port_batch_rejected(problems: &[String]) {
    port_batch_rejected_on(logging::logger(), problems);
}

pub fn port_batch_rejected_on(logger: &Logger, problems: &[String]) {
    logger.event(
        Level::Warn,
        "portscan.batch_rejected",
        &[("problems", &problems.len()), ("msg", &problems.join("; "))],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::{Config, Level, Logger};
    use std::path::Path;

    fn logger(dir: &Path) -> Logger {
        Logger::open(&Config { dir: dir.to_path_buf(), level: Level::Debug, ..Config::default() }).unwrap()
    }

    fn text(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(crate::logging::LOG_FILE_NAME)).unwrap_or_default()
    }

    #[test]
    fn a_missing_action_target_is_a_warning_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        action_target_missing_on(&log, "delete");
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=action.target_missing"), "{text}");
        assert!(text.contains("action=delete"), "{text}");
    }

    #[test]
    fn a_skipped_duplicate_records_its_source_and_both_names() {
        // INFO: skipping a redundant forward is the intended outcome, not a
        // failure — but it explains a batch that created fewer tunnels than rows
        // the user ticked.
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        action_duplicate_skipped_on(&log, "import", "cloud_pc-L-18789", "cloud_pc-L-18789-2");
        let text = text(dir.path());
        assert!(text.contains(" INFO "), "{text}");
        assert!(text.contains("event=action.duplicate_skipped"), "{text}");
        assert!(text.contains("source=import"), "the field order is part of the contract: {text}");
        assert!(text.contains("name=cloud_pc-L-18789"), "{text}");
        assert!(text.contains("existing=cloud_pc-L-18789-2"), "{text}");
        let after_event = text.split("event=action.duplicate_skipped").nth(1).unwrap_or_default();
        assert!(after_event.find("source=") < after_event.find("name="), "field order source/name/existing");
    }

    #[test]
    fn an_invalid_kind_index_records_both_the_index_and_the_length() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        form_kind_index_invalid_on(&log, 99, 3);
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=form.kind_index_invalid"), "{text}");
        assert!(text.contains("index=99"), "{text}");
        assert!(text.contains("len=3"), "{text}");
    }

    #[test]
    fn a_save_attempt_records_the_route_and_the_pane_offer() {
        // The record that answers "did my ctrl+s do anything?": which branch ran,
        // and what the import pane was offering at that moment.
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        save_attempted_on(&log, "typed", true, 2, 2, false, true);
        let text = text(dir.path());
        assert!(text.contains(" INFO "), "{text}");
        assert!(text.contains("event=form.save_attempted"), "{text}");
        let after = text.split("event=form.save_attempted").nth(1).unwrap_or_default();
        for field in ["route=typed", "pane_open=true", "rows=2", "checked=2", "touched=false", "complete=true"] {
            assert!(after.contains(field), "missing {field}: {text}");
        }
        // Field order is part of the contract: route first, then the pane offer.
        assert!(after.find("route=") < after.find("pane_open="), "{text}");
        assert!(after.find("checked=") < after.find("touched="), "{text}");
    }

    #[test]
    fn a_swallowed_save_key_is_a_warning_naming_the_focus() {
        // The counterpart record. Together the pair separates "the key never
        // arrived" from "it arrived and was refused", which both used to look
        // like an empty log file.
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        save_key_swallowed_on(&log, "import_filter");
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=form.save_key_swallowed"), "{text}");
        assert!(text.contains("focus=import_filter"), "{text}");
    }

    #[test]
    fn a_port_scan_lifecycle_is_recorded_with_its_host() {
        // The scan opens a second SSH connection the user did not request from the
        // table; without these lines it would appear in the host's auth log with
        // no explanation on our side.
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        port_scan_started_on(&log, "dev-box", true);
        let started = text(dir.path());
        assert!(started.contains(" INFO "), "{started}");
        assert!(started.contains("event=portscan.started"), "{started}");
        assert!(started.contains("host=dev-box"), "{started}");
        assert!(started.contains("multi=true"), "{started}");

        port_scan_finished_on(&log, "dev-box", 3, 1);
        let finished = text(dir.path());
        assert!(finished.contains("event=portscan.finished"), "{finished}");
        assert!(finished.contains("found=3"), "{finished}");
        assert!(finished.contains("excluded=1"), "{finished}");

        port_scan_failed_on(&log, "dev-box", "没有 ss");
        let failed = text(dir.path());
        assert!(failed.contains(" WARN "), "{failed}");
        assert!(failed.contains("event=portscan.failed"), "{failed}");
        assert!(failed.contains(r#"msg="没有 ss""#), "the message is quoted because it contains a space: {failed}");
    }

    #[test]
    fn a_stale_or_rejected_port_scan_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        port_scan_stale_on(&log, 2, 5);
        port_picker_dropped_on(&log, "a", "b");
        port_batch_rejected_on(&log, &["名称不能为空".to_string()]);
        let logged = text(dir.path());
        assert!(logged.contains("event=portscan.stale"), "{logged}");
        assert!(logged.contains("current=5"), "{logged}");
        assert!(logged.contains("event=portscan.dropped"), "{logged}");
        assert!(logged.contains("event=portscan.batch_rejected"), "{logged}");
        assert!(logged.contains("problems=1"), "{logged}");
    }
}

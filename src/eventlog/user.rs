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
}

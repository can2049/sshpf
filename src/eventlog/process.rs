//! Process- and config-scoped event vocabulary: start-up, shutdown, config
//! load/save/repair, and the handful of app-wide anomalies (`app.*`).
//!
//! These are the records that describe *the program*, not one tunnel: whether it
//! came up, where its state lives, what it had to repair, and why it is going
//! away. A per-tunnel record lives in [`super::tunnel`].
//!
//! Add a new `app.*`, `config.*` or `state.*` event here. The `event=` value is a
//! contract with `docs/logging.md`.

use crate::logging::{self, Level, Logger};
use crate::model::TunnelId;

/// The process started: version and where its state lives.
pub fn app_start(version: &str, config_path: &str, log_path: &str) {
    logging::info(
        "app.start",
        &[
            ("version", &version),
            ("pid", &std::process::id()),
            ("config", &config_path),
            ("log", &log_path),
            ("os", &std::env::consts::OS),
            ("arch", &std::env::consts::ARCH),
        ],
    );
}

/// The process is exiting, with a reason and how long it ran.
pub fn app_exit(reason: &str, uptime_ms: u128, tunnels_up: usize) {
    logging::info("app.exit", &[("reason", &reason), ("uptime_ms", &uptime_ms), ("tunnels_up", &tunnels_up)]);
}

/// A CLI mode that does not start the TUI (`--check`, `--list-hosts`, …).
pub fn cli_mode(mode: &str) {
    logging::debug("app.cli_mode", &[("mode", &mode)]);
}

/// A command-line or configuration error that made the process exit non-zero.
pub fn cli_error(event: &str, message: &str) {
    logging::error(event, &[("msg", &message)]);
}

/// Config loaded. `tunnels` is the number of definitions read.
///
/// A repair (id/name collision, tightened permissions) is a `warn`: the user's
/// file was not what it claimed to be, and knowing that explains later surprises.
pub fn config_loaded(path: &str, tunnels: usize, problems: &[String]) {
    logging::info("config.loaded", &[("path", &path), ("tunnels", &tunnels), ("problems", &problems.len())]);
    for problem in problems {
        logging::warn("config.invalid", &[("path", &path), ("msg", problem)]);
    }
}

/// Loading failed and the app fell back to an empty config. `error`: any tunnel
/// the user thinks they have is about to be invisible.
pub fn config_load_failed(path: &str, error: &str, fallback: &str) {
    logging::error("config.load_failed", &[("path", &path), ("msg", &error), ("fallback", &fallback)]);
}

/// Definitions were repaired on load (duplicate ids, missing timestamps).
pub fn config_repaired(path: &str, count: usize) {
    logging::warn("config.repaired", &[("path", &path), ("entries", &count)]);
}

/// Persisting failed. The change is live but will not survive a restart.
pub fn config_save_failed(path: &str, error: &str) {
    logging::error("config.save_failed", &[("path", &path), ("msg", &error)]);
}

/// The config file was group/world readable and has been tightened to `0600`.
pub fn config_perms_tightened(path: &str, ok: bool) {
    let level = if ok { Level::Warn } else { Level::Error };
    logging::logger().event(level, "config.perms_tightened", &[("path", &path), ("ok", &ok)]);
}

/// The runtime state file could not be written.
pub fn state_save_failed(path: &str, error: &str) {
    logging::warn("state.save_failed", &[("path", &path), ("msg", &error)]);
}

/// Saved entries that meant the same forward were folded into one on load.
///
/// `kept` names the entry that survived, `dropped` how many were removed. A WARN
/// for the same reason `config.repaired` is one: the user's `config.toml` is not
/// what they wrote, their table just got shorter on start-up, and the log is where
/// "why are two of my forwards gone?" has an answer. Emitted once per folded
/// group, so the text can name the survivor.
pub fn config_deduped(kept: &str, dropped: usize) {
    config_deduped_on(logging::logger(), kept, dropped);
}

pub fn config_deduped_on(logger: &Logger, kept: &str, dropped: usize) {
    logger.event(Level::Warn, "config.deduped", &[("kept", &kept), ("dropped", &dropped)]);
}

/// A hand-edited default in `config.toml` was out of range and has been replaced.
///
/// `field` is the stable config key (`retry_initial_secs` / `retry_max_secs`),
/// `original` what the file asked for, `used` what the app will actually do.
/// A `retry_initial_secs = 0` is the interesting case: it turns the reconnect
/// loop into a per-frame storm, so it is clamped instead of obeyed.
pub fn config_defaults_clamped(field: &str, original: u64, used: u64) {
    config_defaults_clamped_on(logging::logger(), field, original, used);
}

pub fn config_defaults_clamped_on(logger: &Logger, field: &str, original: u64, used: u64) {
    logger.event(
        Level::Warn,
        "config.defaults_clamped",
        &[("field", &field), ("original", &original), ("used", &used)],
    );
}

/// The tunnel-id space (`u64`) is exhausted, so no new definition can be given a
/// unique id. Only reachable from a hand-edited config with a huge id; the
/// action is refused rather than reusing an id that already exists.
pub fn id_space_exhausted(max_id: TunnelId) {
    id_space_exhausted_on(logging::logger(), max_id);
}

pub fn id_space_exhausted_on(logger: &Logger, max_id: TunnelId) {
    logger.event(Level::Error, "app.id_space_exhausted", &[("max_id", &max_id)]);
}

/// A supervisor event arrived for a tunnel that no longer exists.
pub fn orphan_event(event: &str, id: TunnelId) {
    logging::debug("app.orphan_event", &[("event", &event), ("tunnel", &id)]);
}

// ---------------------------------------------------------------------------
// client-server runtime
// ---------------------------------------------------------------------------
//
// Two processes replaced one, so every "why is this not what I expect" question
// now has a new candidate answer: *which* process, and did they ever meet. These
// events exist to make that answerable from the log alone. The names are stable
// and documented in `docs/logging/events.md`.

/// The server has acquired the election lock and is listening.
pub fn server_start(config_path: &str, socket: &str, keep_server: bool) {
    logging::info(
        "server.start",
        &[
            ("pid", &std::process::id()),
            ("config", &config_path),
            ("socket", &socket),
            ("keep_server", &keep_server),
        ],
    );
}

/// Another process already owns this config. Normal election outcome, not a fault.
pub fn server_election_lost(lock_path: &str) {
    logging::debug("server.election_lost", &[("lock", &lock_path)]);
}

/// A socket file was left by a `SIGKILL`ed server and has been replaced.
pub fn server_stale_socket_removed(socket: &str) {
    logging::info("server.stale_socket_removed", &[("socket", &socket)]);
}

/// A client completed the handshake. `count` includes it.
pub fn server_client_connected(count: u32) {
    logging::info("server.client_connected", &[("count", &count)]);
}

/// A client left. `reason` is `bye` (it said goodbye) or `eof` (it vanished).
pub fn server_client_disconnected(count: u32, reason: &str) {
    logging::info("server.client_disconnected", &[("count", &count), ("reason", &reason)]);
}

/// The count reached zero; the server is waiting before it shuts down.
pub fn server_grace_started(grace_ms: u64, trigger: &str) {
    logging::info("server.grace_started", &[("grace_ms", &grace_ms), ("trigger", &trigger)]);
}

/// A client arrived during the grace window, so the shutdown was called off.
pub fn server_grace_cancelled(count: u32) {
    logging::info("server.grace_cancelled", &[("count", &count)]);
}

/// Shutdown began: why, and what is about to be stopped.
pub fn server_shutdown_begin(count: u32, active: usize, trigger: &str) {
    logging::info(
        "server.shutdown_begin",
        &[("count", &count), ("active", &active), ("trigger", &trigger)],
    );
}

/// This process attached to a server instead of owning the tunnels.
pub fn client_attached(server_pid: u32, config_path: &str) {
    logging::info("client.attached", &[("server_pid", &server_pid), ("config", &config_path)]);
}

/// This process started the server it then attached to.
pub fn client_server_spawned(pid: u32) {
    logging::info("client.server_spawned", &[("server_pid", &pid)]);
}

/// The link to the server ended.
pub fn client_disconnected(reason: &str) {
    logging::warn("client.disconnected", &[("reason", &reason)]);
}

/// Attaching failed; the reason is what the user is shown.
pub fn client_attach_failed(reason: &str) {
    logging::error("client.attach_failed", &[("msg", &reason)]);
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
    fn a_clamped_default_names_the_field_and_both_numbers() {
        // A repaired config must be discoverable: which key was wrong, what it
        // said, and what sshpf will actually use.
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        config_defaults_clamped_on(&log, "retry_initial_secs", 0, 1);
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=config.defaults_clamped"), "{text}");
        assert!(text.contains("field=retry_initial_secs"), "{text}");
        assert!(text.contains("original=0"), "{text}");
        assert!(text.contains("used=1"), "{text}");
    }

    #[test]
    fn a_folded_duplicate_names_the_survivor_and_the_count() {
        // The line has to answer both halves of "what happened to my forwards?":
        // which entry was kept, and how many went away. A bare count would leave
        // the user unable to tell whether the right one survived.
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        config_deduped_on(&log, "cloud_pc-L-18789-2", 2);
        let text = text(dir.path());
        assert!(text.contains(" WARN "), "{text}");
        assert!(text.contains("event=config.deduped"), "{text}");
        assert!(text.contains("kept=cloud_pc-L-18789-2"), "{text}");
        assert!(text.contains("dropped=2"), "{text}");
    }

    #[test]
    fn an_exhausted_id_space_is_an_error_that_names_the_maximum() {
        let dir = tempfile::tempdir().unwrap();
        let log = logger(dir.path());
        id_space_exhausted_on(&log, u64::MAX);
        let text = text(dir.path());
        assert!(text.contains(" ERROR "), "{text}");
        assert!(text.contains("event=app.id_space_exhausted"), "{text}");
        assert!(text.contains("max_id=18446744073709551615"), "{text}");
    }
}

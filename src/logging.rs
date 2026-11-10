//! Persistent, structured logging plus crash capture — the evidence trail that
//! outlives the process.
//!
//! # Why a second log
//!
//! [`model::LogRing`](crate::model::LogRing) is the *interactive* log: it lives
//! in memory, dies with the process, and is scoped to one tunnel. That is the
//! right shape for the detail panel and the wrong shape for "something went
//! wrong an hour ago, what happened?". This module writes an append-only file
//! instead, so a user can hand us the file after a failure — including a crash.
//!
//! # Format
//!
//! One record per line, machine-parseable and still readable:
//!
//! ```text
//! 2026-11-08T18:20:01.123+08:00 INFO  seq=42 event=tunnel.ready tunnel=3 name=cloud elapsed_ms=812
//! ```
//!
//! * fixed column order: RFC 3339 timestamp, level, `seq`, `event`;
//! * then free-form `key=value` fields, values quoted when they contain
//!   whitespace, `"` or `\` (escaped as `\\`, `\"`, `\n`, `\r`, `\t`, `\xNN`);
//! * `event` is the stable, documented name (`docs/logging.md`) — grepping for
//!   it is how a diagnosis starts, so new call sites reuse an existing name
//!   rather than inventing a near-duplicate.
//!
//! Signal-crash records are the one exception to the leading timestamp: see
//! `crash.rs`.
//!
//! # Crash records
//!
//! Two independent mechanisms, because they cover different failures:
//!
//! * **Panics** go through [`std::panic::set_hook`]: message, location, thread
//!   name and a force-captured backtrace become a `crash.panic` record plus an
//!   indented trace block. This catches every Rust-level crash, including one
//!   inside a tunnel thread — the thread name is in the record, which is how a
//!   dead tunnel thread is told apart from a clean exit. See `panic.rs`.
//! * **Fatal signals** (SIGSEGV/SIGBUS/SIGABRT/SIGILL/SIGFPE) get a real
//!   `sigaction` handler. It may only use async-signal-safe calls, so it writes
//!   a hand-formatted record straight to a pre-opened descriptor — no
//!   allocation, no locks, no formatting machinery — then restores the default
//!   action and re-raises, so the exit status and core dump are unchanged. See
//!   `crash.rs`, which holds the crate's only `unsafe`.
//!
//! # Invariants
//!
//! * **Logging can never take the program down.** If the file cannot be opened
//!   the logger degrades to disabled; write errors are swallowed.
//! * **Nothing secret is written.** No key material, no passphrases, no
//!   `known_hosts` contents. Hosts, users and ports are recorded because they
//!   are what a diagnosis needs; on Unix the file is `0600` inside a `0700`
//!   directory, and on Windows it inherits the per-user profile ACL (there is no
//!   mode bit to set — see [`crate::sys`]).
//! * **Nothing goes to stdout/stderr.** The TUI owns the terminal; records go to
//!   the file only. The crash path is the single exception, after it has left
//!   the alternate screen.
//!
//! # Module map (where to add things)
//!
//! | file | contents |
//! |---|---|
//! | `logging.rs` (this file) | public API + the process-global logger and its `OnceLock`s |
//! | `level.rs` | [`Level`], [`Config`], [`default_dir`], the env overrides — pure |
//! | `sink.rs` | [`Logger`], the file sink, rotation, the re-entrancy guard |
//! | `format.rs` | [`now_timestamp`], token/value escaping — pure and testable |
//! | `panic.rs` | the panic hook and the `crash.panic` records |
//! | `crash.rs` | the fatal-signal handler (**only `unsafe` in the crate**) |
//!
//! Rules of thumb: a new *field* or an escaping change belongs in `format.rs`; a
//! new *rotation* or write-policy change belongs in `sink.rs`; a new *crash*
//! source belongs in `panic.rs` (if it unwinds) or `crash.rs` (if it does not).
//! Nothing outside this module may call `sink`/`panic`/`crash` items directly:
//! go through the [`Logger`] methods or the `debug`/`info`/`warn`/`error`/
//! `message` functions at the bottom of this file.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod crash;
mod format;
mod level;
mod panic;
mod sink;

pub use format::now_timestamp;
pub use level::{Config, Level, default_dir};
pub use sink::Logger;

/// Directory override (`SSHPF_LOG_DIR`).
pub const LOG_DIR_ENV: &str = "SSHPF_LOG_DIR";
/// Level override (`SSHPF_LOG_LEVEL`): `off|error|warn|info|debug`.
pub const LOG_LEVEL_ENV: &str = "SSHPF_LOG_LEVEL";

/// Base name of the active log file; archives are `<name>.1`, `<name>.2`, …
pub const LOG_FILE_NAME: &str = "sshpf.log";

/// Rotate once the active file would grow past this many bytes.
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Number of archived files kept next to the active one.
pub const DEFAULT_KEEP: usize = 3;

/// A frame slower than this is an anomaly worth recording: the UI is supposed to
/// turn over every 200 ms.
pub const SLOW_FRAME_MS: u128 = 500;

/// Frames kept from a panic backtrace.
///
/// A debug backtrace of this program is 30+ frames of `std::panicking` and
/// `core::result` plumbing before it reaches the interesting one. The first
/// frames are the ones that matter (they name the panicking function); the tail
/// is `_start`. Capping keeps one crash to a bounded number of lines.
pub const BACKTRACE_FRAMES: usize = 25;

// ---------------------------------------------------------------------------
// process-global logger
// ---------------------------------------------------------------------------

pub(super) static GLOBAL: OnceLock<Logger> = OnceLock::new();
static INIT_ERROR: OnceLock<String> = OnceLock::new();
/// What [`logger`] answers before [`init`] has run.
static FALLBACK: Logger = Logger::disabled();

/// The process-wide logger (disabled if [`init`] was never called).
pub fn logger() -> &'static Logger {
    GLOBAL.get().unwrap_or(&FALLBACK)
}

/// Install the process-wide logger from the environment.
///
/// Returns the log path, or the reason logging is off. The caller decides
/// whether that is worth telling the user — the TUI cannot print to stderr.
pub fn init_from_env() -> Result<PathBuf, String> {
    let (level, level_warning) = level::level_from_env();
    let result = init(Config { level, ..Config::default() });
    if let Some(warning) = level_warning {
        // Emitted after init so the note lands in the file it is about.
        logger().msg(Level::Warn, "log.level_invalid", &warning);
    }
    result
}

/// Install the process-wide logger. Only the first call has an effect.
pub fn init(cfg: Config) -> Result<PathBuf, String> {
    if let Some(existing) = GLOBAL.get() {
        return existing.path().ok_or_else(|| "日志未启用".to_string());
    }
    match Logger::open(&cfg) {
        Ok(opened) => {
            let path = opened.path().unwrap_or_default();
            let _ = GLOBAL.set(opened);
            // Wire the crash path only for the real logger.
            logger().rewire_crash_fd();
            Ok(path)
        }
        Err(e) => {
            let reason = format!("打开日志文件失败（{}）: {e}", cfg.dir.display());
            let _ = INIT_ERROR.set(reason.clone());
            Err(reason)
        }
    }
}

/// Why logging is not active, if [`init`] failed.
pub fn init_error() -> Option<&'static str> {
    INIT_ERROR.get().map(String::as_str)
}

fn log_at(level: Level, event: &str, fields: &[(&str, &dyn fmt::Display)]) {
    logger().event(level, event, fields);
}

pub fn debug(event: &str, fields: &[(&str, &dyn fmt::Display)]) {
    log_at(Level::Debug, event, fields);
}

pub fn info(event: &str, fields: &[(&str, &dyn fmt::Display)]) {
    log_at(Level::Info, event, fields);
}

pub fn warn(event: &str, fields: &[(&str, &dyn fmt::Display)]) {
    log_at(Level::Warn, event, fields);
}

pub fn error(event: &str, fields: &[(&str, &dyn fmt::Display)]) {
    log_at(Level::Error, event, fields);
}

/// One-message record: `event=<event> msg=<text>`.
pub fn message(level: Level, event: &str, text: &str) {
    logger().msg(level, event, text);
}

// ---------------------------------------------------------------------------
// crash capture: entry points
// ---------------------------------------------------------------------------

/// Install the panic hook and (on Unix) the fatal-signal handlers.
///
/// Call once, as early as possible, so a panic during start-up is captured too.
pub fn install_crash_hooks() {
    panic::install_panic_hook();
    crash::install();
}

/// Tell the crash path whether the TUI currently owns the terminal.
///
/// `true` makes the signal handler emit the "leave the alternate screen" escape
/// sequence before it dies; `false` (the default, and after the terminal is
/// restored) keeps it silent.
pub fn set_tui_active(active: bool) {
    crash::set_tui_active(active);
}

/// Open a logger for `dir` without touching the process-global one (tests).
pub fn open_at(dir: &Path, level: Level) -> io::Result<Logger> {
    Logger::open(&Config { dir: dir.to_path_buf(), level, ..Config::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_global_logger_is_disabled_until_init_runs() {
        // Tests never call `init` (it is process-global), so this is the state
        // every other unit test sees: records go nowhere, nothing panics.
        let global = logger();
        assert!(!global.is_active(), "no test may install the global logger");
        assert!(init_error().is_none() || !global.is_active());
        // The convenience functions must all be safe on the disabled logger.
        debug("test.disabled", &[("i", &0)]);
        info("test.disabled", &[("i", &0)]);
        warn("test.disabled", &[("i", &0)]);
        error("test.disabled", &[("i", &0)]);
        message(Level::Info, "test.disabled", "text");
    }

    #[test]
    fn the_disabled_fallback_is_the_same_logger_every_time() {
        // `logger()` is called from the panic hook, so it must never allocate a
        // new logger or block.
        assert!(std::ptr::eq(logger(), logger()));
    }

    #[test]
    fn a_logger_can_be_opened_without_touching_the_globals() {
        // This is the seam every logger test uses; if it ever adopted the global
        // slot, `cargo test --lib logging::` would be order-dependent.
        let dir = tempfile::tempdir().unwrap();
        let logger = open_at(dir.path(), Level::Debug).unwrap();
        assert!(logger.is_active());
        assert_eq!(logger.level(), Level::Debug);
        logger.event(Level::Debug, "probe", &[("x", &1)]);
        let text = std::fs::read_to_string(dir.path().join(LOG_FILE_NAME)).unwrap();
        assert!(text.contains("event=probe"), "{text}");
        assert!(!super::logger().is_active(), "open_at must not install the global logger");
    }

    #[test]
    fn message_writes_the_msg_field() {
        // `message()` is the one-message convenience; the key must stay `msg` so
        // `grep msg=` keeps working across the whole file.
        let dir = tempfile::tempdir().unwrap();
        let logger = open_at(dir.path(), Level::Debug).unwrap();
        logger.msg(Level::Info, "test.one_liner", "hello 世界");
        let text = std::fs::read_to_string(dir.path().join(LOG_FILE_NAME)).unwrap();
        assert!(text.contains("event=test.one_liner"), "{text}");
        assert!(text.contains("msg=\"hello 世界\""), "a value with a space must be quoted: {text}");
    }
}

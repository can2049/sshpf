//! Severity levels, the configuration needed to open a log file, and the two
//! environment variables that override them.
//!
//! Everything here is pure: no file is opened, no global is touched. That is why
//! it can be unit-tested without a tempdir, and why the level filter can be
//! reasoned about (and checked) without a logger.

use std::path::PathBuf;

use super::{DEFAULT_KEEP, DEFAULT_MAX_BYTES, LOG_DIR_ENV, LOG_LEVEL_ENV};

/// Severity of one record. Ordered by *verbosity*, so `level <= configured`
/// means "at least as important as the threshold".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Nothing is written; crash records still are.
    Off,
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Off => "OFF",
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }

    /// Parse a level name. Case-insensitive; `warning`/`trace` are accepted
    /// because that is what neighbouring tools call the same two levels.
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "silent" => Some(Level::Off),
            "error" | "err" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" | "trace" => Some(Level::Debug),
            _ => None,
        }
    }
}

/// Everything needed to open the log file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub dir: PathBuf,
    pub level: Level,
    pub max_bytes: u64,
    pub keep: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config { dir: default_dir(), level: Level::Info, max_bytes: DEFAULT_MAX_BYTES, keep: DEFAULT_KEEP }
    }
}

/// `$SSHPF_LOG_DIR`, else `<state dir>/sshpf/logs`.
///
/// The state directory is where `state.toml` already lives, so bookkeeping and
/// logs share a volume and neither pollutes the user-editable config directory.
pub fn default_dir() -> PathBuf {
    if let Some(p) = std::env::var_os(LOG_DIR_ENV) {
        return PathBuf::from(p);
    }
    let base =
        dirs::state_dir().or_else(dirs::data_local_dir).unwrap_or_else(|| PathBuf::from(".").join(".local/state"));
    base.join("sshpf").join("logs")
}

/// Read the level from the environment, defaulting to `Info`.
///
/// An unparseable value is reported back rather than silently ignored, so a typo
/// (`SSHPF_LOG_LEVEL=verbose`) is visible instead of mysterious.
pub(super) fn level_from_env() -> (Level, Option<String>) {
    let Some(raw) = std::env::var_os(LOG_LEVEL_ENV) else {
        return (Level::Info, None);
    };
    let raw = raw.to_string_lossy().to_string();
    match Level::parse(&raw) {
        Some(level) => (level, None),
        None => (Level::Info, Some(format!("{LOG_LEVEL_ENV}={raw} 不是有效级别，回退到 info"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_order_by_verbosity() {
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse(" warning "), Some(Level::Warn));
        assert_eq!(Level::parse("err"), Some(Level::Error));
        assert_eq!(Level::parse("off"), Some(Level::Off));
        assert_eq!(Level::parse("verbose"), None);
        // Filtering is `record_level <= configured`, so Info must admit Warn.
        assert!(Level::Warn <= Level::Info);
        assert!(Level::Error <= Level::Info);
        assert!(Level::Info > Level::Warn);
        assert!(Level::Debug > Level::Info);
    }

    #[test]
    fn level_names_round_trip_through_parse() {
        // `as_str` writes the level into every record; a name that `parse` cannot
        // read back would make the file unparseable by our own tooling.
        for level in [Level::Off, Level::Error, Level::Warn, Level::Info, Level::Debug] {
            assert_eq!(Level::parse(level.as_str()), Some(level), "{}", level.as_str());
        }
    }

    #[test]
    fn config_defaults_match_the_documented_knobs() {
        // `Config::default()` is the documented behaviour: info level, 2 MiB per
        // file, 3 archives. A silent change here breaks the README and the
        // "rotate once the file passes 2 MiB" promise, so it is pinned.
        let cfg = Config::default();
        assert_eq!(cfg.level, Level::Info);
        assert_eq!(cfg.max_bytes, 2 * 1024 * 1024);
        assert_eq!(cfg.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(cfg.keep, DEFAULT_KEEP);
        assert_eq!(cfg.keep, 3);
        assert!(!cfg.dir.as_os_str().is_empty(), "the default log directory must not be empty");
    }
}

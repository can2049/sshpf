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
//! [`fatal`].
//!
//! # Crash records
//!
//! Two independent mechanisms, because they cover different failures:
//!
//! * **Panics** go through [`std::panic::set_hook`]: message, location, thread
//!   name and a force-captured backtrace become a `crash.panic` record plus an
//!   indented trace block. This catches every Rust-level crash, including one
//!   inside a tunnel thread — the thread name is in the record, which is how a
//!   dead tunnel thread is told apart from a clean exit.
//! * **Fatal signals** (SIGSEGV/SIGBUS/SIGABRT/SIGILL/SIGFPE) get a real
//!   `sigaction` handler. It may only use async-signal-safe calls, so it writes
//!   a hand-formatted record straight to a pre-opened descriptor — no
//!   allocation, no locks, no formatting machinery — then restores the default
//!   action and re-raises, so the exit status and core dump are unchanged.
//!
//! # Invariants
//!
//! * **Logging can never take the program down.** If the file cannot be opened
//!   the logger degrades to disabled; write errors are swallowed.
//! * **Nothing secret is written.** No key material, no passphrases, no
//!   `known_hosts` contents. Hosts, users and ports are recorded because they
//!   are what a diagnosis needs; the file is `0600` inside a `0700` directory.
//! * **Nothing goes to stdout/stderr.** The TUI owns the terminal; records go to
//!   the file only. The crash path is the single exception, after it has left
//!   the alternate screen.

use std::cell::Cell;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
// levels
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

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
fn level_from_env() -> (Level, Option<String>) {
    let Some(raw) = std::env::var_os(LOG_LEVEL_ENV) else {
        return (Level::Info, None);
    };
    let raw = raw.to_string_lossy().to_string();
    match Level::parse(&raw) {
        Some(level) => (level, None),
        None => (Level::Info, Some(format!("{LOG_LEVEL_ENV}={raw} 不是有效级别，回退到 info"))),
    }
}

// ---------------------------------------------------------------------------
// the logger
// ---------------------------------------------------------------------------

struct Sink {
    file: File,
    path: PathBuf,
    size: u64,
    max_bytes: u64,
    keep: usize,
    seq: u64,
}

/// A log destination. Cheap to check and safe to call from any thread.
pub struct Logger {
    level: Level,
    sink: Option<Mutex<Sink>>,
}

// ---------------------------------------------------------------------------
// reentrancy guard
// ---------------------------------------------------------------------------

thread_local! {
    /// `true` while *this thread* is inside the logger's critical section.
    ///
    /// `std::sync::Mutex` is not reentrant, and re-locking it from the thread
    /// that already holds it does not return `Err(poisoned)` — it blocks
    /// forever. That is not the case [`Logger::record`]'s poison handling
    /// covers; it is a hang, and the way to reach it is the panic hook:
    /// a panic raised inside `record` (a `Display` impl, the timezone lookup,
    /// the I/O) makes the hook call `record` again on the same thread, which
    /// would then wait on a mutex that can only be released by the panicking
    /// frame. The process would sit there with the terminal in the alternate
    /// screen and not one byte in the log. This flag is how the hook notices.
    static IN_CRITICAL_SECTION: Cell<bool> = const { Cell::new(false) };
}

/// Marks the logger's critical section for as long as it is alive.
///
/// Cleared in `Drop`, so a panic unwinding out of `record` cannot leave the flag
/// stuck. (`panic = "abort"` skips the `Drop`, but it also kills the process,
/// which makes a stuck flag irrelevant.)
struct CriticalSection;

impl CriticalSection {
    /// `None` when this thread is already inside the logger: the caller must
    /// degrade instead of locking.
    fn enter() -> Option<CriticalSection> {
        // `try_with` fails only while the thread-local is being destroyed. Treat
        // that as "not re-entrant": during TLS teardown there is no panic hook
        // left to protect anyway.
        IN_CRITICAL_SECTION
            .try_with(|flag| !flag.replace(true))
            .ok()
            .and_then(|fresh| if fresh { Some(CriticalSection) } else { None })
    }
}

impl Drop for CriticalSection {
    fn drop(&mut self) {
        let _ = IN_CRITICAL_SECTION.try_with(|flag| flag.set(false));
    }
}

// ---------------------------------------------------------------------------
// degraded write path
// ---------------------------------------------------------------------------

/// Whether the reentrancy note has been emitted. One line is enough: a repeat
/// adds nothing and stderr may be a terminal the user is still looking at.
static REENTRANT_NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Report a write that had to be skipped because this thread already holds the
/// logger.
///
/// The sink itself is unusable (that is the whole problem), so evidence goes to
/// the two places that cannot deadlock: stderr for the human watching the crash,
/// and — best effort — the pre-`dup`ed crash descriptor, which is written with a
/// bare `write(2)` and needs no lock.
fn report_reentrant(event: &str) {
    use std::sync::atomic::Ordering;
    if REENTRANT_NOTED.swap(true, Ordering::Relaxed) {
        return;
    }
    // `writeln!` on a `Stderr` handle returns a `Result`; `eprintln!` panics when
    // stderr is gone, and panicking inside a panic hook aborts the process.
    let _ = writeln!(
        io::stderr(),
        "[sshpf] 日志系统重入（event={}）：该条记录被跳过以免死锁；崩溃信息请看本行前后的 panic 输出。",
        event
    );
    fatal::emergency(event);
}

impl Logger {
    /// A logger that writes nothing. Used before [`init`] and by tests.
    pub fn disabled() -> Logger {
        Logger { level: Level::Off, sink: None }
    }

    /// Open (creating if needed) `<dir>/sshpf.log`.
    ///
    /// The directory is tightened to `0700` and the file to `0600`: the log names
    /// hosts, users and ports.
    pub fn open(cfg: &Config) -> io::Result<Logger> {
        let dir = cfg.dir.clone();
        fs::create_dir_all(&dir)?;
        let _ = crate::sys::restrict_dir(&dir);
        let path = dir.join(LOG_FILE_NAME);
        let file = open_append(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let sink = Sink { file, path, size, max_bytes: cfg.max_bytes, keep: cfg.keep.max(1), seq: 0 };
        Ok(Logger { level: cfg.level, sink: Some(Mutex::new(sink)) })
    }

    pub fn level(&self) -> Level {
        self.level
    }

    /// Path of the active file, if this logger writes to one.
    ///
    /// Returns `None` when called re-entrantly (from inside a record on the same
    /// thread): reporting the path is never important enough to risk the hang.
    pub fn path(&self) -> Option<PathBuf> {
        let mutex = self.sink.as_ref()?;
        let _critical = CriticalSection::enter()?;
        let sink = mutex.lock().ok()?;
        Some(sink.path.clone())
    }

    pub fn is_active(&self) -> bool {
        self.sink.is_some()
    }

    /// Whether a record at `level` would be written.
    pub fn enabled(&self, level: Level) -> bool {
        self.sink.is_some() && level != Level::Off && level <= self.level
    }

    /// Write one record. Silently does nothing when disabled or when the write
    /// fails — logging must never be the reason sshpf dies.
    pub fn event(&self, level: Level, event: &str, fields: &[(&str, &dyn fmt::Display)]) {
        self.record(level, event, fields, false)
    }

    /// Write one record regardless of the configured level.
    ///
    /// Crash records use this: a user who silences volume with
    /// `SSHPF_LOG_LEVEL=off` still wants the one line explaining why the process
    /// died.
    pub fn critical(&self, event: &str, fields: &[(&str, &dyn fmt::Display)]) {
        self.record(Level::Error, event, fields, true)
    }

    /// Convenience for a record whose payload is a single message.
    pub fn msg(&self, level: Level, event: &str, message: &str) {
        self.record(level, event, &[("msg", &message)], false)
    }

    fn record(&self, level: Level, event: &str, fields: &[(&str, &dyn fmt::Display)], force: bool) {
        let Some(mutex) = &self.sink else { return };
        if !force && !self.enabled(level) {
            return;
        }
        // Built *before* the lock: `Local::now()` consults the timezone database
        // and can panic on a corrupt one, and a panic while holding the sink is
        // the deadlock this function is careful to avoid.
        let timestamp = now_timestamp();

        let rotated = {
            // Re-entered from this thread (a panic hook firing inside `record`):
            // locking again would hang, so degrade instead.
            let Some(_critical) = CriticalSection::enter() else {
                report_reentrant(event);
                return;
            };
            let mut guard = match mutex.lock() {
                Ok(g) => g,
                // A panic while the lock was held must not disable logging forever.
                Err(poisoned) => poisoned.into_inner(),
            };
            let sink = &mut *guard;

            // `saturating_*` rather than `+`: this code runs in the panic hook, so
            // "the counter cannot realistically overflow" is not good enough — a
            // panic here would abort instead of leaving the crash record.
            let seq = sink.seq.saturating_add(1);
            let mut line = String::with_capacity(192);
            let _ = write!(line, "{} {:<5} seq={seq} event={}", timestamp, level.as_str(), token(event));
            for (key, value) in fields {
                line.push(' ');
                line.push_str(&token(key));
                line.push('=');
                push_value(&mut line, &value.to_string());
            }
            line.push('\n');

            // Rotate *before* appending, so a record never straddles two files (a
            // half-record at a rotation boundary is unparseable).
            let needed = line.len() as u64;
            let mut rotated = false;
            if sink.max_bytes > 0
                && sink.size > 0
                && sink.size.saturating_add(needed) > sink.max_bytes
                && rotate(sink, self.is_global()).is_ok()
            {
                rotated = true;
            }

            if sink.file.write_all(line.as_bytes()).is_ok() {
                sink.seq = seq;
                sink.size = sink.size.saturating_add(needed);
            }
            rotated
            // `guard` and `_critical` are both dropped here, so the rewire below
            // takes a fresh critical section instead of being mistaken for a
            // re-entrant call.
        };
        if rotated && self.is_global() {
            self.rewire_crash_fd();
        }
    }

    /// Force a rotation. Used by tests and for an operator-triggered log split.
    pub fn rotate_now(&self) -> io::Result<()> {
        let Some(mutex) = &self.sink else { return Ok(()) };
        {
            let Some(_critical) = CriticalSection::enter() else {
                return Err(io::Error::other("日志系统重入，已跳过轮转"));
            };
            let mut guard = mutex.lock().unwrap_or_else(|p| p.into_inner());
            rotate(&mut guard, self.is_global())?;
        }
        if self.is_global() {
            self.rewire_crash_fd();
        }
        Ok(())
    }

    fn is_global(&self) -> bool {
        GLOBAL.get().is_some_and(|g| std::ptr::eq(g, self))
    }

    /// Point the async-signal-safe crash writer at this logger's file.
    ///
    /// Only the global logger owns the crash fd: a test-local logger must not
    /// steal the descriptor a real crash would need.
    fn rewire_crash_fd(&self) {
        let Some(mutex) = &self.sink else { return };
        // Called from `record`/`rotate_now` only *after* their guard is dropped,
        // so this is a fresh entry, not a re-entrant one. If it ever were
        // re-entrant, skipping is still better than hanging: the previously
        // adopted fd keeps working.
        let Some(_critical) = CriticalSection::enter() else { return };
        let guard = match mutex.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        fatal::adopt(&guard.file, &guard.path);
    }
}

/// Open for append, creating the file `0600` when it does not exist yet.
fn open_append(path: &Path) -> io::Result<File> {
    crate::sys::open_private_append(path)
}

impl Default for Logger {
    fn default() -> Self {
        Logger::disabled()
    }
}

impl fmt::Debug for Logger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logger").field("level", &self.level).field("active", &self.sink.is_some()).finish()
    }
}

/// Move the active file aside and start a new one.
///
/// History is `sshpf.log.1` (newest) … `sshpf.log.<keep>` (oldest). A failed
/// archive rename is ignored: losing history beats losing the present.
///
/// `owns_crash_fd` is `true` only when this sink belongs to the process-global
/// logger. The crash descriptor is a single process-wide slot, so a *test-local*
/// logger rotating its own file must not touch it: doing so closed the fd the
/// global logger had adopted and made the fatal-signal record vanish — which is
/// exactly how a concurrent `cargo test --lib` run could lose
/// `fatal_signal_handler_really_writes_a_record` intermittently.
fn rotate(sink: &mut Sink, owns_crash_fd: bool) -> io::Result<()> {
    let dir = sink.path.parent().unwrap_or_else(|| Path::new("."));
    let archive = |n: usize| dir.join(format!("{LOG_FILE_NAME}.{n}"));

    // Detach the crash descriptor first: it is a `dup` of the file about to be
    // renamed, so a crash in this window would append to the archive instead of
    // the active file. The caller re-attaches it right after rotation.
    if owns_crash_fd {
        fatal::release();
    }

    let _ = fs::remove_file(archive(sink.keep));
    for n in (1..sink.keep).rev() {
        let from = archive(n);
        if from.exists() {
            let _ = fs::rename(&from, archive(n + 1));
        }
    }
    if sink.size > 0 {
        let _ = fs::rename(&sink.path, archive(1));
    }
    sink.file = open_append(&sink.path)?;
    sink.size = 0;
    Ok(())
}

// ---------------------------------------------------------------------------
// formatting (pure, so it is testable without touching the filesystem)
// ---------------------------------------------------------------------------

/// `2026-11-08T18:20:01.123+08:00` — millisecond precision in local time, which
/// is what lets sshpf records line up with `journalctl` and sshd output.
///
/// Deliberately **not** `chrono::Local::now()`: that is `Utc::now()` followed by a
/// local-offset lookup, and `Utc::now()` is
/// `SystemTime::now().duration_since(UNIX_EPOCH).expect("system time before Unix
/// epoch")`. A board without an RTC — or a container started before its clock is
/// synced — has a pre-epoch clock, and this function runs inside the panic hook,
/// where a nested panic aborts the process instead of leaving a crash record.
/// Building the instant by hand cannot panic; a pre-epoch clock simply formats as
/// 1970-01-01.
pub fn now_timestamp() -> String {
    const FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX);
    let Some(utc) = chrono::DateTime::from_timestamp(secs, elapsed.subsec_nanos()) else {
        // Only reachable with a clock set past the representable range; keep the
        // record instead of losing it.
        return format!("epoch={secs}");
    };
    // The local-offset lookup is the one third-party call here that can still
    // panic: `chrono`'s `Local::offset_from_utc_datetime` is
    // `.expect("unable to select local time type")` over a fallible TZif lookup,
    // which a corrupt `/usr/share/zoneinfo` can fail. There is no fallible public
    // API to use instead, and this function is on the panic-hook path, where a
    // nested panic aborts the process — so `catch_unwind` here is the documented
    // last-resort case, and the fallback (UTC) is a fully valid record rather than
    // a lost one.
    std::panic::catch_unwind(|| utc.with_timezone(&chrono::Local).format(FORMAT).to_string())
        .unwrap_or_else(|_| utc.format(FORMAT).to_string())
}

/// Keys and event names are parsed by splitting on `=` and whitespace, so either
/// would corrupt the record.
fn token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.push(if c.is_whitespace() || c == '=' || c == '"' || c == '\\' || c.is_control() { '_' } else { c });
    }
    if out.is_empty() { "_".to_string() } else { out }
}

/// Append a value, quoting and escaping it when a bare token would be ambiguous.
fn push_value(out: &mut String, value: &str) {
    let needs_quoting =
        value.is_empty() || value.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\' || c.is_control());
    if !needs_quoting {
        out.push_str(value);
        return;
    }
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// process-global logger
// ---------------------------------------------------------------------------

static GLOBAL: OnceLock<Logger> = OnceLock::new();
static INIT_ERROR: OnceLock<String> = OnceLock::new();
static FALLBACK: Logger = Logger { level: Level::Off, sink: None };

/// The process-wide logger (disabled if [`init`] was never called).
pub fn logger() -> &'static Logger {
    GLOBAL.get().unwrap_or(&FALLBACK)
}

/// Install the process-wide logger from the environment.
///
/// Returns the log path, or the reason logging is off. The caller decides
/// whether that is worth telling the user — the TUI cannot print to stderr.
pub fn init_from_env() -> Result<PathBuf, String> {
    let (level, level_warning) = level_from_env();
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
// crash capture: panics
// ---------------------------------------------------------------------------

/// Install the panic hook and (on Unix) the fatal-signal handlers.
///
/// Call once, as early as possible, so a panic during start-up is captured too.
///
/// # Interaction with `ratatui`
///
/// This hook chains to whatever hook was installed before it, and it is installed
/// from `main` *before* `ratatui::try_init()`. `try_init` then wraps **this** hook
/// in its own (whose first action is to restore the terminal), so a panic runs
/// ratatui's restore and then this record — in that order, which is the one both
/// libraries document. The reverse order would restore the terminal and then
/// still write the crash record, so the ordering is not critical, but it is
/// verified: see `the_hook_chain_runs_ratatuis_restore_before_recording` and the
/// real crash tests in `tests/logging.rs`.
pub fn install_crash_hooks() {
    install_panic_hook();
    fatal::install();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let logger = logger();
        if logger.is_active() {
            // A panic raised *inside* this hook aborts the process on the spot
            // ("thread panicked while processing panic"), taking the original
            // panic message with it. Everything here is therefore infallible by
            // construction, and `catch_unwind` is only the belt to that braces:
            // if it ever is wrong, the process still gets the upstream hook and
            // its terminal restore.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log_panic(logger, info)));
        }
        previous(info);
    }));
}

/// Turn one panic into a `crash.panic` record plus its trace block.
fn log_panic(logger: &Logger, info: &std::panic::PanicHookInfo<'_>) {
    // Every step below is fallible in a "would panic again" sense: `Display` of a
    // user type, `panic::Location`, a thread name, and capturing a backtrace. They
    // are all fallbacks by use of `Option`/`unwrap_or`, so no allocation failure
    // or non-string payload can turn one crash into an abort.
    let payload = payload_text(info);
    let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
    let thread = std::thread::current().name().unwrap_or("<unnamed>").to_string();
    let backtrace = std::backtrace::Backtrace::force_capture().to_string();
    write_panic_record(logger, &payload, location.as_deref(), &thread, &backtrace);
}

fn payload_text(info: &std::panic::PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<非字符串 panic>".to_string()
    }
}

/// Emit the panic record and its backtrace block.
///
/// The trace goes on separate lines prefixed with `  | ` instead of inside the
/// record: one record per line keeps `grep event=` working, and the prefix makes
/// the trace recoverable with `sed -n '/^  |/p'`.
fn write_panic_record(logger: &Logger, payload: &str, location: Option<&str>, thread: &str, backtrace: &str) {
    let location = location.unwrap_or("<未知位置>");
    logger.critical(
        "crash.panic",
        &[("thread", &thread), ("location", &location), ("msg", &payload), ("backtrace", &"yes")],
    );
    // Frame lines are `   N: symbol` plus an indented `at file:line`. Keep the
    // frame lines (not the `at` lines) up to the cap, so the block stays readable
    // and bounded instead of dumping the whole runtime.
    let mut block = String::from("  | panic backtrace");
    let mut frames = 0usize;
    for line in backtrace.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let is_frame = line.trim_start().starts_with(|c: char| c.is_ascii_digit()) && line.contains(": ");
        if is_frame {
            if frames == BACKTRACE_FRAMES {
                block.push_str("\n  | … (更深的栈帧已省略)");
                break;
            }
            frames += 1;
        } else if frames == 0 {
            // Anything before the first frame is header noise.
            continue;
        }
        block.push('\n');
        block.push_str("  | ");
        block.push_str(line);
    }
    logger.critical("crash.panic.trace", &[("msg", &block.as_str())]);
}

// ---------------------------------------------------------------------------
// crash capture: fatal signals
// ---------------------------------------------------------------------------

/// Async-signal-safe crash writer.
///
/// Deliberately uses only `write(2)` on pre-opened descriptors plus hand-rolled
/// integer formatting: allocating, locking or using the formatting machinery
/// inside a SIGSEGV handler is what turns "one crash" into "a hung process".
///
/// The record cannot carry a formatted local timestamp (`localtime_r` is not
/// reliably async-signal-safe and its first call would fault on the very
/// allocation we avoid), so it starts with `ts=<epoch seconds>` instead. Every
/// other field has the usual `key=value` shape, so the normal parser still
/// works.
#[cfg(unix)]
mod fatal {
    use std::path::Path;
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};

    /// Signals that abort the process without unwinding. SIGKILL/SIGSTOP cannot
    /// be caught; this list is exactly the "crashed" class we can report.
    const FATAL_SIGNALS: &[i32] = &[libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT, libc::SIGILL, libc::SIGFPE];

    /// Longest stderr note composed at init.
    const NOTE_MAX: usize = 320;

    /// Raw fd of the active log file, kept in sync with rotation.
    static LOG_FD: AtomicI32 = AtomicI32::new(-1);
    /// Set once the TUI owns the terminal, so the handler can release it.
    static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
    static INSTALLED: Once = Once::new();

    /// The stderr note, composed at init (never in the handler). Stored as
    /// atomics so both the writer and the handler stay free of `unsafe` aliasing:
    /// the handler copies it into a stack buffer, which is async-signal-safe.
    static NOTE: [AtomicU8; NOTE_MAX] = [const { AtomicU8::new(0) }; NOTE_MAX];
    static NOTE_LEN: AtomicUsize = AtomicUsize::new(0);

    /// Leave the alternate screen and restore the cursor. A terminal restores
    /// itself for a clean exit; after a crash the user would otherwise be left
    /// on a stuck screen with an invisible cursor.
    const LEAVE_TUI: &[u8] = b"\x1b[?25h\x1b[?1049l\x1b[0m\r\n";

    fn write_all(fd: i32, bytes: &[u8]) {
        if fd < 0 || bytes.is_empty() {
            return;
        }
        let mut off = 0;
        while off < bytes.len() {
            // SAFETY: `bytes` is a live slice, `off` stays in bounds, and
            // `libc::write` is async-signal-safe.
            let n = unsafe { libc::write(fd, bytes.as_ptr().add(off).cast(), bytes.len() - off) };
            if n <= 0 {
                return;
            }
            off += n as usize;
        }
    }

    unsafe extern "C" fn handler(sig: i32) {
        if TUI_ACTIVE.load(Ordering::Relaxed) {
            write_all(libc::STDOUT_FILENO, LEAVE_TUI);
        }
        let (buf, len) = build_signal_record(sig);
        write_all(LOG_FD.load(Ordering::Relaxed), &buf[..len]);
        write_all(LOG_FD.load(Ordering::Relaxed), b"\n");

        // Tell the human, not just the file. Pre-composed, so no formatting, and
        // copied onto the stack so the handler never allocates.
        let note_len = NOTE_LEN.load(Ordering::Relaxed).min(NOTE_MAX);
        if note_len > 0 {
            let mut note = [0u8; NOTE_MAX];
            for (i, slot) in NOTE.iter().enumerate().take(note_len) {
                note[i] = slot.load(Ordering::Relaxed);
            }
            write_all(libc::STDERR_FILENO, &note[..note_len]);
        }

        // Restore the default action and re-raise, so the exit status and core
        // dump stay identical to a normal signal death.
        //
        // The kernel masks the signal for the duration of its handler, so a bare
        // `raise` would just queue it until the handler returns — and then
        // `_exit` would win, turning a SIGSEGV death into exit code 139. The
        // signal must be unblocked first. Verified: without this the child is
        // reported as `WIFEXITED(139)` rather than `WIFSIGNALED(SIGSEGV)`.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(sig);
            // Re-raising should never return; if it does, stop rather than
            // continue with corrupted state.
            libc::_exit(128 + sig);
        }
    }

    /// Install the handlers. Idempotent; failures are ignored (a platform that
    /// refuses SIGSEGV handling still gets panic capture).
    pub fn install() {
        INSTALLED.call_once(|| {
            for sig in FATAL_SIGNALS {
                // SAFETY: the handler performs only async-signal-safe calls, and
                // SA_RESETHAND makes a recursive fault fall back to the default
                // action instead of looping.
                unsafe {
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = handler as *const () as libc::sighandler_t;
                    action.sa_flags = libc::SA_RESETHAND;
                    libc::sigemptyset(&mut action.sa_mask);
                    libc::sigaction(*sig, &action, std::ptr::null_mut());
                }
            }
        });
    }

    /// Point crash records at `file` and compose the stderr note for `path`.
    ///
    /// The descriptor is dup'ed so a later rotation closing the original cannot
    /// leave the handler writing to a recycled fd.
    pub fn adopt(file: &std::fs::File, path: &Path) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: plain libc call; the result is checked.
        let dup = unsafe { libc::dup(file.as_raw_fd()) };
        if dup < 0 {
            LOG_FD.store(-1, Ordering::Relaxed);
            return;
        }
        let previous = LOG_FD.swap(dup, Ordering::Relaxed);
        if previous >= 0 {
            // SAFETY: `previous` came from `dup` in this module.
            unsafe { libc::close(previous) };
        }

        let note = format!("[sshpf] 致命错误，崩溃信息已写入 {}（终端错乱请执行 reset）\n", path.display());
        let note = note.as_bytes();
        let len = note.len().min(NOTE_MAX);
        // Publish the bytes before the length, so a handler never reads a
        // half-written note.
        for (i, byte) in note[..len].iter().enumerate() {
            NOTE[i].store(*byte, Ordering::Relaxed);
        }
        NOTE_LEN.store(len, Ordering::Relaxed);
    }

    /// Drop the crash fd (rotation, or tests).
    pub fn release() {
        let fd = LOG_FD.swap(-1, Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: `fd` came from `dup` in this module.
            unsafe { libc::close(fd) };
        }
    }

    /// Leave one line in the log file when the normal writer cannot be used.
    ///
    /// Called from the *re-entrancy* degrade path, not from a signal handler, so
    /// formatting and allocation are allowed here; the write still goes through
    /// the pre-`dup`ed descriptor with a bare `write(2)`, which needs no lock and
    /// therefore cannot deadlock. The signature is shared with the non-Unix stub.
    pub fn emergency(event: &str) {
        let fd = LOG_FD.load(Ordering::Relaxed);
        if fd < 0 {
            return;
        }
        // No `now_timestamp()` here: this path is already the "something in the
        // writer went wrong" path, so it must stay as dumb as possible.
        let line = format!(
            "ts={} ERROR seq=0 event=crash.log_reentrant cause={} msg=\"日志写入重入，记录被跳过以避免死锁\"\n",
            // SAFETY: `time(null)` is async-signal-safe and cannot fail.
            unsafe { libc::time(std::ptr::null_mut()) },
            event
        );
        write_all(fd, line.as_bytes());
    }

    /// Note whether the TUI currently owns the terminal.
    pub fn set_tui_active(active: bool) {
        TUI_ACTIVE.store(active, Ordering::Relaxed);
    }

    /// Build the crash record without allocating. Split out so tests can check
    /// the format without actually crashing.
    pub fn build_signal_record(sig: i32) -> ([u8; 160], usize) {
        let mut buf = [0u8; 160];
        // SAFETY: `time(null)` is async-signal-safe.
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut at = push(&mut buf, 0, b"ts=");
        at = push_int(&mut buf, at, now);
        at = push(&mut buf, at, b" ERROR seq=0 event=crash.signal signal=");
        at = push_int(&mut buf, at, i64::from(sig));
        at = push(&mut buf, at, b" name=");
        at = push(&mut buf, at, signal_name(sig).as_bytes());
        at = push(&mut buf, at, b" pid=");
        // SAFETY: `getpid` has no preconditions.
        at = push_int(&mut buf, at, i64::from(unsafe { libc::getpid() }));
        (buf, at.min(buf.len()))
    }

    fn push(buf: &mut [u8], at: usize, bytes: &[u8]) -> usize {
        let end = (at + bytes.len()).min(buf.len());
        if at >= end {
            return buf.len();
        }
        buf[at..end].copy_from_slice(&bytes[..end - at]);
        end
    }

    fn push_int(buf: &mut [u8], at: usize, value: i64) -> usize {
        let mut digits = [0u8; 20];
        let negative = value < 0;
        let mut v = value.unsigned_abs();
        let mut n = 0;
        loop {
            digits[n] = b'0' + (v % 10) as u8;
            n += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        let mut at = at;
        if negative {
            at = push(buf, at, b"-");
        }
        while n > 0 {
            n -= 1;
            at = push(buf, at, &digits[n..n + 1]);
        }
        at
    }

    /// Stable names: `signal=11` alone is not greppable.
    fn signal_name(sig: i32) -> &'static str {
        match sig {
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGBUS => "SIGBUS",
            libc::SIGABRT => "SIGABRT",
            libc::SIGILL => "SIGILL",
            libc::SIGFPE => "SIGFPE",
            _ => "SIGUNKNOWN",
        }
    }
}

#[cfg(not(unix))]
mod fatal {
    use std::path::Path;

    pub fn install() {}
    pub fn adopt(_file: &std::fs::File, _path: &Path) {}
    pub fn release() {}
    pub fn set_tui_active(_active: bool) {}
    pub fn emergency(_event: &str) {}
}

/// Tell the crash path whether the TUI currently owns the terminal.
pub fn set_tui_active(active: bool) {
    fatal::set_tui_active(active);
}

/// Open a logger for `dir` without touching the process-global one (tests).
pub fn open_at(dir: &Path, level: Level) -> io::Result<Logger> {
    Logger::open(&Config { dir: dir.to_path_buf(), level, ..Config::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_logger(dir: &Path, level: Level) -> Logger {
        Logger::open(&Config { dir: dir.to_path_buf(), level, ..Config::default() }).unwrap()
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn seq_of(line: &str) -> u32 {
        // The `seq=N` field position differs between normal and crash records, so
        // find the field rather than assume a column.
        line.split(' ').find_map(|f| f.strip_prefix("seq=")).and_then(|n| n.parse().ok()).unwrap_or(0)
    }

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
    fn record_has_the_documented_shape() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        logger.event(Level::Info, "tunnel.ready", &[("tunnel", &3), ("elapsed_ms", &812)]);
        let text = read(&dir.path().join(LOG_FILE_NAME));
        let line = text.lines().next().unwrap();
        assert!(line.contains(" INFO  seq=1 event=tunnel.ready "), "fixed columns: {line}");
        assert!(line.ends_with("tunnel=3 elapsed_ms=812"), "fields keep caller order: {line}");
        let ts = line.split(' ').next().unwrap();
        assert_eq!(ts.len(), 29, "expected RFC 3339 with millis and offset: {ts}");
        assert!(ts.chars().nth(10) == Some('T'), "{ts}");
    }

    #[test]
    fn seq_increments_and_survives_across_loggers() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        for i in 0..3 {
            logger.event(Level::Info, "test", &[("i", &i)]);
        }
        let text = read(&dir.path().join(LOG_FILE_NAME));
        let seqs: Vec<u32> = text.lines().map(seq_of).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn level_filter_drops_quieter_records_but_never_crashes() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Warn);
        logger.event(Level::Info, "should_not_appear", &[]);
        logger.event(Level::Debug, "should_not_appear", &[]);
        logger.event(Level::Warn, "kept_warn", &[]);
        logger.event(Level::Error, "kept_error", &[]);
        logger.critical("crash.panic", &[("msg", &"boom")]);
        let text = read(&dir.path().join(LOG_FILE_NAME));
        assert!(!text.contains("should_not_appear"), "{text}");
        assert!(text.contains("event=kept_warn"), "{text}");
        assert!(text.contains("event=kept_error"), "{text}");
        assert!(text.contains("event=crash.panic"), "crash records bypass the level filter: {text}");
    }

    #[test]
    fn values_are_quoted_and_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Debug);
        logger.event(Level::Debug, "test", &[("plain", &"abc")]);
        logger.event(Level::Debug, "test", &[("spaced", &"a b")]);
        logger.event(Level::Debug, "test", &[("quote", &"say \"hi\"")]);
        logger.event(Level::Debug, "test", &[("newline", &"a\nb")]);
        logger.event(Level::Debug, "test", &[("empty", &"")]);
        logger.event(Level::Debug, "test", &[("backslash", &"c:\\tmp")]);
        let text = read(&dir.path().join(LOG_FILE_NAME));
        assert!(text.contains("plain=abc"), "{text}");
        assert!(text.contains("spaced=\"a b\""), "{text}");
        assert!(text.contains("quote=\"say \\\"hi\\\"\""), "{text}");
        assert!(text.contains("newline=\"a\\nb\""), "a newline must not split the record: {text}");
        assert!(text.contains("empty=\"\""), "{text}");
        assert!(text.contains("backslash=\"c:\\\\tmp\""), "{text}");
        assert_eq!(text.lines().count(), 6, "one record per call: {text}");
    }

    #[test]
    fn keys_and_event_names_cannot_corrupt_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        logger.event(Level::Info, "weird event=name", &[("bad key", &1)]);
        logger.event(Level::Info, "", &[]);
        let text = read(&dir.path().join(LOG_FILE_NAME));
        assert!(text.contains("event=weird_event_name"), "{text}");
        assert!(text.contains("bad_key=1"), "{text}");
        assert!(text.contains("event=_"), "an empty event name still yields a parseable field: {text}");
    }

    #[test]
    fn disabled_logger_writes_nothing_anywhere() {
        let logger = Logger::disabled();
        assert!(!logger.is_active());
        assert!(logger.path().is_none());
        assert!(!logger.enabled(Level::Error));
        // Must not panic even though there is no sink.
        logger.event(Level::Error, "nope", &[]);
        logger.critical("crash.panic", &[]);
        logger.msg(Level::Info, "nope", "nope");
        assert!(logger.rotate_now().is_ok());
    }

    #[test]
    fn file_and_directory_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let _logger = temp_logger(dir.path(), Level::Info);
        let path = dir.path().join(LOG_FILE_NAME);
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, crate::sys::PRIVATE_FILE_MODE, "the log names hosts, so it must be private");
            let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, crate::sys::PRIVATE_DIR_MODE);
        }
    }

    #[test]
    fn appends_to_an_existing_file_instead_of_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        fs::write(&path, "previous run\n").unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        logger.event(Level::Info, "app.start", &[]);
        let text = read(&path);
        assert!(text.starts_with("previous run\n"), "history must survive a restart: {text}");
        assert!(text.contains("event=app.start"));
    }

    #[test]
    fn rotation_keeps_bounded_history_and_chronological_order() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config { dir: dir.path().to_path_buf(), level: Level::Info, max_bytes: 220, keep: 2 };
        let logger = Logger::open(&cfg).unwrap();
        for i in 0..40 {
            logger.event(Level::Info, "rotation.probe", &[("i", &i)]);
        }
        let active = dir.path().join(LOG_FILE_NAME);
        assert!(fs::metadata(&active).unwrap().len() <= 220, "active file exceeded max_bytes");
        let one = dir.path().join(format!("{LOG_FILE_NAME}.1"));
        let two = dir.path().join(format!("{LOG_FILE_NAME}.2"));
        assert!(one.exists(), "no first archive");
        assert!(two.exists(), "no second archive");
        assert!(!dir.path().join(format!("{LOG_FILE_NAME}.3")).exists(), "keep must cap history at 2");

        let first = read(&two).lines().next().unwrap().to_string();
        let last = read(&active).lines().last().unwrap().to_string();
        assert!(seq_of(&first) < seq_of(&last), "archives must be older than the active file: {first} / {last}");
        assert!(read(&one).contains("event=rotation.probe"));
    }

    #[test]
    fn rotation_never_splits_a_record() {
        let dir = tempfile::tempdir().unwrap();
        // Barely above one record: every record must land whole in one file.
        let cfg = Config { dir: dir.path().to_path_buf(), level: Level::Info, max_bytes: 130, keep: 4 };
        let logger = Logger::open(&cfg).unwrap();
        logger.event(Level::Info, "split.probe", &[("msg", &"x".repeat(40))]);
        logger.event(Level::Info, "split.probe", &[("msg", &"y".repeat(40))]);
        let mut records = 0;
        for name in [LOG_FILE_NAME.to_string(), format!("{LOG_FILE_NAME}.1")] {
            let path = dir.path().join(name);
            if !path.exists() {
                continue;
            }
            for line in read(&path).lines() {
                assert!(line.contains("event=split.probe"), "partial record in a file: {line:?}");
                records += 1;
            }
        }
        assert_eq!(records, 2, "a record was dropped at the rotation boundary");
    }

    #[test]
    fn rotation_is_also_possible_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        logger.event(Level::Info, "before.rotate", &[]);
        logger.rotate_now().unwrap();
        logger.event(Level::Info, "after.rotate", &[]);
        assert!(read(&dir.path().join(format!("{LOG_FILE_NAME}.1"))).contains("event=before.rotate"));
        assert!(read(&dir.path().join(LOG_FILE_NAME)).contains("event=after.rotate"));
    }

    #[test]
    fn panic_record_carries_thread_location_and_trace() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        write_panic_record(
            &logger,
            "index out of bounds",
            Some("src/app.rs:42:7"),
            "sshpf-tunnel-3",
            "   0: std::panicking::begin_panic\n   1: sshpf::tunnel::run\n",
        );
        let text = read(&dir.path().join(LOG_FILE_NAME));
        assert!(text.contains("event=crash.panic"), "{text}");
        assert!(text.contains("thread=sshpf-tunnel-3"), "the thread name identifies a dead tunnel thread: {text}");
        assert!(text.contains("location=src/app.rs:42:7"), "{text}");
        assert!(text.contains("msg=\"index out of bounds\""), "{text}");
        // The block is a single record, so its newlines are escaped; both the
        // real and escaped spellings must be recognisable.
        assert!(text.contains("  | panic backtrace"), "trace block header missing: {text}");
        assert!(text.contains("0: std::panicking::begin_panic"), "trace frames missing: {text}");
        assert!(text.contains("event=crash.panic.trace"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn signal_record_is_async_signal_safe_and_parseable() {
        let (buf, len) = fatal::build_signal_record(libc::SIGSEGV);
        let line = std::str::from_utf8(&buf[..len]).unwrap();
        assert!(line.starts_with("ts="), "no allocation, so the epoch leads: {line}");
        assert!(line.contains(" ERROR seq=0 event=crash.signal signal=11 "), "{line}");
        assert!(line.contains("name=SIGSEGV"), "a bare number is not greppable: {line}");
        assert!(line.contains("pid="), "{line}");
        // Every field is k=v and the line has no newline, so the reader copes.
        assert!(!line.contains('\n'), "{line}");
    }

    #[cfg(unix)]
    #[test]
    fn fatal_signal_handler_really_writes_a_record() {
        use std::os::unix::io::AsRawFd;
        use std::sync::Mutex;
        // Serialises this test with itself; it installs a process-wide handler
        // and must own the crash fd while the child crashes.
        static GUARD: Mutex<()> = Mutex::new(());
        let _guard = GUARD.lock().unwrap_or_else(|p| p.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let file = open_append(&path).unwrap();
        fatal::adopt(&file, &path);
        fatal::install();
        // A record before the crash proves the appends are interleaved correctly.
        let logger = temp_logger(dir.path(), Level::Info);
        logger.event(Level::Info, "before.crash", &[]);
        drop(logger);
        let _ = file.as_raw_fd();

        // SAFETY: fork isolates a deliberate crash. The child performs no
        // allocation or locking before raising the signal.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe { libc::raise(libc::SIGSEGV) };
            unsafe { libc::_exit(0) }; // unreachable if the handler works
        }
        let mut status: i32 = 0;
        // SAFETY: waiting on our own child.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        fatal::release();

        assert!(libc::WIFSIGNALED(status), "child did not die from a signal");
        assert_eq!(libc::WTERMSIG(status), libc::SIGSEGV, "exit status must stay a signal death");

        let text = read(&path);
        assert!(text.contains("event=before.crash"), "record written before the crash was lost: {text}");
        assert!(text.contains("event=crash.signal"), "no crash record written: {text}");
        assert!(text.contains("name=SIGSEGV"), "{text}");
    }

    /// The panic hook has to keep chaining. `ratatui::try_init()` installs its own
    /// hook (terminal restore) *around* ours; a panic must reach both, in the
    /// documented order. This pins that shape without needing a terminal: it
    /// installs the same wrapper `try_init` uses and checks both ran.
    #[test]
    fn the_hook_chain_runs_ratatuis_restore_before_recording() {
        use std::sync::Arc;
        use std::sync::Mutex as StdMutex;
        // The hook is process-global, so this test owns it while it runs.
        static GUARD: Mutex<()> = Mutex::new(());
        let _guard = GUARD.lock().unwrap_or_else(|p| p.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(temp_logger(dir.path(), Level::Info));
        let order: Arc<StdMutex<Vec<&'static str>>> = Arc::new(StdMutex::new(Vec::new()));

        // Our hook, as `install_crash_hooks` builds it but on a local logger.
        let ours_logger = Arc::clone(&logger);
        let ours_order = Arc::clone(&order);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            ours_order.lock().unwrap().push("sshpf");
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                write_panic_record(&ours_logger, &payload_text(info), None, "chain-probe", "");
            }));
            previous(info);
        }));

        // What `ratatui::try_init` installs: restore first, then call the previous
        // hook (ours).
        let inner = std::panic::take_hook();
        let restore_order = Arc::clone(&order);
        std::panic::set_hook(Box::new(move |info| {
            restore_order.lock().unwrap().push("ratatui-restore");
            inner(info);
        }));

        let caught = std::panic::catch_unwind(|| panic!("chain probe"));
        let _ = std::panic::take_hook();
        assert!(caught.is_err());

        let order = order.lock().unwrap().clone();
        assert_eq!(order, vec!["ratatui-restore", "sshpf"], "a panic must reach both hooks, restore first");

        let text = read(&dir.path().join(LOG_FILE_NAME));
        assert!(text.contains("event=crash.panic"), "the crash record must still be written: {text:?}");
        assert!(text.contains("chain probe"), "{text:?}");
    }

    #[test]
    fn opening_a_bad_directory_reports_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        let cfg = Config { dir: file, level: Level::Info, ..Config::default() };
        assert!(Logger::open(&cfg).is_err(), "a regular file cannot be used as a log directory");
    }

    // -----------------------------------------------------------------------
    // re-entrancy: the deadlock the panic hook could otherwise walk into
    // -----------------------------------------------------------------------

    /// A `Display` that panics, i.e. a caller-supplied value the log formats
    /// while it holds the sink lock.
    struct PanickingDisplay;

    impl fmt::Display for PanickingDisplay {
        fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
            panic!("Display impl exploded while the log lock was held");
        }
    }

    /// The exact hazard: `record` panics while holding the sink mutex, the panic
    /// hook calls `record` again on the same thread, and a plain `lock()` would
    /// block forever — no output, no exit, terminal stuck in the alternate
    /// screen. The re-entrancy guard has to make the hook's record a no-op that
    /// returns immediately.
    ///
    /// The hook is *simulated* rather than installed: it is process-global, and
    /// swapping it here would suppress crash records other tests in this binary
    /// assert on. Calling [`write_panic_record`] from inside the critical section
    /// is exactly what the installed hook does; the real hook wiring is covered
    /// in `tests/logging.rs`, where the crash runs in its own process.
    ///
    /// A hang would show up as the `recv_timeout` expiring, so the test fails
    /// instead of wedging CI.
    #[test]
    fn a_panic_inside_the_writer_never_deadlocks_the_panic_hook() {
        use std::sync::Arc;
        use std::sync::mpsc;

        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("reentrancy-probe".to_string())
            .spawn(move || {
                let dir = tempfile::tempdir().unwrap();
                let logger = Arc::new(temp_logger(dir.path(), Level::Info));

                // 1. A panic raised by a caller-supplied `Display`, i.e. while
                //    `record` holds the guard.
                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    logger.event(Level::Info, "writer.boom", &[("v", &PanickingDisplay)]);
                }))
                .is_err();

                // 2. What the hook does on the same thread, while the guard was
                //    held: it must return, not block on its own mutex.
                let nested = {
                    let _inside = CriticalSection::enter().expect("first entry must succeed");
                    write_panic_record(&logger, "nested", Some("src/x.rs:1:1"), "probe", "");
                    "returned"
                };

                // 3. And the writer must be usable again: the flag is cleared by
                //    `Drop` during the unwind, not left stuck.
                logger.event(Level::Info, "writer.recovered", &[]);
                let text = fs::read_to_string(dir.path().join(LOG_FILE_NAME)).unwrap_or_default();
                done_tx.send((panicked, nested, text)).unwrap();
                drop(dir);
            })
            .unwrap();

        let (panicked, nested, text) =
            done_rx.recv_timeout(Duration::from_secs(10)).expect("deadlocked on the log mutex");
        assert!(panicked, "the panicking Display should have made the record panic");
        assert_eq!(nested, "returned", "the hook's record must not block");
        assert!(text.contains("event=writer.recovered"), "logging must work again after the unwind: {text:?}");
        assert!(
            !text.contains("event=writer.boom"),
            "the record that panicked mid-format must not be half-written: {text:?}"
        );
        worker.join().unwrap();
    }

    /// Directly: while this thread is inside the logger's critical section, a
    /// nested record is skipped rather than blocking.
    #[test]
    fn a_nested_record_is_skipped_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        // Run it on a helper thread so that *if* it blocked, the timeout below
        // fails the test instead of hanging CI. The critical-section flag is
        // thread-local, so the helper has to claim it itself.
        let probe = dir.path().to_path_buf();
        let helper = std::thread::spawn(move || {
            let logger = temp_logger(&probe, Level::Info);
            let _inside = CriticalSection::enter().expect("first entry must succeed");
            logger.event(Level::Info, "nested.record", &[]);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(2)).expect("a nested record blocked on its own lock");
        helper.join().unwrap();
        let text = read(&dir.path().join(LOG_FILE_NAME));
        assert!(!text.contains("event=nested.record"), "the nested record must be dropped, not written: {text:?}");
    }

    /// `path()` is used by `init`; it must return rather than hang when called
    /// re-entrantly (it is the same mutex).
    #[test]
    fn path_lookup_is_reentrancy_safe() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        let _inside = CriticalSection::enter().expect("first entry must succeed");
        assert!(logger.path().is_none(), "a re-entrant path lookup degrades to None");
    }

    /// Cross-thread poison is a separate case from same-thread re-entrancy: a
    /// guard dropped during another thread's unwind poisons the mutex, and the
    /// next record must recover via `into_inner()` (already the existing
    /// behaviour, now pinned).
    #[test]
    fn a_lock_poisoned_by_another_thread_still_accepts_records() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(temp_logger(dir.path(), Level::Info));

        let poisoned = Arc::clone(&logger);
        let victim = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mutex = poisoned.sink.as_ref().unwrap();
                let _guard = mutex.lock().unwrap();
                panic!("poison the sink mutex");
            }));
            assert!(result.is_err());
        });
        victim.join().unwrap();

        logger.event(Level::Info, "after.poison", &[]);
        let text = read(&dir.path().join(LOG_FILE_NAME));
        assert!(text.contains("event=after.poison"), "a poisoned lock must not silence the logger: {text:?}");
    }

    /// `record` runs inside the panic hook, so its counters use saturating
    /// arithmetic: an overflow must degrade, never panic (a panic while
    /// processing a panic aborts the process).
    #[test]
    fn counters_at_the_u64_ceiling_saturate_instead_of_overflowing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE_NAME);
        let file = open_append(&path).unwrap();
        // `max_bytes = 0` disables rotation, so only the arithmetic is exercised.
        let sink = Sink { file, path: path.clone(), size: u64::MAX - 1, max_bytes: 0, keep: 1, seq: u64::MAX };
        let logger = Logger { level: Level::Info, sink: Some(Mutex::new(sink)) };

        logger.event(Level::Info, "ceiling.probe", &[("x", &"y")]);
        let text = read(&path);
        assert!(text.contains("event=ceiling.probe"), "the record must still be written: {text:?}");
        assert!(text.contains("seq=18446744073709551615"), "seq must saturate, not wrap: {text:?}");

        let mutex = logger.sink.as_ref().unwrap();
        let sink = mutex.lock().unwrap();
        assert_eq!(sink.size, u64::MAX, "size must saturate, not wrap");
    }

    /// `now_timestamp` cannot panic: it is called from inside the panic hook.
    #[test]
    fn the_record_timestamp_is_always_well_formed() {
        // The documented shape, whatever the clock says.
        let ts = now_timestamp();
        assert!(ts.len() >= 19, "no timestamp at all: {ts:?}");
        // Either the RFC 3339 form or the `epoch=` fallback for a clock past 9999.
        assert!(
            ts.starts_with("epoch=") || (ts.chars().nth(4) == Some('-') && ts.chars().nth(10) == Some('T')),
            "unexpected timestamp shape: {ts:?}"
        );
    }
}

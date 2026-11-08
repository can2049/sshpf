//! The file sink: the logger that actually writes records, its rotation, and the
//! re-entrancy guard that keeps it from deadlocking the panic hook.
//!
//! # Why the guard exists
//!
//! `std::sync::Mutex` is not reentrant. Re-locking it from the thread that already
//! holds it does not return `Err(poisoned)` — it blocks forever. The way to reach
//! that from here is the panic hook: a panic raised inside [`Logger::record`] (a
//! caller's `Display` impl, the timezone lookup, the I/O) makes the hook call
//! `record` again on the same thread, which would then wait on a mutex that can
//! only be released by the panicking frame. The process would sit there with the
//! terminal in the alternate screen and not one byte in the log.
//!
//! [`CriticalSection`] is the thread-local flag that makes that call degrade
//! instead of hanging.
//!
//! # Rotation
//!
//! One active file `sshpf.log`, archives `sshpf.log.1` … `sshpf.log.<keep>`. A
//! record is rotated *before* it is appended, so no record ever straddles two
//! files (a half-record at a boundary is unparseable). The crash descriptor is a
//! `dup` of the file about to be renamed, so rotation releases it first and the
//! caller re-adopts right after — see [`super::crash`].

use std::cell::Cell;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::crash;
use super::format::{now_timestamp, push_value, token};
use super::level::Config;
use super::{GLOBAL, LOG_FILE_NAME, Level};

/// The open file plus the bookkeeping that decides when to rotate.
pub(super) struct Sink {
    pub(super) file: File,
    pub(super) path: PathBuf,
    pub(super) size: u64,
    pub(super) max_bytes: u64,
    pub(super) keep: usize,
    pub(super) seq: u64,
}

/// A log destination. Cheap to check and safe to call from any thread.
pub struct Logger {
    level: Level,
    pub(super) sink: Option<Mutex<Sink>>,
}

thread_local! {
    /// `true` while *this thread* is inside the logger's critical section.
    static IN_CRITICAL_SECTION: Cell<bool> = const { Cell::new(false) };
}

/// Marks the logger's critical section for as long as it is alive.
///
/// Cleared in `Drop`, so a panic unwinding out of `record` cannot leave the flag
/// stuck. (`panic = "abort"` skips the `Drop`, but it also kills the process,
/// which makes a stuck flag irrelevant.)
pub(super) struct CriticalSection;

impl CriticalSection {
    /// `None` when this thread is already inside the logger: the caller must
    /// degrade instead of locking.
    pub(super) fn enter() -> Option<CriticalSection> {
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
    crash::emergency(event);
}

impl Logger {
    /// A logger that writes nothing. Used before [`super::init`] and by tests.
    ///
    /// `const` so the disabled fallback can be a `static` rather than another
    /// `OnceLock`.
    pub const fn disabled() -> Logger {
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

    /// Render one record. Pure, so it can be checked without a sink.
    ///
    /// Built *before* the lock (see [`Logger::record`]) so that a panic in a
    /// caller's `Display` — or in the timezone lookup — cannot happen while the
    /// sink is held.
    fn line(
        &self,
        timestamp: &str,
        level: Level,
        event: &str,
        fields: &[(&str, &dyn fmt::Display)],
        seq: u64,
    ) -> String {
        let mut line = String::with_capacity(192);
        let _ = write!(line, "{} {:<5} seq={seq} event={}", timestamp, level.as_str(), token(event));
        for (key, value) in fields {
            line.push(' ');
            line.push_str(&token(key));
            line.push('=');
            push_value(&mut line, &value.to_string());
        }
        line.push('\n');
        line
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
            // `saturating_*` rather than `+`: this code runs in the panic hook, so
            // "the counter cannot realistically overflow" is not good enough — a
            // panic here would abort instead of leaving the crash record.
            let seq = guard.seq.saturating_add(1);
            let line = self.line(&timestamp, level, event, fields, seq);
            self.append(&mut guard, &line, seq)
            // `guard` and `_critical` are both dropped here, so the rewire below
            // takes a fresh critical section instead of being mistaken for a
            // re-entrant call.
        };
        if rotated && self.is_global() {
            self.rewire_crash_fd();
        }
    }

    /// Append `line` to the sink, rotating first when it would cross `max_bytes`.
    ///
    /// Returns whether a rotation happened. Rotation happens *before* the append,
    /// so a record never straddles two files (a half-record at a rotation
    /// boundary is unparseable).
    fn append(&self, sink: &mut Sink, line: &str, seq: u64) -> bool {
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
    pub(super) fn rewire_crash_fd(&self) {
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
        crash::adopt(&guard.file, &guard.path);
    }
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

/// Open for append, creating the file `0600` when it does not exist yet.
pub(super) fn open_append(path: &Path) -> io::Result<File> {
    crate::sys::open_private_append(path)
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
        crash::release();
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

#[cfg(test)]
pub(super) mod testing {
    //! Shared helpers for the sibling test modules: a logger at a tempdir and a
    //! reader for the file it writes.

    use super::{Config, Level, Logger};
    use crate::logging::LOG_FILE_NAME;
    use std::path::Path;

    /// A logger rooted at `dir`, at the given level.
    pub(crate) fn temp_logger(dir: &Path, level: Level) -> Logger {
        Logger::open(&Config { dir: dir.to_path_buf(), level, ..Config::default() })
            .expect("a tempdir must be usable as a log directory")
    }

    /// Contents of `dir/sshpf.log`, or `""` when it does not exist.
    pub(crate) fn read(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(LOG_FILE_NAME)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{read, temp_logger};
    use super::*;
    use std::time::Duration;

    fn dir_text(dir: &Path) -> String {
        read(dir)
    }

    fn seq_of(line: &str) -> u32 {
        // The `seq=N` field position differs between normal and crash records, so
        // find the field rather than assume a column.
        line.split(' ').find_map(|f| f.strip_prefix("seq=")).and_then(|n| n.parse().ok()).unwrap_or(0)
    }

    #[test]
    fn record_has_the_documented_shape() {
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        logger.event(Level::Info, "tunnel.ready", &[("tunnel", &3), ("elapsed_ms", &812)]);
        let text = read(dir.path());
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
        let text = dir_text(dir.path());
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
        let text = dir_text(dir.path());
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
        let text = dir_text(dir.path());
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
        let text = dir_text(dir.path());
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
        let text = fs::read_to_string(&path).unwrap();
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

        let first = fs::read_to_string(&two).unwrap().lines().next().unwrap().to_string();
        let last = fs::read_to_string(&active).unwrap().lines().last().unwrap().to_string();
        assert!(seq_of(&first) < seq_of(&last), "archives must be older than the active file: {first} / {last}");
        assert!(fs::read_to_string(&one).unwrap().contains("event=rotation.probe"));
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
            for line in fs::read_to_string(&path).unwrap().lines() {
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
        let archive = dir.path().join(format!("{LOG_FILE_NAME}.1"));
        assert!(fs::read_to_string(&archive).unwrap().contains("event=before.rotate"));
        assert!(read(dir.path()).contains("event=after.rotate"));
    }

    /// The record text is built by [`Logger::line`], split out of `record` so the
    /// layout can be asserted without a sink (and without a lock). This pins the
    /// exact columns the parser depends on.
    #[test]
    fn the_line_builder_produces_the_documented_columns() {
        let logger = Logger::disabled();
        let line = logger.line("2026-11-08T18:20:01.123+08:00", Level::Info, "tunnel.ready", &[("tunnel", &3)], 42);
        assert_eq!(line, "2026-11-08T18:20:01.123+08:00 INFO  seq=42 event=tunnel.ready tunnel=3\n");
        // The level column is padded to 5 so records line up in a `less` view; the
        // trailing newline is what makes "one record per line" true.
        assert!(line.starts_with("2026-11-08T18:20:01.123+08:00 INFO  seq=42 "), "{line:?}");
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
        // No fields at all still yields a complete record.
        let bare = logger.line("ts", Level::Error, "e", &[], 1);
        assert_eq!(bare, "ts ERROR seq=1 event=e\n");
    }

    #[test]
    fn opening_a_bad_directory_reports_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, "x").unwrap();
        let cfg = Config { dir: file, level: Level::Info, ..Config::default() };
        assert!(Logger::open(&cfg).is_err(), "a regular file cannot be used as a log directory");
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
        let text = read(dir.path());
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
        let text = read(dir.path());
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
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("event=ceiling.probe"), "the record must still be written: {text:?}");
        assert!(text.contains("seq=18446744073709551615"), "seq must saturate, not wrap: {text:?}");

        let mutex = logger.sink.as_ref().unwrap();
        let sink = mutex.lock().unwrap();
        assert_eq!(sink.size, u64::MAX, "size must saturate, not wrap");
    }
}

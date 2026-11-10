//! Unit tests for `sink.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

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

//! End-to-end tests for the diagnostic log: the file the user is asked to attach
//! to a bug report.
//!
//! These run the **real binary** (`CARGO_BIN_EXE_sshpf`) rather than calling the
//! library, because the point is the whole chain: process start-up → logger
//! install → record on disk. They also deliberately crash a child process, which
//! is the only way to prove that the crash path actually writes anything.
//!
//! The environment is per-process (`SSHPF_LOG_DIR`), so each test gets its own
//! temp dir instead of sharing global state.

use std::path::{Path, PathBuf};
use std::process::Command;

use sshpf::logging;

/// Env var that turns this test binary into a deliberate crasher when re-executed.
const CRASH_ENV: &str = "SSHPF_TEST_CRASH_CHILD";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sshpf")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Field of a record, e.g. `event=` or `version=`.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    line.split(' ').find_map(|f| f.strip_prefix(&needle))
}

#[test]
fn check_mode_writes_a_start_record_with_version_and_paths() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let out = Command::new(bin())
        .env("SSHPF_LOG_DIR", dir.path())
        .env("SSHPF_CONFIG", &config)
        .arg("--check")
        .output()
        .unwrap();
    assert!(out.status.success(), "--check failed: {out:?}");

    let log = read(&dir.path().join("sshpf.log"));
    let start = log.lines().find(|l| l.contains("event=app.start")).unwrap_or_else(|| panic!("no app.start: {log}"));
    assert_eq!(field(start, "version"), Some(env!("CARGO_PKG_VERSION")), "{start}");
    assert!(middle_of(start, "pid").is_some(), "pid missing: {start}");
    assert!(middle_of(start, "config").is_some_and(|c| c.ends_with("config.toml")), "{start}");
    assert!(middle_of(start, "log").is_some_and(|l| l.ends_with("sshpf.log")), "{start}");
}

/// Values may be quoted, so find `key=` and stop at whitespace.
fn middle_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(' ').find_map(|f| f.strip_prefix(&format!("{key}=")))
}

#[test]
fn check_mode_records_a_broken_config_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    // `version = 999` is above what this build supports: a hard load error.
    std::fs::write(&config, "version = 999\n").unwrap();

    let out = Command::new(bin())
        .env("SSHPF_LOG_DIR", dir.path())
        .env("SSHPF_CONFIG", &config)
        .arg("--check")
        .output()
        .unwrap();
    assert!(!out.status.success(), "a broken config must make --check fail");

    let log = read(&dir.path().join("sshpf.log"));
    let bad = log.lines().find(|l| l.contains("event=config.invalid")).unwrap_or_else(|| panic!("no record: {log}"));
    assert!(bad.contains(" ERROR "), "a broken config is an error, not a note: {bad}");
    assert!(bad.contains("版本"), "the reason must be in the record: {bad}");
}

#[test]
fn bad_arguments_are_recorded_before_exit() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .env("SSHPF_LOG_DIR", dir.path())
        .env("SSHPF_CONFIG", dir.path().join("config.toml"))
        .arg("--definitely-not-a-flag")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "usage errors exit 2");

    let log = read(&dir.path().join("sshpf.log"));
    assert!(log.contains("event=cli.bad_args"), "unexpected log: {log}");
    assert!(log.contains("ERROR"), "{log}");
}

#[test]
fn log_level_env_var_filters_records() {
    let dir = tempfile::tempdir().unwrap();
    // `error` must drop the `app.start` info record but keep the failure.
    let out = Command::new(bin())
        .env("SSHPF_LOG_DIR", dir.path())
        .env("SSHPF_LOG_LEVEL", "error")
        .env("SSHPF_CONFIG", dir.path().join("config.toml"))
        .arg("--definitely-not-a-flag")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    let log = read(&dir.path().join("sshpf.log"));
    assert!(!log.contains("event=app.start"), "info must be filtered at level=error: {log}");
    assert!(log.contains("event=cli.bad_args"), "errors must survive: {log}");
}

#[test]
fn an_invalid_log_level_falls_back_to_info_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    Command::new(bin())
        .env("SSHPF_LOG_DIR", dir.path())
        .env("SSHPF_LOG_LEVEL", "verbose")
        .env("SSHPF_CONFIG", dir.path().join("config.toml"))
        .arg("--check")
        .output()
        .unwrap();

    let log = read(&dir.path().join("sshpf.log"));
    assert!(log.contains("event=log.level_invalid"), "a typo must not be silent: {log}");
    // The fallback is info, so the normal start record is there too.
    assert!(log.contains("event=app.start"), "{log}");
}

#[test]
fn log_is_append_only_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        Command::new(bin())
            .env("SSHPF_LOG_DIR", dir.path())
            .env("SSHPF_CONFIG", dir.path().join("config.toml"))
            .arg("--check")
            .output()
            .unwrap();
    }
    let log = read(&dir.path().join("sshpf.log"));
    assert_eq!(log.matches("event=app.start").count(), 2, "each run appends: {log}");
}

#[cfg(unix)]
#[test]
fn file_permissions_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    Command::new(bin())
        .env("SSHPF_LOG_DIR", dir.path())
        .env("SSHPF_CONFIG", dir.path().join("config.toml"))
        .arg("--check")
        .output()
        .unwrap();

    let mode = std::fs::metadata(dir.path().join("sshpf.log")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "the log names hosts and must be private");
    let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
    assert_eq!(dir_mode & 0o777, 0o700, "log directory must be private");
}

#[test]
fn unwritable_log_dir_does_not_break_the_program() {
    let dir = tempfile::tempdir().unwrap();
    // A *file* where the log directory should be: create_dir_all fails.
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, "not a dir").unwrap();

    let out = Command::new(bin())
        .env("SSHPF_LOG_DIR", &blocked)
        .env("SSHPF_CONFIG", dir.path().join("config.toml"))
        .arg("--check")
        .output()
        .unwrap();
    assert!(out.status.success(), "logging must never be fatal: {out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("校验通过"));
}

// ---------------------------------------------------------------------------
// crash capture
// ---------------------------------------------------------------------------

/// Re-executed as a child: initialise logging, install the hooks, then crash.
///
/// Kept as a real test so `cargo test` reports it, but it only does the crashing
/// when the marker env var is set — otherwise it is a no-op.
#[test]
fn crash_child_helper() {
    if std::env::var_os(CRASH_ENV).is_none() {
        return;
    }
    let dir = PathBuf::from(std::env::var("SSHPF_LOG_DIR").unwrap());
    let kind = std::env::var("SSHPF_TEST_CRASH_KIND").unwrap_or_default();
    let _ = logging::init(logging::Config { dir, ..logging::Config::default() });
    logging::install_crash_hooks();
    match kind.as_str() {
        #[cfg(unix)]
        "signal" => {
            logging::info("test.before_crash", &[]);
            // SAFETY: deliberately crashing this short-lived child.
            unsafe { libc::raise(libc::SIGSEGV) };
            std::process::exit(0); // unreachable if the handler works
        }
        _ => {
            logging::info("test.before_crash", &[]);
            panic!("deliberate panic from a tunnel-like thread");
        }
    }
}

/// Spawn the crashing child and return its (exit code, log contents).
fn run_crashing_child(kind: &str) -> (Option<i32>, String) {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["crash_child_helper", "--exact", "--nocapture", "--test-threads=1"])
        .env(CRASH_ENV, "1")
        .env("SSHPF_TEST_CRASH_KIND", kind)
        .env("SSHPF_LOG_DIR", dir.path())
        .output()
        .unwrap();
    let log = read(&dir.path().join("sshpf.log"));
    (out.status.code(), log)
}

#[test]
fn a_panic_is_written_with_message_location_and_thread() {
    let (code, log) = run_crashing_child("panic");
    assert_ne!(code, Some(0), "the child must die");
    assert!(log.contains("event=test.before_crash"), "records before the panic survive: {log}");

    let line =
        log.lines().find(|l| l.contains("event=crash.panic")).unwrap_or_else(|| panic!("no panic record: {log}"));
    assert!(line.contains(" ERROR "), "a panic is an error: {line}");
    assert!(line.contains("deliberate panic"), "the panic message is in the record: {line}");
    assert!(line.contains("location="), "the source location is in the record: {line}");
    assert!(line.contains("thread="), "the thread identifies which component died: {line}");
    // A panic inside a tunnel thread must be distinguishable from the UI thread.
    assert!(line.contains("thread=crash_child_helper") || line.contains("thread=<unnamed>"), "{line}");
}

#[cfg(unix)]
#[test]
fn a_fatal_signal_is_written_and_still_kills_the_process() {
    let (_code, log) = run_crashing_child("signal");
    assert!(log.contains("event=test.before_crash"), "records before the crash survive: {log}");
    assert!(log.contains("event=crash.signal"), "a segfault must leave a record: {log}");
    assert!(log.contains("name=SIGSEGV"), "the signal name is in the record: {log}");
    // The record is written before the process dies, and it is a single line.
    let crash: Vec<&str> = log.lines().filter(|l| l.contains("event=crash.signal")).collect();
    assert_eq!(crash.len(), 1, "exactly one crash record: {log}");
}

#[cfg(unix)]
#[test]
fn a_fatal_signal_keeps_the_signal_exit_status() {
    // The re-raise must preserve "killed by SIGSEGV", not turn it into exit 139.
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["crash_child_helper", "--exact", "--nocapture", "--test-threads=1"])
        .env(CRASH_ENV, "1")
        .env("SSHPF_TEST_CRASH_KIND", "signal")
        .env("SSHPF_LOG_DIR", dir.path())
        .output()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(out.status.signal(), Some(libc::SIGSEGV), "the process must still die from SIGSEGV");
}

#[test]
fn a_failed_log_dir_is_reported_not_swallowed() {
    // The library must tell the caller, so the TUI can surface it in the status
    // line instead of leaving the user with an empty file and no explanation.
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("blocked");
    std::fs::write(&blocked, "not a dir").unwrap();
    let err = logging::init(logging::Config { dir: blocked, ..logging::Config::default() }).unwrap_err();
    assert!(err.contains("打开日志文件失败"), "{err}");
    assert_eq!(logging::init_error(), Some(err.as_str()), "the reason must be retrievable later");
}

#[test]
fn records_from_different_threads_are_not_interleaved() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logging::open_at(dir.path(), logging::Level::Debug).unwrap();
    std::thread::scope(|scope| {
        for thread in 0..8 {
            let logger = &logger;
            scope.spawn(move || {
                for i in 0..50 {
                    logger.event(logging::Level::Info, "thread.probe", &[("thread", &thread), ("i", &i)]);
                }
            });
        }
    });
    let log = read(&dir.path().join("sshpf.log"));
    assert_eq!(log.lines().count(), 400, "one record per call");
    for line in log.lines() {
        // Every line is complete: a torn write would lose the event field or the
        // trailing newline (which would merge two records into one).
        assert!(line.ends_with("i=49") || line.contains(" i="), "malformed record: {line:?}");
        assert!(line.contains("event=thread.probe"), "malformed record: {line:?}");
        assert!(line.contains("thread="), "malformed record: {line:?}");
    }
}

#[test]
fn no_record_contains_a_newline_so_the_file_stays_parseable() {
    let dir = tempfile::tempdir().unwrap();
    let logger = logging::open_at(dir.path(), logging::Level::Debug).unwrap();
    logger.event(logging::Level::Error, "test.multiline", &[("msg", &"line1\nline2\r\nline3\tend")]);
    let log = read(&dir.path().join("sshpf.log"));
    assert_eq!(log.lines().count(), 1, "one record, escaped: {log}");
    assert!(log.contains("line1\\nline2\\r\\nline3\\tend"), "{log}");
}

//! Unit tests for `crash.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(all(test, unix))]

// The crate-level re-exports the tests exercise, imported explicitly now that
// the glob is gone.
use super::imp::{LOG_FD, RECORD_MAX, build_signal_record, push, push_int, signal_name};
use super::{adopt, emergency, install, release};
use std::sync::atomic::Ordering;

/// The crash descriptor is one process-wide slot, so every test that adopts,
/// releases or inspects it must hold this for its duration. A per-test
/// `static` would not serialise anything — this is the shared one.
static FD_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialise a test against the other crash-fd tests.
fn lock_fd() -> std::sync::MutexGuard<'static, ()> {
    FD_GUARD.lock().unwrap_or_else(|p| p.into_inner())
}

// -----------------------------------------------------------------------
// signal_name: the `&'static str` the handler uses (no allocation allowed)
// -----------------------------------------------------------------------

#[test]
fn every_fatal_signal_has_a_stable_name() {
    // These five are exactly the signals `install()` catches; a handler that
    // forgets one writes `name=SIGUNKNOWN` and makes the record hard to grep.
    assert_eq!(signal_name(libc::SIGSEGV), "SIGSEGV");
    assert_eq!(signal_name(libc::SIGBUS), "SIGBUS");
    assert_eq!(signal_name(libc::SIGABRT), "SIGABRT");
    assert_eq!(signal_name(libc::SIGILL), "SIGILL");
    assert_eq!(signal_name(libc::SIGFPE), "SIGFPE");
}

#[test]
fn unknown_and_out_of_range_signals_fall_back() {
    // `signal_name` is total: it runs inside a handler, where a panic (from a
    // slice index or an `unwrap`) would abort the process mid-crash.
    for sig in [0, 1, 9, 15, 64, -1, i32::MIN, i32::MAX] {
        assert_eq!(signal_name(sig), "SIGUNKNOWN", "sig {sig} must not panic or invent a name");
    }
}

// -----------------------------------------------------------------------
// push / push_int: hand-rolled formatting, so the arithmetic is the risk
// -----------------------------------------------------------------------

#[test]
fn push_appends_and_advances() {
    let mut buf = [0u8; 16];
    let at = push(&mut buf, 0, b"ab");
    assert_eq!(at, 2);
    let at = push(&mut buf, at, b"cd");
    assert_eq!(at, 4);
    assert_eq!(&buf[..4], b"abcd");
}

#[test]
fn push_truncates_instead_of_overrunning() {
    // A short buffer must produce a short record, never a write past the end
    // (that would be memory corruption inside the crash handler).
    let mut buf = [0u8; 4];
    let at = push(&mut buf, 0, b"abcdef");
    assert_eq!(at, 4, "a full buffer reports its length");
    assert_eq!(&buf, b"abcd");

    // Once full, further appends are no-ops and still report the length.
    assert_eq!(push(&mut buf, at, b"x"), 4);
    assert_eq!(&buf, b"abcd");
}

#[test]
fn a_buffer_filled_exactly_to_its_length_is_accepted() {
    // The boundary between "fits" and "truncates": `end == buf.len()` must be
    // a successful copy, and only the *next* byte may be dropped.
    let mut buf = [0u8; 4];
    let at = push(&mut buf, 0, b"abcd");
    assert_eq!(at, 4);
    assert_eq!(&buf, b"abcd");
    // One byte more does not fit and must not be written over `d`.
    assert_eq!(push(&mut buf, at, b"e"), 4);
    assert_eq!(&buf, b"abcd");

    // Same boundary for the integer formatter: 3 bytes is exactly "-12".
    let mut buf = [0u8; 3];
    let at = push_int(&mut buf, 0, -1234);
    assert_eq!(at, 3);
    assert_eq!(&buf, b"-12");
}

#[test]
fn push_into_a_zero_length_buffer_is_a_no_op() {
    let mut buf = [0u8; 0];
    assert_eq!(push(&mut buf, 0, b"anything"), 0);
    assert_eq!(push(&mut buf, 5, b"anything"), 0, "an out-of-range offset must not index");
}

#[test]
fn push_handles_an_offset_at_the_end() {
    let mut buf = [0u8; 4];
    assert_eq!(push(&mut buf, 4, b"x"), 4);
    assert_eq!(push(&mut buf, 99, b"x"), 4, "an offset past the end must not panic");
    assert_eq!(&buf, b"\0\0\0\0");
}

#[test]
fn push_int_renders_zero_positive_and_negative() {
    let mut buf = [0u8; 32];
    let at = push_int(&mut buf, 0, 0);
    assert_eq!(&buf[..at], b"0");

    let at = push_int(&mut buf, 0, 1);
    assert_eq!(&buf[..at], b"1");

    let at = push_int(&mut buf, 0, 42);
    assert_eq!(&buf[..at], b"42");

    let at = push_int(&mut buf, 0, -1);
    assert_eq!(&buf[..at], b"-1");

    let at = push_int(&mut buf, 0, -812);
    assert_eq!(&buf[..at], b"-812");

    let at = push_int(&mut buf, 0, i64::from(libc::SIGSEGV));
    assert_eq!(&buf[..at], b"11");
}

#[test]
fn push_int_handles_the_i64_extremes() {
    // `i64::MIN.unsigned_abs()` is `1 << 63`, which is the case a naive
    // `-value` implementation overflows on.
    let mut buf = [0u8; 32];
    let at = push_int(&mut buf, 0, i64::MIN);
    assert_eq!(&buf[..at], b"-9223372036854775808");

    let at = push_int(&mut buf, 0, i64::MAX);
    assert_eq!(&buf[..at], b"9223372036854775807");

    let at = push_int(&mut buf, 0, i64::from(i32::MIN));
    assert_eq!(&buf[..at], b"-2147483648");

    let at = push_int(&mut buf, 0, i64::from(i32::MAX));
    assert_eq!(&buf[..at], b"2147483647");
}

#[test]
fn push_int_truncates_a_small_buffer_safely() {
    // The sign goes first, so a truncated number is at least parseable-ish and
    // never wraps around.
    let mut buf = [0u8; 3];
    let at = push_int(&mut buf, 0, -12345);
    assert_eq!(at, 3);
    assert_eq!(&buf, b"-12");

    let mut buf = [0u8; 1];
    let at = push_int(&mut buf, 0, -7);
    assert_eq!(at, 1, "no space for the digits after the sign");
    assert_eq!(&buf, b"-");
}

#[test]
fn push_int_appends_after_existing_content() {
    let mut buf = [0u8; 16];
    let at = push(&mut buf, 0, b"seq=");
    let at = push_int(&mut buf, at, 123);
    assert_eq!(&buf[..at], b"seq=123");
}

// -----------------------------------------------------------------------
// build_signal_record: the record the handler actually writes
// -----------------------------------------------------------------------

#[test]
fn signal_record_is_async_signal_safe_and_parseable() {
    let (buf, len) = build_signal_record(libc::SIGSEGV);
    let line = std::str::from_utf8(&buf[..len]).unwrap();
    assert!(line.starts_with("ts="), "no allocation, so the epoch leads: {line}");
    assert!(line.contains(" ERROR seq=0 event=crash.signal signal=11 "), "{line}");
    assert!(line.contains("name=SIGSEGV"), "a bare number is not greppable: {line}");
    assert!(line.contains("pid="), "{line}");
    // Every field is k=v and the line has no newline, so the reader copes.
    assert!(!line.contains('\n'), "{line}");
    assert!(line.ends_with(|c: char| c.is_ascii_digit()), "pid must be the last field: {line}");
}

#[test]
fn every_fatal_signal_gets_its_own_record() {
    // The record must name the signal, otherwise the five handlers are
    // indistinguishable in the file.
    for (sig, name) in [
        (libc::SIGSEGV, "SIGSEGV"),
        (libc::SIGBUS, "SIGBUS"),
        (libc::SIGABRT, "SIGABRT"),
        (libc::SIGILL, "SIGILL"),
        (libc::SIGFPE, "SIGFPE"),
    ] {
        let (buf, len) = build_signal_record(sig);
        let line = std::str::from_utf8(&buf[..len]).unwrap();
        assert!(line.contains(&format!("signal={sig} ")), "{line}");
        assert!(line.contains(&format!("name={name}")), "{line}");
    }
}

#[test]
fn an_unknown_signal_still_produces_a_well_formed_record() {
    // A handler can fire for a signal we did not install (NSIG bounds, a
    // platform-specific number), and that record must still parse.
    let (buf, len) = build_signal_record(0);
    let line = std::str::from_utf8(&buf[..len]).unwrap();
    assert!(line.contains("event=crash.signal signal=0 "), "{line}");
    assert!(line.contains("name=SIGUNKNOWN"), "{line}");
    assert!(!line.contains('\n'), "{line}");
}

#[test]
fn the_record_length_is_always_within_the_buffer() {
    // The invariant the handler relies on before `&buf[..len]`: an out-of-range
    // `len` would panic inside a signal handler, i.e. abort.
    for sig in [i32::MIN, -1, 0, 9, 11, 15, 65, i32::MAX] {
        let (_buf, len) = build_signal_record(sig);
        assert!(len <= RECORD_MAX, "sig {sig} produced len={len} > {RECORD_MAX}");
    }
}

#[test]
fn an_extreme_signal_number_truncates_instead_of_overrunning() {
    // `i32::MIN` is the longest field value we can be handed; even then the
    // whole record has to fit, and the parts that fit must stay parseable.
    let (buf, len) = build_signal_record(i32::MIN);
    assert!(len <= RECORD_MAX);
    let line = std::str::from_utf8(&buf[..len]).unwrap();
    assert!(line.starts_with("ts="), "{line}");
    assert!(line.contains("event=crash.signal"), "{line}");
    assert!(line.contains("signal=-2147483648"), "{line}");
    // A truncated tail is acceptable, but truncation must be announced by the
    // field simply ending — never by garbage or a recycled earlier byte.
    assert!(line.contains("name="), "{line}");
}

// -----------------------------------------------------------------------
// adopt / release: the descriptor lifecycle rotation depends on
// -----------------------------------------------------------------------

#[test]
fn adopt_then_release_clears_the_crash_fd() {
    use std::os::unix::io::AsRawFd;
    let _guard = lock_fd();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sshpf.log");
    let file = std::fs::File::options().append(true).create(true).open(&path).unwrap();
    adopt(&file, &path);
    let adopted = LOG_FD.load(Ordering::Relaxed);
    assert!(adopted >= 0, "adopt must publish a descriptor");
    assert_ne!(adopted, file.as_raw_fd(), "the handler must own a dup, not the logger's fd");

    release();
    assert_eq!(LOG_FD.load(Ordering::Relaxed), -1, "release must clear the slot");

    // Releasing twice is safe: rotation calls it unconditionally.
    release();
    assert_eq!(LOG_FD.load(Ordering::Relaxed), -1);
}

#[test]
fn adopt_replaces_a_previous_descriptor_without_leaking_the_slot() {
    use std::os::unix::io::AsRawFd;
    let _guard = lock_fd();

    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.log");
    let second = dir.path().join("second.log");
    let f1 = std::fs::File::options().append(true).create(true).open(&first).unwrap();
    let f2 = std::fs::File::options().append(true).create(true).open(&second).unwrap();

    adopt(&f1, &first);
    let fd1 = LOG_FD.load(Ordering::Relaxed);
    adopt(&f2, &second);
    let fd2 = LOG_FD.load(Ordering::Relaxed);
    assert!(fd2 >= 0);
    assert_ne!(fd1, fd2, "the second adopt must take a fresh dup");
    assert_ne!(fd2, f2.as_raw_fd());
    release();
    assert_eq!(LOG_FD.load(Ordering::Relaxed), -1);
}

/// The emergency path (re-entrancy degrade) must reach the file through the
/// adopted descriptor without a lock. It is *not* called from a handler, so
/// allocation is fine — this asserts the bytes land where a reader looks.
#[test]
fn emergency_writes_the_degrade_record() {
    let _guard = lock_fd();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sshpf.log");
    let file = std::fs::File::options().append(true).create(true).open(&path).unwrap();
    adopt(&file, &path);
    emergency("crash.panic");
    release();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("event=crash.log_reentrant"), "{text}");
    assert!(text.contains("cause=crash.panic"), "{text}");
    assert!(text.contains("ts="), "the epoch is the only timestamp this path may use: {text}");
    assert_eq!(text.lines().count(), 1, "exactly one line: {text}");
}

/// With no adopted fd the emergency path must do nothing at all (a test-local
/// logger that never adopted one is the normal case).
#[test]
fn emergency_without_a_descriptor_is_a_no_op() {
    let _guard = lock_fd();
    release();
    emergency("crash.panic");
    assert_eq!(LOG_FD.load(Ordering::Relaxed), -1);
}

/// The end-to-end proof that the handler writes (and keeps the signal exit
/// status). This is the test the `unsafe` exists for; it must keep running on
/// a real fork.
#[test]
fn fatal_signal_handler_really_writes_a_record() {
    // `lock_fd` serialises this test with the other fd tests: it installs a
    // process-wide handler and must own the crash fd while the child crashes.
    let _guard = lock_fd();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sshpf.log");
    let file = super::super::sink::open_append(&path).unwrap();
    adopt(&file, &path);
    install();
    // A record before the crash proves the appends are interleaved correctly.
    let logger = super::super::sink::testing::temp_logger(dir.path(), super::super::Level::Info);
    logger.event(super::super::Level::Info, "before.crash", &[]);
    drop(logger);

    // SAFETY: fork isolates a deliberate crash. The child performs no
    // allocation or locking before raising the signal.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        // SAFETY: raising a fatal signal in the child is the whole point; the
        // handler then re-raises and the process dies.
        unsafe { libc::raise(libc::SIGSEGV) };
        // SAFETY: unreachable if the handler works; stops a runaway child.
        unsafe { libc::_exit(0) };
    }
    let mut status: i32 = 0;
    // SAFETY: waiting on our own child with a valid status pointer.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    release();

    assert!(libc::WIFSIGNALED(status), "child did not die from a signal");
    assert_eq!(libc::WTERMSIG(status), libc::SIGSEGV, "exit status must stay a signal death");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("event=before.crash"), "record written before the crash was lost: {text}");
    assert!(text.contains("event=crash.signal"), "no crash record written: {text}");
    assert!(text.contains("name=SIGSEGV"), "{text}");
}

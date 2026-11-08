//! Async-signal-safe crash writer for fatal signals (SIGSEGV/SIGBUS/SIGABRT/
//! SIGILL/SIGFPE) — and the **only** file in the crate allowed to contain
//! `unsafe`.
//!
//! # SAFETY CONTRACT
//!
//! This module exists to write one diagnostic line from inside a signal handler,
//! i.e. in a context where the C library makes almost nothing legal. The
//! contract below is what makes the `unsafe` here auditable; anything added here
//! must keep it.
//!
//! ## Why `unsafe` is needed at all
//!
//! Rust has no safe wrapper for `sigaction`, for `write(2)` on a raw descriptor,
//! for `dup`/`close` on that descriptor, or for `raise` + `_exit`. Each of those
//! is a plain libc call whose preconditions are documented at its use site,
//! below, one `// SAFETY:` comment per block.
//!
//! ## The rules a handler may not break
//!
//! Only these calls may appear on the handler path (all async-signal-safe,
//! POSIX.1-2008 §2.4.1):
//!
//! `write` · `signal` · `sigemptyset`/`sigaddset`/`sigprocmask` · `raise` ·
//! `_exit` · `time` · `getpid` · `dup` · `close`
//!
//! In particular, the handler must **not**:
//!
//! * allocate or free (`malloc`, `Box`, `String`, `format!`, `Vec`, …);
//! * take a lock (`Mutex`, `OnceLock::get_or_init`, atomics are fine);
//! * use the formatting machinery (`write!`, `Display`, `{:?}`);
//! * call `localtime`/`tzset` — that is why the record carries `ts=<epoch>`
//!   instead of the normal formatted timestamp, and why `signal_name` below is
//!   a `match` on constants rather than [`crate::sys::signal_name`], which
//!   allocates a `String` (a `signal_hook` table lookup plus `to_string`).
//!
//! Everything that *can* be prepared in advance is: the stderr note is composed
//! in [`adopt`] (called from ordinary code) and copied into a stack buffer by
//! the handler, and the record is built into a fixed `[u8; 160]` with hand-rolled
//! integer formatting by `build_signal_record`.
//!
//! ## Verification
//!
//! `build_signal_record`/`push`/`push_int`/`signal_name` are unit-tested in this
//! file, including truncation and `i32::MIN`; `fatal_signal_handler_really_writes_a_record`
//! forks a child that really segfaults and checks the record landed and that the
//! exit status stayed `WIFSIGNALED(SIGSEGV)`.
//!
//! # Why a module-internal `imp`
//!
//! The `unsafe` lives in `imp` (Unix) with an empty twin for other platforms, so
//! `grep -rn unsafe src/` names exactly one file and one submodule, and the
//! `#[cfg(not(unix))]` surface is a single list of no-ops rather than a second
//! implementation to audit.

// The only surface `sink`/`panic`/`lib` may use. Everything else in `imp` stays
// private — the unit tests reach it as descendants of this module, which keeps
// the hand-rolled formatter out of the crate's API.
#[cfg(unix)]
pub(super) use imp::{adopt, emergency, install, release, set_tui_active};

#[cfg(not(unix))]
pub(super) use imp::{adopt, emergency, install, release, set_tui_active};

#[cfg(unix)]
mod imp {
    use std::path::Path;
    use std::sync::Once;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};

    /// Signals that abort the process without unwinding. SIGKILL/SIGSTOP cannot
    /// be caught; this list is exactly the "crashed" class we can report.
    const FATAL_SIGNALS: &[i32] = &[libc::SIGSEGV, libc::SIGBUS, libc::SIGABRT, libc::SIGILL, libc::SIGFPE];

    /// Longest stderr note composed at init.
    const NOTE_MAX: usize = 320;

    /// Longest crash record [`build_signal_record`] produces.
    pub(super) const RECORD_MAX: usize = 160;

    /// Raw fd of the active log file, kept in sync with rotation.
    pub(super) static LOG_FD: AtomicI32 = AtomicI32::new(-1);
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

    /// Write `bytes` to `fd`, retrying short writes.
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

    /// The installed handler. Only async-signal-safe calls, by contract.
    ///
    /// `unsafe extern "C"` because `sigaction` calls it from signal context with
    /// a C ABI: it must not unwind, and everything it does must be
    /// async-signal-safe (see the module SAFETY CONTRACT).
    //
    // SAFETY: the `unsafe` marker is what lets `sigaction` call this with a C ABI
    // and no unwinding; the body obeys the contract above — only the calls on the
    // allowed async-signal-safe list, no allocation, no locks, no formatting.
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
        //
        // SAFETY: all five calls are async-signal-safe and are used exactly as
        // POSIX specifies: a zeroed `sigset_t` is initialised by `sigemptyset`,
        // `sigaddset` adds the valid signal number the kernel just handed us,
        // and `SIG_UNBLOCK` restores delivery; `raise`/`_exit` end the process.
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
                // SAFETY: `action` is fully initialised before use (zeroed struct,
                // then every field we rely on), `handler` has the C signal
                // handler signature, and the mask is built by `sigemptyset`.
                // `SA_RESETHAND` makes a recursive fault fall back to the default
                // action instead of looping; the handler itself performs only
                // async-signal-safe calls.
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
        // SAFETY: plain libc call; the source fd is owned by `file` for the call,
        // and the result is checked before use.
        let dup = unsafe { libc::dup(file.as_raw_fd()) };
        if dup < 0 {
            LOG_FD.store(-1, Ordering::Relaxed);
            return;
        }
        let previous = LOG_FD.swap(dup, Ordering::Relaxed);
        if previous >= 0 {
            // SAFETY: `previous` came from `dup` in this module and is not stored
            // anywhere else after the swap, so this is its last use.
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
            // SAFETY: `fd` came from `dup` in this module; the swap above means no
            // other reader can observe it any more, so closing it is safe.
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
        //
        // SAFETY: `time(null)` is async-signal-safe and cannot fail.
        let epoch = unsafe { libc::time(std::ptr::null_mut()) };
        let line = format!(
            "ts={} ERROR seq=0 event=crash.log_reentrant cause={} msg=\"日志写入重入，记录被跳过以避免死锁\"\n",
            epoch, event
        );
        write_all(fd, line.as_bytes());
    }

    /// Note whether the TUI currently owns the terminal.
    pub fn set_tui_active(active: bool) {
        TUI_ACTIVE.store(active, Ordering::Relaxed);
    }

    /// Build the crash record without allocating. Split out so tests can check
    /// the format without actually crashing.
    ///
    /// Always returns `len <= RECORD_MAX`; a truncated record is preferable to a
    /// buffer overrun, and the leading fields are the ones a reader needs.
    pub fn build_signal_record(sig: i32) -> ([u8; RECORD_MAX], usize) {
        let mut buf = [0u8; RECORD_MAX];
        // SAFETY: `time(null)` is async-signal-safe, needs no initialised memory,
        // and ignores its argument when null.
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut at = push(&mut buf, 0, b"ts=");
        at = push_int(&mut buf, at, now);
        at = push(&mut buf, at, b" ERROR seq=0 event=crash.signal signal=");
        at = push_int(&mut buf, at, i64::from(sig));
        at = push(&mut buf, at, b" name=");
        at = push(&mut buf, at, signal_name(sig).as_bytes());
        at = push(&mut buf, at, b" pid=");
        // SAFETY: `getpid` has no preconditions and is async-signal-safe.
        at = push_int(&mut buf, at, i64::from(unsafe { libc::getpid() }));
        (buf, at.min(buf.len()))
    }

    /// Append `bytes`, returning the new write position.
    ///
    /// Once the buffer is full it returns `buf.len()`, so every later append is a
    /// no-op: the record is truncated at the tail rather than overwriting or
    /// overrunning. Never panics, never allocates.
    pub fn push(buf: &mut [u8], at: usize, bytes: &[u8]) -> usize {
        let end = (at + bytes.len()).min(buf.len());
        if at >= end {
            return buf.len();
        }
        buf[at..end].copy_from_slice(&bytes[..end - at]);
        end
    }

    /// Append `value` as ASCII digits without allocating or formatting.
    ///
    /// `i64::MIN` works because `unsigned_abs` yields a `u64` (its absolute value
    /// does not fit in `i64`); the 20-byte scratch buffer holds the longest
    /// decimal form either way.
    pub fn push_int(buf: &mut [u8], at: usize, value: i64) -> usize {
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
    ///
    /// Deliberately **not** [`crate::sys::signal_name`], which returns a `String`:
    /// allocating in a signal handler is undefined behaviour, so this `match` over
    /// the constants the handler can actually receive is the only option.
    pub fn signal_name(sig: i32) -> &'static str {
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
mod imp {
    //! Windows has no `sigaction`. The panic hook still captures Rust-level
    //! crashes; there is no portable way to report a hard fault, so these are
    //! no-ops with the same signatures as the Unix implementation.

    use std::path::Path;

    pub fn install() {}
    pub fn adopt(_file: &std::fs::File, _path: &Path) {}
    pub fn release() {}
    pub fn set_tui_active(_active: bool) {}
    pub fn emergency(_event: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use super::imp::{LOG_FD, RECORD_MAX, build_signal_record, push, push_int, signal_name};
    #[cfg(unix)]
    use std::sync::atomic::Ordering;

    /// The crash descriptor is one process-wide slot, so every test that adopts,
    /// releases or inspects it must hold this for its duration. A per-test
    /// `static` would not serialise anything — this is the shared one.
    #[cfg(unix)]
    static FD_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serialise a test against the other crash-fd tests.
    #[cfg(unix)]
    fn lock_fd() -> std::sync::MutexGuard<'static, ()> {
        FD_GUARD.lock().unwrap_or_else(|p| p.into_inner())
    }

    // -----------------------------------------------------------------------
    // signal_name: the `&'static str` the handler uses (no allocation allowed)
    // -----------------------------------------------------------------------

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn push_appends_and_advances() {
        let mut buf = [0u8; 16];
        let at = push(&mut buf, 0, b"ab");
        assert_eq!(at, 2);
        let at = push(&mut buf, at, b"cd");
        assert_eq!(at, 4);
        assert_eq!(&buf[..4], b"abcd");
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn push_into_a_zero_length_buffer_is_a_no_op() {
        let mut buf = [0u8; 0];
        assert_eq!(push(&mut buf, 0, b"anything"), 0);
        assert_eq!(push(&mut buf, 5, b"anything"), 0, "an out-of-range offset must not index");
    }

    #[cfg(unix)]
    #[test]
    fn push_handles_an_offset_at_the_end() {
        let mut buf = [0u8; 4];
        assert_eq!(push(&mut buf, 4, b"x"), 4);
        assert_eq!(push(&mut buf, 99, b"x"), 4, "an offset past the end must not panic");
        assert_eq!(&buf, b"\0\0\0\0");
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn the_record_length_is_always_within_the_buffer() {
        // The invariant the handler relies on before `&buf[..len]`: an out-of-range
        // `len` would panic inside a signal handler, i.e. abort.
        for sig in [i32::MIN, -1, 0, 9, 11, 15, 65, i32::MAX] {
            let (_buf, len) = build_signal_record(sig);
            assert!(len <= RECORD_MAX, "sig {sig} produced len={len} > {RECORD_MAX}");
        }
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
}

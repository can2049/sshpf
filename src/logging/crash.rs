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

// Every assertion here targets a Unix-only `sigaction` path, so on a non-Unix
// target the module would be empty — and an empty module with a glob import is an
// unused-import error under `-D warnings`, which is how the Windows CI job caught
// it. Gate the whole module rather than sprinkling `#[cfg(unix)]` on each test.
#[cfg(all(test, unix))]
mod tests;

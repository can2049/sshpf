//! Panic capture: turn one panic into `crash.panic` plus its trace block.
//!
//! # Why a hook and not a `catch_unwind` somewhere
//!
//! A panic can happen on any thread — a tunnel thread dying is the case that
//! costs the most debugging time — and `catch_unwind` only sees the threads it
//! was placed on. [`install_panic_hook`] wraps the process-global hook, so every
//! thread is covered, and the thread name ends up in the record, which is how a
//! dead tunnel thread is told apart from a clean exit.
//!
//! # Interaction with `ratatui`
//!
//! The hook chains to whatever hook was installed before it, and it is installed
//! from `main` *before* `ratatui::try_init()`. `try_init` then wraps **this** hook
//! in its own (whose first action is to restore the terminal), so a panic runs
//! ratatui's restore and then this record — in that order, which is the one both
//! libraries document. See `the_hook_chain_runs_ratatuis_restore_before_recording`.
//!
//! # Rules for anything added here
//!
//! A panic raised *inside* this module aborts the process on the spot ("thread
//! panicked while processing panic"), taking the original message with it. So:
//! every step is `Option`/`unwrap_or`-based, nothing here may allocate fatally or
//! call `unwrap`, and [`install_panic_hook`] additionally wraps the body in
//! `catch_unwind` as a belt to that braces. The slot writer goes through
//! [`super::Logger::critical`], which is re-entrancy-safe.

use std::panic::PanicHookInfo;

use super::sink::Logger;
use super::{BACKTRACE_FRAMES, logger};

/// Install the panic hook. Chains to the previously installed one.
///
/// Call once, as early as possible, so a panic during start-up is captured too.
pub(super) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let logger = logger();
        if logger.is_active() {
            // Everything here is infallible by construction; `catch_unwind` is
            // only the belt to that braces: if it ever is wrong, the process
            // still gets the upstream hook and its terminal restore.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log_panic(logger, info)));
        }
        previous(info);
    }));
}

/// Turn one panic into a `crash.panic` record plus its trace block.
fn log_panic(logger: &Logger, info: &PanicHookInfo<'_>) {
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

fn payload_text(info: &PanicHookInfo<'_>) -> String {
    payload_of(info.payload())
}

/// The text of a panic payload, without assuming it is a string.
///
/// Split from [`payload_text`] so the non-`&str` branch is testable: a
/// `PanicHookInfo` cannot be built by hand, a `Box<dyn Any + Send>` can.
fn payload_of(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
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
    let block = trace_block(backtrace);
    logger.critical("crash.panic.trace", &[("msg", &block.as_str())]);
}

/// The indented trace block, capped at [`BACKTRACE_FRAMES`] frames.
///
/// Frame lines are `   N: symbol` plus an indented `at file:line`. Keep the
/// frame lines (not the `at` lines) up to the cap, so the block stays readable
/// and bounded instead of dumping the whole runtime.
fn trace_block(backtrace: &str) -> String {
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
    block
}

#[cfg(test)]
mod tests {
    use super::super::sink::testing::{read, temp_logger};
    use super::*;
    use crate::logging::{LOG_FILE_NAME, Level};

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
        let text = read(dir.path());
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

    #[test]
    fn a_panic_without_a_location_still_produces_a_record() {
        // `location()` is `None` for a manually built `PanicHookInfo`-ish case and
        // for some panics raised outside Rust code; the record must not be lost.
        let dir = tempfile::tempdir().unwrap();
        let logger = temp_logger(dir.path(), Level::Info);
        write_panic_record(&logger, "no location", None, "<unnamed>", "");
        let text = read(dir.path());
        assert!(text.contains("event=crash.panic"), "{text}");
        assert!(text.contains("location=<未知位置>"), "a missing location must be spelled out: {text}");
        assert!(text.contains("thread=<unnamed>"), "{text}");
    }

    #[test]
    fn the_trace_block_caps_the_frame_count() {
        // A debug backtrace is 30+ frames; the cap is what keeps one crash to a
        // bounded number of lines.
        let mut backtrace = String::new();
        for i in 0..(BACKTRACE_FRAMES + 10) {
            backtrace.push_str(&format!("  {i}: sshpf::frame_{i}\n"));
        }
        let block = trace_block(&backtrace);
        let frames = block.lines().filter(|l| l.contains(": sshpf::frame_")).count();
        assert_eq!(frames, BACKTRACE_FRAMES, "the block must stop at the cap:\n{block}");
        assert!(block.contains("更深的栈帧已省略"), "the omission must be announced:\n{block}");
    }

    #[test]
    fn trace_headers_before_the_first_frame_are_dropped() {
        let block = trace_block("stack backtrace:\n   0: sshpf::main\n");
        assert!(!block.contains("stack backtrace:"), "header noise must not be copied:\n{block}");
        assert!(block.contains("0: sshpf::main"), "{block}");
    }

    #[test]
    fn an_empty_backtrace_still_yields_a_block() {
        let block = trace_block("");
        assert_eq!(block, "  | panic backtrace", "the header alone is a valid, parseable block");
    }

    /// Whether this panic is the chain probe and not another test's deliberate
    /// panic on a parallel thread.
    fn is_chain_probe(info: &PanicHookInfo<'_>) -> bool {
        info.payload().downcast_ref::<&str>().is_some_and(|s| *s == "chain probe")
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
        static GUARD: StdMutex<()> = StdMutex::new(());
        let _guard = GUARD.lock().unwrap_or_else(|p| p.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(temp_logger(dir.path(), Level::Info));
        let order: Arc<StdMutex<Vec<&'static str>>> = Arc::new(StdMutex::new(Vec::new()));

        // Our hook, as `install_crash_hooks` builds it but on a local logger.
        //
        // Both hooks only record the probe's own panic: the hook is
        // process-global and `cargo test` runs other tests that panic on purpose
        // (inside `catch_unwind`) on other threads, and their payloads must not be
        // mistaken for this test's ordering evidence.
        let ours_logger = Arc::clone(&logger);
        let ours_order = Arc::clone(&order);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if is_chain_probe(info) {
                ours_order.lock().unwrap().push("sshpf");
            }
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
            if is_chain_probe(info) {
                restore_order.lock().unwrap().push("ratatui-restore");
            }
            inner(info);
        }));

        let caught = std::panic::catch_unwind(|| panic!("chain probe"));
        let _ = std::panic::take_hook();
        assert!(caught.is_err());

        let order = order.lock().unwrap().clone();
        assert_eq!(order, vec!["ratatui-restore", "sshpf"], "a panic must reach both hooks, restore first");

        let text = read(dir.path());
        assert!(text.contains("event=crash.panic"), "the crash record must still be written: {text:?}");
        assert!(text.contains("chain probe"), "{text:?}");
    }

    /// A `Display` that panics, i.e. a caller-supplied value the log formats
    /// while it holds the sink lock.
    struct PanickingDisplay;

    impl std::fmt::Display for PanickingDisplay {
        fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
        use crate::logging::sink::CriticalSection;
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
                let text = std::fs::read_to_string(dir.path().join(LOG_FILE_NAME)).unwrap_or_default();
                done_tx.send((panicked, nested, text)).unwrap();
                drop(dir);
            })
            .unwrap();

        let (panicked, nested, text) =
            done_rx.recv_timeout(std::time::Duration::from_secs(10)).expect("deadlocked on the log mutex");
        assert!(panicked, "the panicking Display should have made the record panic");
        assert_eq!(nested, "returned", "the hook's record must not block");
        assert!(text.contains("event=writer.recovered"), "logging must work again after the unwind: {text:?}");
        assert!(
            !text.contains("event=writer.boom"),
            "the record that panicked mid-format must not be half-written: {text:?}"
        );
        worker.join().unwrap();
    }

    /// The rendered record for a panic whose payload is not a string must say so
    /// rather than lose the crash.
    #[test]
    fn a_non_string_payload_is_reported_not_dropped() {
        // `PanicHookInfo` cannot be built by hand, so use the payload shape a
        // real panic produces: a `Box<dyn Any + Send>`.
        let payload = std::panic::catch_unwind(|| std::panic::panic_any(7u32)).unwrap_err();
        assert_eq!(payload_of(payload.as_ref()), "<非字符串 panic>");
    }

    /// `&str` and `String` payloads are the two normal shapes and must survive
    /// verbatim — the message is the whole point of the record.
    #[test]
    fn string_payloads_survive_verbatim() {
        let text = payload_of(std::panic::catch_unwind(|| panic!("静态消息")).unwrap_err().as_ref());
        assert_eq!(text, "静态消息");

        let owned = payload_of(
            std::panic::catch_unwind(|| std::panic::panic_any(String::from("owned 消息"))).unwrap_err().as_ref(),
        );
        assert_eq!(owned, "owned 消息");
    }
}

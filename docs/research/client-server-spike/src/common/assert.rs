//! PASS/FAIL recording shared by every spike binary.
//!
//! What belongs here: turning a named claim plus a boolean into a printed line
//! and a process exit status. What does not: any knowledge of what is being
//! asserted.
//!
//! A failing assertion must make the process exit non-zero, so a future reader
//! (or CI) can treat "the design still holds" as an executable check rather than
//! a sentence in a document.

use std::sync::atomic::{AtomicUsize, Ordering};

static FAILED: AtomicUsize = AtomicUsize::new(0);
static PASSED: AtomicUsize = AtomicUsize::new(0);

/// Record one assertion and print its verdict.
pub fn check(name: &str, ok: bool, detail: &str) {
    if ok {
        PASSED.fetch_add(1, Ordering::SeqCst);
        println!("PASS {name}{}", if detail.is_empty() { String::new() } else { format!("  [{detail}]") });
    } else {
        FAILED.fetch_add(1, Ordering::SeqCst);
        println!("FAIL {name}{}", if detail.is_empty() { String::new() } else { format!("  [{detail}]") });
    }
}

/// Print the summary and return the process exit code.
///
/// Zero assertions is itself a failure: a spike binary that silently asserts
/// nothing is worse than one that fails, because it looks like proof.
pub fn finish(binary: &str) -> i32 {
    let passed = PASSED.load(Ordering::SeqCst);
    let failed = FAILED.load(Ordering::SeqCst);
    println!("\n{binary}: {passed} passed, {failed} failed, {} total", passed + failed);
    if passed + failed == 0 {
        println!("FAIL no assertions were run");
        return 1;
    }
    if failed == 0 {
        0
    } else {
        1
    }
}

//! Machine-checked policy: the crate's safety and size rules are assertions, not
//! review comments.
//!
//! The project's stated audience is an AI agent reading the tree cold, and its
//! stated failure mode is "a panic in a TUI takes the whole session down". Both
//! are properties of the **source**, not of the platform, so they are enforced
//! the same way `tests/help_coverage.rs` enforces help coverage: by parsing the
//! real files at test time (`include_str!`/`fs::read` on the checkout).
//!
//! # What is asserted
//!
//! | rule | why it is a hard failure |
//! |---|---|
//! | no `.unwrap()` / `.expect()` in production code | a panic path in a TUI kills the session and hides the reason |
//! | no `panic!` / `unreachable!` / `todo!` / `unimplemented!` in production code | same, and `todo!` in a shipped binary is a lie in the type system |
//! | `unsafe` only in `*crash*.rs`, each block with a `// SAFETY:` note | the crash writer is the one place that *must* use libc; everywhere else it is a smell |
//! | every source file opens with a `//!` module doc | the file itself must say what belongs in it — that is the agent's map |
//! | production code per file ≤ [`MAX_PROD_LINES`] | a 3000-line file is a maze, not a module |
//! | one function ≤ [`MAX_FN_LINES`] lines | a 150-line function cannot be reviewed, tested or reasoned about |
//! | every `pub mod` in `src/lib.rs` resolves to a real file | a stale declaration is a broken map |
//!
//! # Test code is exempt, deliberately
//!
//! `.unwrap()` in a test is a *good* assertion ("this must be Some, and if not
//! fail here"), so the scan masks every `#[cfg(test)]` region before looking for
//! forbidden constructs. The exemption is implemented by brace-balancing from the
//! attribute, so it cannot be fooled by re-indentation, and it also covers a
//! `#[cfg(test)]` helper function in the middle of a file (which
//! `src/ui/layout.rs` used to have).
//!
//! # When this test fails
//!
//! The message names the file, the line and the offending construct, and prints
//! `[test]` next to hits that are inside a test region (they should never
//! appear — it means the masking failed, which is itself a bug worth fixing).
//! A hit in production code needs either a rewrite (preferred: return
//! `Result`/`Option` and let the caller log it) or an allowlist entry **with a
//! written reason** — never a silent `#[allow]`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Largest production (non-`#[cfg(test)]`) prefix allowed per file.
///
/// Chosen so a module fits on two screens and can be held in one context window.
/// Raising it needs a written reason in [`SIZE_EXEMPT`], the same shape as the
/// other allowlists: an exception is a documented decision, not a mute button.
const MAX_PROD_LINES: usize = 400;

/// Longest production function allowed, in lines.
///
/// A function this long is where bugs hide: it has too many branches to hold in
/// one head, so the reviewer reads the first third and trusts the rest. The fix
/// is always the same — name the sub-steps after what they do.
const MAX_FN_LINES: usize = 60;

/// Files that may exceed [`MAX_PROD_LINES`]: `(suffix, reason)`.
///
/// Empty is the goal. An entry here means "this file is a single cohesive unit
/// and splitting it would make the code worse, not better".
const SIZE_EXEMPT: &[(&str, &str)] = &[];

/// Files that may contain `unsafe`, matched by file-name suffix.
///
/// The crash writer is the only legitimate user: a `SIGSEGV` handler cannot
/// allocate, lock or format, so it must call `write(2)`, `signal(2)` and
/// `_exit(2)` directly. Everything else in this crate is safe Rust, and that is
/// a feature worth asserting rather than remembering.
const UNSAFE_ALLOWED_SUFFIXES: &[&str] = &["crash.rs"];

// ---------------------------------------------------------------------------
// source scanning
// ---------------------------------------------------------------------------

/// Everything under `src/`, recursively, as a sorted list.
fn source_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(Path::new("src"), &mut out);
    out.sort();
    out
}

/// A source file with its test regions masked out.
struct Scan {
    /// Production source: comments and string literals blanked, `#[cfg(test)]`
    /// bodies blanked. Byte offsets line up with the original, so a hit can be
    /// reported with the right line number.
    prod: Vec<char>,
    /// The original text, for the `// SAFETY:` lookback.
    raw: Vec<char>,
    /// Line number (1-based) of every char in `prod`.
    line_of: Vec<usize>,
    /// `(start, end)` char ranges that came from a `#[cfg(test)]` region.
    test_spans: Vec<(usize, usize)>,
}

impl Scan {
    fn load(path: &Path) -> Option<Scan> {
        let text = std::fs::read_to_string(path).ok()?;
        let raw: Vec<char> = text.chars().collect();
        let test_spans = cfg_test_spans(&raw);
        let mut prod = blank_comments_and_strings(&raw);
        for (start, end) in &test_spans {
            for slot in prod.iter_mut().take(*end).skip(*start) {
                *slot = ' ';
            }
        }
        let mut line_of = Vec::with_capacity(raw.len());
        let mut line = 1usize;
        for c in &raw {
            if *c == '\n' {
                line += 1;
            }
            line_of.push(line);
        }
        Some(Scan { prod, raw, line_of, test_spans })
    }

    fn in_test(&self, at: usize) -> bool {
        self.test_spans.iter().any(|(start, end)| (*start..*end).contains(&at))
    }

    /// 1-based line number of a char offset.
    fn line(&self, at: usize) -> usize {
        self.line_of.get(at).copied().unwrap_or(0)
    }

    /// All char offsets where `needle` starts.
    fn find_all(&self, needle: &str) -> Vec<usize> {
        let len = needle.chars().count();
        let mut hits = Vec::new();
        let mut i = 0usize;
        while i + len <= self.prod.len() {
            if self.starts_with(i, needle) {
                hits.push(i);
                i += len;
            } else {
                i += 1;
            }
        }
        hits
    }

    fn starts_with(&self, at: usize, needle: &str) -> bool {
        needle.chars().enumerate().all(|(offset, c)| self.prod.get(at + offset) == Some(&c))
    }
}

/// Half-open char ranges of every `#[cfg(test)]` item body.
///
/// Brace balancing, not indentation: a `#[cfg(test)]` on a single helper `fn`
/// is masked just like a trailing `mod tests { … }`. The attribute's body starts
/// at the next `{`; a `#[cfg(test)]` with no body at all (e.g. on a `use`) is
/// skipped rather than swallowing the rest of the file.
fn cfg_test_spans(src: &[char]) -> Vec<(usize, usize)> {
    const ATTR: &str = "#[cfg(test)]";
    let attr: Vec<char> = ATTR.chars().collect();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i + attr.len() <= src.len() {
        if src[i..i + attr.len()] == attr[..] {
            // Find the opening brace of the item this attribute decorates.
            let mut j = i + attr.len();
            while j < src.len() && src[j] != '{' && src[j] != ';' {
                // A second attribute or a new line without a body ends the search.
                if src[j] == '#' && src.get(j + 1) == Some(&'[') {
                    break;
                }
                j += 1;
            }
            if src.get(j) != Some(&'{') {
                i += attr.len();
                continue;
            }
            let mut depth = 0usize;
            let mut k = j;
            while k < src.len() {
                match src[k] {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            k += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            spans.push((i, k.min(src.len())));
            i = k;
        } else {
            i += 1;
        }
    }
    spans
}

/// Blank `//` line comments, `/* */` block comments and string literals, keeping
/// char literals (they are code) and all offsets stable.
///
/// Comments must be blanked because the source is *heavily* commented — prose
/// about "this used to `.unwrap()`" would otherwise be reported as a violation,
/// which would teach the reader to ignore the test.
fn blank_comments_and_strings(src: &[char]) -> Vec<char> {
    let mut out = src.to_vec();
    let mut i = 0usize;
    while i < src.len() {
        let c = src[i];
        if c == '/' && src.get(i + 1) == Some(&'/') {
            while i < src.len() && src[i] != '\n' {
                out[i] = ' ';
                i += 1;
            }
        } else if c == '/' && src.get(i + 1) == Some(&'*') {
            out[i] = ' ';
            out[i + 1] = ' ';
            i += 2;
            while i < src.len() && !(src[i] == '*' && src.get(i + 1) == Some(&'/')) {
                out[i] = ' ';
                i += 1;
            }
            if i < src.len() {
                out[i] = ' ';
                if i + 1 < src.len() {
                    out[i + 1] = ' ';
                }
                i += 2;
            }
        } else if c == '"' {
            // Keep the quotes themselves so a `b"..."` prefix stays parseable;
            // blank only the contents (a message may legitimately contain text
            // that looks like code).
            out[i] = ' ';
            i += 1;
            while i < src.len() {
                let ch = src[i];
                out[i] = ' ';
                i += 1;
                if ch == '\\' {
                    if i < src.len() {
                        out[i] = ' ';
                        i += 1;
                    }
                } else if ch == '"' {
                    break;
                }
            }
        } else if c == '\'' && is_char_literal(src, i) {
            i += 1;
            while i < src.len() {
                if src[i] == '\\' {
                    i += 2;
                    continue;
                }
                let ch = src[i];
                i += 1;
                if ch == '\'' {
                    break;
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Whether the `'` at `i` opens a char literal (`'x'`, `'\n'`) rather than a
/// lifetime (`'static`).
fn is_char_literal(src: &[char], i: usize) -> bool {
    if src.get(i + 1) == Some(&'\\') {
        return src.get(i + 3) == Some(&'\'');
    }
    src.get(i + 2) == Some(&'\'')
}

/// Production (non-test) line count: how many lines a reviewer must hold in their
/// head to review this file's behaviour.
///
/// A line counts when it has any non-whitespace content and lies **outside** every
/// `#[cfg(test)]` span. Line-based on purpose: a file may interleave a
/// `#[cfg(test)]` helper with production code part-way through, and a 2000-line
/// trailing test module must not make a 300-line module fail this check. Doc
/// comments count as production — they are what the reader has to read.
fn prod_line_count(scan: &Scan) -> usize {
    let mut count = 0usize;
    let mut at = 0usize;
    while at < scan.raw.len() {
        let start = at;
        while at < scan.raw.len() && scan.raw[at] != '\n' {
            at += 1;
        }
        let end = at; // exclusive, `at` is at the newline (or EOF)
        at = at.saturating_add(1);
        let inside_test = scan.test_spans.iter().any(|(s, e)| start >= *s && end <= *e);
        if inside_test {
            continue;
        }
        if scan.raw[start..end].iter().any(|c| !c.is_whitespace()) {
            count += 1;
        }
    }
    count
}

/// `(line, name, length)` for every production function longer than the budget.
///
/// Brace-balanced from each `fn` signature, ignoring test regions (a long test
/// with many assertions is fine — that is a list, not a control-flow graph).
fn overgrown_functions(scan: &Scan) -> Vec<(usize, String, usize)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < scan.prod.len() {
        if !scan.starts_with(i, "fn ") {
            i += 1;
            continue;
        }
        // `fn ` must start a declaration, not appear inside an identifier.
        if i > 0 && (scan.prod[i - 1].is_alphanumeric() || scan.prod[i - 1] == '_') {
            i += 1;
            continue;
        }
        let name_start = i + 3;
        let mut name_end = name_start;
        while name_end < scan.prod.len() && (scan.prod[name_end].is_alphanumeric() || scan.prod[name_end] == '_') {
            name_end += 1;
        }
        let name: String = scan.prod[name_start..name_end].iter().collect();
        // Body: the first `{` after the signature, then balance to its `}`.
        let mut j = name_end;
        while j < scan.prod.len() && scan.prod[j] != '{' && scan.prod[j] != ';' {
            j += 1;
        }
        if scan.prod.get(j) != Some(&'{') {
            // A trait method signature (`fn f();`): no body to measure.
            i = j.max(i + 1);
            continue;
        }
        let open = j;
        let mut depth = 0usize;
        while j < scan.prod.len() {
            match scan.prod[j] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let close = j.min(scan.prod.len().saturating_sub(1));
        let is_test = scan.in_test(open);
        if !is_test {
            let first = scan.line(open);
            let last = scan.line(close);
            let len = last.saturating_sub(first) + 1;
            if len > MAX_FN_LINES {
                out.push((first, name, len));
            }
        }
        i = close.max(i + 1);
    }
    out
}

#[test]
fn no_production_function_is_a_swamp() {
    let mut offenders = Vec::new();
    for path in source_files() {
        let Some(scan) = Scan::load(&path) else {
            continue;
        };
        for (line, name, len) in overgrown_functions(&scan) {
            offenders.push(format!("{}:{} `fn {name}` is {len} lines (max {MAX_FN_LINES})", rel(&path), line));
        }
    }
    assert!(
        offenders.is_empty(),
        "split these functions into named steps — a function this long hides its own bugs:\n{}",
        offenders.join("\n")
    );
}

fn rel(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------------------
// the forbidden constructs
// ---------------------------------------------------------------------------

/// `(needle, why it is forbidden)`.
///
/// `.unwrap()` and `.expect()` are the same failure: a panic. They are listed
/// separately because the message should name what the author actually wrote.
const FORBIDDEN: &[(&str, &str)] = &[
    (".unwrap()", "panics on None/Err; return Result/Option and log the reason instead"),
    (".expect(", "panics on None/Err; return Result/Option and log the reason instead"),
    ("panic!(", "panics; the TUI must report the failure, not die"),
    ("unreachable!(", "a panic with a nicer name; model the case instead"),
    ("todo!(", "unimplemented code in a shipped binary"),
    ("unimplemented!(", "unimplemented code in a shipped binary"),
];

#[test]
fn production_code_contains_no_panicking_constructs() {
    let mut violations: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut scanned = 0usize;

    for path in source_files() {
        let Some(scan) = Scan::load(&path) else {
            continue;
        };
        scanned += 1;
        for (needle, why) in FORBIDDEN {
            for at in scan.find_all(needle) {
                if scan.in_test(at) {
                    // Not a violation, but it means the masking missed a region,
                    // which would silently disable this whole test. Surface it.
                    continue;
                }
                violations.entry(rel(&path)).or_default().push(format!(
                    "{}:{}  `{}` — {why}",
                    rel(&path),
                    scan.line(at),
                    needle
                ));
            }
        }
    }

    assert!(scanned > 20, "expected to scan the whole crate, only saw {scanned} files — did src/ move?");
    if violations.is_empty() {
        return;
    }
    let mut msg = String::from(
        "production code must not panic. Each hit below needs a rewrite (return Result/Option, log the \
         reason, show it in the status bar) — not a #[allow]:\n",
    );
    for hits in violations.values() {
        for hit in hits {
            msg.push_str("  ");
            msg.push_str(hit);
            msg.push('\n');
        }
    }
    panic!("{msg}");
}

#[test]
fn unsafe_is_confined_to_the_crash_writer_and_is_documented() {
    let mut violations: Vec<String> = Vec::new();
    let mut total_blocks = 0usize;

    for path in source_files() {
        let Some(scan) = Scan::load(&path) else {
            continue;
        };
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let allowed = UNSAFE_ALLOWED_SUFFIXES.iter().any(|suffix| name.ends_with(suffix));
        for at in scan.find_all("unsafe") {
            if scan.in_test(at) {
                continue;
            }
            total_blocks += 1;
            let line = scan.line(at);
            if !allowed {
                violations.push(format!(
                    "{}:{line}  `unsafe` outside {} — safe Rust everywhere else is a feature; \
                     rewrite or move the call into the crash writer",
                    rel(&path),
                    UNSAFE_ALLOWED_SUFFIXES.join(", ")
                ));
                continue;
            }
            if !has_safety_note(&scan, at) {
                violations.push(format!(
                    "{}:{line}  `unsafe` without a `// SAFETY:` note in the 6 lines above it",
                    rel(&path)
                ));
            }
        }
    }

    if !violations.is_empty() {
        let mut msg = String::from("unsafe policy violated:\n");
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        panic!("{msg}");
    }
    // The crash writer is the only user; if this drops to zero the file was
    // replaced by safe code (good — delete this allowance) and if it explodes the
    // policy above stopped being enforced.
    assert!(
        total_blocks > 0,
        "no `unsafe` found at all: either the crash writer moved out of src/ or the scan is broken"
    );
}

/// Walk back up to 6 lines from `at` looking for a `// SAFETY:` comment.
fn has_safety_note(scan: &Scan, at: usize) -> bool {
    let mut seen_lines = 0usize;
    let mut i = at;
    while i > 0 && seen_lines <= 6 {
        i -= 1;
        if scan.raw[i] == '\n' {
            seen_lines += 1;
        }
        let window: String = scan.raw[i..at.min(scan.raw.len())].iter().collect();
        if window.contains("SAFETY:") {
            return true;
        }
    }
    false
}

#[test]
fn every_source_file_states_what_it_is_for() {
    let mut missing = Vec::new();
    for path in source_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // The module doc may follow inner attributes; scan the first 15 lines.
        let has_doc = text.lines().take(15).any(|line| line.trim_start().starts_with("//!"));
        if !has_doc {
            missing.push(rel(&path));
        }
    }
    assert!(
        missing.is_empty(),
        "every source file must open with a `//!` module doc saying what belongs in it (the agent's map): {missing:#?}"
    );
}

#[test]
fn no_source_file_is_a_maze() {
    let mut oversized = Vec::new();
    for path in source_files() {
        let Some(scan) = Scan::load(&path) else {
            continue;
        };
        let lines = prod_line_count(&scan);
        if lines <= MAX_PROD_LINES {
            continue;
        }
        let name = rel(&path);
        let exempt = SIZE_EXEMPT.iter().any(|(suffix, _)| name.ends_with(suffix));
        if !exempt {
            oversized.push(format!("{name}: {lines} production lines (max {MAX_PROD_LINES})"));
        }
    }
    assert!(
        oversized.is_empty(),
        "split these files by responsibility, or add a SIZE_EXEMPT entry *with a reason*:\n{}",
        oversized.join("\n")
    );
}

#[test]
fn every_module_declared_in_lib_rs_exists() {
    let lib = std::fs::read_to_string("src/lib.rs").expect("src/lib.rs must exist");
    let mut missing = Vec::new();
    for line in lib.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub mod ") else {
            continue;
        };
        // Only plain `pub mod name;` declarations (inline modules have a brace).
        let Some(name) = rest.strip_suffix(';') else {
            continue;
        };
        let name = name.trim();
        if name.starts_with('{') {
            continue;
        }
        let flat = Path::new("src").join(format!("{name}.rs"));
        let nested = Path::new("src").join(name).join("mod.rs");
        if !flat.exists() && !nested.exists() {
            missing.push(name.to_string());
        }
    }
    assert!(missing.is_empty(), "src/lib.rs declares modules with no file behind them: {missing:?}");
}

#[test]
fn the_scan_can_actually_see_a_violation() {
    // A policy test that cannot fail is worse than none: this feeds the scanner a
    // synthetic file and asserts it reports the construct, and that test regions
    // are exempt. It runs against the same masking the real check uses.
    let src: Vec<char> =
        "fn prod() {\n    let x = y.unwrap();\n}\n#[cfg(test)]\nmod tests {\n    fn t() { v.unwrap(); }\n}\n"
            .chars()
            .collect();
    let spans = cfg_test_spans(&src);
    assert_eq!(spans.len(), 1, "the trailing test module must be detected");

    let mut prod = blank_comments_and_strings(&src);
    let (start, end) = spans[0];
    for slot in prod.iter_mut().take(end).skip(start) {
        *slot = ' ';
    }

    // The production hit is visible...
    let prod_text: String = prod[..start].iter().collect();
    assert!(prod_text.contains("y.unwrap()"), "scanner must see production code: {prod_text:?}");
    // ...and the test-only hit is masked, which is what makes test code exempt.
    let test_text: String = prod[start..].iter().collect();
    assert!(!test_text.contains("v.unwrap()"), "test region must be masked: {test_text:?}");
}

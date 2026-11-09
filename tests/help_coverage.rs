//! Machine-checked coverage: the in-app help must describe **every** key and
//! mouse gesture the app really handles — and must not describe anything it does
//! not.
//!
//! `src/help.rs` is a hand-written bilingual catalog; `src/keys.rs` is the code
//! that really binds the keys. Two hand-written lists drift apart, so this test
//! derives one from the other **at compile time** (`include_str!`) and compares
//! them in both directions:
//!
//! ```text
//! src/keys.rs  ──(brace-balanced handler bodies + KeyCode literals)──▶ code tokens ─┐
//!                                                                                   ├─ equal?
//! src/help.rs  ──(split `keys` on " / ", normalise symbols)──────────▶ help tokens ──┘
//! ```
//!
//! * **Forward — "must not omit".** A dispatched key with no help row is a
//!   *silent* gap: the user presses a key, something happens, and the help never
//!   mentioned it. That is precisely the defect this feature exists to remove, so
//!   it fails here.
//! * **Reverse — "must not lie".** A help row for a key nobody dispatches is
//!   worse than no row: the user presses it and nothing happens, then distrusts
//!   the rest of the help.
//!
//! Both directions report *every* offending section and token in one failure, so
//! a red run tells you exactly which row to add or delete.
//!
//! # How the code side is parsed
//!
//! Brace matching, not indentation or line numbers: a handler body is found by
//! its `fn <name>(` signature, then scanned to the `}` that closes it, skipping
//! comments and string literals first. Renaming a variable or reflowing a match
//! arm does not break this; adding a key does — which is the point.
//!
//! Within a body, every `KeyCode::` literal becomes a token, and the modifiers
//! of the **same match arm** are attached (`KeyCode::Char('s') if …
//! KeyModifiers::CONTROL` → `ctrl+s`). The arm boundaries are found by scanning
//! out from each literal to the nearest `=>` / `;` / `{` / `}`, so a multi-line
//! arm still attributes its modifiers correctly and a guard on one arm never
//! leaks onto its neighbour.
//!
//! `KeyCode::Char(c)` (a binding, not a literal) produces no token: "type any
//! character" cannot be enumerated, and faking a token for it would make the
//! comparison meaningless. Sections that need to say "typing filters" say it in a
//! `HelpItem::note` row instead.
//!
//! # How the help side is parsed
//!
//! Team spec §3 fixes the spelling: parallel bindings are separated by `" / "`,
//! modifiers stay glued (`ctrl+s`), arrows are written as `↓ ↑ ← →`. Tokens are
//! normalised before comparing so both sides can be written naturally:
//!
//! | help writes | normalises to |
//! |---|---|
//! | `↓` `↑` `←` `→` | `down` `up` `left` `right` |
//! | `shift+↓` | `shift+down` |
//! | `shift+j` (same key as `J` in a terminal) | `J` |
//!
//! # The whitelists
//!
//! A whitelist is a *documented exception*, never a mute button. Both are explicit
//! constants below with the reason spelled out, and the assertions still run for
//! every other token.

// ---------------------------------------------------------------------------
// every `event=` the code emits is documented
// ---------------------------------------------------------------------------

/// Every `logging::*("event", …)` call in `src/eventlog/**` must have a row in
/// `docs/logging/events.md`.
///
/// Same argument as the keys above: the catalogue is what a user (or an agent)
/// reads to interpret a log line, and an undocumented event is a line nobody can
/// act on. Two hand-written lists drift, so one is derived from the source.
///
/// Only the event *name* is compared, never its level or fields: the name is the
/// stable contract, and a field added later must not fail this test.
#[test]
fn every_logged_event_is_in_the_catalogue() {
    let mut missing = Vec::new();
    for (module, source) in EVENTLOG_SOURCES {
        for name in event_names(source) {
            if !EVENT_CATALOGUE.contains(&format!("`{name}`")) {
                missing.push(format!("{module}: {name}"));
            }
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "docs/logging/events.md has no row for these events (write the name in backticks):\n{}",
        missing.join("\n")
    );
    // A scanner that found nothing would pass vacuously; the catalogue is large,
    // so the source side must be too.
    let total: usize = EVENTLOG_SOURCES.iter().map(|(_, src)| event_names(src).len()).sum();
    assert!(total > 60, "the event scan found only {total} events, so it is not reading the sources");
}

/// Every `eventlog` module that owns `event=` names.
///
/// `include_str!` embeds exactly the files named here, so a new module (or a
/// moved file) fails loudly rather than being silently unscanned.
const EVENTLOG_SOURCES: &[(&str, &str)] = &[
    ("process", include_str!("../src/eventlog/process.rs")),
    ("tunnel", include_str!("../src/eventlog/tunnel.rs")),
    ("ssh", include_str!("../src/eventlog/ssh.rs")),
    ("user", include_str!("../src/eventlog/user.rs")),
];

const EVENT_CATALOGUE: &str = include_str!("../docs/logging/events.md");

/// Every event-shaped literal in `source`, with comments blanked first.
///
/// An "event name" is a dotted lowercase token (`tunnel.retry_scheduled`,
/// `client.attach_failed`). Scanning for the *shape* rather than for
/// `logging::info(` calls is deliberate: many events are emitted through helper
/// functions (`tunnel_event_on(logger, level, "tunnel.exit", …)`), so a scan that
/// only looked at direct `logging::` call sites would silently miss most of the
/// catalogue — which is exactly the failure mode this test exists to prevent.
///
/// String-literal contents are *not* blanked (the names live there); only comments
/// are, so a name quoted in prose cannot invent a false positive.
fn event_names(source: &str) -> Vec<String> {
    let chars = blank_comments(source);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        // Collect one literal's contents.
        i += 1;
        let mut text = String::new();
        while let Some(&c) = chars.get(i) {
            if c == '"' {
                break;
            }
            text.push(c);
            i += 1;
        }
        i += 1;
        if is_event_name(&text) {
            out.push(text);
        }
    }
    out
}

/// Whether `text` has the shape of an event name: dotted, lowercase, no spaces.
///
/// The segment rules are what keep this from matching version strings ("0.7.0"),
/// file names ("config.toml") or log paths, which are the near-misses a looser
/// pattern picks up.
fn is_event_name(text: &str) -> bool {
    let mut segments = 0usize;
    for segment in text.split('.') {
        if segment.is_empty() {
            return false;
        }
        if !segment.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return false;
        }
        // Purely numeric segments mean a version, not a name.
        if segment.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        segments += 1;
    }
    segments >= 2
}

/// Blank `//` and `/* */` comment text, keeping every other character in place.
///
/// Char-based throughout: these sources are full of Chinese, and mixing byte and
/// `char` offsets is how a scan silently compares the wrong region.
fn blank_comments(source: &str) -> Vec<char> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = chars.clone();
    let mut i = 0usize;
    let mut state = ScanState::Code;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match state {
            ScanState::LineComment => {
                if c == '\n' {
                    state = ScanState::Code;
                } else {
                    out[i] = ' ';
                }
                i += 1;
            }
            ScanState::BlockComment => {
                if c == '*' && next == Some('/') {
                    out[i] = ' ';
                    out[i + 1] = ' ';
                    i += 2;
                    state = ScanState::Code;
                } else {
                    if c != '\n' {
                        out[i] = ' ';
                    }
                    i += 1;
                }
            }
            ScanState::String => {
                if c == '\\' {
                    i += 2;
                } else {
                    if c == '"' {
                        state = ScanState::Code;
                    }
                    i += 1;
                }
            }
            ScanState::Code => {
                if c == '/' && next == Some('/') {
                    out[i] = ' ';
                    out[i + 1] = ' ';
                    i += 2;
                    state = ScanState::LineComment;
                } else if c == '/' && next == Some('*') {
                    out[i] = ' ';
                    out[i + 1] = ' ';
                    i += 2;
                    state = ScanState::BlockComment;
                } else if c == '"' {
                    state = ScanState::String;
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    out
}

enum ScanState {
    Code,
    LineComment,
    BlockComment,
    String,
}

use std::collections::{BTreeMap, BTreeSet};

use sshpf::help::{SECTIONS, SectionId};

/// The real key dispatch. Embedded so this test re-runs whenever it changes.
///
/// The handlers no longer live in one file: `src/keys.rs` is the routing map, and
/// each context has its own submodule. Every file that binds a key must be listed
/// here — `include_str!` only embeds the one file it names, so a missing entry
/// would silently drop a whole context from the comparison (the section would
/// look "documented but not dispatched", or worse, pass vacuously). Adding a key
/// binding file under `src/keys/` therefore means adding a line here too; the
/// `every_key_handler_file_is_embedded` test below fails if you forget.
const KEYS_SRC: &str = concat!(
    include_str!("../src/keys.rs"),
    include_str!("../src/keys/main.rs"),
    include_str!("../src/keys/select.rs"),
    include_str!("../src/keys/form.rs"),
    include_str!("../src/keys/import.rs"),
    include_str!("../src/keys/ports.rs"),
    include_str!("../src/keys/dropdown.rs"),
    include_str!("../src/keys/misc.rs"),
    // `testkit.rs` is test-only fixtures and binds no keys, so it is deliberately
    // absent: including it would add the tests' own `handle_key(...)` calls to the
    // scanned text for no benefit.
);

/// Every file under `src/keys/` that really binds keys, i.e. must be in
/// [`KEYS_SRC`]. Kept next to the list so the two cannot drift apart unnoticed.
///
/// `src/keys.rs` (the router) plus one file per context; `testkit.rs` is the only
/// submodule that is not a dispatcher.
const KEY_HANDLER_FILES: &[&str] = &[
    "src/keys.rs",
    "src/keys/main.rs",
    "src/keys/select.rs",
    "src/keys/form.rs",
    "src/keys/import.rs",
    "src/keys/ports.rs",
    "src/keys/dropdown.rs",
    "src/keys/misc.rs",
];

/// The mouse hit-target enum: one variant per clickable thing.
const HIT_SRC: &str = include_str!("../src/ui/hit.rs");
/// The human-facing key reference. Every documented token must appear in it.
const KEYMAP_DOC: &str = include_str!("../docs/keymap.md");

// ---------------------------------------------------------------------------
// handler -> section map
// ---------------------------------------------------------------------------

/// Which help section owns each dispatch function.
///
/// The unit is the **handler function**, not the file: after the v0.3.0 split the
/// dispatch is spread across `src/keys.rs` and `src/keys/*.rs`, and this table is
/// what keeps the mapping intact now that "one file" no longer implies "one
/// context". The functions are still named by what context they serve, so the
/// table is unchanged by the move — only [`KEYS_SRC`] had to grow.
///
/// A handler that is split into "entry + called helpers" is listed as *separate
/// rows*, each mapped to the same section: the section's code tokens are the union
/// of its handlers, so `handle_field_editing` + `edit_field` together describe
/// "form editing". Splitting a body must therefore add a row here for the new
/// function, or `every_dispatched_key_is_documented` reports the moved keys as
/// undocumented (they would be dispatched by a function this table does not know).
///
/// `handle_form_key` only delegates (its body contains no `KeyCode` literal), so
/// it contributes nothing on its own; it is listed anyway so that a future key
/// added directly to the delegation layer is caught by this test instead of being
/// silently undocumented. The same holds for `handle_key`, the global entry point:
/// its only literal is the `ctrl+c` quit binding, which belongs to the
/// cross-cutting "Safety" section.
///
/// A handler split for length (entry + `handle_import_row_key`) is listed twice for
/// the same section on purpose: the tokens live in the helper, but the context is
/// the entry point's.
///
/// Not listed on purpose: `move_field` (pure field arithmetic, no `KeyCode`) and
/// `active_filter_column` (returns a column, no key). Listing them would add
/// nothing and would make the "handler was renamed" failure message misleading.
const HANDLERS: &[(&str, SectionId)] = &[
    ("handle_key", SectionId::Safety),
    ("handle_main_key", SectionId::Main),
    ("handle_select_key", SectionId::Selection),
    ("handle_form_key", SectionId::Form),
    ("handle_field_navigating", SectionId::Form),
    ("activate_field", SectionId::Form),
    ("adjust_field", SectionId::Form),
    ("handle_field_editing", SectionId::FormEditing),
    ("edit_field", SectionId::FormEditing),
    ("handle_dropdown_key", SectionId::HostPicker),
    // Movement half of the dropdown, split out for length only: same context, so
    // the same section, and its keys stay attributed to `host_picker`.
    ("handle_dropdown_move_key", SectionId::HostPicker),
    ("handle_import_key", SectionId::ImportPane),
    // `handle_import_key` delegates its row half to a helper: same context, so the
    // same section. Listing it is what keeps the moved keys attributed to
    // `import_pane` instead of vanishing from the comparison.
    ("handle_import_row_key", SectionId::ImportPane),
    ("handle_import_move_key", SectionId::ImportPane),
    ("handle_import_filter_key", SectionId::ImportFilter),
    ("handle_port_key", SectionId::PortPicker),
    // `handle_port_key` delegates to two helpers (movement, then actions), the
    // same shape as the import pane's split: one section, so every one of them
    // maps to `port_picker` and the section's keys are their union.
    ("handle_port_move_key", SectionId::PortPicker),
    ("handle_port_row_key", SectionId::PortPicker),
    ("handle_confirm_key", SectionId::Confirm),
    ("handle_detail_key", SectionId::Detail),
    ("handle_filter_key", SectionId::FilterBar),
    ("handle_help_key", SectionId::HelpOverlay),
];

/// `(section, token)` pairs a section may **document** although its own handler
/// does not dispatch them, because another handler does and this section is where
/// the user looks for it. Every entry needs a reason; anything not listed must be
/// a real dispatch.
///
/// * `("import_pane", "ctrl+s")` — `handle_import_key` returns `false` for
///   `ctrl+s`, deliberately handing it back to the form so it saves *without*
///   closing the pane. The dispatch is in `handle_field_navigating`, but the user
///   reads it in the import pane's footer, so the pane must document it.
///
/// The `safety` section names `y` / `enter` / `n` / `esc` only inside a
/// `HelpItem::note`, and notes are never parsed as bindings (that is what makes
/// them safe for prose), so no entry is needed for them.
const DOC_ONLY: &[(&str, &str)] = &[("import_pane", "ctrl+s")];

/// `(section, token)` pairs a section may **dispatch** without documenting them
/// itself, because another section owns the explanation. Symmetrical half of the
/// mechanism, so a future deliberate exception is written down here *with its
/// reason* instead of quietly loosening the test. Currently empty: every
/// dispatched key is documented by its own section.
const CODE_ONLY: &[(&str, &str)] = &[];

// ---------------------------------------------------------------------------
// the closed token vocabulary
// ---------------------------------------------------------------------------

/// Fixed spellings from team spec §3. Anything outside this list plus a single
/// ASCII letter is a typo, and a typo is how a real binding silently stops being
/// covered (the test would compare two different spellings and demand a
/// nonexistent key).
const NAMED_TOKENS: &[&str] = &[
    "space",
    "enter",
    "esc",
    "tab",
    "shift+tab",
    "backspace",
    "delete",
    "home",
    "end",
    "pgup",
    "pgdn",
    "up",
    "down",
    "left",
    "right",
    "f2",
    "f10",
    "ctrl+c",
    "ctrl+h",
    "ctrl+o",
    "ctrl+r",
    "ctrl+s",
    "ctrl+z",
    "shift+down",
    "shift+up",
    "?",
    "/",
];

/// Letters are case-sensitive: `x` and `X`, `r` and `R`, `v` and `V`, `s` and `S`
/// are genuinely different bindings in this app (single-row vs batch actions).
fn is_allowed_token(token: &str) -> bool {
    NAMED_TOKENS.contains(&token) || (token.len() == 1 && token.chars().all(|c| c.is_ascii_alphabetic()))
}

// ---------------------------------------------------------------------------
// source scanning helpers (pure string handling: Windows-safe, no regex crate)
// ---------------------------------------------------------------------------

/// Replace comments and string literals with spaces, keeping **char** literals.
///
/// Two things are deliberate here:
///
/// * Comment text is blanked, not deleted, so byte/char offsets still line up
///   with the original source — error messages can quote the file meaningfully.
/// * Comments must be blanked because the dispatch is *heavily* commented
///   ("`f` outranks filter typing", "`i` closes the pane"). Scanning raw text
///   would attribute a `KeyCode` mentioned in prose to the code, or worse, pick
///   up `KeyModifiers::CONTROL` from an explanatory sentence.
/// * Char literals are kept: they *are* the payload (`Char('j')` → `j`).
fn strip_noise(src: &str) -> Vec<char> {
    let s: Vec<char> = src.chars().collect();
    let mut out = s.clone();
    let mut i = 0;
    while i < s.len() {
        if s[i] == '/' && next_is(&s, i, '/') {
            while i < s.len() && s[i] != '\n' {
                out[i] = ' ';
                i += 1;
            }
        } else if s[i] == '/' && next_is(&s, i, '*') {
            let mut depth = 0;
            while i < s.len() {
                if s[i] == '/' && next_is(&s, i, '*') {
                    depth += 1;
                    out[i] = ' ';
                    out[i + 1] = ' ';
                    i += 2;
                } else if s[i] == '*' && next_is(&s, i, '/') {
                    depth -= 1;
                    out[i] = ' ';
                    out[i + 1] = ' ';
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out[i] = ' ';
                    i += 1;
                }
            }
        } else if s[i] == '"' {
            out[i] = ' ';
            i += 1;
            while i < s.len() {
                let c = s[i];
                out[i] = ' ';
                i += 1;
                if c == '\\' {
                    if i < s.len() {
                        out[i] = ' ';
                        i += 1;
                    }
                } else if c == '"' {
                    break;
                }
            }
        } else if s[i] == '\'' && is_char_literal(&s, i) {
            // Keep the literal verbatim; skip past it so its quote cannot be
            // mistaken for the start of a lifetime.
            i += 1;
            while i < s.len() {
                if s[i] == '\\' {
                    i += 2;
                    continue;
                }
                let c = s[i];
                i += 1;
                if c == '\'' {
                    break;
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

/// `s[i] == c` and the next char is `c` too (a two-char operator like `//`).
fn next_is(s: &[char], i: usize, c: char) -> bool {
    s.get(i + 1) == Some(&c)
}

/// Whether the `'` at `i` opens a char literal (`'x'`, `'\n'`, `'\''`) rather
/// than a lifetime (`'static`). Only char literals carry a key token.
fn is_char_literal(s: &[char], i: usize) -> bool {
    if s.get(i + 1) == Some(&'\\') {
        return s.get(i + 3) == Some(&'\'');
    }
    s.get(i + 2) == Some(&'\'')
}

fn at(s: &[char], i: usize) -> Option<char> {
    s.get(i).copied()
}

fn starts_with(s: &[char], i: usize, needle: &str) -> bool {
    needle.chars().enumerate().all(|(offset, c)| at(s, i + offset) == Some(c))
}

/// Half-open span of the body of `fn <name>(…) { … }`, found by brace balancing.
fn function_span(src: &[char], name: &str) -> (usize, usize) {
    let sig = format!("fn {name}(");
    let mut start = None;
    for i in 0..src.len() {
        if starts_with(src, i, &sig) {
            start = Some(i);
            break;
        }
    }
    let start = start.unwrap_or_else(|| panic!("cannot find `fn {name}(` in src/keys.rs: the handler was renamed or removed, so update HANDLERS in tests/help_coverage.rs"));
    // The opening brace of the body: the first `{` after the parameter list.
    let mut i = start;
    while at(src, i) != Some('{') {
        i += 1;
        assert!(i < src.len(), "`fn {name}` has no body (no `{{`) — cannot derive its key map");
    }
    let body_start = i;
    let mut depth = 0usize;
    while i < src.len() {
        match src[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return (body_start, i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("`fn {name}` has unbalanced braces — cannot derive its key map");
}

/// The source text of the guard clause around `pos`, used to attribute modifiers.
///
/// Finds the enclosing statement/arm: backwards to the previous `;`, `{`, `}` or
/// `=>`, forwards to the next `;`, `{` or `=>`. Then narrows to the **`||`
/// branch** holding the literal, because that is the unit Rust evaluates: in
///
/// ```ignore
/// if matches!(key.code, KeyCode::F(10))
///     || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL))
/// ```
///
/// `f10` must **not** inherit `CONTROL` — only `ctrl+s` saves, plain `f10` also
/// saves, and they are separate alternatives. Attributing `ctrl+f10` would send
/// the catalog looking for a key that does not exist, and the real `f10` would
/// look undocumented. That false positive is exactly what this narrowing fixed:
/// taking the whole statement (or the whole body) is too coarse.
fn clause_around(src: &[char], pos: usize) -> String {
    let mut start = 0;
    let mut k = pos;
    while k > 0 {
        k -= 1;
        if matches!(src[k], ';' | '{' | '}') || (src[k] == '>' && k > 0 && src[k - 1] == '=') {
            start = k + 1;
            break;
        }
    }
    let mut end = src.len();
    let mut j = pos;
    while j < src.len() {
        if matches!(src[j], ';' | '{') || (src[j] == '=' && at(src, j + 1) == Some('>')) {
            end = j;
            break;
        }
        j += 1;
    }
    let region = &src[start..end];
    let offset = pos - start;
    // Split on `||` — a single `|` separates pattern alternatives of one arm and
    // must stay inside one clause, so only the doubled operator splits.
    let mut idx = 0;
    loop {
        let mut stop = region.len();
        let mut k = idx;
        while k + 1 < region.len() {
            if region[k] == '|' && region[k + 1] == '|' {
                stop = k;
                break;
            }
            k += 1;
        }
        if offset >= idx && offset < stop {
            return region[idx..stop].iter().collect();
        }
        if stop == region.len() {
            break;
        }
        idx = stop + 2;
    }
    region.iter().collect()
}

/// Modifier prefix of the guard clause around `pos`.
///
/// Scoped to the clause (not the line, not the body) so that a `SHIFT` guard on
/// one arm cannot be attributed to its neighbour, a multi-line arm still works,
/// and `a || b` alternatives do not leak into each other.
fn modifiers(src: &[char], pos: usize) -> &'static str {
    let clause = clause_around(src, pos);
    if clause.contains("KeyModifiers::CONTROL") {
        "ctrl+"
    } else if clause.contains("KeyModifiers::SHIFT") {
        "shift+"
    } else {
        ""
    }
}

fn read_ident(src: &[char], mut i: usize) -> (String, usize) {
    let start = i;
    while i < src.len() && (src[i].is_alphanumeric() || src[i] == '_') {
        i += 1;
    }
    (src[start..i].iter().collect(), i)
}

/// Read the char literal whose opening quote is at `i`; returns the payload.
fn read_char_literal(src: &[char], i: usize) -> char {
    let j = i + 1;
    if src[j] == '\\' {
        let escaped = src[j + 1];
        return match escaped {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            other => other,
        };
    }
    src[j]
}

/// Collapse `shift+<letter>` to the uppercase letter.
///
/// A terminal reports Shift+J as `Char('J')`, and this source spells the same
/// binding both ways: `Char('J')` in one arm and `Char('j')` guarded by
/// `KeyModifiers::SHIFT` in another. Both are one key press, so both normalise to
/// `J` — otherwise the catalog would have to document a binding twice under two
/// spellings, and the duplicate-token check would have to be disabled.
fn normalize_shift_letter(token: String) -> String {
    if let Some(rest) = token.strip_prefix("shift+") {
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && c.is_ascii_alphabetic()
        {
            return c.to_ascii_uppercase().to_string();
        }
    }
    token
}

/// Named `KeyCode` variants → the canonical spelling in team spec §3.
fn named_token(variant: &str) -> Option<&'static str> {
    Some(match variant {
        "Down" => "down",
        "Up" => "up",
        "Left" => "left",
        "Right" => "right",
        "Enter" => "enter",
        "Esc" => "esc",
        "Tab" => "tab",
        "BackTab" => "shift+tab",
        "Home" => "home",
        "End" => "end",
        "PageUp" => "pgup",
        "PageDown" => "pgdn",
        "Delete" => "delete",
        "Backspace" => "backspace",
        _ => return None,
    })
}

/// Every key token a handler body really dispatches.
fn handler_tokens(src: &[char], fn_name: &str) -> BTreeSet<String> {
    let (start, end) = function_span(src, fn_name);
    let body = &src[start..end];
    let mut tokens = BTreeSet::new();
    let mut i = 0;
    while i < body.len() {
        if !starts_with(body, i, "KeyCode::") {
            i += 1;
            continue;
        }
        let after = i + "KeyCode::".len();
        let (variant, j) = read_ident(body, after);
        let mods = modifiers(body, j);
        match variant.as_str() {
            // `Char('x')` is a literal binding; `Char(c)` / `Char(_)` is a
            // wildcard and yields no token (see the module docs).
            "Char" => {
                if at(body, j) == Some('(') && at(body, j + 1) == Some('\'') {
                    let c = read_char_literal(body, j + 1);
                    let token = if c == ' ' { "space".to_string() } else { c.to_string() };
                    tokens.insert(normalize_shift_letter(format!("{mods}{token}")));
                }
            }
            "F" => {
                let mut k = j;
                if at(body, k) == Some('(') {
                    k += 1;
                    let digits = k;
                    while k < body.len() && body[k].is_ascii_digit() {
                        k += 1;
                    }
                    let n: String = body[digits..k].iter().collect();
                    assert!(!n.is_empty(), "`KeyCode::F()` with no number at offset {i} in `fn {fn_name}`");
                    tokens.insert(normalize_shift_letter(format!("{mods}f{n}")));
                }
            }
            other => match named_token(other) {
                // BackTab already *is* shift+tab; prefixing again would invent
                // `shift+shift+tab`.
                Some("shift+tab") => {
                    tokens.insert("shift+tab".to_string());
                }
                Some(token) => {
                    tokens.insert(normalize_shift_letter(format!("{mods}{token}")));
                }
                None => panic!(
                    "unknown `KeyCode::{other}` in `fn {fn_name}` (src/keys.rs). Add it to named_token() in \
                     tests/help_coverage.rs *and* document it in src/help.rs — otherwise this key can never be \
                     covered."
                ),
            },
        }
        i = after;
    }
    tokens
}

// ---------------------------------------------------------------------------
// help-catalog side
// ---------------------------------------------------------------------------

/// Normalise one help-written token to the code-side spelling.
///
/// Returning `None` means "not a key token" (empty or whitespace), which is how a
/// trailing `/` or a stray space is tolerated rather than panicking.
fn normalize_token(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let mut t = t.replace('↓', "down").replace('↑', "up").replace('←', "left").replace('→', "right");
    // `shift+j` and `J` are the same key press in a terminal (Shift+J arrives as
    // an uppercase char). Accept both spellings so the catalog can say either.
    if let Some(rest) = t.strip_prefix("shift+") {
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && c.is_ascii_alphabetic()
        {
            t = c.to_ascii_uppercase().to_string();
        }
    }
    Some(t)
}

/// What a section documents: token → the `keys` field it came from (for errors).
fn documented(id: SectionId) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for section in SECTIONS.iter().filter(|s| s.id == id) {
        for item in section.items {
            collect_tokens(item.keys, &mut out);
        }
    }
    out
}

fn collect_tokens(keys: &str, out: &mut BTreeMap<String, String>) {
    if keys.is_empty() {
        return;
    }
    for raw in keys.split(" / ") {
        if let Some(token) = normalize_token(raw) {
            out.insert(token, keys.to_string());
        }
    }
}

/// Sections that document **gestures and glyphs**, not key bindings.
///
/// `Mouse` describes clicks and the wheel (`src/ui/hit.rs` is its truth source,
/// checked by `every_hit_target_is_described_in_the_mouse_section`), and `Status`
/// describes the five status glyphs. Neither is dispatched by `keys.rs`, so
/// comparing them against the key map would be a category error. They still have
/// to be non-empty and bilingual, which `the_catalog_is_complete_and_bilingual`
/// enforces.
fn is_prose_section(id: SectionId) -> bool {
    matches!(id, SectionId::Mouse | SectionId::Status)
}

/// The code tokens of every handler that belongs to `id`.
fn code_tokens(id: SectionId) -> BTreeSet<String> {
    let src = strip_noise(KEYS_SRC);
    HANDLERS.iter().filter(|(_, section)| *section == id).flat_map(|(name, _)| handler_tokens(&src, name)).collect()
}

// ---------------------------------------------------------------------------
// the two directions
// ---------------------------------------------------------------------------

/// **Forward: every dispatched key is documented.** ("Must not omit".)
#[test]
fn every_dispatched_key_is_documented() {
    let mut report = Vec::new();
    for id in SectionId::ALL.into_iter().filter(|id| !is_prose_section(*id)) {
        let doc = documented(id);
        for token in code_tokens(id) {
            if doc.contains_key(&token) || CODE_ONLY.contains(&(id.key(), token.as_str())) {
                continue;
            }
            report.push(format!(
                "  section `{}`: key `{token}` is dispatched by the code but never documented",
                id.key()
            ));
        }
    }
    assert!(
        report.is_empty(),
        "the in-app help omits keys the app really dispatches (add a HelpItem to the section named below):\n{}",
        report.join("\n")
    );
}

/// **Reverse: every documented key really is dispatched.** ("Must not lie".)
#[test]
fn every_documented_key_is_dispatched() {
    let mut report = Vec::new();
    for id in SectionId::ALL.into_iter().filter(|id| !is_prose_section(*id)) {
        let code = code_tokens(id);
        for (token, keys) in documented(id) {
            if code.contains(&token) || DOC_ONLY.contains(&(id.key(), token.as_str())) {
                continue;
            }
            report.push(format!(
                "  section `{}`: key `{token}` is documented in `{keys}` but the code never dispatches it",
                id.key()
            ));
        }
    }
    assert!(
        report.is_empty(),
        "the in-app help advertises keys that do nothing (fix or remove the rows named below):\n{}",
        report.join("\n")
    );
}

/// Every key-binding file is embedded, and no handler vanished in the move.
///
/// `concat!(include_str!(…))` is the one part of this test that cannot be derived
/// from the source it checks: a new `src/keys/<context>.rs` would compile fine
/// while its keys silently left the comparison, and the section it belongs to
/// would just look "documented but not dispatched" — a false red, or worse a
/// vacuous green if the section were prose. Two independent guards:
///
/// * the files listed in [`KEY_HANDLER_FILES`] are on disk (a rename breaks the
///   build loudly), and each one still contains at least one `KeyCode::` literal
///   after [`strip_noise`] (so an emptied or gutted file is caught);
/// * the directory and the list are the same set, so adding a dispatcher requires
///   adding it to [`KEYS_SRC`] in the same edit.
#[test]
fn every_key_handler_file_is_embedded() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/keys");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .expect("src/keys/ must exist: the key dispatch is split into it")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    on_disk.sort();

    // `testkit.rs` is test-only fixtures; every other submodule binds keys.
    let expected: Vec<String> = KEY_HANDLER_FILES
        .iter()
        .filter_map(|path| path.strip_prefix("src/keys/"))
        .map(str::to_string)
        .chain(std::iter::once("testkit.rs".to_string()))
        .collect();
    let mut expected = expected;
    expected.sort();
    assert_eq!(
        on_disk, expected,
        "`src/keys/` and KEY_HANDLER_FILES disagree. A file in the directory but not in the list \
         means its keys are NOT compared against the help — add it to KEYS_SRC and KEY_HANDLER_FILES."
    );

    let mut missing = Vec::new();
    for path in KEY_HANDLER_FILES {
        let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        let Ok(text) = std::fs::read_to_string(&full) else {
            missing.push(format!("  {path}: listed in KEY_HANDLER_FILES but does not exist"));
            continue;
        };
        let clean: String = strip_noise(&text).iter().collect();
        if !clean.contains("KeyCode::") {
            missing.push(format!("  {path}: listed as a key handler but dispatches no `KeyCode::`"));
        }
    }
    assert!(missing.is_empty(), "the embedded key sources are out of date:\n{}", missing.join("\n"));
}

/// The two sets really are compared, not trivially empty.
///
/// A parser that silently found nothing would make both directions above pass
/// vacuously. This pins the shapes: every handler must yield a body, and the
/// busiest sections must have a plausible number of keys.
#[test]
fn the_comparison_is_not_vacuous() {
    let src = strip_noise(KEYS_SRC);
    for (name, _) in HANDLERS {
        function_span(&src, name); // panics if the handler vanished
    }
    let main = code_tokens(SectionId::Main);
    assert!(main.len() >= 20, "main dispatches only {} keys — the parser is broken: {main:?}", main.len());
    assert!(main.contains("j") && main.contains("down"), "main must parse `j` and `↓`: {main:?}");
    for id in SectionId::ALL.into_iter().filter(|id| !is_prose_section(*id)) {
        assert!(
            !documented(id).is_empty() || !code_tokens(id).is_empty(),
            "section `{}` documents and dispatches nothing — either the parser lost it or the section is dead",
            id.key()
        );
    }
    // The prose sections are covered by other tests, but they must still say
    // something: a `Mouse` section that lost its rows would otherwise pass here.
    for id in [SectionId::Mouse, SectionId::Status] {
        let section = SECTIONS.iter().find(|s| s.id == id).expect("section exists");
        assert!(section.items.len() >= 5, "`{}` has only {} rows", id.key(), section.items.len());
    }
}

// ---------------------------------------------------------------------------
// catalog invariants
// ---------------------------------------------------------------------------

/// The catalog covers every context, has content, and speaks both languages.
#[test]
fn the_catalog_is_complete_and_bilingual() {
    assert!(!SECTIONS.is_empty(), "SECTIONS is empty");
    assert_eq!(SECTIONS.len(), SectionId::ALL.len(), "one section per SectionId, in order");
    for (i, id) in SectionId::ALL.iter().enumerate() {
        assert_eq!(SECTIONS[i].id, *id, "section {i} is out of order; the overlay shows them in this order");
    }
    for section in SECTIONS {
        assert!(
            !section.items.is_empty(),
            "section `{}` has no items: every context must say something",
            section.id.key()
        );
        assert!(!section.title_zh.is_empty() && !section.title_en.is_empty(), "`{}`: missing title", section.id.key());
        for item in section.items {
            assert!(!item.zh.is_empty(), "`{}`: empty Chinese for `{}`", section.id.key(), item.keys);
            assert!(!item.en.is_empty(), "`{}`: empty English for `{}`", section.id.key(), item.keys);
            assert!(
                item.zh.chars().any(is_cjk),
                "`{}`: Chinese description has no CJK for `{}` — the two languages must not be swapped",
                section.id.key(),
                item.keys
            );
            assert!(
                !item.en.chars().any(is_cjk),
                "`{}`: English description contains CJK for `{}`",
                section.id.key(),
                item.keys
            );
            assert_eq!(
                item.keys.is_empty(),
                item.is_note(),
                "`{}`: keys/is_note disagree for `{}`",
                section.id.key(),
                item.keys
            );
        }
    }
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// Tokens use the fixed spelling, so a typo cannot silently "cover" a key.
#[test]
fn every_documented_token_uses_the_canonical_spelling() {
    let mut report = Vec::new();
    for section in SECTIONS {
        for item in section.items {
            if item.keys.is_empty() {
                continue;
            }
            for raw in item.keys.split(" / ") {
                let Some(token) = normalize_token(raw) else { continue };
                if !is_allowed_token(&token) {
                    report.push(format!(
                        "  section `{}`: `{raw}` in `{}` normalises to `{token}`, which is not a canonical key \
                         spelling (see team spec §3 / NAMED_TOKENS)",
                        section.id.key(),
                        item.keys
                    ));
                }
            }
        }
    }
    assert!(report.is_empty(), "non-canonical key spellings would let a binding go uncovered:\n{}", report.join("\n"));
}

/// One key, one meaning per context. A token appearing twice in a section means
/// the section contradicts itself or two rows should be merged; the user cannot
/// tell which row to believe.
#[test]
fn no_section_documents_the_same_key_twice() {
    let mut report = Vec::new();
    for section in SECTIONS {
        let mut seen: BTreeMap<String, &str> = BTreeMap::new();
        for item in section.items {
            if item.keys.is_empty() {
                continue;
            }
            for raw in item.keys.split(" / ") {
                let Some(token) = normalize_token(raw) else { continue };
                if let Some(first) = seen.insert(token.clone(), item.keys) {
                    report.push(format!(
                        "  section `{}`: `{token}` is documented twice (in `{first}` and again in `{}`)",
                        section.id.key(),
                        item.keys
                    ));
                }
            }
        }
    }
    assert!(
        report.is_empty(),
        "a key with two meanings in one context makes the help ambiguous:\n{}",
        report.join("\n")
    );
}

// ---------------------------------------------------------------------------
// mouse: every HitTarget variant must be explained
// ---------------------------------------------------------------------------

/// Variant → text that must appear in `SectionId::Mouse` (Chinese or English,
/// matched case-insensitively).
///
/// The enum is the real list of things a click can hit (`src/ui/hit.rs`), so this
/// table is what ties it to the prose. Adding a `HitTarget` variant without a row
/// here fails `every_hit_target_is_described`, which is the mouse equivalent of
/// "must not omit".
const HIT_TARGET_EVIDENCE: &[(&str, &[&str])] = &[
    ("TunnelRow", &["单击表格行", "click a table row"]),
    ("TunnelList", &["表格列表空白处", "table list area"]),
    ("ImportCheckbox", &["导入复选框", "import checkbox"]),
    ("ImportRow", &["单击导入行", "click an import row"]),
    ("ImportColumnHeader", &["导入列头", "import column header"]),
    ("ImportFilterChip", &["chip 的 ×", "filter chip"]),
    ("ImportClearFilters", &["清除全部筛选", "clear all filters"]),
    ("ImportUndoFilter", &["撤销筛选", "undo filter"]),
    ("ImportCheckAll", &["全选", "select all"]),
    ("ImportInvert", &["反选", "invert"]),
    ("ImportUncheckAll", &["全不选", "select none"]),
    ("ImportList", &["导入列表空白处", "import list area"]),
    ("BatchAction", &["批量操作", "batch operation"]),
    ("BatchExit", &["退出选择模式", "exit selection mode"]),
    ("BatchSelectAll", &["多选模式「全选」", "select all in selection mode"]),
    ("FilterBar", &["过滤输入行", "filter line"]),
    ("FilterClear", &["「× 清除」", "clear affordance"]),
    ("Popup", &["弹窗吞掉点击", "popups swallow clicks"]),
];

/// Parse the variant names out of `pub enum HitTarget { … }`.
fn hit_target_variants(src: &str) -> BTreeSet<String> {
    let clean = strip_noise(src);
    let mut start = None;
    for i in 0..clean.len() {
        if starts_with(&clean, i, "enum HitTarget") {
            start = Some(i);
            break;
        }
    }
    let start = start.expect("cannot find `enum HitTarget` in src/ui/hit.rs");
    let mut i = start;
    while at(&clean, i) != Some('{') {
        i += 1;
    }
    let mut depth = 0usize;
    let mut parens = 0usize;
    let mut chunk_start = i + 1;
    let mut variants = BTreeSet::new();
    while i < clean.len() {
        match clean[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            '(' | '[' => parens += 1,
            ')' | ']' => parens = parens.saturating_sub(1),
            ',' if depth == 1 && parens == 0 => {
                push_variant(&clean[chunk_start..i], &mut variants);
                chunk_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    push_variant(&clean[chunk_start..i.min(clean.len())], &mut variants);
    assert!(!variants.is_empty(), "parsed no HitTarget variants from src/ui/hit.rs");
    variants
}

/// The leading identifier of one enum-variant chunk.
fn push_variant(chunk: &[char], out: &mut BTreeSet<String>) {
    let text: String = chunk.iter().collect();
    let name: String = text.trim_start().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    if !name.is_empty() {
        out.insert(name);
    }
}

/// The mouse section's full prose (both languages), lower-cased for matching.
fn mouse_section_text() -> String {
    let mut text = String::new();
    for section in SECTIONS.iter().filter(|s| s.id == SectionId::Mouse) {
        text.push_str(section.title_zh);
        text.push(' ');
        text.push_str(section.title_en);
        for item in section.items {
            text.push(' ');
            text.push_str(item.keys);
            text.push(' ');
            text.push_str(item.zh);
            text.push(' ');
            text.push_str(item.en);
        }
    }
    text.to_lowercase()
}

/// Every clickable thing the renderer can record is explained in the help.
#[test]
fn every_hit_target_is_described_in_the_mouse_section() {
    let variants = hit_target_variants(HIT_SRC);
    let text = mouse_section_text();
    let mut report = Vec::new();

    for variant in &variants {
        let Some((_, evidence)) = HIT_TARGET_EVIDENCE.iter().find(|(name, _)| name == variant) else {
            report.push(format!(
                "  HitTarget::{variant} has no row in HIT_TARGET_EVIDENCE — add one, and explain it in \
                 SectionId::Mouse"
            ));
            continue;
        };
        if !evidence.iter().any(|needle| text.contains(&needle.to_lowercase())) {
            report
                .push(format!("  section `mouse` never explains HitTarget::{variant} (expected one of {evidence:?})"));
        }
    }
    for (stale, _) in HIT_TARGET_EVIDENCE {
        if !variants.contains(*stale) {
            report
                .push(format!("  HIT_TARGET_EVIDENCE is stale: HitTarget::{stale} no longer exists in src/ui/hit.rs"));
        }
    }
    assert!(
        report.is_empty(),
        "the mouse help and the real click targets disagree ({} variants checked):\n{}",
        variants.len(),
        report.join("\n")
    );
    assert!(variants.len() >= 15, "only {} HitTarget variants parsed — parser is broken", variants.len());
}

// ---------------------------------------------------------------------------
// the human-facing key reference must cover the catalog
// ---------------------------------------------------------------------------

/// `docs/keymap.md` documents every token the catalog can show.
///
/// The in-app help is only a summary of what fits on screen; the keymap is the
/// place a user goes to look something up. It drifting behind the catalog is the
/// same bug as the catalog drifting behind the code, one level up.
#[test]
fn every_catalog_token_appears_in_the_keymap_doc() {
    let mut tokens = BTreeSet::new();
    for section in SECTIONS {
        for item in section.items {
            if item.keys.is_empty() {
                continue;
            }
            for raw in item.keys.split(" / ") {
                if let Some(token) = normalize_token(raw) {
                    tokens.insert(token);
                }
            }
        }
    }
    assert!(!tokens.is_empty(), "no key tokens in the catalog at all");
    let missing: Vec<&String> = tokens
        .iter()
        .filter(|token| !display_spellings(token).iter().any(|s| KEYMAP_DOC.contains(&format!("`{s}`"))))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/keymap.md does not document these catalog keys (write them as a backticked key cell): {missing:?}"
    );
}

/// The ways a token may legitimately be written in the keymap.
///
/// The docs use the same notation a user sees on screen (`↓`, `shift+↓`), while
/// the catalog/comparison uses the canonical names (`down`, `shift+down`).
/// Accepting both halves of that pair is what lets the keymap read naturally
/// without weakening the check: the token still has to appear in a key cell.
fn display_spellings(token: &str) -> Vec<String> {
    let mut out = vec![token.to_string()];
    let (mods, key) = match token.strip_prefix("shift+") {
        Some(rest) => ("shift+", rest),
        None => ("", token),
    };
    let arrow = match key {
        "down" => Some("↓"),
        "up" => Some("↑"),
        "left" => Some("←"),
        "right" => Some("→"),
        _ => None,
    };
    if let Some(arrow) = arrow {
        out.push(format!("{mods}{arrow}"));
    }
    out
}

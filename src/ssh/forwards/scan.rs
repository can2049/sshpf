//! The scanner: `~/.ssh/config` text → the forwarding directives that apply to one
//! host, in file order.
//!
//! Three concerns, and only these:
//!
//! 1. **Scoping** — which `Host` block are we in, does its pattern list match the
//!    alias, is the block excluded by a `!` clause. A `Match` block is always
//!    inactive: we do not model connection contexts, and treating its forwards as
//!    live could silently enable something the user gated behind it.
//! 2. **`Include` expansion** — pattern → concrete files, with a depth guard (a
//!    cycle cannot hang the UI thread) and a queue cap (a generated `config.d`
//!    cannot allocate without bound).
//! 3. **`Host` pattern matching** — `*` / `?`, case-insensitive, per OpenSSH.
//!
//! Argument parsing is [`crate::ssh::forwards::parse`]'s job; this file only
//! decides *whether* a directive applies.

use std::path::{Path, PathBuf};

use crate::model::{ForwardKind, SshForward};

/// Cap on queued `Include` targets.
///
/// The depth guard stops a *cycle*, but a single file may name thousands of
/// includes (a generated `config.d`), and every one is pushed before the depth
/// check runs. The cap bounds the queue instead of the processing: running out
/// means "we stopped reading this config", which is worth recording, not worth
/// allocating unboundedly for.
const MAX_PENDING_INCLUDES: usize = 256;

/// Drop repeats of the same forwarding spec (a `Host *` block re-declaring what
/// a specific block already declared), keeping the first occurrence.
pub(super) fn dedup(rows: Vec<SshForward>) -> Vec<SshForward> {
    let mut seen = std::collections::HashSet::new();
    rows.into_iter().filter(|r| seen.insert(r.key())).collect()
}

/// Which `Host` patterns are currently in effect while scanning a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Before the first `Host` line: OpenSSH treats this as a global block that
    /// applies to every host, exactly like `Host *`.
    Global,
    /// Inside a `Host` block; separate booleans so the two ways of deciding
    /// whether to skip the whole block can be combined without losing info.
    InBlock { applies: bool, excluded: bool },
}

impl Scope {
    /// Whether forwarding directives in this scope apply to the queried alias.
    fn is_active(self) -> bool {
        match self {
            Scope::Global => true,
            Scope::InBlock { applies, excluded } => applies && !excluded,
        }
    }
}

/// Scan one config file's text for forwarding directives that apply to `alias`.
///
/// `base` resolves the paths of `Include` globs. Any `Include` found is pushed
/// onto `stack` for the caller to read after this file (so the caller owns the
/// I/O and the recursion guard).
pub(super) fn scan_config(
    text: &str,
    alias: &str,
    base: &Path,
    stack: &mut Vec<PathBuf>,
    out: &mut Vec<SshForward>,
    line_offset: usize,
) {
    let mut scope = Scope::Global;
    for (lineno, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((verb, rest)) = split_verb(line) else {
            continue;
        };
        // Lines that only move the scope produce no row; the rest are either an
        // `Include` (queue work for the caller) or a forwarding directive.
        if let Some(next) = scope_for_verb(&verb, rest, alias) {
            scope = next;
            continue;
        }
        if verb == "include" {
            queue_includes(rest, base, stack);
            continue;
        }
        let Some(kind) = forward_kind(&verb) else {
            continue;
        };
        if !scope.is_active() {
            continue;
        }
        if let Some(mut fwd) = super::parse::parse_directive(kind, rest, alias, lineno + 1 + line_offset) {
            fwd.raw = line.to_string();
            out.push(fwd);
        }
    }
}

/// The new scope when `verb` opens a block, or `None` for anything else.
///
/// A `Match` block is deliberately inactive: it only applies to some connection
/// contexts, which we do not model, and treating its forwards as live could
/// silently enable something the user gated behind it.
fn scope_for_verb(verb: &str, rest: &str, alias: &str) -> Option<Scope> {
    match verb {
        "host" => {
            let (applies, excluded) = host_block_matches(rest, alias);
            Some(Scope::InBlock { applies, excluded })
        }
        "match" => Some(Scope::InBlock { applies: false, excluded: true }),
        _ => None,
    }
}

/// Resolve one `Include` line into the files the caller will read next.
///
/// The cap bounds the *queue*, not the processing: a generated `config.d` can
/// name thousands of entries, and running out means "we stopped reading this
/// config", which is worth a record rather than unbounded allocation.
fn queue_includes(rest: &str, base: &Path, stack: &mut Vec<PathBuf>) {
    for pat in shell_words(rest) {
        for p in expand_include(&pat, base) {
            if stack.len() >= MAX_PENDING_INCLUDES {
                crate::logging::warn(
                    "sshconfig.include_capped",
                    &[("max", &MAX_PENDING_INCLUDES), ("msg", &"Include 数量超过上限，剩余文件未读取")],
                );
                continue;
            }
            stack.push(p);
        }
    }
}

/// The forwarding flavour a verb names, if it is one of the three we import.
fn forward_kind(verb: &str) -> Option<ForwardKind> {
    match verb {
        "localforward" => Some(ForwardKind::Local),
        "remoteforward" => Some(ForwardKind::Remote),
        "dynamicforward" => Some(ForwardKind::Dynamic),
        _ => None,
    }
}

/// Split `Verb args…` (or `Verb=args`) on the first whitespace, lower-casing
/// the verb.
pub(super) fn split_verb(line: &str) -> Option<(String, &str)> {
    let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    if end == 0 {
        return None;
    }
    let verb = line[..end].to_ascii_lowercase();
    let rest = line[end..].trim_start_matches(['=', ' ', '\t']);
    Some((verb, rest))
}

/// Whether a `Host <patterns>` clause list matches `alias`.
///
/// Returns `(any_positive_matches, any_negation_matches)`; the block is active
/// when the first is true and the second is false.
fn host_block_matches(rest: &str, alias: &str) -> (bool, bool) {
    let mut positive = false;
    let mut negative = false;
    for pat in shell_words(rest) {
        let (negated, pattern) = match pat.strip_prefix('!') {
            Some(rest) => (true, rest.to_string()),
            None => (false, pat),
        };
        if glob_match(&pattern, alias) {
            if negated {
                negative = true;
            } else {
                positive = true;
            }
        }
    }
    (positive, negative)
}

/// OpenSSH `Host` pattern matching: `*` and `?`, **case-insensitive**.
///
/// A one-line adapter over [`crate::text::glob_match`]. The matcher used to exist
/// twice — here (case-insensitive, per OpenSSH `Host` semantics) and in
/// `ssh/known_hosts.rs` (case-sensitive, per that file format) — and the copies
/// had already begun to drift. Only the case rule belongs here: it is a property
/// of the ssh config format, not of glob matching.
fn glob_match(pattern: &str, name: &str) -> bool {
    crate::text::glob_match(pattern, name, true)
}

/// Expand `~` and `~user`-style include paths into concrete files, honouring
/// globs. A plain relative path resolves against `base` (the `.ssh` directory).
pub(super) fn expand_include(pat: &str, base: &Path) -> Vec<PathBuf> {
    let expanded = if let Some(rest) = pat.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest).to_string_lossy().to_string(),
            None => pat.to_string(),
        }
    } else if pat.starts_with('/') {
        pat.to_string()
    } else {
        base.join(pat).to_string_lossy().to_string()
    };
    if !expanded.contains('*') && !expanded.contains('?') {
        let p = PathBuf::from(&expanded);
        return if p.is_file() { vec![p] } else { Vec::new() };
    }
    // Globbing: split the pattern at the last separator and match entries.
    let path = PathBuf::from(&expanded);
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let pat = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| p.file_name().is_some_and(|n| glob_match(&pat, &n.to_string_lossy())))
        .collect();
    out.sort();
    out
}

/// Split a directive's arguments, honouring double quotes (paths with spaces).
pub(super) fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut had_quote = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                had_quote = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() || had_quote {
                    out.push(std::mem::take(&mut cur));
                    had_quote = false;
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || had_quote {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scan `text` as one config file, with a nonexistent `Include` base.
    fn scan(text: &str, alias: &str) -> Vec<SshForward> {
        let mut stack = Vec::new();
        let mut out = Vec::new();
        scan_config(text, alias, std::path::Path::new("/nonexistent"), &mut stack, &mut out, 0);
        out
    }

    #[test]
    fn directives_are_scoped_to_their_host_block() {
        let text = "\
Host a
  LocalForward 1111 127.0.0.1:1111
Host b
  LocalForward 2222 127.0.0.1:2222
";
        let a = scan(text, "a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].listen_port, 1111);
        let b = scan(text, "b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].listen_port, 2222);
        // An unrelated host gets nothing.
        assert!(scan(text, "c").is_empty());
    }

    #[test]
    fn host_star_forwards_apply_to_every_host() {
        let text = "\
Host *
  LocalForward 9000 127.0.0.1:9000
Host web
  LocalForward 8080 10.0.0.1:80
";
        let rows = scan(text, "web");
        let ports: Vec<u16> = rows.iter().map(|r| r.listen_port).collect();
        assert_eq!(ports, vec![9000, 8080], "global block first, in file order");
    }

    #[test]
    fn wildcard_and_query_patterns_match() {
        let text = "\
Host *.example.com
  LocalForward 7001 127.0.0.1:7001
Host db?
  LocalForward 7002 127.0.0.1:7002
";
        assert_eq!(scan(text, "api.example.com").len(), 1);
        assert_eq!(scan(text, "db1").len(), 1);
        assert_eq!(scan(text, "db12").len(), 0, "? matches exactly one char");
    }

    #[test]
    fn negated_clauses_beat_positive_ones() {
        let text = "\
Host prod-* !prod-db
  LocalForward 7003 127.0.0.1:7003
";
        assert_eq!(scan(text, "prod-web").len(), 1);
        assert!(scan(text, "prod-db").is_empty(), "negation must exclude the block");
    }

    #[test]
    fn match_blocks_are_never_imported() {
        // A forward gated behind `Match` must not be enabled implicitly.
        let text = "\
Host web
  LocalForward 7004 127.0.0.1:7004
Match host other
  LocalForward 7005 127.0.0.1:7005
";
        let rows = scan(text, "web");
        assert_eq!(rows.len(), 1, "{rows:#?}");
        assert_eq!(rows[0].listen_port, 7004);
    }

    #[test]
    fn commented_out_directives_are_ignored() {
        // The user's own config is full of `# LocalForward 3081 …`.
        let text = "\
Host web
  # LocalForward 3081 127.0.0.1:3081
  LocalForward 3082 127.0.0.1:3082
";
        let rows = scan(text, "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_port, 3082);
    }

    #[test]
    fn duplicate_forwards_from_several_blocks_collapse() {
        let text = "\
Host web
  LocalForward 8080 127.0.0.1:80
Host *
  LocalForward 8080 127.0.0.1:80
";
        let mut stack = Vec::new();
        let mut out = Vec::new();
        scan_config(text, "web", Path::new("/nonexistent"), &mut stack, &mut out, 0);
        let rows = dedup(out);
        assert_eq!(rows.len(), 1, "one port must not produce two tunnels: {rows:#?}");
    }

    #[test]
    fn blank_and_comment_only_files_yield_nothing() {
        assert!(scan("", "web").is_empty());
        assert!(scan("# just a comment\n\n", "web").is_empty());
    }

    #[test]
    fn line_numbers_are_one_based_and_point_at_the_directive() {
        let text = "Host web\n  HostName 1.2.3.4\n\n  LocalForward 8080 127.0.0.1:80\n";
        let rows = scan(text, "web");
        assert_eq!(rows[0].line, 4);
    }

    #[test]
    fn glob_match_handles_the_documented_wildcards() {
        assert!(glob_match("web", "web"));
        assert!(glob_match("WEB", "web"), "case-insensitive, like OpenSSH");
        assert!(glob_match("*", "anything"));
        assert!(glob_match("web-*", "web-1"));
        assert!(glob_match("web-*-db", "web-1-db"));
        assert!(glob_match("db?", "db1"));
        assert!(!glob_match("db?", "db"));
        assert!(!glob_match("db?", "db12"));
        assert!(!glob_match("web", "web2"));
        assert!(glob_match("*.example.com", "a.b.example.com"));
    }

    #[test]
    fn include_globs_expand_against_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.conf"), "Host x\n  LocalForward 1 1.1.1.1:1\n").unwrap();
        std::fs::write(dir.path().join("b.conf"), "").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
        let mut files = expand_include("*.conf", dir.path());
        files.sort();
        assert_eq!(files.len(), 2, "{files:?}");
        // A literal existing path comes back as-is; a missing one is dropped.
        assert_eq!(expand_include("a.conf", dir.path()).len(), 1);
        assert!(expand_include("nope.conf", dir.path()).is_empty());
    }

    #[test]
    fn shell_words_keeps_quoted_paths_together() {
        assert_eq!(shell_words("8080 127.0.0.1:80"), vec!["8080", "127.0.0.1:80"]);
        assert_eq!(shell_words("\"/tmp/a b/x.sock\" /tmp/y"), vec!["/tmp/a b/x.sock", "/tmp/y"]);
        assert_eq!(shell_words("   "), Vec::<String>::new());
    }
}

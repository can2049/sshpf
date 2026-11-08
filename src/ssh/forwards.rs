//! Forwards declared in `~/.ssh/config` (`LocalForward` / `RemoteForward` /
//! `DynamicForward`).
//!
//! ssh config already says how to reach a host *and* which ports the user wanted
//! forwarded on it. `sshpf` never writes to `~/.ssh/config`, but it can read
//! those directives and offer them as ready-made tunnels: that is the whole
//! point of this module.
//!
//! # Why not `ssh2-config`'s own fields
//!
//! The parser in use models `RemoteForward` but treats `LocalForward` and
//! `DynamicForward` as *unsupported* fields: with `ParseRule::ALLOW_UNKNOWN_FIELDS`
//! they land in `HostParams.unsupported_fields` (or are silently dropped), one
//! argument list per directive name — which loses both the order and the count
//! when a host declares several of them. So this module scans the config text
//! itself for the three forwarding verbs, and uses `ssh2-config` only for the
//! connection parameters (`ssh/config.rs`).
//!
//! Scanning is deliberately line-based and quote-aware rather than a full config
//! parser: the directives we care about are single-line and, unlike `Host`,
//! `Match` and `Include`, their scoping rules are simple (applies to the `Host`
//! block it appears in; `Host *` blocks apply to everyone).
//!
//! # Scoping, the rules we implement
//!
//! * Every `Host` block whose pattern matches the alias contributes its forwards,
//!   in file order — this mirrors OpenSSH, which *accumulates* forwarding
//!   directives from every matching block (`Host web` plus `Host *`).
//! * Negated clauses (`!prod`) win over positive ones, as in OpenSSH.
//! * `Include`d files are followed, because users do put forwards in a separate
//!   `config.d/` file.
//! * Two rows with the same [`SshForward::key`] are de-duplicated: `Host *`
//!   repeating a forward that a specific block already declared must not produce
//!   two tunnels for one port.
//!
//! Unix-domain-socket forms (`LocalForward /run/a.sock /run/b.sock`) and `-R`
//! without a destination are *not* silently dropped: [`SshForward`] carries
//! enough to render them, and `unsupported_reason()` explains why they cannot be
//! imported.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::model::{ForwardKind, SshForward};

use super::config::{default_config_paths, strip_match_blocks};

/// Largest number of `Include` hops we follow, so a cyclic include cannot hang
/// the UI thread.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Cap on queued `Include` targets.
///
/// The depth guard stops a *cycle*, but a single file may name thousands of
/// includes (a generated `config.d`), and every one is pushed before the depth
/// check runs. The cap bounds the queue instead of the processing: running out
/// means "we stopped reading this config", which is worth recording, not worth
/// allocating unboundedly for.
const MAX_PENDING_INCLUDES: usize = 256;

/// Read every forwarding directive that applies to `alias`.
///
/// The result is in file order (matching block order, then line order inside a
/// block), de-duplicated by [`SshForward::key`].
pub fn forwards_for_host(alias: &str) -> Result<Vec<SshForward>> {
    // `first()` rather than `paths[0]`: the emptiness check makes the index safe
    // today, but this parses user-supplied files and the index would be a panic if
    // the check ever moved.
    let Some(main) = default_config_paths().first().cloned() else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    let mut stack = vec![main];
    let mut depth = 0;
    let mut seen_files: Vec<PathBuf> = Vec::new();
    while let Some(path) = stack.pop() {
        if depth > MAX_INCLUDE_DEPTH || seen_files.contains(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        seen_files.push(path.clone());
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        scan_config(&strip_match_blocks(&text), alias, &base, &mut stack, &mut found, 0);
        depth += 1;
    }
    Ok(dedup(found))
}

/// Drop repeats of the same forwarding spec (a `Host *` block re-declaring what
/// a specific block already declared), keeping the first occurrence.
fn dedup(rows: Vec<SshForward>) -> Vec<SshForward> {
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

/// Scan one config file's text for forwarding directives that apply to `alias`.
///
/// `base` resolves the paths of `Include` globs. Any `Include` found is pushed
/// onto `stack` for the caller to read after this file (so the caller owns the
/// I/O and the recursion guard).
fn scan_config(
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
        match verb.as_str() {
            "host" => {
                let (applies, excluded) = host_block_matches(rest, alias);
                scope = Scope::InBlock { applies, excluded };
                continue;
            }
            "match" => {
                // A `Match` block only applies to some connection contexts; we
                // do not model them, and treating its forwards as active could
                // silently enable a forward the user gated behind `Match`.
                scope = Scope::InBlock { applies: false, excluded: true };
                continue;
            }
            "include" => {
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
                continue;
            }
            _ => {}
        }

        let kind = match verb.as_str() {
            "localforward" => ForwardKind::Local,
            "remoteforward" => ForwardKind::Remote,
            "dynamicforward" => ForwardKind::Dynamic,
            _ => continue,
        };
        let active = match scope {
            Scope::Global => true,
            Scope::InBlock { applies, excluded } => applies && !excluded,
        };
        if !active {
            continue;
        }
        if let Some(mut fwd) = parse_directive(kind, rest, alias, lineno + 1 + line_offset) {
            fwd.raw = line.to_string();
            out.push(fwd);
        }
    }
}

/// Split `Verb args…` (or `Verb=args`) on the first whitespace, lower-casing
/// the verb.
fn split_verb(line: &str) -> Option<(String, &str)> {
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

/// OpenSSH wildcard matching: `*` and `?`, case-insensitive.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    // Iterative backtracking match; linear in practice and needs no dependency.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Expand `~` and `~user`-style include paths into concrete files, honouring
/// globs. A plain relative path resolves against `base` (the `.ssh` directory).
fn expand_include(pat: &str, base: &Path) -> Vec<PathBuf> {
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
fn shell_words(s: &str) -> Vec<String> {
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

/// Parse the arguments of one forwarding directive.
///
/// Accepted shapes (the ones OpenSSH documents):
///
/// ```text
/// LocalForward  [bind_address:]port  host:hostport
/// RemoteForward [bind_address:]port  host:hostport
/// DynamicForward [bind_address:]port
/// ```
///
/// `[bind_address:]port` may also be written as two arguments (`8080` or
/// `127.0.0.1 8080`-style arguments are accepted where unambiguous). Sockets and
/// anything we cannot map onto a `TunnelConfig` produce a row with port 0 so the
/// UI can list it as un-importable instead of hiding it.
fn parse_directive(kind: ForwardKind, rest: &str, alias: &str, line: usize) -> Option<SshForward> {
    let args = shell_words(rest);
    let mut fwd = SshForward {
        kind,
        listen_addr: String::new(),
        listen_port: 0,
        target_host: String::new(),
        target_port: 0,
        host_alias: alias.to_string(),
        line,
        raw: String::new(),
    };

    match kind {
        ForwardKind::Dynamic => {
            let spec = args.first()?;
            let (addr, port) = split_host_port(spec)?;
            fwd.listen_addr = addr.unwrap_or_default();
            fwd.listen_port = port;
        }
        ForwardKind::Local | ForwardKind::Remote => {
            // Either `<listen> <target>` or `<addr> <port> <target>`.
            let (listen_spec, target_spec) = match args.len() {
                0 => return Some(fwd),
                // One argument: a listen endpoint with no destination (an
                // un-importable `-R`, or a truncated line). Still report the
                // port, so the table can explain what is wrong with the row.
                1 => {
                    if let Some((addr, port)) = split_host_port(&args[0]) {
                        fwd.listen_addr = addr.unwrap_or_default();
                        fwd.listen_port = port;
                    }
                    return Some(fwd);
                }
                2 => (args[0].as_str(), args[1].as_str()),
                _ => {
                    // Three+ args: `<addr> <port> <target…>`.
                    let combined = format!("{}:{}", args[0], args[1]);
                    // Leak-free: build the two specs up front.
                    let target = args[2..].join(" ");
                    let (addr, port) = split_host_port(&combined)?;
                    fwd.listen_addr = addr.unwrap_or_default();
                    fwd.listen_port = port;
                    let (host, tport) = split_host_port(&target)?;
                    fwd.target_host = host.unwrap_or_default();
                    fwd.target_port = tport;
                    return Some(fwd);
                }
            };
            match split_host_port(listen_spec) {
                Some((addr, port)) => {
                    fwd.listen_addr = addr.unwrap_or_default();
                    fwd.listen_port = port;
                }
                // A unix-socket listen path is not a host:port; leave port 0 so
                // the row is listed as unsupported rather than misparsed.
                None => return Some(fwd),
            }
            match split_host_port(target_spec) {
                Some((host, port)) => {
                    fwd.target_host = host.unwrap_or_default();
                    fwd.target_port = port;
                }
                None => return Some(fwd),
            }
        }
    }
    Some(fwd)
}

/// Split `[host:]port` into `(host, port)`.
///
/// `None` when the port is not a number (a socket path, or a hostname that the
/// user left as `host:service`).
fn split_host_port(spec: &str) -> Option<(Option<String>, u16)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    // `[::1]:8080` — bracketed IPv6.
    if let Some(rest) = spec.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':').and_then(|p| p.parse::<u16>().ok())?;
        return Some((Some(host.to_string()), port));
    }
    match spec.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse::<u16>().ok()?;
            Some((Some(host.to_string()), port))
        }
        // Bare `8080`: listen on the default bind address.
        None => Some((None, spec.parse::<u16>().ok()?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch `.ssh`-like directory plus helpers to scan its text.
    fn scan(text: &str, alias: &str) -> Vec<SshForward> {
        let mut stack = Vec::new();
        let mut out = Vec::new();
        scan_config(text, alias, Path::new("/nonexistent"), &mut stack, &mut out, 0);
        out
    }

    #[test]
    fn parses_the_three_directive_kinds() {
        let text = "\
Host cloud_pc
  HostName 10.0.0.1
  LocalForward 18789 127.0.0.1:18789
  LocalForward 127.0.0.1:44897 127.0.0.1:44897
  DynamicForward 1080
  RemoteForward 7000 localhost:22
";
        let rows = scan(text, "cloud_pc");
        assert_eq!(rows.len(), 4, "{rows:#?}");

        assert_eq!(rows[0].kind, ForwardKind::Local);
        assert_eq!(rows[0].listen_addr, "");
        assert_eq!(rows[0].listen_port, 18789);
        assert_eq!(rows[0].target_host, "127.0.0.1");
        assert_eq!(rows[0].target_port, 18789);
        assert_eq!(rows[0].line, 3);
        assert_eq!(rows[0].host_alias, "cloud_pc");

        assert_eq!(rows[1].listen_addr, "127.0.0.1");
        assert_eq!(rows[1].listen_port, 44897);

        assert_eq!(rows[2].kind, ForwardKind::Dynamic);
        assert_eq!(rows[2].listen_port, 1080);
        assert_eq!(rows[2].target_display(), "(SOCKS5)");

        assert_eq!(rows[3].kind, ForwardKind::Remote);
        assert_eq!(rows[3].listen_port, 7000);
        assert_eq!(rows[3].target_port, 22);
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
    fn unix_sockets_are_listed_but_unsupported() {
        let text = "\
Host web
  LocalForward /run/app.sock /run/remote.sock
  RemoteForward /tmp/x.sock
";
        let rows = scan(text, "web");
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.listen_port, 0);
            assert!(r.unsupported_reason().is_some(), "{r:?} should be unsupported");
        }
    }

    #[test]
    fn remote_forward_without_a_destination_is_unsupported() {
        let rows = scan("Host web\n  RemoteForward 7006\n", "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_port, 7006);
        assert!(rows[0].unsupported_reason().is_some(), "no destination means nothing to forward to");
    }

    #[test]
    fn three_argument_form_is_accepted() {
        let rows = scan("Host web\n  LocalForward 127.0.0.1 8080 10.0.0.1:80\n", "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_addr, "127.0.0.1");
        assert_eq!(rows[0].listen_port, 8080);
        assert_eq!(rows[0].target_host, "10.0.0.1");
        assert_eq!(rows[0].target_port, 80);
    }

    #[test]
    fn ipv6_listen_and_target_parse() {
        let rows = scan("Host web\n  LocalForward [::1]:8080 [2001:db8::1]:443\n", "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_addr, "::1");
        assert_eq!(rows[0].listen_port, 8080);
        assert_eq!(rows[0].target_host, "2001:db8::1");
        assert_eq!(rows[0].target_port, 443);
    }

    #[test]
    fn verb_spelling_and_case_do_not_matter() {
        let rows = scan("Host web\n  localforward 8081 127.0.0.1:81\n", "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_port, 8081);
        let rows = scan("Host web\n  LocalForward=8082 127.0.0.1:82\n", "web");
        assert_eq!(rows.len(), 1, "`Verb=args` spelling must work");
    }

    #[test]
    fn raw_text_is_preserved_for_display() {
        let rows = scan("Host web\n  LocalForward 8080 127.0.0.1:80\n", "web");
        assert_eq!(rows[0].raw, "LocalForward 8080 127.0.0.1:80");
    }

    #[test]
    fn suggested_name_mentions_host_kind_and_port() {
        let rows = scan("Host cloud_pc\n  LocalForward 8080 127.0.0.1:80\n", "cloud_pc");
        assert_eq!(rows[0].suggested_name(), "cloud_pc-L-8080");
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
}

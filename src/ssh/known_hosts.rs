//! Host-key verification against `known_hosts`.
//!
//! # Why not `russh::keys::known_hosts`
//!
//! Its `check_known_hosts_path` is a `bool` API and is measurably wrong in four
//! ways (reproduced, see `docs/research/pure-rust-ssh-spike.md` §8):
//!
//! | case | OpenSSH | `russh` |
//! |---|---|---|
//! | `*.example.com` entry | match | **no match** |
//! | Tab-separated line | match | **no match** |
//! | leading whitespace | match | **no match** |
//! | `@revoked` + normal entry, same key | **reject** | **`Ok(true)` — accepts a revoked key** |
//!
//! The last one is a security bug, and the `bool` return cannot express the
//! four outcomes this app needs (unknown / changed / revoked / CA). So we parse
//! and match ourselves, and only use `ssh_key` types for the key material.
//!
//! # Semantics implemented
//!
//! * `host` patterns: `*` and `?` globs, `!` negation, comma lists,
//!   `[host]:port` for non-default ports.
//! * `|1|salt|hash` hashed entries (HMAC-SHA1).
//! * `@revoked` wins over any matching acceptance — a revoked key is never
//!   accepted, regardless of other lines.
//! * `@cert-authority` / `@cert-authority` entries are recognised and skipped
//!   for plain host-key matching (we do not implement CA-signed host certs).
//!
//! # Policy
//!
//! [`HostKeyPolicy`] mirrors `StrictHostKeyChecking`:
//! * `Strict` — unknown host or changed key is a hard failure.
//! * `AcceptNew` — unknown host is learned (written back); a *changed* key is
//!   still a hard failure.
//! * `Ask` — unknown host is reported back so the caller can prompt; not used
//!   by default (the tunnel starts unattended).

use std::io::Write;
use std::path::{Path, PathBuf};

use russh::keys::PublicKey;

/// Result of verifying a server key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyVerdict {
    /// Key matched a known_hosts entry.
    Trusted,
    /// Host has no entry at all.
    Unknown,
    /// Host is known but presented a different key. Always fatal.
    Changed { line: usize },
    /// The presented key is explicitly revoked. Always fatal.
    Revoked { line: usize },
}

impl HostKeyVerdict {
    /// Whether this verdict permits the connection under `policy`.
    pub fn is_acceptable(&self, policy: HostKeyPolicy) -> bool {
        match self {
            HostKeyVerdict::Trusted => true,
            HostKeyVerdict::Unknown => matches!(policy, HostKeyPolicy::AcceptNew | HostKeyPolicy::Ask),
            HostKeyVerdict::Changed { .. } | HostKeyVerdict::Revoked { .. } => false,
        }
    }

    /// Short human-readable reason for the failure detail panel.
    pub fn describe(&self, host: &str, port: u16) -> String {
        let hp = format_host(host, port);
        match self {
            HostKeyVerdict::Trusted => format!("{hp} 的主机密钥已校验"),
            HostKeyVerdict::Unknown => format!("{hp} 不在 known_hosts 中"),
            HostKeyVerdict::Changed { line } => {
                format!("{hp} 的主机密钥已改变（known_hosts 第 {line} 行），可能存在中间人攻击")
            }
            HostKeyVerdict::Revoked { line } => format!("{hp} 的主机密钥已被吊销（known_hosts 第 {line} 行）"),
        }
    }
}

/// How to treat an unknown host key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKeyPolicy {
    /// Refuse to connect to an unknown host.
    Strict,
    /// Learn the key on first contact; still reject a *changed* key.
    #[default]
    AcceptNew,
    /// Report unknown hosts to the caller so it can prompt.
    Ask,
}

/// `host` for port 22, `[host]:port` otherwise — the OpenSSH spelling.
fn format_host(host: &str, port: u16) -> String {
    if port == 22 { host.to_string() } else { format!("[{host}]:{port}") }
}

/// One parsed `known_hosts` line.
#[derive(Debug)]
struct Entry {
    line_no: usize,
    /// `true` for `@revoked`, `false` for a normal host key entry.
    revoked: bool,
    /// `true` for `@cert-authority` — recognised, not used for host matching.
    cert_authority: bool,
    /// The raw host-pattern field (may be comma-separated, may be `|1|…`).
    hosts: String,
    key: PublicKey,
}

fn known_hosts_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("SSHPF_KNOWN_HOSTS") {
        return Some(PathBuf::from(p));
    }
    dirs::home_dir().map(|h| h.join(".ssh").join("known_hosts"))
}

/// Parse the file, skipping unparsable lines (OpenSSH also ignores them).
fn parse_entries(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        // Fields may be separated by any run of spaces/tabs; leading whitespace
        // is ignored (russh fails on both — see the module docs).
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace().peekable();
        let Some(mut first) = fields.next() else { continue };
        let mut revoked = false;
        let mut cert_authority = false;
        if first.starts_with('@') {
            match first {
                "@revoked" => revoked = true,
                "@cert-authority" => cert_authority = true,
                _ => continue, // unknown marker: skip, like OpenSSH
            }
            let Some(next) = fields.next() else { continue };
            first = next;
        }
        let hosts = first.to_string();
        let Some(algo) = fields.next() else { continue };
        let Some(b64) = fields.next() else { continue };
        let Ok(key) = PublicKey::from_openssh(&format!("{algo} {b64}")) else {
            continue;
        };
        out.push(Entry { line_no, revoked, cert_authority, hosts, key });
    }
    out
}

/// Does `pattern_field` (possibly a comma list, possibly hashed) match `host`?
fn host_field_matches(pattern_field: &str, host: &str) -> bool {
    for pattern in pattern_field.split(',') {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        if let Some(rest) = pattern.strip_prefix("|1|") {
            if hashed_matches(rest, host) {
                return true;
            }
            continue;
        }
        let (negated, pat) = match pattern.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, pattern),
        };
        if glob_match(pat, host) {
            // A negated pattern matching means "explicitly excluded".
            return !negated;
        }
    }
    false
}

/// `|1|salt|hash` verification (HMAC-SHA1 over the host).
fn hashed_matches(rest: &str, host: &str) -> bool {
    use hmac::{KeyInit, Mac, SimpleHmac};
    use sha1::Sha1;

    let Some((salt_b64, hash_b64)) = rest.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(expected)) = (b64_decode(salt_b64), b64_decode(hash_b64)) else {
        return false;
    };
    let Ok(mut mac) = <SimpleHmac<Sha1> as KeyInit>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(host.as_bytes());
    mac.verify_slice(&expected).is_ok()
}

/// Minimal base64 decode (standard alphabet, padding tolerated).
fn b64_decode(s: &str) -> Result<Vec<u8>, ()> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    // `saturating_mul`: a base64 field longer than `usize::MAX / 3` would overflow
    // here, and this parser runs on a user-supplied file. Capacity is only an
    // optimisation — the decoder itself stops on the first invalid byte — so
    // capping it also keeps a hostile 100 MB line from pre-allocating 75 MB.
    let mut out = Vec::with_capacity(s.len().saturating_mul(3) / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = rev[c as usize];
        if v == 255 {
            return Err(());
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// Glob match supporting `*` (any run, including empty) and `?` (one char).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Iterative backtracking matcher: O(n*m) worst case, no recursion.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Verify `server_key` for `host:port` against `known_hosts`.
///
/// Reads `SSHPF_KNOWN_HOSTS` if set, else `~/.ssh/known_hosts`.
pub fn verify(host: &str, port: u16, server_key: &PublicKey) -> HostKeyVerdict {
    match known_hosts_path() {
        Some(p) => verify_path(host, port, server_key, &p),
        None => HostKeyVerdict::Unknown,
    }
}

/// Verify against an explicit file (used by tests).
pub fn verify_path(host: &str, port: u16, server_key: &PublicKey, path: &Path) -> HostKeyVerdict {
    let wanted = format_host(host, port);
    let entries = parse_entries(path);

    // Revocation beats acceptance: check every revoked line first.
    for e in entries.iter().filter(|e| e.revoked) {
        if host_field_matches(&e.hosts, &wanted) && same_key(&e.key, server_key) {
            return HostKeyVerdict::Revoked { line: e.line_no };
        }
    }

    let mut saw_host = false;
    let mut changed_line = None;
    for e in entries.iter().filter(|e| !e.revoked && !e.cert_authority) {
        if !host_field_matches(&e.hosts, &wanted) {
            continue;
        }
        saw_host = true;
        if same_key(&e.key, server_key) {
            return HostKeyVerdict::Trusted;
        }
        changed_line.get_or_insert(e.line_no);
    }

    match changed_line {
        Some(line) => HostKeyVerdict::Changed { line },
        None if saw_host => HostKeyVerdict::Unknown, // same host, different algorithm
        None => HostKeyVerdict::Unknown,
    }
}

/// Compare two public keys ignoring the comment.
///
/// `ssh_key::PublicKey`'s `PartialEq` includes the comment, so a key loaded
/// from a `.pub` file never equals the same key parsed from `known_hosts`
/// (which has no comment). Reproduced as a false `KeyChanged` in russh's
/// checker; comparing the wire encoding avoids it.
fn same_key(a: &PublicKey, b: &PublicKey) -> bool {
    a.algorithm() == b.algorithm() && a.to_bytes() == b.to_bytes()
}

/// Append a key to `known_hosts` (used under [`HostKeyPolicy::AcceptNew`]).
///
/// Follows OpenSSH's `[host]:port` spelling, keeps the file newline-terminated,
/// and does not duplicate an identical existing entry.
pub fn learn(host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    let Some(path) = known_hosts_path() else {
        return Ok(());
    };
    learn_path(host, port, key, &path)
}

/// Learn into an explicit file (used by tests).
pub fn learn_path(host: &str, port: u16, key: &PublicKey, path: &Path) -> std::io::Result<()> {
    // Never re-add something we would refuse.
    match verify_path(host, port, key, path) {
        HostKeyVerdict::Trusted | HostKeyVerdict::Revoked { .. } => return Ok(()),
        // A changed key must not be silently appended; the caller decides.
        HostKeyVerdict::Changed { .. } => return Ok(()),
        HostKeyVerdict::Unknown => {}
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let needs_newline = match std::fs::read_to_string(path) {
        Ok(s) => !s.is_empty() && !s.ends_with('\n'),
        Err(_) => false,
    };
    let mut f = std::fs::OpenOptions::new().create(true).append(true).read(true).open(path)?;
    if needs_newline {
        f.write_all(b"\n")?;
    }
    let line = format!("{} {}\n", format_host(host, port), key.to_openssh().map_err(std::io::Error::other)?);
    f.write_all(line.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICsMBDhtVmpjA0bCZd/VKZGm+ZlWHaIdTFWca6QP35EN";
    const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";

    fn key(b64_line: &str) -> PublicKey {
        PublicKey::from_openssh(b64_line).unwrap()
    }

    // The TempDir is held only to keep the directory alive for the test.
    struct TempFile(PathBuf, #[allow(dead_code)] tempfile::TempDir);

    impl TempFile {
        fn with(contents: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("known_hosts");
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(contents.as_bytes()).unwrap();
            TempFile(path, dir)
        }
        fn empty() -> Self {
            Self::with("")
        }
        fn path(&self) -> &Path {
            &self.0
        }
        #[allow(dead_code)]
        fn read(&self) -> String {
            std::fs::read_to_string(&self.0).unwrap_or_default()
        }
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*.example.com", "a.example.com"));
        // `*` is greedy but the rest of the pattern is literal, so a
        // different suffix cannot match.
        assert!(!glob_match("*.example.com", "example.com.evil"));
        assert!(!glob_match("*.example.com", "example.com"));
        assert!(glob_match("web?", "web1"));
        assert!(!glob_match("web?", "web12"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }

    #[test]
    fn exact_match_is_trusted() {
        let f = TempFile::with(&format!("[127.0.0.1]:22200 {KEY_A}\n"));
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    }

    #[test]
    fn default_port_has_no_brackets() {
        let f = TempFile::with(&format!("example.com {KEY_A}\n"));
        assert_eq!(verify_path("example.com", 22, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
        // The bracketed spelling must not match port 22.
        assert_eq!(verify_path("example.com", 2200, &key(KEY_A), f.path()), HostKeyVerdict::Unknown);
    }

    #[test]
    fn wildcard_entry_matches() {
        let f = TempFile::with(&format!("*.example.com {KEY_A}\n"));
        assert_eq!(verify_path("a.example.com", 22, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    }

    #[test]
    fn tab_separated_line_matches() {
        let f = TempFile::with(&format!("[127.0.0.1]:22200\t{KEY_A}\n"));
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    }

    #[test]
    fn leading_whitespace_line_matches() {
        let f = TempFile::with(&format!("   [127.0.0.1]:22200 {KEY_A}\n"));
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    }

    #[test]
    fn comma_list_matches_any() {
        let f = TempFile::with(&format!("a.example.com,b.example.com {KEY_A}\n"));
        assert_eq!(verify_path("b.example.com", 22, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    }

    #[test]
    fn negated_pattern_excludes() {
        let f = TempFile::with(&format!("!secret.example.com,*.example.com {KEY_A}\n"));
        assert_eq!(verify_path("ok.example.com", 22, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
        assert_eq!(verify_path("secret.example.com", 22, &key(KEY_A), f.path()), HostKeyVerdict::Unknown);
    }

    #[test]
    fn changed_key_is_reported() {
        let f = TempFile::with(&format!("[127.0.0.1]:22200 {KEY_A}\n"));
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_B), f.path()), HostKeyVerdict::Changed { line: 1 });
    }

    #[test]
    fn unknown_host() {
        let f = TempFile::with(&format!("other.example.com {KEY_A}\n"));
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_B), f.path()), HostKeyVerdict::Unknown);
    }

    /// The security bug in russh's checker: revoked + normal entry for the same
    /// key returns `true` there. We must reject.
    #[test]
    fn revoked_wins_even_when_a_normal_entry_also_matches() {
        let f = TempFile::with(&format!("@revoked [127.0.0.1]:22200 {KEY_A}\n[127.0.0.1]:22200 {KEY_A}\n"));
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Revoked { line: 1 });
        assert!(!HostKeyVerdict::Revoked { line: 1 }.is_acceptable(HostKeyPolicy::AcceptNew));
    }

    #[test]
    fn revoked_alone_is_revoked_not_unknown() {
        let f = TempFile::with(&format!("@revoked [127.0.0.1]:22200 {KEY_A}\n"));
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Revoked { line: 1 });
    }

    #[test]
    fn cert_authority_lines_are_skipped_for_host_matching() {
        let f = TempFile::with(&format!("@cert-authority *.example.com {KEY_A}\n"));
        // Not treated as a host key match (we do not implement host certs).
        assert_eq!(verify_path("a.example.com", 22, &key(KEY_B), f.path()), HostKeyVerdict::Unknown);
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let f = TempFile::with(&format!("# comment\n\n[127.0.0.1]:22200 {KEY_A}\n"));
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    }

    #[test]
    fn hashed_entry_roundtrip() {
        // Build a `|1|salt|hash` line the way ssh-keygen -H does.
        use hmac::{KeyInit, Mac, SimpleHmac};
        use sha1::Sha1;
        let host = "[127.0.0.1]:22200";
        let salt = b"0123456789abcdefghij"; // 20 bytes
        let mut mac = <SimpleHmac<Sha1> as KeyInit>::new_from_slice(salt).unwrap();
        mac.update(host.as_bytes());
        let hash = mac.finalize().into_bytes();
        let line = format!("|1|{}|{} {KEY_A}\n", b64_encode(salt), b64_encode(&hash));
        let f = TempFile::with(&line);
        assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    }

    fn b64_encode(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
        }
        out
    }

    #[test]
    fn base64_decode_matches_encode() {
        for s in [&b"a"[..], b"ab", b"abc", b"abcd", b"hello world"] {
            assert_eq!(b64_decode(&b64_encode(s)).unwrap(), s, "roundtrip failed for {s:?}");
        }
        assert!(b64_decode("!!!").is_err());
    }

    #[test]
    fn learn_then_verify() {
        let f = TempFile::empty();
        assert_eq!(verify_path("h.example.com", 2222, &key(KEY_A), f.path()), HostKeyVerdict::Unknown);
        learn_path("h.example.com", 2222, &key(KEY_A), f.path()).unwrap();
        assert_eq!(verify_path("h.example.com", 2222, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
        assert!(f.read().contains("[h.example.com]:2222"), "got: {}", f.read());
    }

    #[test]
    fn learn_is_idempotent() {
        let f = TempFile::empty();
        learn_path("h.example.com", 22, &key(KEY_A), f.path()).unwrap();
        learn_path("h.example.com", 22, &key(KEY_A), f.path()).unwrap();
        let content = f.read();
        let lines: Vec<_> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "duplicate entry written: {content:?}");
    }

    #[test]
    fn learn_does_not_append_a_changed_key() {
        // Port 22 is spelled without brackets (OpenSSH convention).
        let f = TempFile::with(&format!("h.example.com {KEY_A}\n"));
        learn_path("h.example.com", 22, &key(KEY_B), f.path()).unwrap();
        let content = f.read();
        assert_eq!(
            content.lines().filter(|l| !l.is_empty()).count(),
            1,
            "a changed key must not be appended: {content:?}"
        );
    }

    #[test]
    fn policy_acceptance() {
        assert!(HostKeyVerdict::Trusted.is_acceptable(HostKeyPolicy::Strict));
        assert!(!HostKeyVerdict::Unknown.is_acceptable(HostKeyPolicy::Strict));
        assert!(HostKeyVerdict::Unknown.is_acceptable(HostKeyPolicy::AcceptNew));
        assert!(HostKeyVerdict::Unknown.is_acceptable(HostKeyPolicy::Ask));
        // A changed key is never acceptable, under any policy.
        for p in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew, HostKeyPolicy::Ask] {
            assert!(!HostKeyVerdict::Changed { line: 1 }.is_acceptable(p));
            assert!(!HostKeyVerdict::Revoked { line: 1 }.is_acceptable(p));
        }
    }

    #[test]
    fn same_key_ignores_comment() {
        let with_comment = PublicKey::from_openssh(&format!("{KEY_A} some-host-comment")).unwrap();
        let without = key(KEY_A);
        assert_eq!(with_comment.comment().as_str().ok(), Some("some-host-comment"));
        assert!(same_key(&with_comment, &without), "comment must not affect key equality");
    }
}

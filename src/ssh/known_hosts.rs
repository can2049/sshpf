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
//! * `Ignore` — a *changed* key is accepted too (opt-in escape hatch for hosts
//!   whose key legitimately rotates, e.g. a dev jump host). `@revoked` is still
//!   fatal: an explicit revocation is a statement about the key, not about the
//!   host's convenience. Nothing is written back, so the trust anchor on disk is
//!   never silently rewritten.

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
    ///
    /// `Revoked` is refused under *every* policy, including [`HostKeyPolicy::Ignore`]:
    /// a `@revoked` line is a deliberate statement that this key must never be
    /// trusted, so ignoring a *change* must not also ignore a revocation.
    pub fn is_acceptable(&self, policy: HostKeyPolicy) -> bool {
        match self {
            HostKeyVerdict::Trusted => true,
            HostKeyVerdict::Unknown => {
                matches!(policy, HostKeyPolicy::AcceptNew | HostKeyPolicy::Ask | HostKeyPolicy::Ignore)
            }
            HostKeyVerdict::Changed { .. } => matches!(policy, HostKeyPolicy::Ignore),
            HostKeyVerdict::Revoked { .. } => false,
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

/// The `SHA256:…` fingerprint of a presented key, for the "ignored a change"
/// record.
///
/// Worth logging because accepting a changed key is exactly the moment an
/// operator later wants to compare against what they expected. Never fails: a
/// fingerprint is diagnostic detail, not a reason to refuse a connection.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(russh::keys::HashAlg::Sha256).to_string()
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
    /// Accept a *changed* key as well as an unknown one (opt-in escape hatch).
    ///
    /// Never writes to `known_hosts` — the on-disk trust anchor is left exactly
    /// as the user wrote it, so the change stays visible next run. A `@revoked`
    /// key is still refused; see [`HostKeyVerdict::is_acceptable`].
    Ignore,
}

/// Whether `policy` accepts a *changed* key, i.e. whether it is the
/// ignore-the-change escape hatch.
///
/// Named for the question the connector asks ("is this acceptance the degraded
/// path?"), so the logging call sites cannot drift from the acceptance rule.
pub fn accepts_changed(policy: HostKeyPolicy) -> bool {
    matches!(policy, HostKeyPolicy::Ignore)
}

/// `host` for port 22, `[host]:port` otherwise — the OpenSSH spelling.
///
/// Exposed for the callers that must name a hop in a user-facing message
/// (`client::handler`'s ignored-change warning) without keeping a second copy of
/// the bracket rule.
pub fn describe_host(host: &str, port: u16) -> String {
    format_host(host, port)
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

/// `known_hosts` host-pattern matching: `*` (any run, including empty) and `?`
/// (one char), **case-sensitive**.
///
/// A one-line adapter over [`crate::text::glob_match`]: the matcher used to exist
/// twice (here and in `ssh/forwards.rs`, which needs it case-*insensitive* for
/// ssh `Host` patterns) and the copies had begun to drift. Only the case rule
/// lives here — it is a property of the `known_hosts` file format, not of glob
/// matching. Folding case would be a security bug: `Web1` and `web1` are
/// different hosts.
fn glob_match(pattern: &str, text: &str) -> bool {
    crate::text::glob_match(pattern, text, false)
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

    // Entries are keyed by `host` **and algorithm**, exactly like OpenSSH: a host
    // legitimately has one `ssh-ed25519` *and* one `ecdsa-sha2-nistp256` line, and
    // negotiating the other one is not a key change. Only a *different key under
    // the same algorithm* is a change.
    //
    // This used to record any non-matching entry as `Changed`, which reported a
    // false "possible man-in-the-middle" for the ordinary case of a server
    // offering several algorithms while `known_hosts` holds only one of them —
    // measured on a host whose only entry was ECDSA: OpenSSH connected under
    // `StrictHostKeyChecking=yes` while sshpf refused the same session.
    let mut changed_line = None;
    for e in entries.iter().filter(|e| !e.revoked && !e.cert_authority) {
        if !host_field_matches(&e.hosts, &wanted) {
            continue;
        }
        if same_key(&e.key, server_key) {
            return HostKeyVerdict::Trusted;
        }
        // Same algorithm + different key material = a genuine replacement.
        if e.key.algorithm() == server_key.algorithm() {
            changed_line.get_or_insert(e.line_no);
        }
    }

    // No `Changed` means either the host has no entry at all, or it only has
    // entries under *other* algorithms. Both are `Unknown`: nothing is recorded
    // for this key, so the connection needs the same trust decision as a host
    // never seen before (learn it, or refuse it under `Strict`).
    match changed_line {
        Some(line) => HostKeyVerdict::Changed { line },
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
mod tests;

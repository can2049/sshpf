//! Unit tests for `known_hosts.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use std::io::Write;

const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICsMBDhtVmpjA0bCZd/VKZGm+ZlWHaIdTFWca6QP35EN";
const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
/// A different *algorithm* for the same host — the case that used to be
/// misreported as a changed key.
const KEY_ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBOTx8hm89xA3Wg6fNTi9QU6deJHYW2EjimXszmMH3QVp8xJCjvnprJ3MK3twYXqP61zUjL6KMZi6wkXgqrtHA3w=";

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

/// The whole point of the escape hatch, at the level of a real file: the
/// verdict itself stays `Changed` (it is a fact about the file), and only
/// the *acceptance* changes. That keeps the WARN record truthful.
#[test]
fn a_changed_key_is_verdict_changed_and_only_ignore_accepts_it() {
    let f = TempFile::with(&format!("[127.0.0.1]:22200 {KEY_A}\n"));
    let verdict = verify_path("127.0.0.1", 22200, &key(KEY_B), f.path());
    assert_eq!(verdict, HostKeyVerdict::Changed { line: 1 });
    assert!(verdict.is_acceptable(HostKeyPolicy::Ignore));
    for p in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew, HostKeyPolicy::Ask] {
        assert!(!verdict.is_acceptable(p), "a changed key was accepted under {p:?}");
    }
}

/// `Ignore` must not soften a revocation even when the same file also has a
/// normal entry for the key — the combination russh's checker gets wrong.
#[test]
fn ignore_does_not_accept_a_revoked_key() {
    let f = TempFile::with(&format!("@revoked [127.0.0.1]:22200 {KEY_A}\n[127.0.0.1]:22200 {KEY_A}\n"));
    let verdict = verify_path("127.0.0.1", 22200, &key(KEY_A), f.path());
    assert_eq!(verdict, HostKeyVerdict::Revoked { line: 1 });
    assert!(!verdict.is_acceptable(HostKeyPolicy::Ignore));
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

/// The mirror of [`learn_does_not_append_a_changed_key`]: a *different
/// algorithm* for a known host is not a change, so learning it must append
/// (OpenSSH keeps one line per host+algorithm). Refusing to append here would
/// leave the second algorithm permanently un-learnable.
#[test]
fn learn_appends_a_second_algorithm_for_the_same_host() {
    let f = TempFile::with(&format!("h.example.com {KEY_A}\n"));
    learn_path("h.example.com", 22, &key(KEY_ECDSA), f.path()).unwrap();
    let content = f.read();
    assert_eq!(content.lines().filter(|l| !l.is_empty()).count(), 2, "second algorithm must be learned: {content:?}");
    assert_eq!(verify_path("h.example.com", 22, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    assert_eq!(verify_path("h.example.com", 22, &key(KEY_ECDSA), f.path()), HostKeyVerdict::Trusted);
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
    assert_eq!(content.lines().filter(|l| !l.is_empty()).count(), 1, "a changed key must not be appended: {content:?}");
}

#[test]
fn policy_acceptance() {
    assert!(HostKeyVerdict::Trusted.is_acceptable(HostKeyPolicy::Strict));
    assert!(HostKeyVerdict::Trusted.is_acceptable(HostKeyPolicy::Ignore));
    assert!(!HostKeyVerdict::Unknown.is_acceptable(HostKeyPolicy::Strict));
    assert!(HostKeyVerdict::Unknown.is_acceptable(HostKeyPolicy::AcceptNew));
    assert!(HostKeyVerdict::Unknown.is_acceptable(HostKeyPolicy::Ask));
    assert!(HostKeyVerdict::Unknown.is_acceptable(HostKeyPolicy::Ignore));
    // A changed key is refused everywhere except the explicit escape hatch.
    for p in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew, HostKeyPolicy::Ask] {
        assert!(!HostKeyVerdict::Changed { line: 1 }.is_acceptable(p), "changed accepted under {p:?}");
    }
    assert!(HostKeyVerdict::Changed { line: 1 }.is_acceptable(HostKeyPolicy::Ignore));
    // A revocation is refused under *every* policy, `Ignore` included:
    // `@revoked` is a statement about the key, not about the host's
    // convenience, so ignoring a change must not also ignore a revocation.
    for p in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew, HostKeyPolicy::Ask, HostKeyPolicy::Ignore] {
        assert!(!HostKeyVerdict::Revoked { line: 1 }.is_acceptable(p), "revoked accepted under {p:?}");
    }
}

/// `accepts_changed` is the question the connector and the log call sites
/// ask; it must agree with the acceptance rule exactly, or a key could be
/// accepted without a warning (or warned about without being accepted).
#[test]
fn only_the_ignore_policy_accepts_a_changed_key() {
    assert!(!accepts_changed(HostKeyPolicy::Strict));
    assert!(!accepts_changed(HostKeyPolicy::AcceptNew));
    assert!(!accepts_changed(HostKeyPolicy::Ask));
    assert!(accepts_changed(HostKeyPolicy::Ignore));
    for p in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew, HostKeyPolicy::Ask, HostKeyPolicy::Ignore] {
        assert_eq!(
            accepts_changed(p),
            HostKeyVerdict::Changed { line: 7 }.is_acceptable(p),
            "policy {p:?} disagrees with `Changed::is_acceptable`"
        );
    }
}

/// The fingerprint is the only identifier of *what the server actually
/// presented* in the WARN record, so it has to be SHA256 (not MD5/SHA1),
/// stable for one key, and different for another.
#[test]
fn fingerprint_is_a_stable_sha256_of_the_key() {
    let a = key(KEY_A);
    let b = key(KEY_B);
    let fa = fingerprint(&a);
    assert!(fa.starts_with("SHA256:"), "fingerprint is not SHA256: {fa}");
    // SHA256 is 32 bytes → 43 base64 chars, unpadded. SHA1 would be 28.
    let body = fa.trim_start_matches("SHA256:");
    assert_eq!(body.len(), 43, "not a 32-byte base64 body: {body}");
    assert!(!body.contains('='), "fingerprint must not be padding-padded: {body}");
    assert_eq!(fa, fingerprint(&a), "the same key must fingerprint identically");
    assert_ne!(fa, fingerprint(&b), "two different keys collided");
}

/// Regression: a host whose `known_hosts` entry is ECDSA, when the server
/// presents its ED25519 key, is **not** a changed key — OpenSSH keys entries
/// by `host + algorithm` and connects happily. We used to record the
/// non-matching line as `Changed`, i.e. accuse the user of a possible
/// man-in-the-middle attack on an ordinary multi-algorithm server.
///
/// Measured on a real host: OpenSSH accepted it under
/// `StrictHostKeyChecking=yes` while sshpf refused the same session.
#[test]
fn a_different_algorithm_is_unknown_not_changed() {
    let f = TempFile::with(&format!("[127.0.0.1]:22200 {KEY_ECDSA}\n"));
    // Server presents ed25519; only an ecdsa entry is recorded.
    assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Unknown);
    // And the reverse direction: ed25519 recorded, ecdsa presented.
    let g = TempFile::with(&format!("[127.0.0.1]:22200 {KEY_A}\n"));
    assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_ECDSA), g.path()), HostKeyVerdict::Unknown);
    // The genuine change — same algorithm, different key — is still caught.
    assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_B), g.path()), HostKeyVerdict::Changed { line: 1 });
}

/// A host with entries under *both* algorithms stays trusted for each of them;
/// adding the second algorithm must not invalidate the first.
#[test]
fn multiple_algorithms_are_all_trusted() {
    let f = TempFile::with(&format!("[127.0.0.1]:22200 {KEY_A}\n[127.0.0.1]:22200 {KEY_ECDSA}\n"));
    assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_A), f.path()), HostKeyVerdict::Trusted);
    assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_ECDSA), f.path()), HostKeyVerdict::Trusted);
    assert_eq!(verify_path("127.0.0.1", 22200, &key(KEY_B), f.path()), HostKeyVerdict::Changed { line: 1 });
}

#[test]
fn same_key_ignores_comment() {
    let with_comment = PublicKey::from_openssh(&format!("{KEY_A} some-host-comment")).unwrap();
    let without = key(KEY_A);
    assert_eq!(with_comment.comment().as_str().ok(), Some("some-host-comment"));
    assert!(same_key(&with_comment, &without), "comment must not affect key equality");
}

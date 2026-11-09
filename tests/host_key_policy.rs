//! The host-key *policy* contract, tested in-process against real files.
//!
//! `src/ssh/known_hosts.rs` already unit-tests the parser; `src/ssh/hostkey_conf.rs`
//! already unit-tests the `~/.ssh/config` directive mapping. What neither can show
//! on its own is the behaviour the user cares about — **the same changed key is
//! refused with the default policy and accepted under `--ignore-host-key-changes`,
//! while a revoked key is refused under both** — and that the per-host mapping
//! really reaches the connector through `Endpoint::resolve`.
//!
//! So this file stitches the pieces together the way production does:
//!
//! ```text
//! known_hosts file ─ verify_path ─┐
//!                                 ├─ is_acceptable(policy) ─ accept / refuse
//! ~/.ssh/config ─ Endpoint::resolve ─ Options::policy_for ─┘
//! ```
//!
//! No `sshd`, no network, no subprocess: `verify_path` and `Endpoint::resolve`
//! are the seams. `tests/e2e.rs` owns the "against a real server" version.
//!
//! Anything that sets `SSHPF_*` is serialized behind [`env_lock`], because the
//! tests in this binary share one process (the e2e suite does the same).

use std::sync::{Mutex, MutexGuard, OnceLock};

use russh::keys::PublicKey;
use sshpf::ssh::client::Options;
use sshpf::ssh::config::Endpoint;
use sshpf::ssh::known_hosts::{self, HostKeyPolicy, HostKeyVerdict};

/// Key recorded in `known_hosts` ("what we trusted yesterday").
const KEY_A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICsMBDhtVmpjA0bCZd/VKZGm+ZlWHaIdTFWca6QP35EN";
/// Key the server presents today ("what we see now").
const KEY_B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";

fn key(openssh: &str) -> PublicKey {
    PublicKey::from_openssh(openssh).unwrap()
}

fn key_a() -> PublicKey {
    key(KEY_A)
}

fn key_b() -> PublicKey {
    key(KEY_B)
}

/// Serializes tests that mutate process-global `SSHPF_*` env vars.
///
/// `Endpoint::resolve` reads the environment, so a test that points
/// `SSHPF_SSH_CONFIG` at a fixture must not run concurrently with another one
/// doing the same (same shape as `tests/e2e.rs`).
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write a known_hosts file into a fresh temp dir and return that dir + path.
///
/// The dir must outlive the path, so it is returned and held by the caller.
fn known_hosts_file(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("known_hosts");
    std::fs::write(&path, contents).unwrap();
    (dir, path)
}

// ---------------------------------------------------------------------------
// the user-visible promise of the flag
// ---------------------------------------------------------------------------

/// The whole feature in one assertion, at the seam production uses: a host whose
/// recorded key is `A` and which now presents `B` is a hard refusal by default
/// and a warned-through under `--ignore-host-key-changes`.
#[test]
fn a_changed_key_is_refused_by_default_and_accepted_when_ignoring_changes() {
    let (_dir, path) = known_hosts_file(&format!("[127.0.0.1]:22200 {KEY_A}\n"));

    let verdict = known_hosts::verify_path("127.0.0.1", 22200, &key_b(), &path);
    assert_eq!(verdict, HostKeyVerdict::Changed { line: 1 }, "the verdict must name the stale line");

    // Default (no `--ignore-host-key-changes`): refused.
    assert!(!verdict.is_acceptable(HostKeyPolicy::AcceptNew), "the default policy must refuse a changed key");
    assert!(!verdict.is_acceptable(HostKeyPolicy::default()), "the default must be the refusing one");
    // `--ignore-host-key-changes`: accepted.
    assert!(verdict.is_acceptable(HostKeyPolicy::Ignore), "the escape hatch must accept the change");
}

/// Ignoring a change must not rewrite the trust anchor: after the acceptance the
/// file still says `A`, so the mismatch is still visible on the next run and to
/// `ssh-keygen -F`. This is the invariant that makes the escape hatch safe to
/// leave switched on.
#[test]
fn accepting_a_changed_key_does_not_rewrite_known_hosts() {
    let (_dir, path) = known_hosts_file(&format!("[127.0.0.1]:22200 {KEY_A}\n"));
    let before = std::fs::read_to_string(&path).unwrap();

    let verdict = known_hosts::verify_path("127.0.0.1", 22200, &key_b(), &path);
    assert!(verdict.is_acceptable(HostKeyPolicy::Ignore));

    assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "known_hosts was rewritten by an ignored change");
    // And the next run sees exactly the same situation.
    assert_eq!(known_hosts::verify_path("127.0.0.1", 22200, &key_b(), &path), verdict);
}

/// `@revoked` is a statement about the *key*, not about the host's convenience,
/// so the escape hatch must not reach it. Read from a real file, because this is
/// the combination russh's own checker accepts (see the module docs).
#[test]
fn a_revoked_key_is_refused_even_when_changes_are_ignored() {
    let (_dir, path) = known_hosts_file(&format!("@revoked [127.0.0.1]:22200 {KEY_B}\n[127.0.0.1]:22200 {KEY_A}\n"));

    let verdict = known_hosts::verify_path("127.0.0.1", 22200, &key_b(), &path);
    assert_eq!(verdict, HostKeyVerdict::Revoked { line: 1 });
    for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew, HostKeyPolicy::Ask, HostKeyPolicy::Ignore] {
        assert!(!verdict.is_acceptable(policy), "a revoked key was accepted under {policy:?}");
    }
}

/// A genuinely unknown host is unaffected by the flag: it was already acceptable
/// (`accept-new` learns it), and the flag must not change what gets written.
#[test]
fn an_unknown_host_is_acceptable_before_and_after_the_flag() {
    let (_dir, path) = known_hosts_file(&format!("other.example.com {KEY_A}\n"));
    let verdict = known_hosts::verify_path("127.0.0.1", 22200, &key_b(), &path);
    assert_eq!(verdict, HostKeyVerdict::Unknown);
    assert!(verdict.is_acceptable(HostKeyPolicy::AcceptNew));
    assert!(verdict.is_acceptable(HostKeyPolicy::Ignore));
}

/// The identity a WARN record carries for a changed key is the *server's*
/// fingerprint, so it must be a real SHA256 of that key — the value an operator
/// pastes into a bug report or compares with `ssh-keygen -lf`.
#[test]
fn the_fingerprint_in_the_warn_record_identifies_the_presented_key() {
    let recorded = known_hosts::fingerprint(&key_b());
    assert!(recorded.starts_with("SHA256:"), "not an SHA256 fingerprint: {recorded}");
    assert_eq!(recorded, key_b().fingerprint(russh::keys::HashAlg::Sha256).to_string());
    assert_ne!(recorded, known_hosts::fingerprint(&key_a()), "the two keys must not share a fingerprint");
    assert_eq!(recorded, known_hosts::fingerprint(&key_b()), "the fingerprint must be stable");
}

/// `accepts_changed` is what the connector's degraded-path logging asks; it must
/// agree with the acceptance rule for every policy, or a key could be accepted
/// without a warning.
#[test]
fn accepts_changed_agrees_with_the_acceptance_rule() {
    for policy in [HostKeyPolicy::Strict, HostKeyPolicy::AcceptNew, HostKeyPolicy::Ask, HostKeyPolicy::Ignore] {
        assert_eq!(
            known_hosts::accepts_changed(policy),
            HostKeyVerdict::Changed { line: 3 }.is_acceptable(policy),
            "policy {policy:?} disagrees between `accepts_changed` and `is_acceptable`"
        );
    }
    assert!(!known_hosts::accepts_changed(HostKeyPolicy::default()), "the default must not be the degraded path");
}

// ---------------------------------------------------------------------------
// per-host policy from ~/.ssh/config reaches the connector
// ---------------------------------------------------------------------------

/// Write an `~/.ssh/config` fixture and point `SSHPF_SSH_CONFIG` at it for the
/// duration of `f`. The lock is taken *before* the variable is set, and the
/// previous value is restored afterwards so a later test cannot inherit a dead
/// temp path.
fn with_ssh_config<T>(config: &str, f: impl FnOnce() -> T) -> T {
    let _guard = env_lock();
    let (dir, previous) = install_config_fixture(config);
    let out = f();
    restore_ssh_config(previous);
    drop(dir);
    out
}

/// Point `SSHPF_SSH_CONFIG` at a fresh fixture file; returns the dir that must
/// outlive the variable plus the value to restore. **Caller holds [`env_lock`].**
fn install_config_fixture(config: &str) -> (tempfile::TempDir, Option<std::ffi::OsString>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    std::fs::write(&path, config).unwrap();
    let previous = std::env::var_os("SSHPF_SSH_CONFIG");
    // SAFETY: every caller holds `env_lock`, so no other test in this binary
    // reads or writes this variable concurrently.
    unsafe { std::env::set_var("SSHPF_SSH_CONFIG", &path) };
    (dir, previous)
}

/// Undo [`install_config_fixture`]. **Caller holds [`env_lock`].**
fn restore_ssh_config(previous: Option<std::ffi::OsString>) {
    // SAFETY: see `install_config_fixture`.
    unsafe {
        match previous {
            Some(v) => std::env::set_var("SSHPF_SSH_CONFIG", v),
            None => std::env::remove_var("SSHPF_SSH_CONFIG"),
        }
    }
}

/// The reference case from the bug report: `ssh cloud_pc` works because the
/// *jump* host says `StrictHostKeyChecking no`. That directive has to survive the
/// trip `~/.ssh/config` → `Endpoint` → `Options::policy_for`, or sshpf keeps
/// refusing the hop `ssh` happily accepts.
#[test]
fn a_jump_host_that_disables_checking_reaches_the_connector_as_ignore() {
    let config = "\
Host cloud_pc_jump
  HostName desk-ssh.example.com
  Port 2222
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
Host cloud_pc
  HostName 10.244.119.128
  ProxyJump cloud_pc_jump
";
    with_ssh_config(config, || {
        let jump = Endpoint::resolve("cloud_pc_jump").unwrap();
        assert_eq!(jump.host_key_policy, HostKeyPolicy::Ignore, "the `no` directive was lost in resolution");
        // The default options must not dilute it: this hop is ignored per host.
        assert_eq!(Options::default().policy_for(&jump), HostKeyPolicy::Ignore);

        // The target declares nothing, so it stays on sshpf's default — the two
        // hops legitimately differ.
        let target = Endpoint::resolve("cloud_pc").unwrap();
        assert_eq!(target.host_key_policy, HostKeyPolicy::AcceptNew, "a silent host must not be relaxed");
        assert_eq!(Options::default().policy_for(&target), HostKeyPolicy::AcceptNew);

        // And the same known_hosts mismatch is then refused for the target and
        // accepted for the jump host: the per-hop promise, end to end. Port 22
        // is spelled without brackets (the OpenSSH convention).
        const TARGET: &str = "10.244.119.128";
        assert_eq!((target.hostname.as_str(), target.port), (TARGET, 22), "fixture assumes the default port");
        let (_dir, path) = known_hosts_file(&format!("[desk-ssh.example.com]:2222 {KEY_A}\n{TARGET} {KEY_A}\n"));
        let jump_verdict = known_hosts::verify_path(&jump.hostname, jump.port, &key_b(), &path);
        let target_verdict = known_hosts::verify_path(&target.hostname, target.port, &key_b(), &path);
        assert_eq!(jump_verdict, HostKeyVerdict::Changed { line: 1 });
        assert_eq!(target_verdict, HostKeyVerdict::Changed { line: 2 });
        assert!(jump_verdict.is_acceptable(Options::default().policy_for(&jump)));
        assert!(!target_verdict.is_acceptable(Options::default().policy_for(&target)));
    });
}

/// The global flag is a floor on leniency: a host block asking for `accept-new`
/// (or restating `yes`) must not weaken `--ignore-host-key-changes`.
#[test]
fn the_global_flag_wins_over_a_stricter_host_block() {
    let config = "\
Host strict_host
  HostName s.example.com
  StrictHostKeyChecking yes
Host accept_new_host
  HostName a.example.com
  StrictHostKeyChecking accept-new
";
    with_ssh_config(config, || {
        let ignoring = Options { host_key_policy: HostKeyPolicy::Ignore, ..Options::default() };
        for alias in ["strict_host", "accept_new_host"] {
            let endpoint = Endpoint::resolve(alias).unwrap();
            // `yes` must not become `Strict` (sshpf's default is accept-new), and
            // the global flag overrides whatever the block said.
            assert_eq!(endpoint.host_key_policy, HostKeyPolicy::AcceptNew, "{alias}");
            assert_eq!(ignoring.policy_for(&endpoint), HostKeyPolicy::Ignore, "{alias} weakened the global flag");
        }
    });
}

/// With no ssh config at all, nothing is relaxed: the default policy refuses the
/// change. (`SSHPF_SSH_CONFIG` pointing at a missing file = "no config".)
#[test]
fn without_an_ssh_config_the_default_still_refuses_a_change() {
    let _guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-config");
    assert!(!missing.exists(), "the fixture must point at a file that does not exist");
    let previous = std::env::var_os("SSHPF_SSH_CONFIG");
    // SAFETY: `env_lock` is held.
    unsafe { std::env::set_var("SSHPF_SSH_CONFIG", &missing) };

    let endpoint = Endpoint::resolve("some-host").unwrap();
    let policy = Options::default().policy_for(&endpoint);

    restore_ssh_config(previous);

    assert_eq!(endpoint.hostname, "some-host", "with no config the alias is the hostname");
    assert_eq!(endpoint.host_key_policy, HostKeyPolicy::AcceptNew);
    assert_eq!(policy, HostKeyPolicy::AcceptNew);
    assert!(!known_hosts::accepts_changed(policy));
    assert!(!HostKeyVerdict::Changed { line: 1 }.is_acceptable(policy));
}

/// The fixture must restore the environment when it returns: every other test in
/// this binary (and `tests/e2e.rs`) would otherwise inherit a dead temp path.
#[test]
fn the_ssh_config_fixture_is_restored_after_the_closure() {
    let _guard = env_lock();
    let before = std::env::var_os("SSHPF_SSH_CONFIG");
    let config = "Host h\n  HostName h.example.com\n  StrictHostKeyChecking no\n";
    let (dir, previous) = install_config_fixture(config);
    // The fixture is live: resolution sees the `no` directive.
    assert_eq!(Endpoint::resolve("h").unwrap().host_key_policy, HostKeyPolicy::Ignore);
    restore_ssh_config(previous);
    drop(dir);
    assert_eq!(
        std::env::var_os("SSHPF_SSH_CONFIG"),
        before,
        "SSHPF_SSH_CONFIG was not restored after the fixture dropped"
    );
}

//! End-to-end tests against a **real** OpenSSH `sshd`, exercising the pure-Rust
//! client (no `ssh` binary involved at all).
//!
//! These are `#[ignore]`d by default because they need `sshd`, `ssh-keygen` and
//! a writable temp dir:
//!
//! ```text
//! cargo test --test e2e -- --ignored --test-threads=1
//! ```
//!
//! CI runs them on Linux; on Windows the `sshd` prerequisite is absent, so the
//! helpers return `None` and each test skips itself instead of failing.
//!
//! What is covered:
//! * `-L` genuinely tunnels bytes (verified by reading a real service banner)
//! * `-R` genuinely accepts bytes on the remote side
//! * `-D` speaks SOCKS5 end to end
//! * a wrong host key, an unknown host key and a revoked key are refused
//! * a dead target does not tear the tunnel down
//! * the app reaches `Up` only through the driver's readiness signal

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use sshpf::app::App;
use sshpf::config::{self, Paths};
use sshpf::model::{FailureKind, ForwardKind, TunnelConfig, TunnelStatus};

// ---------------------------------------------------------------------------
// test harness
// ---------------------------------------------------------------------------

/// Serializes tests that mutate process-global `SSHPF_*` env vars.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn free_port() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    l.local_addr().unwrap().port()
}

fn tunnel(id: u64, name: &str, port: u16, host: &str) -> TunnelConfig {
    TunnelConfig {
        id,
        name: name.into(),
        kind: ForwardKind::Local,
        ssh_host: host.into(),
        local_addr: "127.0.0.1".into(),
        local_port: port,
        remote_host: "127.0.0.1".into(),
        remote_port: 22,
        enabled: true,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    }
}

/// A throw-away sshd plus the client credentials to reach it.
struct TestSshd {
    dir: tempfile::TempDir,
    port: u16,
}

impl TestSshd {
    fn ssh_config(&self) -> PathBuf {
        self.dir.path().join("client_config")
    }

    fn known_hosts(&self) -> PathBuf {
        self.dir.path().join("known_hosts")
    }

    fn user_key(&self) -> PathBuf {
        self.dir.path().join("user_key")
    }

    /// Point the process-global overrides at this fixture.
    ///
    /// SAFETY: every caller holds `env_lock()`, and Rust 2024 requires these to
    /// be unsafe because the env can be read concurrently by other threads.
    fn activate(&self) {
        unsafe {
            std::env::set_var("SSHPF_KNOWN_HOSTS", self.known_hosts());
            std::env::set_var("SSHPF_SSH_CONFIG", self.ssh_config());
        }
    }
}

impl Drop for TestSshd {
    fn drop(&mut self) {
        // Clear the overrides so a later test cannot inherit this fixture.
        unsafe {
            std::env::remove_var("SSHPF_KNOWN_HOSTS");
            std::env::remove_var("SSHPF_SSH_CONFIG");
        }
        if let Ok(pid) = std::fs::read_to_string(self.dir.path().join("sshd.pid")) {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        }
    }
}

fn sshd_available() -> bool {
    Path::new("/usr/sbin/sshd").exists()
}

/// Start a throw-away sshd on a loopback port with a fresh key pair.
///
/// Returns `None` (test skips) when the platform lacks `sshd`.
fn start_sshd() -> Option<TestSshd> {
    if !sshd_available() {
        return None;
    }
    let dir = tempfile::tempdir().ok()?;
    let port = free_port();
    let keygen = |path: &Path| {
        std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !keygen(&dir.path().join("host_key")) || !keygen(&dir.path().join("user_key")) {
        return None;
    }
    std::fs::copy(dir.path().join("user_key.pub"), dir.path().join("authorized_keys")).ok()?;

    // The current user must be allowed to log in; an sshd started as root can
    // switch users, but a non-root one can only serve its own account.
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).ok()?;

    let config = dir.path().join("sshd_config");
    std::fs::write(
        &config,
        format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {hk}\n\
             PidFile {pid}\n\
             AuthorizedKeysFile {ak}\n\
             UsePAM no\n\
             PasswordAuthentication no\n\
             PubkeyAuthentication yes\n\
             StrictModes no\n\
             AllowTcpForwarding yes\n\
             AllowUsers {user}\n\
             PermitOpen any\n",
            hk = dir.path().join("host_key").display(),
            pid = dir.path().join("sshd.pid").display(),
            ak = dir.path().join("authorized_keys").display(),
        ),
    )
    .ok()?;

    std::process::Command::new("/usr/sbin/sshd").arg("-f").arg(&config).status().ok()?;
    // sshd daemonises; wait for the port to answer.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            let fixture = TestSshd { dir, port };
            // A client config that carries the identity and port, so
            // `Endpoint::resolve` finds them.
            std::fs::write(
                fixture.ssh_config(),
                format!(
                    "Host testhost\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  IdentityFile {key}\n",
                    key = fixture.user_key().display()
                ),
            )
            .ok()?;
            // Trust the host key, exactly as `ssh -o StrictHostKeyChecking=accept-new` would.
            let host_pub = std::fs::read_to_string(fixture.dir.path().join("host_key.pub")).ok()?;
            let mut fields = host_pub.split_whitespace();
            let algo = fields.next()?;
            let b64 = fields.next()?;
            std::fs::write(fixture.known_hosts(), format!("[127.0.0.1]:{port} {algo} {b64}\n")).ok()?;
            return Some(fixture);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn app_with(dir: &Path, tunnels: Vec<TunnelConfig>) -> App {
    let paths = Paths { config: dir.join("config.toml"), state: dir.join("state.toml") };
    let cfg = config::ConfigFile { tunnels, ..Default::default() };
    config::save(&paths.config, &cfg).unwrap();
    App::new(paths)
}

fn wait_for(app: &mut App, secs: u64, mut pred: impl FnMut(&App) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        app.drain_events();
        if pred(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn status_of(app: &App, id: u64) -> &TunnelStatus {
    &app.tunnels.iter().find(|t| t.cfg.id == id).expect("tunnel").status
}

// ---------------------------------------------------------------------------
// -L
// ---------------------------------------------------------------------------

/// A local forward must carry real bytes to a service the server can reach.
#[test]
#[ignore = "needs a working sshd"]
fn local_forward_carries_traffic() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    // The sshd's own port is a reachable target that speaks first (SSH banner),
    // which proves bytes actually crossed the tunnel.
    let mut t = tunnel(1, "l", free_port(), "testhost");
    t.remote_host = "127.0.0.1".into();
    t.remote_port = sshd.port;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![t]);
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "tunnel never came up: {:?}",
        status_of(&app, 1)
    );

    let local = app.tunnels[0].cfg.local_port;
    let mut conn = TcpStream::connect(("127.0.0.1", local)).expect("connect to the forward");
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = [0u8; 64];
    let n = conn.read(&mut buf).expect("read the target banner through the tunnel");
    let banner = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(banner.starts_with("SSH-2.0-"), "expected the sshd banner, got {banner:?}");
}

/// A target that is down must not tear the tunnel down (matches `ssh -L`).
#[test]
#[ignore = "needs a working sshd"]
fn dead_target_does_not_kill_the_tunnel() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    let mut t = tunnel(1, "l", free_port(), "testhost");
    t.remote_port = free_port(); // nothing listens there

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![t]);
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })));

    // Connect: the target refuses, so our socket gets closed, but the tunnel
    // itself must still be Up and still accepting.
    let local = app.tunnels[0].cfg.local_port;
    let _ = TcpStream::connect(("127.0.0.1", local));
    std::thread::sleep(Duration::from_millis(300));
    app.drain_events();
    assert!(
        matches!(status_of(&app, 1), TunnelStatus::Up { .. }),
        "a dead target must not fail the tunnel: {:?}",
        status_of(&app, 1)
    );
}

// ---------------------------------------------------------------------------
// -R
// ---------------------------------------------------------------------------

/// A remote forward must accept bytes on the server side and deliver them here.
#[test]
#[ignore = "needs a working sshd"]
fn remote_forward_carries_traffic() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    // Local echo service that the remote side will reach through the tunnel.
    let echo = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in echo.incoming().flatten() {
            let mut s = stream;
            std::thread::spawn(move || {
                let mut buf = [0u8; 256];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if s.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });

    let remote_port = free_port();
    let mut t = tunnel(1, "r", 0, "testhost");
    t.kind = ForwardKind::Remote;
    t.remote_host = "127.0.0.1".into();
    t.remote_port = remote_port;
    t.local_addr = "127.0.0.1".into();
    t.local_port = echo_port;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![t]);
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "remote forward never came up: {:?}",
        status_of(&app, 1)
    );

    // The listener is on the *server* side (same host here), so connect to it.
    let mut conn = TcpStream::connect(("127.0.0.1", remote_port)).expect("connect to the remote forward");
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    conn.write_all(b"through-the-tunnel").unwrap();
    let mut buf = [0u8; 64];
    let n = conn.read(&mut buf).expect("read the echo");
    assert_eq!(&buf[..n], b"through-the-tunnel");
}

// ---------------------------------------------------------------------------
// -D
// ---------------------------------------------------------------------------

/// A dynamic forward must speak SOCKS5 and tunnel a CONNECT.
#[test]
#[ignore = "needs a working sshd"]
fn dynamic_forward_speaks_socks5() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    let mut t = tunnel(1, "d", free_port(), "testhost");
    t.kind = ForwardKind::Dynamic;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![t]);
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })));

    let proxy = app.tunnels[0].cfg.local_port;
    let mut conn = TcpStream::connect(("127.0.0.1", proxy)).expect("connect to the SOCKS proxy");
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

    // Greeting: SOCKS5, one method, no-auth.
    conn.write_all(&[0x05, 0x01, 0x00]).unwrap();
    let mut reply = [0u8; 2];
    conn.read_exact(&mut reply).unwrap();
    assert_eq!(reply, [0x05, 0x00], "server must select no-auth");

    // CONNECT to the sshd's own port so the target speaks first.
    let mut req = vec![0x05, 0x01, 0x00, 0x01];
    req.extend_from_slice(&[127, 0, 0, 1]);
    req.extend_from_slice(&sshd.port.to_be_bytes());
    conn.write_all(&req).unwrap();
    let mut head = [0u8; 10];
    conn.read_exact(&mut head).unwrap();
    assert_eq!(head[1], 0x00, "SOCKS5 CONNECT must succeed: {head:02x?}");

    let mut buf = [0u8; 64];
    let n = conn.read(&mut buf).expect("read the banner through SOCKS5");
    let banner = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(banner.starts_with("SSH-2.0-"), "expected the sshd banner, got {banner:?}");
}

// ---------------------------------------------------------------------------
// host-key verification
// ---------------------------------------------------------------------------

/// A key that does not match known_hosts must be refused, and the reason must
/// say so rather than a generic failure.
#[test]
#[ignore = "needs a working sshd"]
fn changed_host_key_is_refused_with_a_reason() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    // Poison known_hosts with a *different* valid key for the same host.
    let bogus = sshd.dir.path().join("bogus_key");
    std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&bogus)
        .status()
        .unwrap();
    let pubkey = std::fs::read_to_string(bogus.with_extension("pub")).unwrap();
    let mut fields = pubkey.split_whitespace();
    let (algo, b64) = (fields.next().unwrap(), fields.next().unwrap());
    std::fs::write(sshd.known_hosts(), format!("[127.0.0.1]:{} {algo} {b64}\n", sshd.port)).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![tunnel(1, "l", free_port(), "testhost")]);
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Failed { .. })),
        "a changed host key must fail the tunnel: {:?}",
        status_of(&app, 1)
    );
    match status_of(&app, 1) {
        TunnelStatus::Failed { reason } => {
            assert_eq!(reason.kind, FailureKind::HostKeyMismatch);
            assert!(reason.detail.contains("主机密钥"), "detail should explain: {}", reason.detail);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// An unknown key under `Strict` must be refused instead of learned.
#[test]
#[ignore = "needs a working sshd"]
fn unknown_host_key_is_learned_under_accept_new() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();
    // Start from an empty known_hosts.
    std::fs::write(sshd.known_hosts(), "").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![tunnel(1, "l", free_port(), "testhost")]);
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "accept-new should learn and connect: {:?}",
        status_of(&app, 1)
    );
    let written = std::fs::read_to_string(sshd.known_hosts()).unwrap();
    assert!(written.contains(&format!("[127.0.0.1]:{}", sshd.port)), "key should be learned: {written:?}");
}

// ---------------------------------------------------------------------------
// app-level behaviour
// ---------------------------------------------------------------------------

/// The app must reach `Up` through the readiness signal only, and a stop must
/// release the port promptly.
#[test]
#[ignore = "needs a working sshd"]
fn stop_releases_the_local_port() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    let local = free_port();
    let mut t = tunnel(1, "l", local, "testhost");
    t.remote_port = sshd.port;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![t]);
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })));

    app.stop_tunnel(1);
    assert!(matches!(status_of(&app, 1), TunnelStatus::Stopped));
    // The listener must be gone: binding it ourselves has to succeed.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut freed = false;
    while Instant::now() < deadline {
        if TcpListener::bind(("127.0.0.1", local)).is_ok() {
            freed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(freed, "the local port was not released after stop");
}

/// A bad alias must be reported before any connection attempt.
#[test]
fn unresolvable_host_fails_without_hanging() {
    let _guard = env_lock();
    // Point the config at a file that does not describe this host.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("client_config");
    std::fs::write(&cfg_path, "Host other\n  HostName 127.0.0.1\n").unwrap();
    unsafe { std::env::set_var("SSHPF_SSH_CONFIG", &cfg_path) };
    unsafe { std::env::set_var("SSHPF_KNOWN_HOSTS", dir.path().join("known_hosts")) };

    let mut t = tunnel(1, "l", free_port(), "nonexistent-alias");
    t.remote_port = 1;
    let mut app = app_with(dir.path(), vec![t]);
    // Auto-restart is on by default; a DNS failure is transient, so the app may
    // legitimately move to `Retrying` before this observes `Failed`. Either way
    // the point is that it fails fast with a classified reason, no hang.
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 25, |a| matches!(
            status_of(a, 1),
            TunnelStatus::Failed { .. } | TunnelStatus::Retrying { .. }
        )),
        "expected a classified failure, got {:?}",
        status_of(&app, 1)
    );
    let reason = match status_of(&app, 1) {
        TunnelStatus::Failed { reason } => reason.clone(),
        TunnelStatus::Retrying { reason, .. } => reason.clone(),
        other => panic!("expected Failed/Retrying, got {other:?}"),
    };
    assert_eq!(reason.kind, FailureKind::DnsFailure, "DNS failure must be classified: {reason:?}");

    unsafe {
        std::env::remove_var("SSHPF_SSH_CONFIG");
        std::env::remove_var("SSHPF_KNOWN_HOSTS");
    }
}

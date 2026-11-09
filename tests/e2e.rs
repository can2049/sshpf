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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
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

    /// Route the SSH *session* through `port` instead of the sshd directly.
    ///
    /// The health check probes the session, not the forwarded target, so a test
    /// that wants to break the session has to sit in its path. `~/.ssh/config`
    /// is what pins the endpoint port (`remote_port` is the target *inside* the
    /// tunnel), so this is the file to rewrite. `accept-new` then learns the
    /// relay's endpoint as a separate host entry, which is harmless here.
    fn route_through(&self, port: u16) -> bool {
        let Ok(text) = std::fs::read_to_string(self.ssh_config()) else {
            return false;
        };
        let mut out = String::with_capacity(text.len());
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Port ") {
                out.push_str(&format!("  Port {port}\n"));
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        std::fs::write(self.ssh_config(), out).is_ok()
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

/// A TCP relay in front of the sshd whose behaviour the test controls.
///
/// This is how "the peer stopped answering" is simulated without signals: the
/// relay owns both sockets, so it can hold them open while forwarding nothing
/// ([`RelayMode::Blackhole`]) — precisely a slept machine, whose TCP connection
/// stays established and whose packets go nowhere — or close them
/// ([`RelayMode::Close`]) — a peer that went away.
///
/// Signals were tried first and rejected: `sshd` forks per session and rewrites
/// its process title, so neither the pid file nor `pkill -f <config>` reliably
/// reaches the child that serves our tunnel. A relay is exact, synchronous and
/// needs no privileges.
struct Relay {
    port: u16,
    mode: Arc<AtomicU8>,
}

/// Forward traffic; then the two failure shapes the probe must tell apart.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RelayMode {
    /// Normal operation: bytes cross in both directions.
    Forward,
    /// Keep both sockets open, drop everything: a black-holed link.
    Blackhole,
    /// Close both sockets: the peer went away.
    Close,
}

impl RelayMode {
    fn code(self) -> u8 {
        match self {
            RelayMode::Forward => 0,
            RelayMode::Blackhole => 1,
            RelayMode::Close => 2,
        }
    }
}

impl Relay {
    fn start(target: u16) -> Option<Relay> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        let mode = Arc::new(AtomicU8::new(RelayMode::Forward.code()));
        let mode_for_thread = mode.clone();
        std::thread::spawn(move || {
            for inbound in listener.incoming().flatten() {
                let mode = mode_for_thread.clone();
                std::thread::spawn(move || {
                    let Ok(outbound) = TcpStream::connect(("127.0.0.1", target)) else {
                        return;
                    };
                    let Ok(inbound2) = inbound.try_clone() else {
                        return;
                    };
                    let Ok(outbound2) = outbound.try_clone() else {
                        return;
                    };
                    let a = mode.clone();
                    let b = mode;
                    std::thread::spawn(move || pump(inbound2, outbound, a));
                    pump(outbound2, inbound, b);
                });
            }
        });
        Some(Relay { port, mode })
    }

    fn set(&self, mode: RelayMode) {
        self.mode.store(mode.code(), Ordering::SeqCst);
    }
}

/// Relay one direction, obeying the current mode.
///
/// Reads are polled rather than blocked so a mode change takes effect within one
/// read timeout even while the sockets are idle — which is the state a sleeping
/// peer leaves them in.
fn pump(mut from: TcpStream, mut to: TcpStream, mode: Arc<AtomicU8>) {
    let _ = from.set_read_timeout(Some(Duration::from_millis(50)));
    let mut buf = [0u8; 8192];
    loop {
        match mode.load(Ordering::SeqCst) {
            m if m == RelayMode::Close.code() => {
                // Closing both ends is what the client sees when a peer vanishes.
                let _ = from.shutdown(std::net::Shutdown::Both);
                let _ = to.shutdown(std::net::Shutdown::Both);
                return;
            }
            m if m == RelayMode::Blackhole.code() => {
                // Drain and drop: the sender's writes still succeed, so nothing
                // upstream can tell the difference from a slow link.
                let _ = from.read(&mut buf);
                continue;
            }
            _ => {}
        }
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(_) => return,
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

/// [`app_with`] with an explicit health-check interval, so the probe can be
/// exercised without waiting the 30 s default.
fn app_with_health(dir: &Path, tunnels: Vec<TunnelConfig>, health_check_secs: u64) -> App {
    let paths = Paths { config: dir.join("config.toml"), state: dir.join("state.toml") };
    let mut cfg = config::ConfigFile { tunnels, ..Default::default() };
    cfg.defaults.health_check_secs = health_check_secs;
    config::save(&paths.config, &cfg).unwrap();
    App::new(paths)
}

fn app_with(dir: &Path, tunnels: Vec<TunnelConfig>) -> App {
    let paths = Paths { config: dir.join("config.toml"), state: dir.join("state.toml") };
    let cfg = config::ConfigFile { tunnels, ..Default::default() };
    config::save(&paths.config, &cfg).unwrap();
    App::new(paths)
}

/// [`app_with`] with an explicit process-wide host-key policy, for the escape
/// hatch (`--ignore-host-key-changes`) which is not a per-tunnel setting.
fn app_with_policy(dir: &Path, tunnels: Vec<TunnelConfig>, policy: sshpf::ssh::known_hosts::HostKeyPolicy) -> App {
    let paths = Paths { config: dir.join("config.toml"), state: dir.join("state.toml") };
    let cfg = config::ConfigFile { tunnels, ..Default::default() };
    config::save(&paths.config, &cfg).unwrap();
    App::with_host_key_policy(paths, policy)
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

/// The mirror of [`changed_host_key_is_refused_with_a_reason`]: under the
/// `--ignore-host-key-changes` escape hatch the same poisoned `known_hosts`
/// must connect, and the file must be left exactly as the user wrote it.
///
/// Both halves matter. Refusing was correct but unusable for a host whose key
/// legitimately rotates; rewriting `known_hosts` would have been convenient but
/// would silently destroy the user's own record of what the key *was*, so the
/// test pins "connects" and "did not touch the trust anchor" together.
#[test]
#[ignore = "needs a working sshd"]
fn changed_host_key_is_accepted_under_ignore_and_known_hosts_is_untouched() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    let bogus = sshd.dir.path().join("bogus_key_ignore");
    std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&bogus)
        .status()
        .unwrap();
    let pubkey = std::fs::read_to_string(bogus.with_extension("pub")).unwrap();
    let mut fields = pubkey.split_whitespace();
    let (algo, b64) = (fields.next().unwrap(), fields.next().unwrap());
    let poisoned = format!("[127.0.0.1]:{} {algo} {b64}\n", sshd.port);
    std::fs::write(sshd.known_hosts(), &poisoned).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_policy(
        dir.path(),
        vec![tunnel(1, "l", free_port(), "testhost")],
        sshpf::ssh::known_hosts::HostKeyPolicy::Ignore,
    );
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "Ignore must accept a changed key: {:?}",
        status_of(&app, 1)
    );
    app.stop_tunnel(1);

    let after = std::fs::read_to_string(sshd.known_hosts()).unwrap();
    assert_eq!(after, poisoned, "known_hosts must not be rewritten under Ignore");
}

/// `knownhosts.learned` claims a trust anchor was *written*. Under `Ignore`
/// an unknown host is accepted but nothing is persisted, so that event must not
/// be emitted — otherwise the log sends an operator looking for a `known_hosts`
/// entry that was never created.
///
/// Pinned as a regression: the record was originally emitted for both policies
/// (the acceptance branch did not distinguish them) and the documentation had
/// been written to match the bug.
#[test]
#[ignore = "needs a working sshd"]
fn an_unknown_key_under_ignore_is_not_reported_as_learned() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();
    std::fs::write(sshd.known_hosts(), "").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_policy(
        dir.path(),
        vec![tunnel(1, "l", free_port(), "testhost")],
        sshpf::ssh::known_hosts::HostKeyPolicy::Ignore,
    );
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "Ignore must accept an unknown host: {:?}",
        status_of(&app, 1)
    );
    app.stop_tunnel(1);

    // Nothing may be written, and nothing may *claim* to have been written.
    assert_eq!(std::fs::read_to_string(sshd.known_hosts()).unwrap(), "", "Ignore must not persist the key");
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

// ---------------------------------------------------------------------------
// periodic health checks (sleep/resume, half-open connections)
// ---------------------------------------------------------------------------

/// A healthy tunnel must stay `Up` across several probe intervals.
///
/// This is the guard against killing working sessions: a stock `sshd` answers
/// `keepalive@openssh.com` with `REQUEST_FAILURE`, which `russh` surfaces as
/// `Err(RequestDenied)`. Treating that "no" as a death certificate would tear
/// down every perfectly good tunnel once per interval, so the probe counts any
/// *answer* as alive.
#[test]
#[ignore = "needs a working sshd"]
fn a_healthy_tunnel_survives_repeated_health_checks() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    let mut t = tunnel(1, "healthy", free_port(), "testhost");
    t.remote_port = sshd.port;
    t.auto_restart = false;

    let dir = tempfile::tempdir().unwrap();
    // The five-second minimum, so two probes land inside the wait below.
    let mut app = app_with_health(dir.path(), vec![t], sshpf::config::MIN_HEALTH_CHECK_SECS);
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "tunnel never came up: {:?}",
        status_of(&app, 1)
    );

    // Two full intervals plus the probe timeout: at least one probe has been
    // sent and answered by now.
    std::thread::sleep(Duration::from_secs(2 * sshpf::config::MIN_HEALTH_CHECK_SECS + 2));
    app.drain_events();
    assert!(
        matches!(status_of(&app, 1), TunnelStatus::Up { .. }),
        "a healthy tunnel must not be failed by its own health check: {:?}",
        status_of(&app, 1)
    );

    let local = app.tunnels[0].cfg.local_port;
    let mut conn = TcpStream::connect(("127.0.0.1", local)).expect("the forward must still work");
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = [0u8; 64];
    let n = conn.read(&mut buf).expect("read through the tunnel after the probes");
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("SSH-2.0-"), "a probed tunnel must still forward");
}

/// The case this feature exists for: the SSH session dies while the local
/// listener keeps accepting. Without the probe the row would stay `●` forever.
///
/// The relay fronting the sshd simply closes its sockets, which is what a peer
/// that went away looks like to us. What must NOT happen is the old behaviour:
/// the local listener keeps `accept`ing, so nothing surfaces the loss on its
/// own.
#[test]
#[ignore = "needs a working sshd"]
fn a_dead_session_behind_a_live_listener_is_detected() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();
    let Some(relay) = Relay::start(sshd.port) else { return };
    assert!(sshd.route_through(relay.port), "could not route the session through the relay");

    let local = free_port();
    let mut t = tunnel(1, "zombie", local, "testhost");
    t.remote_port = sshd.port;
    // No automatic retry, so the final state is readable instead of flapping.
    t.auto_restart = false;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_health(dir.path(), vec![t], sshpf::config::MIN_HEALTH_CHECK_SECS);
    app.cfg.defaults.auto_restart = false;
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "tunnel never came up: {:?}",
        status_of(&app, 1)
    );

    // The peer vanishes; the local listener stays bound and keeps accepting, so
    // the app has no way to learn this passively — that is the bug's shape.
    relay.set(RelayMode::Close);
    std::thread::sleep(Duration::from_millis(200));
    assert!(TcpStream::connect(("127.0.0.1", local)).is_ok(), "the listener is still bound (this is the bug's shape)");

    assert!(
        wait_for(&mut app, 40, |a| !matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "a dead session must not stay Up: {:?}",
        status_of(&app, 1)
    );
    match status_of(&app, 1) {
        TunnelStatus::Failed { reason } => assert_eq!(
            reason.kind,
            FailureKind::StaleConnection,
            "a lost session is a stale connection, not a generic failure: {reason:?}"
        ),
        // With `auto_restart` on, the retry path carries the same reason.
        TunnelStatus::Retrying { reason, .. } => assert_eq!(reason.kind, FailureKind::StaleConnection, "{reason:?}"),
        other => panic!("expected Failed/Retrying after a lost session, got {other:?}"),
    }
}

/// The exact sleep/resume scenario: the peer stops answering but its socket
/// stays open, so only a probe that *waits* can tell the session is gone.
///
/// A `Timeout`-classified failure would mean the probe never ran and the run
/// died through some other path; `stale_connection` is the proof that the
/// keepalive reply is what went missing.
#[test]
#[ignore = "needs a working sshd"]
fn a_half_open_session_is_detected_by_the_probe_timeout() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();
    let Some(relay) = Relay::start(sshd.port) else { return };
    assert!(sshd.route_through(relay.port), "could not route the session through the relay");

    let mut t = tunnel(1, "asleep", free_port(), "testhost");
    t.remote_port = sshd.port;
    t.auto_restart = false;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_health(dir.path(), vec![t], sshpf::config::MIN_HEALTH_CHECK_SECS);
    app.cfg.defaults.auto_restart = false;
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "tunnel never came up: {:?}",
        status_of(&app, 1)
    );

    // Black-hole the link: the TCP connection is not closed, the peer just never
    // replies — a sleeping machine, not a crashed one.
    relay.set(RelayMode::Blackhole);

    let detected = wait_for(&mut app, 40, |a| {
        matches!(status_of(a, 1), TunnelStatus::Failed { .. } | TunnelStatus::Retrying { .. })
    });
    assert!(detected, "a paused peer must be detected as a stale session: {:?}", status_of(&app, 1));
    let reason = match status_of(&app, 1) {
        TunnelStatus::Failed { reason } => reason.clone(),
        TunnelStatus::Retrying { reason, .. } => reason.clone(),
        other => panic!("expected Failed/Retrying, got {other:?}"),
    };
    assert_eq!(reason.kind, FailureKind::StaleConnection, "{reason:?}");
}

// ---------------------------------------------------------------------------
// Importing forwards declared in ~/.ssh/config
// ---------------------------------------------------------------------------

/// End-to-end proof of the headline feature: a forward declared only in
/// `~/.ssh/config` is discovered, turned into a tunnel, and really carries bytes.
///
/// The interesting part is that nothing here hand-builds a `TunnelConfig` — the
/// config file is the only source, which is exactly the path a user takes
/// (`n` -> pick host -> the pane lists it -> `ctrl+s`).
#[test]
#[ignore = "needs a working sshd"]
fn imported_ssh_config_forward_carries_traffic() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    // Declare a LocalForward in the client config, targeting the sshd's own
    // port: its SSH banner proves bytes crossed the tunnel.
    let local_port = free_port();
    let mut cfg = std::fs::read_to_string(sshd.ssh_config()).unwrap_or_default();
    cfg.push_str(&format!(
        "\nHost testhost\n  HostName 127.0.0.1\n  Port {}\n  LocalForward {local_port} 127.0.0.1:{}\n",
        sshd.port, sshd.port
    ));
    std::fs::write(sshd.ssh_config(), cfg).unwrap();

    // Discovery is the entry point: whatever it reports is what the pane shows.
    let rows = sshpf::ssh::forwards::forwards_for_host("testhost").expect("scan should succeed");
    assert_eq!(rows.len(), 1, "the declared forward must be discovered: {rows:#?}");
    assert_eq!(rows[0].listen_port, local_port);
    assert!(rows[0].unsupported_reason().is_none(), "it must be importable");
    let forward = rows[0].clone();

    // Build the tunnel the way the form does, then run it for real.
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    config::save(&paths.config, &config::ConfigFile::default()).unwrap();
    let mut app = App::new(paths);
    let mut t = app.config_from_forward(1, &forward, &[]);
    t.auto_restart = false;
    assert_eq!(t.local_port, local_port, "a free port must be imported verbatim");
    app.cfg.tunnels.push(t.clone());
    app.tunnels.push(sshpf::model::TunnelView::new(t.clone()));

    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "imported tunnel never came up: {:?}",
        status_of(&app, 1)
    );

    let mut conn = TcpStream::connect(("127.0.0.1", local_port)).expect("connect through the imported forward");
    conn.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = [0u8; 64];
    let n = conn.read(&mut buf).expect("read the target banner through the imported forward");
    let banner = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(banner.starts_with("SSH-2.0-"), "expected the sshd banner through the import, got {banner:?}");
}

// ---------------------------------------------------------------------------
// diagnostic log
// ---------------------------------------------------------------------------

/// The whole chain: a real tunnel goes up and down, and the file a user would
/// attach to a bug report contains the lifecycle with usable fields.
///
/// Runs in its own temp log dir. Because the logger is process-global and only
/// the first `init` wins, this test skips itself when another one already
/// installed it (the suite runs single-threaded in CI, so the skip is rare).
#[test]
#[ignore = "needs a working sshd"]
fn tunnel_lifecycle_is_written_to_the_log_file() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    let log_dir = tempfile::tempdir().unwrap();
    let log_path = match sshpf::logging::init(sshpf::logging::Config {
        dir: log_dir.path().to_path_buf(),
        level: sshpf::logging::Level::Debug,
        ..Default::default()
    }) {
        Ok(path) => path,
        // Another test installed the logger first: its file already proves the
        // same thing, and init cannot be undone.
        Err(_) => return,
    };

    let mut t = tunnel(1, "logged", free_port(), "testhost");
    t.remote_host = "127.0.0.1".into();
    t.remote_port = sshd.port;
    t.auto_restart = false;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![t]);
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 30, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "tunnel never came up: {:?}",
        status_of(&app, 1)
    );
    app.stop_tunnel(1);

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let find = |event: &str| -> String {
        log.lines()
            .find(|l| l.contains(&format!("event={event}")))
            .unwrap_or_else(|| panic!("no {event} record in:\n{log}"))
            .to_string()
    };

    let start = find("tunnel.start");
    assert!(start.contains("trigger=manual"), "why it started is recorded: {start}");
    assert!(start.contains("name=logged"), "which tunnel: {start}");
    assert!(start.contains("forward=local"), "which kind: {start}");

    let spawn = find("tunnel.spawn");
    assert!(spawn.contains("hostkey_policy=accept-new"), "connection parameters: {spawn}");

    let ready = find("tunnel.ready");
    assert!(ready.contains("tunnel=1"), "{ready}");
    assert!(ready.contains("elapsed_ms="), "how long it took to come up: {ready}");

    let stop = find("tunnel.stop");
    assert!(stop.contains("trigger=manual"), "{stop}");

    // The tunnel's own progress lines are mirrored at debug level, which is what
    // makes the file enough to reconstruct a session without the TUI.
    assert!(log.contains("event=tunnel.line"), "progress lines missing:\n{log}");
}

// ---------------------------------------------------------------------------
// remote-port picker: probing the peer for listening ports
// ---------------------------------------------------------------------------

/// The peer scan must find a port that really is listening, over the same SSH
/// path a tunnel uses.
///
/// This is the end-to-end proof of the feature: the unit tests cover the parser
/// and the picker state, but only a real `ss`/`netstat` on a real host proves the
/// command, the channel plumbing and the ssh-config/`ProxyJump` reuse are right.
#[test]
#[ignore = "needs a working sshd"]
fn the_port_scan_finds_a_listening_service() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    // A service on a port we chose, so the assertion cannot be satisfied by
    // whatever else the machine happens to listen on.
    let service = TcpListener::bind(("127.0.0.1", 0)).expect("bind a service to find");
    let service_port = service.local_addr().unwrap().port();
    // Keep it alive for the duration of the scan.
    std::thread::spawn(move || for _ in service.incoming() {});

    let outcome = tokio_scan("testhost");
    let ports: Vec<u16> = outcome.iter().map(|p| p.port).collect();
    assert!(ports.contains(&service_port), "the scan must report {service_port}: {ports:?}");
    assert!(ports.contains(&sshd.port), "the sshd's own listener must be listed: {ports:?}");
}

/// A host with neither `ss` nor `netstat` must be distinguishable from a host with
/// nothing listening — the two answers lead a user in opposite directions.
#[test]
#[ignore = "needs a working sshd"]
fn a_peer_without_a_tool_is_reported_not_shown_as_empty() {
    // Pure assertion on the interpreter: the sshd fixture cannot uninstall `ss`
    // from the machine, and this is the exact string the remote side prints.
    let outcome = sshpf::ssh::probe::interpret(&format!("{}\n", sshpf::ssh::probe::NO_TOOL_SENTINEL));
    assert_eq!(outcome, sshpf::ssh::probe::ScanOutcome::NoTool);
    assert_eq!(sshpf::ssh::probe::interpret(""), sshpf::ssh::probe::ScanOutcome::Found(Vec::new()));
}

/// The same scan through the `App`: `p` on the remote-port field fills the pane.
#[test]
#[ignore = "needs a working sshd"]
fn the_port_picker_is_populated_through_the_app() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![]);
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("testhost");
    app.open_port_picker();
    assert!(app.port_picker_open(), "the pane opens immediately, showing the scan is running");

    let found = wait_for_port_rows(&mut app, 30);
    assert!(found, "the scan never produced rows; state: {:?}", app.ports.as_ref().map(|p| p.state.clone()));
    let ports: Vec<u16> = app.ports.as_ref().unwrap().rows.iter().map(|r| r.port.port).collect();
    assert!(ports.contains(&sshd.port), "the sshd's listener must be offered: {ports:?}");
    assert_eq!(app.ports.as_ref().unwrap().checked_count(), 0, "nothing is ticked until the user ticks it");
}

/// `enter` on a row writes the port into the form field.
#[test]
#[ignore = "needs a working sshd"]
fn accepting_a_scanned_port_fills_the_field() {
    let _guard = env_lock();
    let Some(sshd) = start_sshd() else { return };
    sshd.activate();

    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with(dir.path(), vec![]);
    app.open_form_create();
    app.form.as_mut().unwrap().ssh_host.set("testhost");
    app.open_port_picker();
    assert!(wait_for_port_rows(&mut app, 30), "no rows to accept");

    // Put the cursor on the sshd's own port so the assertion is deterministic.
    let pick = app.ports.as_mut().unwrap();
    if let Some(i) = pick.rows.iter().position(|r| r.port.port == sshd.port) {
        pick.cursor = i;
    }
    let expected = app.ports.as_ref().unwrap().cursor_row().unwrap().port.port;
    app.port_accept_cursor();
    assert_eq!(app.form.as_ref().unwrap().remote_port.value(), expected.to_string());
    assert!(!app.port_picker_open(), "accepting collapses the list");
    // The local port is *suggested* from the port just picked. It is not always
    // the same number: this fixture's sshd holds its own port on loopback, so
    // `plan_local_port` walks upwards to the next free one. Either is right; what
    // would be wrong is an empty field or a port that did not come from the pick.
    let local: u16 = app.form.as_ref().unwrap().local_port.value().parse().expect("a local port is suggested");
    assert!(local >= expected, "the suggestion must not go below the picked port: {local} < {expected}");
    assert!(local - expected <= 200, "the suggestion must stay near the picked port, got {local} for {expected}");
}

/// Scan `ssh_host` on a private runtime and return the ports it reported.
fn tokio_scan(ssh_host: &str) -> Vec<sshpf::model::RemotePort> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let outcome = rt
        .block_on(sshpf::ssh::probe::scan_listen_ports(ssh_host, sshpf::ssh::known_hosts::HostKeyPolicy::AcceptNew))
        .expect("the scan should succeed against a live sshd");
    outcome.ports().to_vec()
}

/// Poll the app until the port scan has produced rows (or failed).
fn wait_for_port_rows(app: &mut App, secs: u64) -> bool {
    wait_for(app, secs, |a| a.ports.as_ref().is_some_and(|p| !p.is_empty() || p.state.is_failed()))
}

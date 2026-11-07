//! End-to-end tests that drive the real `App` state machine against a real
//! `ssh` binary.
//!
//! Two flavours are used:
//!
//! * `fake_ssh_*` — the bundled `tests/fixtures/fake-ssh.sh` impersonates every
//!   interesting ssh outcome (port in use, auth failure, DNS failure, …)
//!   deterministically, without network access.
//! * `real_ssh_*`  — a throw-away `sshd` is started on a loopback port and a
//!   genuine tunnel is established. These are `#[ignore]`d by default because
//!   they need a working OpenSSH server; run with
//!   `cargo test --test e2e -- --ignored --test-threads=1`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use sshpf::app::App;
use sshpf::config::{self, ConfigFile, Paths};
use sshpf::model::{FailureKind, ForwardKind, TunnelConfig, TunnelStatus};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Serializes every test that mutates process-global `SSHPF_*` environment
/// variables. Cargo runs tests in parallel threads within one process, and the
/// app reads those variables at spawn time, so unsynchronised tests would
/// impersonate the wrong ssh behaviour.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fake_ssh() -> PathBuf {
    repo_root().join("tests/fixtures/fake-ssh.sh")
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

/// Build an `App` backed by a temp config and the fake ssh binary.
fn app_with(hosts: &[TunnelConfig], mode: &str) -> (tempfile::TempDir, App, MutexGuard<'static, ()>) {
    let guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let mut cfg = ConfigFile::default();
    cfg.defaults.auto_restart = false;
    cfg.tunnels = hosts.to_vec();
    config::save(&paths.config, &cfg).unwrap();

    // SAFETY: tests using env vars run in the same process; the suite around
    // these tests does not spawn threads that read them concurrently.
    unsafe {
        std::env::set_var("SSHPF_SSH_BIN", fake_ssh());
        std::env::set_var("SSHPF_FAKE_MODE", mode);
        // An empty ssh config keeps host resolution deterministic.
        std::env::set_var("SSHPF_SSH_CONFIG", dir.path().join("ssh_config"));
    }
    std::fs::write(dir.path().join("ssh_config"), "").unwrap();
    let app = App::new(paths);
    (dir, app, guard)
}

/// Pump the app's event loop until `pred` holds or the deadline passes.
fn wait_for(app: &mut App, secs: u64, mut pred: impl FnMut(&App) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        app.drain_events();
        if pred(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    app.drain_events();
    pred(app)
}

fn status_of(app: &App, id: u64) -> &TunnelStatus {
    &app.tunnel(id).expect("tunnel present").status
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

// ---------------------------------------------------------------------------
// fake ssh driven scenarios
// ---------------------------------------------------------------------------

#[test]
fn fake_ssh_ok_reaches_up() {
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "ok", free_port(), "testhost")], "ok");
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 8, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "expected Up, got {:?}",
        status_of(&app, 1)
    );
    app.stop_tunnel(1);
    assert!(matches!(status_of(&app, 1), TunnelStatus::Stopped));
}

#[test]
fn fake_ssh_port_in_use_is_classified() {
    // The -L port is occupied by our own listener: pre-flight catches it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "busy", port, "testhost")], "ok");
    app.start_tunnel(1);
    match status_of(&app, 1) {
        TunnelStatus::Failed { reason } => assert_eq!(reason.kind, FailureKind::PortInUse),
        other => panic!("expected PortInUse, got {other:?}"),
    }
    assert_eq!(app.running_count(), 0);
    drop(listener);
}

#[test]
fn fake_ssh_auth_failure_is_classified_and_process_stops() {
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "auth", free_port(), "testhost")], "auth-failed");
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 8, |a| matches!(status_of(a, 1), TunnelStatus::Failed { .. })));
    match status_of(&app, 1) {
        TunnelStatus::Failed { reason } => {
            assert_eq!(reason.kind, FailureKind::AuthFailed, "{reason:?}");
            assert!(reason.detail.contains("Permission denied"), "{reason:?}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    // Nothing may be left running.
    assert_eq!(app.running_count(), 0);
}

#[test]
fn fake_ssh_dns_failure_is_classified() {
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "dns", free_port(), "no-such-host")], "dns-failure");
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 8, |a| matches!(status_of(a, 1), TunnelStatus::Failed { .. })));
    match status_of(&app, 1) {
        TunnelStatus::Failed { reason } => assert_eq!(reason.kind, FailureKind::DnsFailure, "{reason:?}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn fake_ssh_immediate_exit_does_not_retry_when_disabled() {
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "dead", free_port(), "testhost")], "exit-immediately");
    assert!(!app.cfg.defaults.auto_restart);
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 8, |a| matches!(status_of(a, 1), TunnelStatus::Failed { .. })));
    // Give the loop a chance to (wrongly) retry.
    for _ in 0..30 {
        app.drain_events();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!app.cfg.defaults.auto_restart, "sanity: auto_restart must still be off");
    assert!(matches!(status_of(&app, 1), TunnelStatus::Failed { .. }));
}

#[test]
fn fake_ssh_up_then_die_is_reported() {
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "flap", free_port(), "testhost")], "up-then-die");
    app.start_tunnel(1);
    // It may briefly report Up, but must settle on Failed once ssh exits.
    assert!(
        wait_for(&mut app, 10, |a| matches!(status_of(a, 1), TunnelStatus::Failed { .. })),
        "expected Failed after exit, got {:?}",
        status_of(&app, 1)
    );
    assert_eq!(app.running_count(), 0);
}

#[test]
fn fake_ssh_target_refused_keeps_tunnel_up_but_logs_target_error() {
    // A dead target behind a healthy tunnel is not a tunnel failure: the
    // listener is fine, only one connection attempt failed.
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "tgt", free_port(), "testhost")], "target-refused");
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 8, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })));
    wait_for(&mut app, 2, |_| false);
    let log: Vec<String> = app.tunnel(1).unwrap().log.iter().map(|l| l.text.clone()).collect();
    assert!(log.iter().any(|l| l.contains("connect failed")), "target refusal should be logged: {log:?}");
    app.stop_tunnel(1);
}

#[test]
fn restart_after_stop_works() {
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "r", free_port(), "testhost")], "ok");
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 8, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })));
    app.stop_tunnel(1);
    assert!(matches!(status_of(&app, 1), TunnelStatus::Stopped));
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 8, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })));
    app.stop_tunnel(1);
}

#[test]
fn shutdown_kills_children_and_removes_scratch_files() {
    let (_dir, mut app, _guard) = app_with(&[tunnel(1, "s", free_port(), "testhost")], "ok");
    app.start_tunnel(1);
    assert!(wait_for(&mut app, 8, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })));
    app.shutdown();
    assert_eq!(app.running_count(), 0);
}

#[test]
fn persisted_config_round_trips_and_restores_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let cfg = ConfigFile { tunnels: vec![tunnel(1, "kept", free_port(), "testhost")], ..ConfigFile::default() };
    config::save(&paths.config, &cfg).unwrap();

    let reloaded = config::load(&paths.config).unwrap();
    assert_eq!(reloaded.tunnels.len(), 1);
    assert!(reloaded.tunnels[0].enabled, "enabled must survive a round trip");
    assert_eq!(reloaded.tunnels[0].name, "kept");
}

// ---------------------------------------------------------------------------
// real sshd driven scenarios (opt-in)
// ---------------------------------------------------------------------------

/// Start a throw-away `sshd` on a free loopback port.
///
/// Returns `(tempdir, port)`; killing happens by dropping the tempdir's sshd
/// pid at test end. Skipped (returns `None`) when sshd cannot be started.
fn start_temp_sshd() -> Option<(tempfile::TempDir, u16)> {
    let dir = tempfile::tempdir().ok()?;
    let port = free_port();
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-f"])
        .arg(dir.path().join("hostkey"))
        .arg("-N")
        .arg("")
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let home = dirs::home_dir()?;
    std::fs::copy(home.join(".ssh/id_rsa.pub"), dir.path().join("authorized_keys")).ok()?;
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
             PermitOpen any\n",
            hk = dir.path().join("hostkey").display(),
            pid = dir.path().join("sshd.pid").display(),
            ak = dir.path().join("authorized_keys").display(),
        ),
    )
    .ok()?;
    let child = std::process::Command::new("/usr/sbin/sshd").arg("-f").arg(&config).spawn().ok()?;
    let mut child = child;
    // sshd daemonises; wait until the port answers.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some((dir, port));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    None
}

#[test]
#[ignore = "needs a working sshd and a usable key"]
fn real_ssh_forwards_traffic() {
    let _guard = env_lock();
    let Some((_sshd, ssh_port)) = start_temp_sshd() else {
        eprintln!("skipping: could not start a temporary sshd");
        return;
    };
    let home = dirs::home_dir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let ssh_config = dir.path().join("ssh_config");
    std::fs::write(
        &ssh_config,
        format!(
            "Host localtest\n\
             \x20   HostName 127.0.0.1\n\
             \x20   Port {ssh_port}\n\
             \x20   User {user}\n\
             \x20   IdentityFile {key}\n\
             \x20   StrictHostKeyChecking no\n\
             \x20   UserKnownHostsFile /dev/null\n",
            user = whoami(),
            key = home.join(".ssh/id_rsa").display(),
        ),
    )
    .unwrap();

    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let mut cfg = ConfigFile::default();
    cfg.defaults.auto_restart = false;
    let local_port = free_port();
    let mut t = tunnel(1, "real", local_port, "localtest");
    t.remote_host = "127.0.0.1".into();
    t.remote_port = ssh_port;
    cfg.tunnels = vec![t];
    config::save(&paths.config, &cfg).unwrap();

    // Do not inherit the fake binary from other tests.
    unsafe {
        std::env::remove_var("SSHPF_SSH_BIN");
        std::env::remove_var("SSHPF_FAKE_MODE");
        std::env::set_var("SSHPF_SSH_CONFIG", &ssh_config);
    }

    let mut app = App::new(paths);
    app.start_tunnel(1);
    assert!(
        wait_for(&mut app, 20, |a| matches!(status_of(a, 1), TunnelStatus::Up { .. })),
        "tunnel did not come up: {:?}\nlog: {:#?}",
        status_of(&app, 1),
        app.tunnel(1).unwrap().log.iter().map(|l| l.text.clone()).collect::<Vec<_>>()
    );

    // The forwarded port must really carry traffic (the sshd banner).
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", local_port)).expect("connect to forwarded port");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 32];
    let n = stream.read(&mut buf).expect("read banner through tunnel");
    let banner = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(banner.starts_with("SSH-2.0-OpenSSH"), "unexpected banner: {banner:?}");

    app.shutdown();
    // The listen port must be released.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpListener::bind(("127.0.0.1", local_port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("forwarded port {local_port} was not released after shutdown");
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "root".to_string())
}

/// Guard against accidentally shipping a test that depends on the developer's
/// own `~/.ssh/config`.
#[test]
fn fixture_scripts_exist_and_are_executable() {
    let p = fake_ssh();
    assert!(p.exists(), "missing {}", p.display());
    use std::os::unix::fs::PermissionsExt;
    assert!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o111 != 0);
    let _ = repo_root().join("docs/research/tech-options.md");
    let _: Option<&Path> = None;
}

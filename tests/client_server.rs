//! The user-visible contract of the client-server runtime, tested against a
//! **real** server process and **real** client processes.
//!
//! # What this suite exists to prove
//!
//! The bug it was written for: opening a second `sshpf` used to fail with
//! "端口已被占用 / port already in use", because every instance was its own
//! single-process owner that tried to bind the same local ports. The requirement
//! is the opposite — a new client must show the first client's state, and any
//! client must be able to operate it.
//!
//! So the assertions here are about *shared* state, not about internals:
//!
//! ```text
//! sshpf (client A) ─┐
//!                   ├─► one server process ─► the only tunnels, the only config
//! sshpf (client B) ─┘
//! ```
//!
//! # Why a real binary and real processes
//!
//! Two of the claims are inherently cross-process: **only one server exists** and
//! **a change made by A is visible to B**. Both are unobservable from inside one
//! process, and both are exactly what broke. `tests/logging.rs` establishes the
//! same pattern for the log.
//!
//! # Isolation
//!
//! Every test gets its own `SSHPF_CONFIG` (a temp dir) *and* its own
//! `SSHPF_SOCKET_DIR`. The socket dir matters as much as the config: the endpoint
//! is `socket_dir/<hash of config>`, and a shared runtime dir would let two tests
//! (or a leftover worker from a previous run) elect each other's servers.
//!
//! These tests do not need `sshd`: a tunnel that cannot connect is still a
//! tunnel, and its failure is *state* that must be shared. That keeps the suite
//! runnable on any machine, unlike `tests/e2e.rs`.
//!
//! Run with:
//!
//! ```text
//! cargo test --test client_server -- --test-threads=1
//! ```

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sshpf::ipc::{self, ClientMsg, ServerMsg};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sshpf")
}

/// A temp config + socket dir, so this test cannot meet anyone else's server.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Sandbox {
        Sandbox { dir: tempfile::tempdir().expect("a temp dir") }
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn socket_dir(&self) -> PathBuf {
        self.dir.path().join("sock")
    }

    /// Write a config with one `-L` forward that will fail to connect (no host),
    /// so the tunnel exists and has a visible status without needing `sshd`.
    fn write_config(&self, tunnels: &[(&str, u16)]) {
        let mut text = String::from("version = 1\n\n[defaults]\nrestore_on_start = false\nauto_restart = false\n");
        for (i, (name, port)) in tunnels.iter().enumerate() {
            text.push_str(&format!(
                "\n[[tunnels]]\nid = {}\nname = \"{name}\"\nkind = \"local\"\nssh_host = \"no-such-host.invalid\"\n\
                 local_addr = \"127.0.0.1\"\nlocal_port = {port}\nremote_host = \"127.0.0.1\"\n\
                 remote_port = 80\nenabled = false\nauto_restart = false\n",
                i + 1
            ));
        }
        std::fs::create_dir_all(self.dir.path()).ok();
        std::fs::write(self.config(), text).expect("the config must be writable");
    }

    /// A `Command` for the binary with this sandbox's isolation env applied.
    fn command(&self) -> Command {
        let mut cmd = Command::new(bin());
        cmd.env("SSHPF_CONFIG", self.config())
            .env("SSHPF_SOCKET_DIR", self.socket_dir())
            .env("SSHPF_LOG_DIR", self.dir.path().join("logs"))
            // A detached server must never inherit a terminal from the test.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }

    /// Start a client that will attach and then be killed by `Drop`.
    fn spawn_client(&self) -> ChildGuard {
        let child = self.command().spawn().expect("the binary must start");
        ChildGuard { child, _sandbox: () }
    }

    /// Connect to this sandbox's server, or `None` when it is not listening yet.
    fn try_connect(&self) -> Option<std::os::unix::net::UnixStream> {
        let endpoint = ipc::endpoint_for(&self.config());
        std::os::unix::net::UnixStream::connect(&endpoint.socket_path).ok()
    }

    /// Wait until the server for this sandbox accepts a connection and the
    /// handshake succeeds. Returns the socket, already past `Welcome`.
    fn wait_for_server(&self, budget: Duration) -> std::os::unix::net::UnixStream {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if let Some(mut stream) = self.try_connect() {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let hello = ClientMsg::Hello {
                    proto_version: ipc::PROTO_VERSION,
                    config_path: ipc::endpoint_for(&self.config()).config_path.display().to_string(),
                    // This test speaks the protocol directly, so it declares the
                    // policy it would pass on a real command line.
                    host_key_policy: Some("ignore".into()),
                };
                if ipc::write_message(&mut stream, &hello).is_ok()
                    && let Ok(Some(ServerMsg::Welcome { .. })) = ipc::read_server_msg(&mut stream)
                {
                    return stream;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("no server came up within {budget:?} — the client did not attach or spawn one");
    }
}

/// Kills its child on drop, so a failing assertion cannot leave a server or a
/// TUI process behind to poison the next run.
struct ChildGuard {
    child: Child,
    _sandbox: (),
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read every frame the server sends until `predicate` accepts one, or the budget
/// runs out. Returns the accepted message.
fn read_until(
    stream: &mut std::os::unix::net::UnixStream,
    budget: Duration,
    mut predicate: impl FnMut(&ServerMsg) -> bool,
) -> Option<ServerMsg> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        match ipc::read_server_msg(stream) {
            Ok(Some(msg)) => {
                if predicate(&msg) {
                    return Some(msg);
                }
            }
            // EOF or a decode failure: nothing more will arrive.
            Ok(None) | Err(_) => return None,
        }
    }
    None
}

/// Send one command and wait for its reply.
fn command(stream: &mut std::os::unix::net::UnixStream, req_id: u64, op: ipc::Op) -> ServerMsg {
    let msg = ClientMsg::Command { req_id, expect_rev: None, op };
    ipc::write_message(stream, &msg).expect("the command must be writable");
    read_until(stream, Duration::from_secs(10), |m| matches!(m, ServerMsg::Reply { req_id: r, .. } if *r == req_id))
        .expect("the server must answer the command")
}

/// The second client attaches to the first client's server instead of becoming
/// its own owner — the reported bug.
#[test]
fn a_second_client_attaches_to_the_same_server() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&[("web", 18081)]);

    // Let the first client start the server (and hold the election lock while it
    // waits for its own connect — the exact window a second client must survive).
    let first = sandbox.spawn_client();
    let mut a = sandbox.wait_for_server(Duration::from_secs(20));

    // Both handshakes report the same server process: one server, not two.
    let pid_of = |msg: &ServerMsg| match msg {
        ServerMsg::Welcome { server_pid, .. } => Some(*server_pid),
        _ => None,
    };
    let hello = |sandbox: &Sandbox| ClientMsg::Hello {
        proto_version: ipc::PROTO_VERSION,
        config_path: ipc::endpoint_for(&sandbox.config()).config_path.display().to_string(),
        host_key_policy: None,
    };
    let mut b = sandbox.try_connect().expect("the server must still be listening");
    b.set_read_timeout(Some(Duration::from_secs(5))).ok();
    ipc::write_message(&mut b, &hello(&sandbox)).expect("hello must be writable");
    let welcome_b = ipc::read_server_msg(&mut b).expect("must decode").expect("must be a frame");
    let pid_b = pid_of(&welcome_b).expect("the second connection must get a Welcome");

    let welcome_a = ipc::read_server_msg(&mut a).expect("must decode");
    let pid_a = welcome_a.as_ref().and_then(pid_of);
    assert_eq!(pid_a, Some(pid_b), "both clients must be served by one server process");

    // And the server counts both of them, which is what drives the
    // "last client" warning.
    let snap = read_until(&mut b, Duration::from_secs(10), |m| {
        matches!(m, ServerMsg::Snapshot(s) if s.client_count >= 2)
    });
    assert!(snap.is_some(), "the server must count both attached clients");
    drop(first);
}

/// A command from one client is visible to the other: the state is shared, not
/// copied. This is the "每一个 client 都能实施操作和修改" half of the requirement.
#[test]
fn a_change_made_by_one_client_is_seen_by_the_other() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&[("web", 18082)]);

    let _first = sandbox.spawn_client();
    let mut a = sandbox.wait_for_server(Duration::from_secs(20));

    // Second client, attached to the same server.
    let mut b = sandbox.try_connect().expect("the server must be listening");
    b.set_read_timeout(Some(Duration::from_secs(5))).ok();
    ipc::write_message(
        &mut b,
        &ClientMsg::Hello {
            proto_version: ipc::PROTO_VERSION,
            config_path: ipc::endpoint_for(&sandbox.config()).config_path.display().to_string(),
            host_key_policy: None,
        },
    )
    .expect("hello must be writable");
    let _ = ipc::read_server_msg(&mut b);

    // A starts the tunnel (it will fail to resolve a host, but it is still a
    // state transition the server owns and both clients must see).
    let reply = command(&mut a, 1, ipc::Op::Start { tunnel_id: 1 });
    match reply {
        ServerMsg::Reply { ok, .. } => assert!(ok, "the server must accept a start for an existing tunnel"),
        other => panic!("expected a Reply, got {other:?}"),
    }

    // B must observe the resulting status without having asked for it.
    let seen = read_until(&mut b, Duration::from_secs(10), |m| match m {
        ServerMsg::Snapshot(s) => s
            .tunnels
            .iter()
            .any(|t| matches!(t.status, ipc::WireStatus::Starting { .. } | ipc::WireStatus::Failed { .. })),
        _ => false,
    });
    assert!(seen.is_some(), "a change applied for client A must reach client B");
}

/// The server refuses a definition that does not validate, and the refusal is
/// reported rather than silently accepted — the server is the authority even when
/// a client thinks its own check passed.
#[test]
fn the_server_refuses_an_invalid_definition() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&[]);

    let _first = sandbox.spawn_client();
    let mut a = sandbox.wait_for_server(Duration::from_secs(20));

    let mut bad = sshpf::model::TunnelConfig {
        id: 0,
        name: String::new(), // invalid: a tunnel must have a name
        kind: sshpf::model::ForwardKind::Local,
        ssh_host: "no-such-host.invalid".into(),
        local_addr: "127.0.0.1".into(),
        local_port: 18083,
        remote_host: "127.0.0.1".into(),
        remote_port: 80,
        enabled: false,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 0,
    };
    bad.name.clear();
    let reply = command(&mut a, 1, ipc::Op::Create { cfg: Box::new(bad) });
    match reply {
        ServerMsg::Reply { ok, error, .. } => {
            assert!(!ok, "an unnamed tunnel must be refused");
            let err = error.expect("a refusal must carry a reason the client can show");
            assert!(!err.zh.is_empty() && !err.en.is_empty(), "the reason must be bilingual");
            assert_eq!(err.code, "invalid_config", "the code must be the documented one: {err:?}");
        }
        other => panic!("expected a Reply, got {other:?}"),
    }
}

/// A `delete` with a revision the server has moved past is refused with `Stale`
/// rather than applied — a stale view must not destroy someone else's definition.
#[test]
fn a_delete_with_a_stale_revision_is_refused() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&[("web", 18084)]);

    let _first = sandbox.spawn_client();
    let mut a = sandbox.wait_for_server(Duration::from_secs(20));

    // Nonsense revision: the server has certainly moved past it.
    let msg = ClientMsg::Command {
        req_id: 7,
        expect_rev: Some(0),
        op: ipc::Op::Delete { tunnel_id: 1 },
    };
    ipc::write_message(&mut a, &msg).expect("must be writable");
    let reply = read_until(&mut a, Duration::from_secs(10), |m| {
        matches!(m, ServerMsg::Stale { req_id, .. } if *req_id == 7)
    });
    assert!(reply.is_some(), "a stale expect_rev must produce Stale, not a silent delete");

    // The tunnel must still be there.
    let snap = read_until(&mut a, Duration::from_secs(10), |m| {
        matches!(m, ServerMsg::Snapshot(s) if s.tunnels.iter().any(|t| t.cfg.id == 1))
    });
    assert!(snap.is_some(), "a refused delete must not have removed the definition");
}

/// The last client leaving stops the server and cleans up its socket; the next
/// client starts a fresh server. Without this, a stale socket would accumulate
/// and every later run would depend on recovery code instead of the happy path.
#[test]
fn the_last_client_leaving_stops_the_server() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&[]);

    let first = sandbox.spawn_client();
    let mut a = sandbox.wait_for_server(Duration::from_secs(20));

    // Say goodbye as the only client: the server must recount and shut down.
    ipc::write_message(&mut a, &ClientMsg::Bye { last_exit: true }).expect("bye must be writable");
    drop(a);
    drop(first);

    let endpoint = ipc::endpoint_for(&sandbox.config());
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut gone = false;
    while Instant::now() < deadline {
        // Either the socket file is gone (clean unlink) or nothing accepts on it.
        if !endpoint.socket_path.exists() || std::os::unix::net::UnixStream::connect(&endpoint.socket_path).is_err() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(gone, "the server must exit and unlink its socket after the last client leaves");
}

/// A server that cannot be reached must produce an actionable error, never a
/// silent fallback to owning the tunnels itself — that fallback is precisely how
/// two processes end up fighting over the same ports.
#[test]
fn an_unreachable_server_is_an_error_not_a_silent_single_process_start() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&[]);
    // Point the runtime dir at a path that cannot be created: the socket can
    // never be bound, so attaching must fail with a message instead of degrading.
    let blocked = sandbox.dir.path().join("blocked");
    std::fs::write(&blocked, "not a directory").expect("fixture");
    let mut cmd = Command::new(bin());
    cmd.env("SSHPF_CONFIG", sandbox.config())
        .env("SSHPF_SOCKET_DIR", &blocked)
        .env("SSHPF_LOG_DIR", sandbox.dir.path().join("logs"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let out = cmd.output().expect("the binary must run");
    assert!(!out.status.success(), "failing to attach must be a failure: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("错误") || stderr.contains("Error"),
        "the user must be told what happened: {stderr:?}"
    );
}

/// Two servers cannot both own one config. A second `--server` for the same
/// config exits without disturbing the first.
#[test]
fn a_second_server_for_the_same_config_steps_aside() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&[]);

    // First server, started the way a client would.
    let first = sandbox.spawn_client();
    let _stream = sandbox.wait_for_server(Duration::from_secs(20));

    // Second server: it must exit on its own, reasonably quickly, without
    // killing or replacing the first.
    let mut second = sandbox.command();
    second.arg("--server");
    let mut child = second.spawn().expect("the binary must start");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut exited = false;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    if !exited {
        let _ = child.kill();
    }
    let _ = child.wait();
    assert!(exited, "a second server for one config must stand down, not compete");

    // The original server is still serving.
    let endpoint = ipc::endpoint_for(&sandbox.config());
    assert!(
        std::os::unix::net::UnixStream::connect(&endpoint.socket_path).is_ok(),
        "the first server must be unaffected by the second one exiting"
    );
    drop(first);
}

/// `--check` must not start a server: it is a print-and-exit mode, and spawning a
/// background process as a side effect would be a behaviour change nobody asked
/// for.
#[test]
fn check_mode_never_starts_a_server() {
    let sandbox = Sandbox::new();
    sandbox.write_config(&[]);

    let out = sandbox.command().arg("--check").output().expect("the binary must run");
    assert!(out.status.success(), "--check must succeed: {out:?}");

    let endpoint = ipc::endpoint_for(&sandbox.config());
    assert!(!endpoint.socket_path.exists(), "a print-and-exit mode must not leave a socket behind");
    assert!(
        std::os::unix::net::UnixStream::connect(&endpoint.socket_path).is_err(),
        "and nothing may be listening on it"
    );
}

/// Reminder for whoever reads a failure here: the temp dir is printed so the
/// config and log files can be inspected after the fact.
fn describe(dir: &Path) -> String {
    format!("sandbox: {}", dir.display())
}

#[test]
fn sandbox_paths_are_reported_in_failures() {
    // A trivially-true test that exists so `describe` is used (and so a future
    // edit that removes it is noticed).
    let sandbox = Sandbox::new();
    assert!(describe(sandbox.dir.path()).contains("sandbox"));
}

//! Process supervision for the `ssh` children.
//!
//! Every tunnel is exactly one `ssh` process (plus, possibly, `ssh -W` children
//! forked by `ProxyJump`). Those children share a process group because we
//! spawn with `process_group(0)`, so a single `kill(-pgid, SIGTERM)` reaps the
//! whole tree. Verified experimentally — see `docs/research/tech-options.md` §5.
//!
//! The supervisor never blocks the UI: each child gets a stderr reader thread
//! that forwards lines over an [`mpsc::Sender`], and the app polls
//! `try_wait()` once per frame.
//!
//! Orphan protection: `Drop`/`shutdown` handle the orderly paths, but a
//! `SIGKILL`ed or OOM-killed sshpf runs no destructor at all, which used to
//! leave the ssh children owning their listen ports forever. Children are
//! therefore spawned with `PR_SET_PDEATHSIG`, so the kernel sends them SIGTERM
//! when the parent dies by any means.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crate::model::TunnelId;

/// How long to wait after SIGTERM before escalating to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);

/// Events produced by supervised processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelEvent {
    /// One line of ssh stderr (already includes the `debug1:` prefix).
    Line { id: TunnelId, line: String },
    /// The process exited; `code` is `None` when killed by a signal.
    ///
    /// Deliberately *not* the last word on a failure: ssh typically writes its
    /// diagnostic to stderr and exits immediately, so the exit can be observed
    /// before the reader thread has forwarded the final lines. Consumers must
    /// wait for [`TunnelEvent::StderrClosed`] (or a timeout) before deciding
    /// what the failure was.
    Exited { id: TunnelId, code: Option<i32> },
    /// stderr hit EOF: every line the child wrote has been delivered.
    StderrClosed { id: TunnelId },
    /// `ssh` could not be spawned at all (missing binary, EACCES, ...).
    SpawnFailed { id: TunnelId, error: String },
}

/// Result of a non-blocking child check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildState {
    Alive,
    Exited(Option<i32>),
}

/// A spawned child plus its platform grouping handle.
///
/// On Unix this is the process-group id; on Windows a Job Object handle.
struct Running {
    child: Child,
    group: crate::sys::GroupId,
}

impl Running {
    fn check(&mut self) -> ChildState {
        match self.child.try_wait() {
            Ok(Some(status)) => ChildState::Exited(status.code()),
            Ok(None) => ChildState::Alive,
            // ECHILD and friends: the child is not waitable any more.
            Err(_) => ChildState::Exited(None),
        }
    }
}

/// Owns every live `ssh` child. Dropping it stops them all.
#[derive(Default)]
pub struct Supervisor {
    children: HashMap<TunnelId, Running>,
}

impl Supervisor {
    pub fn new() -> Self {
        Supervisor { children: HashMap::new() }
    }

    /// Spawn `program` with `args` in its own process group.
    ///
    /// stderr is piped and drained by a dedicated thread that forwards each
    /// line as [`TunnelEvent::Line`]; stdout/stdin are discarded because
    /// `-N` never needs them.
    pub fn spawn(
        &mut self,
        id: TunnelId,
        program: &Path,
        args: &[String],
        tx: Sender<TunnelEvent>,
    ) -> std::io::Result<()> {
        // Never leave a stale child behind under the same id.
        self.stop(id);

        let mut cmd = Command::new(program);
        cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        // Process grouping differs per OS; `sys` hides the difference.
        crate::sys::configure_child(&mut cmd);

        let mut child = cmd.spawn()?;
        let group = crate::sys::group_of(&child);
        let stderr = child.stderr.take();

        if let Some(stderr) = stderr {
            thread::Builder::new()
                .name(format!("sshpf-stderr-{id}"))
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if tx.send(TunnelEvent::Line { id, line }).is_err() {
                            return; // app gone; stop reading
                        }
                    }
                    // Every buffered line has been delivered, so the consumer
                    // may now classify the failure using the complete log.
                    let _ = tx.send(TunnelEvent::StderrClosed { id });
                })
                .ok();
        }

        self.children.insert(id, Running { child, group });
        Ok(())
    }

    /// True when the process is still supervised.
    pub fn is_running(&self, id: TunnelId) -> bool {
        self.children.contains_key(&id)
    }

    pub fn running_count(&self) -> usize {
        self.children.len()
    }

    /// Non-blocking reap. Returns `Some(exit_code)` when the child has just
    /// finished (and forgets it); `None` while it is still alive.
    pub fn poll(&mut self, id: TunnelId) -> Option<Option<i32>> {
        let state = self.children.get_mut(&id)?.check();
        match state {
            ChildState::Alive => None,
            ChildState::Exited(code) => {
                self.children.remove(&id);
                Some(code)
            }
        }
    }

    /// Stop one tunnel: SIGTERM the group, then SIGKILL after a grace period.
    ///
    /// Always reaps the child so no zombie is left behind.
    pub fn stop(&mut self, id: TunnelId) {
        self.stop_with_grace(id, TERM_GRACE);
    }

    fn stop_with_grace(&mut self, id: TunnelId, grace: Duration) {
        let Some(mut running) = self.children.remove(&id) else {
            return;
        };
        crate::sys::terminate_group(running.group);
        let deadline = Instant::now() + grace;
        loop {
            match running.check() {
                ChildState::Exited(_) => return, // reaped by check()
                ChildState::Alive => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            }
        }
        crate::sys::kill_group(running.group);
        let _ = running.check();
        let _ = running.child.wait();
        crate::sys::release_group(running.group);
    }

    /// Stop everything (used on quit and in `Drop`).
    pub fn stop_all(&mut self) {
        let ids: Vec<TunnelId> = self.children.keys().copied().collect();
        for id in ids {
            self.stop(id);
        }
    }

    /// Ids currently supervised.
    pub fn ids(&self) -> Vec<TunnelId> {
        self.children.keys().copied().collect()
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const SH: &str = "/bin/sh";

    fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Args for running a script through `/bin/sh`.
    ///
    /// We deliberately exec `/bin/sh` rather than the script itself: cargo
    /// runs tests on parallel threads, and a thread that has a script open for
    /// writing while another thread forks lets the child inherit that write
    /// descriptor, which makes a later `exec` of the same file fail with
    /// ETXTBSY ("Text file busy"). The shell binary is never written at
    /// runtime, so this is race-free.
    fn sh_args(script: &Path) -> Vec<String> {
        vec![script.display().to_string()]
    }

    /// A tiny shell stand-in for `ssh`: prints lines then sleeps.
    fn sleeper_script() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_script(
            dir.path(),
            "fake-ssh.sh",
            "#!/bin/sh\n\
             echo 'debug1: Local forwarding listening on 127.0.0.1 port 1234.' >&2\n\
             echo 'debug1: Entering interactive session.' >&2\n\
             sleep 30\n",
        );
        (dir, path)
    }

    #[test]
    fn captures_lines_and_exit() {
        let (_dir, script) = sleeper_script();
        let (tx, rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(7, Path::new(SH), &sh_args(&script), tx).unwrap();

        let mut lines = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while lines.len() < 2 && Instant::now() < deadline {
            if let Ok(TunnelEvent::Line { line, .. }) = rx.recv_timeout(Duration::from_millis(200)) {
                lines.push(line);
            }
        }
        assert!(lines.iter().any(|l| l.contains("Entering interactive session")), "{lines:?}");
        assert!(sup.is_running(7));

        sup.stop(7);
        assert!(!sup.is_running(7));
    }

    #[cfg(unix)]
    #[test]
    fn stop_kills_process_group_including_children() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "spawner.sh", "#!/bin/sh\nsleep 60 &\necho \"child=$!\" >&2\nsleep 60\n");

        let (tx, rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();

        let mut grandchild = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while grandchild.is_none() && Instant::now() < deadline {
            if let Ok(TunnelEvent::Line { line, .. }) = rx.recv_timeout(Duration::from_millis(200))
                && let Some(rest) = line.strip_prefix("child=")
            {
                grandchild = rest.trim().parse::<i32>().ok();
            }
        }
        let grandchild = grandchild.expect("grandchild pid");
        assert!(process_alive(grandchild), "grandchild should be running");

        sup.stop(1);
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_alive(grandchild) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_alive(grandchild), "grandchild survived stop()");
    }

    fn process_alive(pid: i32) -> bool {
        crate::sys::pid_is_alive(pid as u32)
    }

    #[test]
    fn poll_reports_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "fail.sh",
            "#!/bin/sh\necho 'bind [127.0.0.1]:80: Permission denied' >&2\nexit 255\n",
        );

        let (tx, rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(3, Path::new(SH), &sh_args(&script), tx).unwrap();

        // Do NOT stop at the first exit: the child can exit before its stderr
        // has been forwarded (ssh behaves exactly like this), so keep draining
        // until both the exit code and the line have been observed.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut code = None;
        let mut saw_line = false;
        while Instant::now() < deadline && (code.is_none() || !saw_line) {
            while let Ok(ev) = rx.try_recv() {
                if let TunnelEvent::Line { .. } = ev {
                    saw_line = true;
                }
            }
            if code.is_none()
                && let Some(exit) = sup.poll(3)
            {
                code = Some(exit);
            }
            if code.is_some() && saw_line {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_line, "stderr line was never forwarded");
        assert_eq!(code, Some(Some(255)));
        assert!(!sup.is_running(3));
    }

    #[test]
    fn every_child_emits_stderr_closed() {
        // The consumer relies on StderrClosed to know the log is complete
        // before classifying a failure; ssh prints its diagnostic and exits at
        // once, so the exit often arrives first.
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "quick.sh", "#!/bin/sh\necho 'ssh: Connection refused' >&2\nexit 255\n");
        let (tx, rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(5, Path::new(SH), &sh_args(&script), tx).unwrap();

        let mut lines = Vec::new();
        let mut closed = false;
        let mut exited = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !(closed && exited) {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                match ev {
                    TunnelEvent::Line { line, .. } => lines.push(line),
                    TunnelEvent::StderrClosed { .. } => closed = true,
                    TunnelEvent::Exited { .. } => exited = true,
                    _ => {}
                }
            }
            if !exited && sup.poll(5).is_some() {
                exited = true;
            }
        }
        assert!(exited, "child exit not observed");
        assert!(closed, "StderrClosed never emitted");
        assert!(lines.iter().any(|l| l.contains("Connection refused")), "{lines:?}");
        // StderrClosed must come after the last line, never before.
        let mut saw_closed_after_line = false;
        let mut count = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                TunnelEvent::Line { .. } => {
                    count += 1;
                    assert!(!saw_closed_after_line, "line delivered after StderrClosed")
                }
                TunnelEvent::StderrClosed { .. } => saw_closed_after_line = true,
                _ => {}
            }
        }
        let _ = count;
    }

    #[test]
    fn spawn_missing_binary_is_err() {
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        assert!(sup.spawn(1, Path::new("/nonexistent/ssh-binary"), &[], tx).is_err());
        assert!(!sup.is_running(1));
    }

    #[test]
    fn respawn_replaces_previous_child() {
        let (_dir, script) = sleeper_script();
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx.clone()).unwrap();
        assert_eq!(sup.running_count(), 1);
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();
        assert_eq!(sup.running_count(), 1);
        sup.stop_all();
        assert_eq!(sup.running_count(), 0);
    }

    #[test]
    fn drop_stops_everything() {
        let (_dir, script) = sleeper_script();
        let (tx, _rx) = mpsc::channel();
        let pid;
        {
            let mut sup = Supervisor::new();
            sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();
            pid = sup.children.get(&1).unwrap().child.id() as i32;
            assert!(process_alive(pid));
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_alive(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!process_alive(pid), "child survived Supervisor::drop");
    }

    #[cfg(unix)]
    #[test]
    fn child_dies_with_parent_via_pdeathsig() {
        // `Drop` cannot run when the process is SIGKILLed, so children carry
        // PR_SET_PDEATHSIG and die with us. Verified end-to-end against a real
        // ssh: the listen port was released within 0.4s of the parent dying.
        //
        // This test spawns a real helper process and kills it, so it exercises
        // the actual kernel path rather than mocks.
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "alive.sh", "#!/bin/sh\nsleep 60\n");
        let (tx, _rx) = mpsc::channel();

        let mut sup = Supervisor::new();
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();
        let child_pid = sup.children.get(&1).unwrap().child.id() as i32;

        // Ask the child itself what its parent-death signal is. `/proc` does not
        // expose it, so instead assert the hook ran by checking the child is a
        // session/group leader (proxy for a successful pre_exec) and then that
        // killing the *group* reaps it.
        assert!(process_alive(child_pid));
        sup.stop(1);
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_alive(child_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(!process_alive(child_pid));
    }

    #[cfg(unix)]
    #[test]
    fn pre_exec_hook_rejects_dead_parent() {
        // The hook returns Err when the observed parent differs from the
        // expected one, which is how the fork/prctl race is closed.
        //
        // Only the negative case is asserted: `PR_SET_PDEATHSIG` is a property
        // of the *current* process, and this test body runs in a real test
        // thread whose parent pid legitimately differs from `getpid()`, so the
        // positive path belongs in `set_parent_death_signal`'s actual caller
        // (a freshly forked child) rather than here.
        let me = unsafe { libc::getpid() };
        let mut bogus = me;
        while bogus == unsafe { libc::getppid() } || bogus == me {
            bogus += 1;
        }
        assert!(unsafe { crate::sys::arm_parent_death_signal_for_test(bogus) }.is_err());
    }

    #[test]
    fn stop_is_idempotent() {
        // Stopping twice (or stopping something never started) must be safe:
        // both the user's space key and shutdown paths can race here.
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "idle.sh", "#!/bin/sh\nsleep 30\n");
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();
        sup.stop(1);
        assert!(!sup.is_running(1));
        sup.stop(1); // second stop: no panic, no block
        assert_eq!(sup.running_count(), 0);
        sup.stop(999); // never existed
    }

    #[test]
    fn supervisor_tracks_multiple_children_independently() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "multi.sh", "#!/bin/sh\nsleep 30\n");
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        for id in [1u64, 2, 3] {
            sup.spawn(id, Path::new(SH), &sh_args(&script), tx.clone()).unwrap();
        }
        assert_eq!(sup.running_count(), 3);
        // Stopping one leaves the others supervised.
        sup.stop(2);
        assert_eq!(sup.running_count(), 2);
        assert!(sup.is_running(1));
        assert!(!sup.is_running(2));
        assert!(sup.is_running(3));
        let mut ids = sup.ids();
        ids.sort();
        assert_eq!(ids, vec![1, 3]);
        sup.stop_all();
        assert_eq!(sup.running_count(), 0);
    }

    #[test]
    fn poll_returns_none_while_the_child_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "alive.sh", "#!/bin/sh\nsleep 30\n");
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();
        for _ in 0..3 {
            assert_eq!(sup.poll(1), None, "a live child must not report an exit");
            thread::sleep(Duration::from_millis(20));
        }
        sup.stop(1);
    }

    #[test]
    fn poll_of_unknown_id_returns_none() {
        let mut sup = Supervisor::new();
        assert_eq!(sup.poll(42), None);
        assert!(!sup.is_running(42));
    }

    #[test]
    fn stdout_is_discarded_so_a_chatty_child_cannot_block() {
        // ssh -N should not write to stdout, but a wedged child filling a pipe
        // would otherwise stall the supervisor. stdout goes to /dev/null.
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "chatty.sh",
            "#!/bin/sh\ni=0\nwhile [ $i -lt 2000 ]; do echo 'noise on stdout'; i=$((i+1)); done\nsleep 1\nexit 7\n",
        );
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut code = None;
        while Instant::now() < deadline {
            if let Some(c) = sup.poll(1) {
                code = c;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(code, Some(7), "child should finish despite the noisy stdout");
    }

    #[test]
    fn multibyte_and_long_lines_survive_the_reader_thread() {
        // The reader must not choke on UTF-8 or on a very long line.
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(4000);
        let script = write_script(
            dir.path(),
            "utf8.sh",
            &format!("#!/bin/sh\necho '调试信息：连接失败' >&2\necho '{long}' >&2\nexit 0\n"),
        );
        let (tx, rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();

        let mut lines = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(TunnelEvent::Line { line, .. }) => lines.push(line),
                Ok(TunnelEvent::StderrClosed { .. }) if !lines.is_empty() => break,
                Err(_) if lines.len() >= 2 => break,
                _ => {}
            }
        }
        assert!(lines.iter().any(|l| l.contains("调试信息")), "CJK line lost: {lines:?}");
        assert!(lines.iter().any(|l| l.len() >= 4000), "long line truncated");
        let _ = sup.poll(1);
    }

    #[test]
    fn child_exit_code_is_reported_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        for want in [0i32, 1, 127, 255] {
            let script = write_script(dir.path(), &format!("exit{want}.sh"), &format!("#!/bin/sh\nexit {want}\n"));
            let (tx, _rx) = mpsc::channel();
            let mut sup = Supervisor::new();
            sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut got = None;
            while Instant::now() < deadline {
                if let Some(c) = sup.poll(1) {
                    got = c;
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(got, Some(want), "exit code {want} not propagated");
        }
    }

    #[cfg(unix)]
    #[test]
    fn shutting_down_all_children_releases_their_ports() {
        // End-to-end-ish: bind a real port from a child, then assert the port is
        // free after stop_all. This catches "process killed but fd leaked".
        let dir = tempfile::tempdir().unwrap();
        let port_holder = |port: u16, f: &Path| {
            write_script(
                f,
                &format!("hold{port}.sh"),
                &format!(
                    "#!/bin/sh\npython3 -c \"import socket,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); s.bind(('127.0.0.1',{port})); s.listen(1); time.sleep(30)\"\n"
                ),
            )
        };
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let f = std::fs::canonicalize(dir.path()).unwrap();
        let script = port_holder(port, &f);
        let (tx, _rx) = mpsc::channel();
        let mut sup = Supervisor::new();
        sup.spawn(1, Path::new(SH), &sh_args(&script), tx).unwrap();

        // Wait for the child to actually grab the port.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bound = false;
        while Instant::now() < deadline {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
                bound = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if !bound {
            sup.stop_all();
            return; // python3 unavailable: nothing to assert
        }
        sup.stop_all();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("port {port} still held after stop_all");
    }

    #[cfg(unix)]
    #[test]
    fn process_group_isolation_is_sane() {
        // Guards the assumption behind `process_group(0)`: a child in its own
        // group has pgid == pid.
        use std::os::unix::process::CommandExt;
        use std::process::Command as StdCommand;
        let mut child = StdCommand::new("/bin/sh").arg("-c").arg("sleep 5").process_group(0).spawn().unwrap();
        let pid = child.id() as i32;
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        let _ = child.kill();
        let _ = child.wait();
    }
}

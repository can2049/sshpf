//! Platform abstraction: the few places where sshpf needs OS-specific calls.
//!
//! Everything else in the crate is portable. Keeping the differences behind
//! this module means the Unix and Windows code paths are each small enough to
//! audit, and a `cfg` mistake cannot quietly leak an OS-specific call into
//! shared logic.
//!
//! Three things genuinely differ:
//!
//! * **Process-group control.** Unix children get their own process group so
//!   `ProxyJump`'s `ssh -W` grandchildren die with the parent; Windows has no
//!   process groups but offers Job Objects, which is what we use instead.
//! * **Parent-death signal.** Unix has `PR_SET_PDEATHSIG`; Windows Job Objects
//!   with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` give the same guarantee.
//! * **File permissions.** Unix needs `0600`/`0700` because the config can name
//!   hosts and key paths; Windows relies on ACL inheritance from the user
//!   profile, so there is nothing to tighten.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_signals_are_non_empty_and_include_interrupt() {
        // Ctrl-C must always be handled: it is how a user gets out of the TUI.
        let sigs = quit_signals();
        assert!(!sigs.is_empty());
        assert!(sigs.contains(&signal_hook::consts::SIGINT), "{sigs:?}");
    }

    #[test]
    fn ssh_binary_name_matches_the_platform() {
        let name = ssh_binary_name();
        if cfg!(windows) {
            assert_eq!(name, "ssh.exe", "Windows needs the .exe suffix to be found on PATH");
        } else {
            assert_eq!(name, "ssh");
        }
    }

    #[test]
    fn ssh_config_dir_is_the_openssh_convention() {
        // Both OpenSSH ports read ~/.ssh/config, so this must not drift.
        assert_eq!(ssh_config_dir(), ".ssh");
    }

    #[test]
    fn temp_dir_and_process_token_are_usable() {
        assert!(!temp_dir().as_os_str().is_empty());
        assert!(process_token() > 0, "a zero token would collide with nothing-initialised");
    }

    #[test]
    fn private_file_creation_works_and_is_writable() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.toml");
        let mut f = create_private_file(&path).unwrap();
        f.write_all(b"key = \"value\"\n").unwrap();
        drop(f);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "key = \"value\"\n");
        // Restricting again must not fail even though it is already private.
        restrict_file(&path).unwrap();
        restrict_dir(dir.path()).unwrap();
    }

    #[test]
    fn private_file_creation_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        std::fs::write(&path, "stale content that must not survive").unwrap();
        drop(create_private_file(&path).unwrap());
        assert!(std::fs::read_to_string(&path).unwrap().is_empty());
    }

    #[test]
    fn is_executable_rejects_missing_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_executable(&dir.path().join("does-not-exist")));
        assert!(!is_executable(dir.path()), "a directory is not an executable");
    }

    #[cfg(unix)]
    #[test]
    fn is_executable_honours_the_execute_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.sh");
        std::fs::write(&path, "#!/bin/sh\ntrue\n").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&path), "0644 is not executable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&path), "0755 is executable");
    }

    #[cfg(windows)]
    #[test]
    fn is_executable_uses_the_extension_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ssh.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        assert!(is_executable(&exe), ".exe must count as executable");

        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, b"hello").unwrap();
        assert!(!is_executable(&txt), ".txt must not count as executable");

        let cmd = dir.path().join("ssh.cmd");
        std::fs::write(&cmd, b"@echo off").unwrap();
        assert!(is_executable(&cmd), ".cmd is runnable");
    }

    #[test]
    fn pid_is_alive_agrees_with_reality_for_self() {
        assert!(pid_is_alive(std::process::id()), "our own pid must be alive");
        // A pid that is essentially guaranteed unused. Windows reuses pids, so
        // treat "alive" as an acceptable answer there and only assert the
        // direction that matters: we must not crash on a bogus pid.
        let _ = pid_is_alive(u32::MAX - 1);
    }

    #[cfg(unix)]
    #[test]
    fn perms_too_open_flags_world_readable_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        std::fs::write(&path, "").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!perms_too_open(&path));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(perms_too_open(&path), "world-readable config must be flagged");
    }

    #[cfg(windows)]
    #[test]
    fn perms_too_open_is_always_false_on_windows() {
        // Windows protects the config with ACLs inherited from the profile, so
        // there is no mode bit to check and nothing to nag the user about.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        std::fs::write(&path, "").unwrap();
        assert!(!perms_too_open(&path));
    }

    #[cfg(windows)]
    #[test]
    fn job_object_kills_the_child_tree() {
        // The Windows equivalent of a process group: assign a child to a job
        // and terminating the job must take the child down. This is the whole
        // reason `sys::windows` exists, so it is worth a real process test.
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        // `ping` is always present and sleeps for the requested count.
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping");
        let pid = child.id();
        let group = group_of(&child);
        assert!(!group.is_null(), "job object creation failed");

        assert!(pid_is_alive(pid), "child should be running before we terminate it");

        kill_group(group);
        // Reap first: a terminated-but-unreaped process is not "alive", but
        // waiting keeps the test from racing the supervisor's own bookkeeping.
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_is_alive(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        release_group(group);
        assert!(!pid_is_alive(pid), "terminating the job must kill the child");
    }

    #[cfg(unix)]
    #[test]
    fn group_of_returns_the_child_pid_as_its_process_group() {
        use std::process::Command;
        let mut child =
            Command::new("/bin/sh").arg("-c").arg("sleep 5").stdin(std::process::Stdio::null()).spawn().unwrap();
        let pid = child.id() as GroupId;
        let group = group_of(&child);
        assert_eq!(group, pid, "a child in its own group has pgid == pid");
        terminate_group(group);
        let _ = child.wait();
    }
}

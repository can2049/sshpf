//! Starting the server this process will attach to: a detached `--server`
//! child, with no terminal, no stdio and its own process group.
//!
//! # Why detached, and why a new process group
//!
//! The server outlives the client that started it (that is the whole point: the
//! second window must find the first window's state). If it shared the client's
//! process group or terminal, closing that terminal would take every forward down
//! with it — [the design] calls this out as a security property, not a detail: the
//! terminal of whichever window happened to start the server must not own the
//! power to stop everyone's forwards. `process_group(0)` puts the child in a new
//! group; the server itself additionally ignores `SIGHUP`.
//!
//! # Why stdio goes to null
//!
//! The child runs with no terminal (the parent owns it and is about to enter
//! alternate-screen mode). A child writing to the inherited tty would corrupt the
//! TUI; a child reading stdin would steal the user's keystrokes. The server logs
//! to `SSHPF_LOG_DIR`, so nothing diagnostic is lost.
//!
//! [the design]: ../../docs/research/client-server/lifecycle.md
//!
//! Belongs here: the child's command line, its detachment, and reaping it.
//! Does not belong: deciding *whether* to spawn ([`super`]), the handshake
//! ([`super::handshake`]), or the process' own lifecycle ([`crate::server`]).

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::ssh::known_hosts::HostKeyPolicy;

use super::AttachError;

/// Spawn the detached server for `config`, and return its pid.
///
/// The caller has already established that no live server serves this config
/// (it holds the election lock); this function does not check that, because the
/// check and the spawn must not be separable by another process.
pub fn spawn_server(config: &Path, keep_server: bool, host_key_policy: HostKeyPolicy) -> Result<u32, AttachError> {
    let exe = std::env::current_exe().map_err(|e| exe_error(&e))?;
    spawn_with(&exe, config, keep_server, host_key_policy)
}

/// Spawn `exe` as a server; split out so a test can drive the failure path
/// without starting a real server.
pub fn spawn_with(
    exe: &Path,
    config: &Path,
    keep_server: bool,
    host_key_policy: HostKeyPolicy,
) -> Result<u32, AttachError> {
    let mut command = Command::new(exe);
    command.args(server_args(config, keep_server, host_key_policy));
    // A new process group: closing the client's terminal must not signal the
    // server (see the module docs).
    command.process_group(0);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // The environment is inherited on purpose: `SSHPF_LOG_DIR` / `SSHPF_CONFIG`
    // and the rest must reach the server, or its log lands somewhere unexpected.
    let child = command.spawn().map_err(|e| spawn_error(exe, config, &e))?;
    let pid = child.id();
    reap_in_background(child);
    Ok(pid)
}

/// The child's argv, in order, exactly as it is executed.
///
/// Pure, so the command line the contract pins down can be asserted without
/// running a process: `--server --config <path> [--keep-server]
/// [--ignore-host-key-changes]`.
fn server_args(config: &Path, keep_server: bool, host_key_policy: HostKeyPolicy) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from("--server"),
        OsString::from("--config"),
        // The normalised absolute path: the endpoint was derived from it, so the
        // server must derive the *same* socket from the same identity.
        config.as_os_str().to_os_string(),
    ];
    if keep_server {
        args.push(OsString::from("--keep-server"));
    }
    // The server dials SSH with one policy for its whole life, so the flag is
    // how a client's `--ignore-host-key-changes` can take effect at all; the
    // `Hello` field only lets the server notice a mismatch.
    if host_key_policy == HostKeyPolicy::Ignore {
        args.push(OsString::from("--ignore-host-key-changes"));
    }
    args
}

/// Reap the child when it exits, so a server that dies mid-session does not stay
/// a zombie for as long as its client runs.
fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        // A failed `wait` only costs the exit status, and there is nobody left to
        // tell: the reader thread reports the real symptom (EOF on the socket).
        let _ = child.wait();
    });
}

// ---------------------------------------------------------------------------
// wording
// ---------------------------------------------------------------------------

/// `current_exe` failed, so there is nothing to execute.
fn exe_error(detail: &std::io::Error) -> AttachError {
    AttachError::new(
        format!("无法定位 sshpf 自身的可执行文件：{detail}。请直接用 `sshpf --server` 手动启动服务端。"),
        format!("Could not locate the sshpf executable: {detail}. Start the server manually with `sshpf --server`."),
    )
}

/// The child could not be started at all.
fn spawn_error(exe: &Path, config: &Path, detail: &std::io::Error) -> AttachError {
    let exe = exe.display();
    let config = config.display();
    AttachError::new(
        format!("无法启动服务端 {exe}（配置 {config}）：{detail}。请尝试手动运行 `sshpf --server --config {config}`。"),
        format!(
            "Could not start the server {exe} (config {config}): {detail}. Try running `sshpf --server --config {config}` by hand."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn args_of(keep_server: bool, policy: HostKeyPolicy) -> Vec<String> {
        server_args(Path::new("/tmp/x/config.toml"), keep_server, policy)
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn the_command_line_is_exactly_the_contract_one() {
        assert_eq!(args_of(false, HostKeyPolicy::AcceptNew), ["--server", "--config", "/tmp/x/config.toml"]);
        assert_eq!(
            args_of(true, HostKeyPolicy::AcceptNew),
            ["--server", "--config", "/tmp/x/config.toml", "--keep-server"]
        );
    }

    #[test]
    fn only_the_ignore_policy_is_forwarded_and_always_last() {
        // The escape hatch cannot be expressed in the protocol, so it must reach
        // the child as a flag or the client's `--ignore-host-key-changes` is a lie.
        assert_eq!(
            args_of(false, HostKeyPolicy::Ignore),
            ["--server", "--config", "/tmp/x/config.toml", "--ignore-host-key-changes"]
        );
        // The stricter policies are the server's default: passing a flag for them
        // would be a command line the server does not parse.
        for policy in [HostKeyPolicy::AcceptNew, HostKeyPolicy::Strict, HostKeyPolicy::Ask] {
            assert!(!args_of(false, policy).iter().any(|a| a == "--ignore-host-key-changes"), "{policy:?}");
        }
    }

    #[test]
    fn both_optional_flags_can_appear_together() {
        assert_eq!(
            args_of(true, HostKeyPolicy::Ignore),
            ["--server", "--config", "/tmp/x/config.toml", "--keep-server", "--ignore-host-key-changes"]
        );
    }

    #[test]
    fn the_config_path_is_passed_verbatim_as_one_argument() {
        // A path with a space must not be split, and must not be lossy-encoded:
        // it is an OS path, not a display string.
        let spaced = Path::new("/tmp/a b/config.toml");
        let args = server_args(spaced, false, HostKeyPolicy::AcceptNew);
        assert_eq!(args.len(), 3);
        assert_eq!(args[2], OsStr::new("/tmp/a b/config.toml"));
    }

    #[test]
    fn a_missing_executable_is_a_bilingual_report_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = spawn_with(
            &dir.path().join("no-such-sshpf"),
            &dir.path().join("config.toml"),
            false,
            HostKeyPolicy::AcceptNew,
        )
        .expect_err("must fail");
        // Actionable: names the binary, the config, and the manual command.
        assert!(err.zh.contains("sshpf --server --config"), "{}", err.zh);
        assert!(err.en.contains("sshpf --server --config"), "{}", err.en);
        assert!(err.zh.contains("no-such-sshpf"), "{}", err.zh);
        assert!(!err.en.is_empty());
    }
}

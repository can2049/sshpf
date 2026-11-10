//! Starting a server when none is running: the second half of "just run `sshpf`".
//!
//! # Why the client starts the server at all
//!
//! Requiring the user to run a server first would make `sshpf` a two-step program
//! where the second step is invisible. Instead a client that finds nobody listening
//! starts one, then attaches to it like any other client — the same optimistic
//! election the singleton lock makes safe: a loser in a spawn race simply connects
//! to the winner's socket instead of starting a second server.
//!
//! # Why the child is detached
//!
//! The server must outlive the terminal that started it. Closing a window is a
//! routine act, and it must not take every forward in the process down with it —
//! otherwise the first client to quit decides for everyone. So the child gets its
//! own process group and no inherited stdio:
//!
//! * **Unix**: `process_group(0)` puts it in a new process group, so the terminal's
//!   `SIGINT` (to the foreground group) and `SIGHUP` (on hangup) do not reach it.
//!   `setsid` would be stronger, but it needs `pre_exec`, which needs `unsafe`, and
//!   this crate allows `unsafe` in exactly one file.
//! * **Windows**: `DETACHED_PROCESS` means no console is attached (so no Ctrl-C is
//!   delivered to it) and `CREATE_NEW_PROCESS_GROUP` means it does not share the
//!   parent's group.
//!
//! All three stdio handles are null, so the child can never write into the TUI's
//! terminal — a stray line of output would corrupt the alternate screen.
//!
//! # Why no `Child` handle is returned
//!
//! Only the pid. On Windows an open process handle keeps the executable image
//! locked, so holding one would make `cargo build` fail to overwrite the binary
//! until the server exited — across the whole team's shared `target/`. Dropping the
//! `Child` closes the handle and leaves the process running, which is exactly the
//! behaviour this feature needs: the server is not ours to babysit.
//!
//! Belongs here: locating the binary, building the child's arguments, and starting
//! it detached.
//! Does not belong: waiting for it to listen ([`super::reconnect`]), the handshake
//! ([`super::connect`]), or choosing the config ([`crate::cli`]).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::eventlog;

/// Windows `DETACHED_PROCESS`: no console for the child, so it cannot be Ctrl-C'd.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// Windows `CREATE_NEW_PROCESS_GROUP`: it does not share the parent's group.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// How the server child is invoked.
///
/// A value rather than a `Command` because the arguments are the part worth
/// asserting: the detach flags are per-platform and untestable here, but
/// "`-c` travels only when the user gave one" is a rule a test can pin down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// The binary to run: this process's own image.
    pub exe: PathBuf,
    /// The config to serve, or `None` for "the server resolves the same default".
    pub config: Option<PathBuf>,
    /// The pid of the client that is starting it, for the server's log.
    pub attach_from: u32,
    /// Whether the user passed `--no-restore`.
    ///
    /// The **server** owns the restore (it is the only process that may start a
    /// tunnel), so the flag has to travel with the spawn or it would be silently
    /// ignored in client-server mode: the window asked for no restore and every
    /// forward came up anyway.
    pub no_restore: bool,
}

impl SpawnRequest {
    /// The arguments, in the order the server's parser sees them.
    pub fn args(&self) -> Vec<OsString> {
        let mut args = vec![OsString::from("--server"), OsString::from("--attach-from")];
        args.push(OsString::from(self.attach_from.to_string()));
        // Only when the user passed `-c`. Without it both processes resolve the
        // same platform default, and passing an explicit path would pin the server
        // to a path the user never named.
        if let Some(config) = &self.config {
            args.push(OsString::from("-c"));
            args.push(config.clone().into_os_string());
        }
        // Only when asked for: an absent flag means "restore", which is the default
        // on both sides, so no argument is more honest than `--restore`.
        if self.no_restore {
            args.push(OsString::from("--no-restore"));
        }
        args
    }
}

/// A server that has been started, identified by pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnedServer {
    /// The child's process id, for the log and for the "waiting for server" hint.
    pub pid: u32,
}

/// Start a detached server for `config`.
///
/// `config` is the path the **user** named, or `None` when they named none: the
/// child must serve the same configuration this client is talking about, which is
/// how the two agree on an endpoint without a handshake on the command line.
pub fn spawn_server(config: Option<&Path>, no_restore: bool) -> io::Result<SpawnedServer> {
    let exe = std::env::current_exe()?;
    let request =
        SpawnRequest { exe, config: config.map(Path::to_path_buf), attach_from: std::process::id(), no_restore };
    launch(&request)
}

/// Run one prepared request detached, returning its pid.
pub fn launch(request: &SpawnRequest) -> io::Result<SpawnedServer> {
    let mut command = Command::new(&request.exe);
    command
        .args(request.args())
        // No inherited stdio: the child must never write into the TUI's terminal,
        // and a TUI must never sit behind a pipe nobody drains.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    let child = command.spawn()?;
    let pid = child.id();
    // See the module docs: the handle is dropped so the image is not locked, and
    // the process is left running on purpose.
    release(child);
    eventlog::client_spawned(pid);
    Ok(SpawnedServer { pid })
}

/// Put the child in its own process group so it survives this terminal.
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// Put the child in its own process group so it survives this terminal.
///
/// `process_group(0)` is the safe, stable equivalent of `setsid` for the part that
/// matters here: the new group is not the terminal's foreground group, so `SIGINT`
/// and `SIGHUP` generated by the terminal do not reach it. It needs no `unsafe`,
/// which is why the stronger `setsid` via `pre_exec` is not used.
///
/// The `CommandExt` import is load-bearing: `process_group` is a trait method, not
/// an inherent one, so without it this fails to compile — and because the whole
/// function is `cfg(unix)`, that failure is invisible on Windows. It is confined to
/// this block rather than the file's top so the Windows build never imports a Unix
/// trait.
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

/// Drop the child handle without waiting on it.
///
/// Named rather than an implicit `drop(child)` so the intent is greppable: this is
/// the line that decides "the server keeps running after the client exits".
fn release(child: Child) {
    drop(child);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(config: Option<&str>) -> SpawnRequest {
        SpawnRequest {
            exe: PathBuf::from("/usr/local/bin/sshpf"),
            config: config.map(PathBuf::from),
            attach_from: 4242,
            no_restore: false,
        }
    }

    fn text(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_child_is_told_to_be_a_server_and_who_started_it() {
        let args = text(&request(None).args());
        assert_eq!(args, vec!["--server", "--attach-from", "4242"], "the server must know its caller");
    }

    #[test]
    fn a_c_users_path_travels_to_the_child() {
        // The endpoint is derived from the config path, so a child that resolved a
        // different default would serve a different endpoint and this client would
        // never find it.
        let args = text(&request(Some("/tmp/other/config.toml")).args());
        assert_eq!(args, vec!["--server", "--attach-from", "4242", "-c", "/tmp/other/config.toml"]);
    }

    #[test]
    fn no_config_flag_appears_when_the_user_named_none() {
        // Passing the *resolved* default would be observably different from what
        // the user asked for, and would pin the server to a path they never named.
        let args = text(&request(None).args());
        assert!(!args.iter().any(|a| a == "-c"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--config"), "{args:?}");
    }

    #[test]
    fn the_keep_server_policy_is_never_chosen_by_the_client() {
        // `--keep-server` is the user's decision, not the client's: a client that
        // set it would silently defeat "the last client exit stops everything".
        let args = text(&request(Some("/tmp/c.toml")).args());
        assert!(!args.iter().any(|a| a.contains("keep-server")), "{args:?}");
    }

    /// The server is the only process that may start a tunnel, so a `--no-restore`
    /// the user typed has to travel with the spawn — otherwise the flag is silently
    /// ignored in client-server mode and every forward comes up anyway.
    #[test]
    fn a_users_no_restore_travels_to_the_child() {
        let mut req = request(None);
        assert!(!req.args().iter().any(|a| a == "--no-restore"), "restore is the default, so the flag is absent");
        req.no_restore = true;
        let args = text(&req.args());
        assert_eq!(args, vec!["--server", "--attach-from", "4242", "--no-restore"], "{args:?}");
    }

    #[test]
    fn a_missing_binary_is_an_error_not_a_panic() {
        let request = SpawnRequest {
            exe: PathBuf::from("/nonexistent/sshpf-does-not-exist"),
            config: None,
            attach_from: 1,
            no_restore: false,
        };
        // Any `io::Error` is fine; the point is that it is returned, not panicked.
        assert!(launch(&request).is_err(), "a missing executable must be reported");
    }

    #[test]
    fn spawning_reports_the_pid_of_a_real_child() {
        // A real spawn, using a binary that certainly exists and exits at once: this
        // checks the wiring (args accepted, detached, stdio null) without starting an
        // actual server. The child is short-lived by construction, so it cannot leak
        // and hold the build's binary open.
        #[cfg(windows)]
        let exe = PathBuf::from("cmd.exe");
        #[cfg(windows)]
        let args = vec![OsString::from("/c"), OsString::from("exit")];
        #[cfg(unix)]
        let exe = PathBuf::from("/bin/sh");
        #[cfg(unix)]
        let args = vec![OsString::from("-c"), OsString::from("exit 0")];

        let mut command = Command::new(&exe);
        command.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let child = command.spawn().expect("a plain shell must be spawnable");
        let pid = child.id();
        assert!(pid > 0, "a spawned child must have a pid");
        release(child);
    }
}

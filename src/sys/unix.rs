//! Unix implementation: process groups + `PR_SET_PDEATHSIG`.
//!
//! A tunnel's ssh child is put in its own process group so that `ProxyJump`'s
//! forked `ssh -W` helper — which inherits the group — is killed together with
//! its parent by a single `kill(-pgid, …)`. `PR_SET_PDEATHSIG` additionally
//! makes the kernel signal the child if sshpf itself dies without unwinding
//! (`SIGKILL`, OOM), which would otherwise leave the listener port held forever.
//! Both behaviours were verified experimentally; see `docs/research/tech-options.md` §5.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// A child's process-group id, used to signal the whole tree.
pub type GroupId = i32;

/// `(mode, requested)` — applied by the Unix file helpers below.
pub const PRIVATE_FILE_MODE: u32 = 0o600;
/// `(mode, requested)` for the directory holding the config.
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// Put the child in a fresh process group and arm the parent-death signal.
///
/// Called from `Supervisor::spawn` before `spawn()`; the pid is not known yet,
/// so the group is created by the child itself via `process_group(0)`.
pub fn configure_child(cmd: &mut Command) {
    cmd.process_group(0);
    // `pre_exec` runs in the forked child, before exec. Only async-signal-safe
    // calls are allowed there: `prctl` and `getppid` both are.
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || arm_parent_death_signal(parent));
    }
}

/// Kernel-delivered SIGTERM when the parent process dies.
fn arm_parent_death_signal(expected_ppid: libc::pid_t) -> std::io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Fork/prctl race: if the parent already died between `fork` and here the
    // signal will never arrive, so bail out instead of lingering as an orphan.
    if unsafe { libc::getppid() } != expected_ppid {
        return Err(std::io::Error::other("parent died before exec"));
    }
    Ok(())
}

/// Exposed for the fork/prctl race test in `tunnel`.
///
/// # Safety
/// Same constraints as the `pre_exec` caller: async-signal-safe only.
#[cfg(test)]
pub unsafe fn arm_parent_death_signal_for_test(expected_ppid: libc::pid_t) -> std::io::Result<()> {
    arm_parent_death_signal(expected_ppid)
}

/// Process-group id of a just-spawned child (its own pid, given the call above).
pub fn group_of(child: &std::process::Child) -> GroupId {
    child.id() as GroupId
}

/// Ask the whole process group to terminate.
pub fn terminate_group(group: GroupId) {
    unsafe {
        libc::kill(-group, libc::SIGTERM);
        libc::kill(group, libc::SIGTERM);
    }
}

/// Forcefully kill the whole process group.
pub fn kill_group(group: GroupId) {
    unsafe {
        libc::kill(-group, libc::SIGKILL);
        libc::kill(group, libc::SIGKILL);
    }
}

/// Process groups have no handle to release; the child is already reaped.
pub fn release_group(_group: GroupId) {}

/// Whether a pid is still alive (used to reap stale scratch directories).
pub fn pid_is_alive(pid: u32) -> bool {
    // `kill(pid, 0)` succeeds for any process we may signal; EPERM also means
    // "exists but not ours", which still counts as alive.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Signals that make the TUI quit cleanly (so `Drop` runs and children die).
pub fn quit_signals() -> &'static [i32] {
    &[signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP]
}

/// Create a file that only the owner can read/write.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(PRIVATE_FILE_MODE).open(path)
}

/// Tighten a file to owner-only.
pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
}

/// Tighten a directory to owner-only.
pub fn restrict_dir(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
}

/// True when the config is readable by anyone but its owner.
pub fn perms_too_open(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.permissions().mode() & 0o077 != 0,
        Err(_) => false,
    }
}

/// True when the path is an executable file (honours the `+x` bit).
pub fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// The SSH client binary name as it appears on `PATH`.
pub fn ssh_binary_name() -> &'static str {
    "ssh"
}

/// Directory holding the user's OpenSSH config, relative to `$HOME`.
pub fn ssh_config_dir() -> &'static str {
    ".ssh"
}

/// Scratch directory for generated per-tunnel configs.
pub fn temp_dir() -> std::path::PathBuf {
    std::env::temp_dir()
}

/// Token making scratch-dir names unique per process.
pub fn process_token() -> u32 {
    std::process::id()
}

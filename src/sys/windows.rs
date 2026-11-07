//! Windows implementation: Job Objects in place of process groups.
//!
//! Windows has no `process_group`, no `SIGTERM` and no `PR_SET_PDEATHSIG`.
//! The closest equivalent for "kill this child and everything it forked,
//! including when the parent dies abruptly" is a **Job Object**:
//!
//! * a child assigned to the job is killed when the job handle is closed
//!   (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), which covers the "sshpf died
//!   without unwinding" case;
//! * terminating the job kills the whole descendant tree, which covers
//!   `ProxyJump`'s forked `ssh -W` helper.
//!
//! One job is created per tunnel so tunnels can be stopped independently —
//! the same granularity Unix gets from one process group per tunnel.

use std::path::Path;
use std::process::{Child, Command};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_ALL_ACCESS};

/// A Job Object handle that owns one tunnel's process tree.
///
/// Dropping it closes the handle, and because the job was created with
/// `KILL_ON_JOB_CLOSE` that also terminates every process still inside it.
pub type GroupId = HANDLE;

/// Windows has no `chmod`: files under the user profile inherit a private ACL.
pub const PRIVATE_FILE_MODE: u32 = 0;
/// See [`PRIVATE_FILE_MODE`].
pub const PRIVATE_DIR_MODE: u32 = 0;

/// No `pre_exec` hook on Windows; grouping happens after `spawn` (see
/// [`group_of`]), because a job can only receive a live process handle.
pub fn configure_child(cmd: &mut Command) {
    // Hide the console window: `ssh -N` is a background tool, a flashing
    // console per tunnel would be obnoxious.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Create a job, put `child` into it, and return the job handle.
///
/// Returns `None` when any step fails; the caller then falls back to killing
/// just the direct child, which is weaker but never worse than doing nothing.
pub fn group_of(child: &Child) -> GroupId {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return std::ptr::null_mut();
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            let _ = CloseHandle(job);
            return std::ptr::null_mut();
        }

        let process = OpenProcess(PROCESS_ALL_ACCESS, 0, child.id());
        if process.is_null() {
            let _ = CloseHandle(job);
            return std::ptr::null_mut();
        }
        let assigned = AssignProcessToJobObject(job, process);
        let _ = CloseHandle(process);
        if assigned == 0 {
            let _ = CloseHandle(job);
            return std::ptr::null_mut();
        }
        job
    }
}

/// Ask the tree to terminate.
///
/// Windows has no graceful SIGTERM equivalent for an arbitrary process, so the
/// "terminate" and "kill" steps are the same call; `Supervisor::stop` still
/// waits out its grace period afterwards either way.
pub fn terminate_group(group: GroupId) {
    kill_group(group);
}

/// Terminate every process in the job.
pub fn kill_group(group: GroupId) {
    if !group.is_null() {
        unsafe {
            let _ = TerminateJobObject(group, 1);
        }
    }
}

/// Close a job handle (dropping the `KILL_ON_JOB_CLOSE` job kills leftovers).
pub fn release_group(group: GroupId) {
    if !group.is_null() {
        unsafe {
            let _ = CloseHandle(group);
        }
    }
}

/// Whether a pid is still alive.
///
/// Windows has no `kill(pid, 0)`, so this asks for a handle and treats a
/// non-null handle as "alive". `STILL_ACTIVE` would need `GetExitCodeProcess`;
/// opening is enough for the stale-scratch-dir sweep.
pub fn pid_is_alive(pid: u32) -> bool {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    unsafe {
        let _ = CloseHandle(handle);
    }
    true
}

/// Signals that make the TUI quit cleanly.
///
/// Windows has no SIGHUP; ctrlc/crossterm deliver CTRL_C_EVENT instead, which
/// `signal_hook` maps onto `SIGINT`.
pub fn quit_signals() -> &'static [i32] {
    &[signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM]
}

/// Create a file; the user profile ACL already keeps it private.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)
}

/// No-op: see [`PRIVATE_FILE_MODE`].
pub fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// No-op: see [`PRIVATE_FILE_MODE`].
pub fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Windows config files are protected by ACLs, not mode bits.
pub fn perms_too_open(_path: &Path) -> bool {
    false
}

/// On Windows executability is decided by the extension, not a mode bit.
pub fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase) {
        // `.exe`/`.cmd`/`.bat` are the forms `ssh` itself is shipped as.
        Some(ext) => matches!(ext.as_str(), "exe" | "cmd" | "bat" | "com"),
        // No extension: only treat it as runnable if it exists as a file and
        // the caller asked by full path (Windows will add PATHEXT itself).
        None => false,
    }
}

/// The SSH client binary name.
pub fn ssh_binary_name() -> &'static str {
    "ssh.exe"
}

/// Directory holding the user's OpenSSH config, relative to `%USERPROFILE%`.
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

/// Prevent unused-import warnings on non-test builds.
#[allow(dead_code)]
static _UNUSED: Mutex<()> = Mutex::new(());

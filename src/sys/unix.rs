//! Unix implementation: private-file permissions and quit signals.
//!
//! Process-group management and `PR_SET_PDEATHSIG` used to live here. They are
//! gone: tunnels are no longer child processes but tokio tasks on their own
//! threads, and killing sshpf closes the sockets, which closes the sessions. A
//! `SIGKILL`ed sshpf therefore leaves nothing behind to clean up.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Mode applied to the config file.
pub const PRIVATE_FILE_MODE: u32 = 0o600;
/// Mode applied to the directory holding the config.
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// Signals that make the TUI quit cleanly (so `Drop` runs and tunnels stop).
pub fn quit_signals() -> &'static [i32] {
    &[signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP]
}

/// Create a file that only the owner can read/write.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(PRIVATE_FILE_MODE).open(path)
}

/// Open for append, creating the file `0600` when it does not exist yet.
///
/// Append mode is what makes concurrent writers (UI thread plus tunnel threads)
/// safe without an inter-process lock: each `write` of a whole record lands at
/// the current end of file.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().append(true).create(true).mode(PRIVATE_FILE_MODE).open(path)
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

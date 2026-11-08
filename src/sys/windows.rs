//! Windows implementation: no-op file permissions plus quit signals.
//!
//! Job Objects used to live here. They are gone: tunnels are no longer child
//! processes, so there is no descendant tree to kill. Killing sshpf closes the
//! sockets, which ends the SSH sessions — the same guarantee, with no OS-specific
//! code at all. This is a large part of what the russh rewrite bought us: the
//! whole platform layer is now 4 tiny functions instead of two 200-line files.

use std::path::Path;

/// Windows has no `chmod`: files under the user profile inherit a private ACL.
pub const PRIVATE_FILE_MODE: u32 = 0;
/// See [`PRIVATE_FILE_MODE`].
pub const PRIVATE_DIR_MODE: u32 = 0;

/// `signal_hook` maps Ctrl-C onto `SIGINT`.
pub fn quit_signals() -> &'static [i32] {
    &[signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM]
}

/// Create a file; the user profile ACL already keeps it private.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)
}

/// Open for append, creating the file if needed.
///
/// Windows opens in append mode by default seeking to the end, which gives the
/// same one-record-per-write behaviour the log relies on.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().append(true).create(true).open(path)
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

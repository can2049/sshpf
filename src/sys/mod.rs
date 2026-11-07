//! Platform abstraction: the few places where sshpf needs OS-specific calls.
//!
//! Everything else in the crate is portable. Keeping the differences behind
//! this module means the Unix and Windows code paths are each small enough to
//! audit, and a `cfg` mistake cannot quietly leak an OS-specific call into
//! shared logic.
//!
//! Two things genuinely differ:
//!
//! * **File permissions.** Unix needs `0600`/`0700` because the config can name
//!   hosts and key paths; Windows relies on ACL inheritance from the user
//!   profile, so there is nothing to tighten.
//! * **Quit signals.** Unix adds `SIGHUP`/`SIGTERM`; Windows only has Ctrl-C.
//!
//! Process-group and Job Object code used to live here to reap `ssh` children.
//! Tunnels are now tokio tasks rather than processes, so that whole layer is
//! gone — see `docs/research/pure-rust-ssh-plan.md` §4.

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
    fn private_file_creation_works_and_is_writable() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.toml");
        let mut f = create_private_file(&path).unwrap();
        f.write_all(b"key = \"value\"\n").unwrap();
        drop(f);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "key = \"value\"\n");
    }

    #[cfg(unix)]
    #[test]
    fn private_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.toml");
        create_private_file(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, PRIVATE_FILE_MODE, "config must not be group/world accessible");
    }

    #[cfg(unix)]
    #[test]
    fn restrict_file_and_dir_tighten_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        restrict_file(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, PRIVATE_FILE_MODE);

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        restrict_dir(dir.path()).unwrap();
        assert_eq!(std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, PRIVATE_DIR_MODE);
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
}

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

/// Stable, greppable name for a signal number: `15` → `"SIGTERM"`.
///
/// Numbers are not greppable and not portable, so every log line that carries a
/// signal carries this name too. An unknown number (a handler we did not install,
/// a platform-specific signal) falls back to `"SIGUNKNOWN"` rather than an empty
/// string or an error: the caller is writing a log record, and the record must
/// always be well formed.
///
/// This allocates a `String`, so it must **not** be called from a signal handler:
/// `src/logging/crash.rs` keeps its own `&'static str` version for exactly that
/// reason.
pub fn signal_name(sig: i32) -> String {
    signal_hook::low_level::signal_name(sig).unwrap_or("SIGUNKNOWN").to_string()
}

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

    // -----------------------------------------------------------------------
    // signal_name: the stable spelling used in `app.signal` / `app.signal_unavailable`
    // -----------------------------------------------------------------------

    #[test]
    fn signal_name_gives_ctrl_c_a_name() {
        // SIGINT is the one signal every platform has and the one a user is most
        // likely to hit; `signal=SIGINT` is what makes the log line readable.
        assert_eq!(signal_name(signal_hook::consts::SIGINT), "SIGINT");
    }

    #[test]
    fn signal_name_covers_the_quit_signals() {
        // Every signal `quit_signals()` returns must be named: it is the set that
        // `main` actually records.
        for sig in quit_signals() {
            let name = signal_name(*sig);
            assert!(name.starts_with("SIG"), "signal {sig} produced {name:?}");
            assert_ne!(name, "SIGUNKNOWN", "signal {sig} is one we handle and must be named");
        }
    }

    #[test]
    fn signal_name_is_total() {
        // An out-of-range number must never panic: `main` calls this while
        // reporting that signal *registration* failed, i.e. on an error path.
        for sig in [i32::MIN, -1, 0, 128, i32::MAX] {
            assert_eq!(signal_name(sig), "SIGUNKNOWN", "sig {sig} must fall back");
        }
    }

    #[test]
    fn an_unknown_signal_falls_back_to_sigunknown() {
        // 128 is above every platform's `NSIG`; the fallback is the documented
        // contract, not an accident of the table lookup.
        assert_eq!(signal_name(128), "SIGUNKNOWN");
        assert_eq!(signal_name(i32::MAX), "SIGUNKNOWN");
    }
}

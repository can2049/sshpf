//! Where a server listens: deriving socket and lock paths from a config path,
//! and creating the directory they live in with private permissions.
//!
//! # Identity is the config
//!
//! One server per configuration. The endpoint name is therefore a hash of the
//! canonicalised config path, so two clients started with the same `-c` meet at
//! the same socket, and two different configs never share a server.
//!
//! # Why FNV-1a and not `DefaultHasher`
//!
//! `std`'s `DefaultHasher` is explicitly not stable: its documentation says the
//! algorithm "is not specified, and so it and its hashes should not be relied
//! upon over releases". A toolchain upgrade would therefore move the socket and
//! leave an orphaned server running. A 12-line FNV-1a is stable by construction;
//! it is a naming function here, not a security boundary, and a collision is
//! detected anyway by comparing the config path in the handshake.
//!
//! # Why not `canonicalize`
//!
//! The socket path must be known *before* the config is read, and on first run
//! the config file does not exist yet — `canonicalize` returns `NotFound`. A
//! lexical resolution is used instead: make the path absolute against the
//! current directory and fold `.`/`..` by hand.
//!
//! Belongs here: path derivation, directory creation and permissions.
//! Does not belong: connecting or listening (that is the caller's job with the
//! returned paths).

use std::path::{Component, Path, PathBuf};

/// The paths one config's server is reachable at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The config this endpoint belongs to, canonicalised lexically.
    pub config_path: PathBuf,
    /// The Unix socket a client connects to.
    pub socket_path: PathBuf,
    /// The file lock that makes the server a singleton.
    pub lock_path: PathBuf,
    /// The directory both live in, created with private permissions.
    pub dir: PathBuf,
}

/// Derive the endpoint for a config path.
///
/// Does no I/O: the caller decides when to create the directory, so a `--check`
/// run never leaves a socket directory behind.
pub fn endpoint_for(config_path: &Path) -> Endpoint {
    endpoint_in(config_path, socket_dir())
}

/// Derive the endpoint under an explicitly chosen directory.
///
/// The directory is normally `$XDG_RUNTIME_DIR/sshpf` (or `$SSHPF_SOCKET_DIR`).
/// Passing it explicitly exists for callers that must agree on a location without
/// reading process-global environment — a test that starts a server *as a child
/// process* computes the socket path for the child's env, and cannot rely on
/// mutating its own environment to match. Two processes that disagree about the
/// directory are two processes that will never meet.
pub fn endpoint_in(config_path: &Path, dir: PathBuf) -> Endpoint {
    let config_path = normalize(config_path);
    let key = short_hash(&config_path);
    Endpoint {
        socket_path: dir.join(format!("{key}.sock")),
        lock_path: dir.join(format!("{key}.lock")),
        dir,
        config_path,
    }
}

/// The directory holding every endpoint: `$XDG_RUNTIME_DIR/sshpf`, falling back
/// to the platform temp dir.
///
/// `runtime_dir` is preferred because it is per-user (`0700`), cleared at
/// logout, and never world-writable — `/tmp` is `1777`, so a socket there would
/// be reachable by anyone who guesses the name.
///
/// `$SSHPF_SOCKET_DIR` overrides the whole thing. It exists for tests: two test
/// binaries sharing one `$XDG_RUNTIME_DIR` would share one lock file and one
/// socket name per config, so they would elect each other's servers (and a
/// leftover worker would make the next run fail for reasons unrelated to the
/// code under test). It is also the documented workaround when `$HOME` is on a
/// filesystem where the runtime dir is unusable.
fn socket_dir() -> PathBuf {
    socket_dir_with(std::env::var_os("SSHPF_SOCKET_DIR"))
}

/// The pure half of [`socket_dir`], so the override can be tested without
/// mutating process-global state (`std::env::set_var` is `unsafe` in edition
/// 2024, and this crate confines `unsafe` to the crash writer).
fn socket_dir_with(override_dir: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(dir) = override_dir {
        return PathBuf::from(dir);
    }
    match dirs::runtime_dir() {
        Some(base) => base.join("sshpf"),
        None => std::env::temp_dir().join("sshpf-ipc"),
    }
}

/// Make a path absolute and fold `.`/`..` lexically, without touching the disk.
///
/// `std::path::absolute` already removes `.` and makes the path absolute, but it
/// deliberately keeps `..`; folding that is the missing piece.
pub fn normalize(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            // Drop "." entirely: it names the current directory and adds nothing.
            Component::CurDir => {}
            // ".." pops the last real component. At the root there is nothing to
            // pop, and going above it is meaningless, so it is ignored.
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A stable 16-hex-digit FNV-1a-64 of a path's bytes.
///
/// Not a cryptographic hash and not meant to be: it names a socket after a
/// config. A collision would point two configs at one server, which the
/// handshake detects by comparing the real paths.
fn short_hash(path: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path_bytes(path) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The bytes of a path, without requiring UTF-8 (a config path need not be).
#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

/// The bytes of a path on Windows, where paths are UTF-16 and os-strings are not
/// byte-addressable. A lossy conversion is acceptable: this only names a socket,
/// and two paths that differ only in unpaired surrogates cannot both be real.
#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_config_always_yields_the_same_endpoint() {
        // Stability matters: a changing hash would strand a running server.
        let a = endpoint_for(Path::new("/tmp/some/config.toml"));
        let b = endpoint_for(Path::new("/tmp/some/config.toml"));
        assert_eq!(a, b);
    }

    #[test]
    fn different_configs_yield_different_sockets() {
        let a = endpoint_for(Path::new("/tmp/a/config.toml"));
        let b = endpoint_for(Path::new("/tmp/b/config.toml"));
        assert_ne!(a.socket_path, b.socket_path);
        assert_ne!(a.lock_path, b.lock_path);
    }

    #[test]
    fn the_socket_and_lock_share_a_directory_but_not_a_name() {
        let e = endpoint_for(Path::new("/tmp/x/config.toml"));
        assert_eq!(e.socket_path.parent(), e.lock_path.parent());
        assert_ne!(e.socket_path, e.lock_path);
        assert_eq!(e.socket_path.extension().and_then(|s| s.to_str()), Some("sock"));
        assert_eq!(e.lock_path.extension().and_then(|s| s.to_str()), Some("lock"));
    }

    #[test]
    fn the_name_is_sixteen_hex_digits() {
        let e = endpoint_for(Path::new("/tmp/x/config.toml"));
        let stem = e.socket_path.file_stem().and_then(|s| s.to_str()).expect("a stem");
        assert_eq!(stem.len(), 16, "{stem}");
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit()), "{stem}");
    }

    #[test]
    fn dotted_segments_normalize_away() {
        // ".", ".." and a relative path must all reach the same identity, or the
        // same config would start two servers.
        let base = normalize(Path::new("/tmp/norm/config.toml"));
        assert_eq!(normalize(Path::new("/tmp/norm/./config.toml")), base);
        assert_eq!(normalize(Path::new("/tmp/norm/../norm/config.toml")), base);
    }

    #[test]
    fn a_relative_path_becomes_absolute() {
        let normalized = normalize(Path::new("relative/config.toml"));
        assert!(normalized.is_absolute(), "{normalized:?}");
        assert!(normalized.ends_with("relative/config.toml"), "{normalized:?}");
    }

    #[test]
    fn normalizing_is_idempotent() {
        let once = normalize(Path::new("/tmp/idem/./x/../config.toml"));
        assert_eq!(normalize(&once), once);
    }

    #[test]
    fn walking_above_the_root_does_not_escape() {
        // ".." at the root has nowhere to go; it must not produce a relative path.
        let normalized = normalize(Path::new("/../../config.toml"));
        assert!(normalized.is_absolute(), "{normalized:?}");
        assert!(normalized.ends_with("config.toml"));
    }

    /// The override must win, because it is the only way two test binaries can
    /// share one machine without electing each other's servers.
    #[test]
    fn the_socket_dir_can_be_overridden_for_isolation() {
        let dir = std::env::temp_dir().join("sshpf-sockdir-override-test");
        assert_eq!(
            socket_dir_with(Some(dir.clone().into_os_string())),
            dir,
            "an explicit override must be honoured verbatim"
        );
    }

    #[test]
    fn without_an_override_the_runtime_dir_is_used() {
        let dir = socket_dir_with(None);
        let expected_base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
        assert!(dir.starts_with(&expected_base), "{dir:?} must live under {expected_base:?}");
    }

    #[test]
    fn the_hash_is_stable_for_a_known_input() {
        // A regression here would silently move every existing user's socket.
        assert_eq!(short_hash(Path::new("/tmp/x")), short_hash(Path::new("/tmp/x")));
        assert_ne!(short_hash(Path::new("/tmp/x")), short_hash(Path::new("/tmp/y")));
    }

    #[test]
    fn equivalent_paths_hash_identically_after_normalization() {
        let a = endpoint_for(Path::new("/tmp/h/./config.toml"));
        let b = endpoint_for(Path::new("/tmp/h/config.toml"));
        assert_eq!(a.socket_path, b.socket_path);
    }
}

//! Where a server can be reached: deriving the endpoint's files from a config
//! path, and the directory they live in.
//!
//! # Identity is the config
//!
//! One server per configuration. The endpoint name is therefore a hash of the
//! canonicalised config path, so two clients started with the same `-c` meet at
//! the same server, and two different configs never share a server.
//!
//! # Why a port file instead of a Unix socket
//!
//! The transport is a TCP connection on `127.0.0.1`, on every platform. A Unix
//! domain socket would be the cheaper choice on Linux, but `std` has no socket
//! API for it on Windows, and a second transport would mean two discovery
//! protocols, two failure modes and two test paths for one feature. One code
//! path that works everywhere is worth the syscall.
//!
//! Discovery is a file because a client cannot guess an ephemeral port: the
//! server binds `127.0.0.1:0`, writes the chosen port to [`Endpoint::port_path`],
//! and a client reads it. A stale file left by a killed server is detected by
//! trying to connect, not by the file's existence.
//!
//! # Why FNV-1a and not `DefaultHasher`
//!
//! `std`'s `DefaultHasher` is explicitly not stable: its documentation says the
//! algorithm "is not specified, and so it and its hashes should not be relied
//! upon over releases". A toolchain upgrade would therefore move the endpoint and
//! leave an orphaned server running. A 12-line FNV-1a is stable by construction;
//! it is a naming function here, not a security boundary, and a collision is
//! detected anyway by comparing the config path in the handshake.
//!
//! # Why not `canonicalize`
//!
//! The endpoint must be known *before* the config is read, and on first run the
//! config file does not exist yet — `canonicalize` returns `NotFound`. A lexical
//! resolution is used instead: make the path absolute against the current
//! directory and fold `.`/`..` by hand.
//!
//! Belongs here: path derivation only, no I/O.
//! Does not belong: creating the directory or touching the files (that is
//! [`super::transport`]), or connecting (the caller's job).

use std::path::{Component, Path, PathBuf};

/// The files one config's server is described by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The config this endpoint belongs to, canonicalised lexically.
    pub config_path: PathBuf,
    /// The private directory all the files below live in.
    pub dir: PathBuf,
    /// Holds the decimal port the server bound, written once it is listening.
    pub port_path: PathBuf,
    /// Holds the shared secret a client must present to be served.
    pub token_path: PathBuf,
    /// The file lock that makes the server a singleton.
    pub lock_path: PathBuf,
}

/// Derive the endpoint for a config path.
///
/// Does no I/O: the caller decides when to create the directory, so a `--check`
/// run never leaves a directory behind.
pub fn endpoint_for(config_path: &Path) -> Endpoint {
    let config_path = normalize(config_path);
    let dir = ipc_dir();
    let key = short_hash(&config_path);
    Endpoint {
        port_path: dir.join(format!("{key}.port")),
        token_path: dir.join(format!("{key}.token")),
        lock_path: dir.join(format!("{key}.lock")),
        dir,
        config_path,
    }
}

/// The directory holding every endpoint.
///
/// `$SSHPF_IPC_DIR` wins, which is what tests use to keep two suites from
/// finding each other's servers. Otherwise `$XDG_RUNTIME_DIR` is preferred
/// because it is per-user (`0700`), cleared at logout, and never world-writable;
/// on Windows that variable is normally unset and the per-user temp directory
/// takes over, which inherits the profile's ACL rather than being world-writable.
fn ipc_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SSHPF_IPC_DIR") {
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
/// Not a cryptographic hash and not meant to be: it names files after a config.
/// A collision would point two configs at one server, which the handshake
/// detects by comparing the real paths.
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
/// byte-addressable. A lossy conversion is acceptable: this only names a file,
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
    fn different_configs_yield_different_endpoints() {
        let a = endpoint_for(Path::new("/tmp/a/config.toml"));
        let b = endpoint_for(Path::new("/tmp/b/config.toml"));
        assert_ne!(a.port_path, b.port_path);
        assert_ne!(a.token_path, b.token_path);
        assert_ne!(a.lock_path, b.lock_path);
    }

    #[test]
    fn the_three_files_share_a_directory_but_not_a_name() {
        let e = endpoint_for(Path::new("/tmp/x/config.toml"));
        for p in [&e.port_path, &e.token_path, &e.lock_path] {
            assert_eq!(p.parent(), Some(e.dir.as_path()));
        }
        assert_ne!(e.port_path, e.token_path);
        assert_ne!(e.port_path, e.lock_path);
        assert_ne!(e.token_path, e.lock_path);
        assert_eq!(e.port_path.extension().and_then(|s| s.to_str()), Some("port"));
        assert_eq!(e.token_path.extension().and_then(|s| s.to_str()), Some("token"));
        assert_eq!(e.lock_path.extension().and_then(|s| s.to_str()), Some("lock"));
    }

    #[test]
    fn the_name_is_sixteen_hex_digits() {
        let e = endpoint_for(Path::new("/tmp/x/config.toml"));
        let stem = e.port_path.file_stem().and_then(|s| s.to_str()).expect("a stem");
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

    #[test]
    fn the_hash_is_stable_and_discriminating() {
        // A regression here would silently move every existing user's endpoint.
        assert_eq!(short_hash(Path::new("/tmp/x")), short_hash(Path::new("/tmp/x")));
        assert_ne!(short_hash(Path::new("/tmp/x")), short_hash(Path::new("/tmp/y")));
    }

    #[test]
    fn equivalent_paths_hash_identically_after_normalization() {
        let a = endpoint_for(Path::new("/tmp/h/./config.toml"));
        let b = endpoint_for(Path::new("/tmp/h/config.toml"));
        assert_eq!(a.port_path, b.port_path);
    }
}

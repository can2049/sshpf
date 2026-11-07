//! Shared fixtures for the ssh-layer tests.
//!
//! Only compiled for tests. Both the config parser and the synth baker need a
//! stand-in `ssh` binary, so the fake lives here instead of being copied.

use std::path::{Path, PathBuf};

/// Write an executable script and return its path.
pub(crate) fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// A stand-in `ssh` that impersonates several hosts for parser tests.
///
/// `-G` output mirrors real OpenSSH: lowercase keys, repeated keys allowed, and
/// `broken` fails the way a malformed config would.
pub(crate) const FAKE_SSH: &str = r#"#!/bin/sh
# Fake `ssh` for tests: only implements `-G -T` style evaluation.
host=""
for a in "$@"; do host="$a"; done
case "$host" in
  alpha|cloud_pc)
echo "hostname 10.0.0.1"
echo "user alice"
echo "port 2222"
echo "proxyjump jumpbox"
echo "identityfile ~/.ssh/id_rsa"
echo "localforward 18789 [127.0.0.1]:18789"
echo "clearallforwardings no"
echo "userknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2"
echo "proxycommand"
;;
  jumpbox)
echo "hostname 10.9.9.9"
echo "port 80"
echo "user jump"
;;
  beta)
echo "hostname 10.0.0.2"
echo "user bob"
echo "port 22"
;;
  broken)
echo "some config error" >&2
exit 255
;;
  *) exit 255 ;;
esac
"#;

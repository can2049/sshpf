//! Shared fixtures for the ssh-layer tests.
//!
//! Only compiled for tests. Both the config parser and the synth baker need a
//! stand-in `ssh` program, so the fake lives here instead of being copied.
//!
//! # Why not a shell script
//!
//! The obvious fixture is a 20-line `#!/bin/sh` script. That works on Unix and
//! fails on Windows: `CreateProcess` rejects `.sh` with
//! `%1 is not a valid Win32 application (os error 193)`, so every parser test
//! there would fail for a reason unrelated to the code under test.
//!
//! # What we do instead
//!
//! Compile a tiny Rust program with `rustc` into the temp dir, once per test
//! run, and point `SSHPF_SSH_BIN` at it. Rust is guaranteed present (the tests
//! are running under cargo) and the result is a native executable on every
//! platform. Compiling the helper takes ~80 ms, which is far cheaper than
//! maintaining two byte-identical shell variants — and it means the fake is
//! plain Rust that a reviewer can read.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Body of the fake `ssh`. Mirrors OpenSSH's `-G` output for canned hosts.
///
/// Kept as a `&str` of Rust source: it is compiled at test time, so the fixture
/// stays readable and there is no build-script indirection.
const FAKE_SSH_SRC: &str = r#"
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Real ssh treats the last argument as the destination.
    let host = args.last().map(String::as_str).unwrap_or("");
    let body = match host {
        "alpha" | "cloud_pc" => "\
hostname 10.0.0.1
user alice
port 2222
proxyjump jumpbox
identityfile ~/.ssh/id_rsa
localforward 18789 [127.0.0.1]:18789
clearallforwardings no
userknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2
proxycommand
",
        "jumpbox" => "\
hostname 10.9.9.9
port 80
user jump
",
        "beta" => "\
hostname 10.0.0.2
user bob
port 22
",
        "broken" => {
            eprintln!("some config error");
            std::process::exit(255);
        }
        _ => {
            // Unknown host: real ssh exits non-zero too.
            std::process::exit(255);
        }
    };
    print!("{}", body);
}
"#;

/// Write an executable file and return its path.
///
/// Used by tunnelling tests that need a script-like child; on Windows callers
/// should use the compiled helper below instead.
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

/// Path of the compiled fake `ssh`, built once per process.
///
/// Returns `None` when `rustc` cannot be run — tests then skip rather than fail
/// for an environment reason.
pub(crate) fn fake_ssh_binary() -> Option<&'static Path> {
    static BUILT: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("sshpf-fake-ssh-{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok()?;
            let src = dir.join("fake_ssh.rs");
            std::fs::write(&src, FAKE_SSH_SRC).ok()?;

            let exe = dir.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
            let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
            let out = std::process::Command::new(rustc).arg(&src).arg("-o").arg(&exe).output().ok()?;
            if !out.status.success() {
                eprintln!("fake ssh build failed: {}", String::from_utf8_lossy(&out.stderr));
                return None;
            }
            Some(exe)
        })
        .as_deref()
}

/// Hostnames the fake knows about, for assertions.
#[allow(dead_code)]
pub(crate) const FAKE_HOSTS: &[&str] = &["alpha", "cloud_pc", "jumpbox", "beta"];

/// Compile a fake `ssh` that prints exactly `stdout_lines`, one `echo` each.
///
/// This replaces the old "write a shell script" pattern for tests that need
/// custom `-G` output. Compiling keeps the fixture portable (Windows cannot run
/// a `.sh`) and still lets each test state its intent as plain data.
///
/// Returns `None` when `rustc` is unavailable, in which case the caller should
/// return early instead of failing for an environment reason.
pub(crate) fn fake_ssh_with_output(dir: &Path, tag: &str, stdout_lines: &[&str]) -> Option<PathBuf> {
    let mut program = String::from("fn main() {\n");
    for line in stdout_lines {
        // Rust string literal escaping is close enough for our fixture text.
        let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
        program.push_str(&format!("    println!(\"{escaped}\");\n"));
    }
    program.push_str("}\n");

    let src = dir.join(format!("{tag}.rs"));
    std::fs::write(&src, program).ok()?;
    let exe = dir.join(if cfg!(windows) { format!("{tag}.exe") } else { tag.to_string() });
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = std::process::Command::new(rustc).arg(&src).arg("-o").arg(&exe).output().ok()?;
    if !out.status.success() {
        eprintln!("fixture build failed for {tag}: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    Some(exe)
}

/// Compile a fake tunnel child that streams `stderr_lines` then sleeps.
///
/// Replaces the `/bin/sh` scripts the tunnel tests used to write: those cannot
/// run on Windows, and the point of those tests (supervising a real child) has
/// nothing to do with the host shell. `exit_code` is returned after the sleep
/// so the caller can assert code propagation.
pub(crate) fn fake_tunnel_child(
    dir: &Path,
    tag: &str,
    stderr_lines: &[&str],
    sleep_secs: u64,
    exit_code: i32,
) -> Option<PathBuf> {
    let mut program = String::from("use std::io::Write;\nfn main() {\n");
    program.push_str("    let mut err = std::io::stderr();\n");
    for line in stderr_lines {
        let escaped = line.replace('\\', "\\\\").replace('"', "\\\"");
        program.push_str(&format!("    writeln!(err, \"{escaped}\").unwrap();\n"));
        program.push_str("    err.flush().unwrap();\n");
    }
    program.push_str(&format!(
        "    std::thread::sleep(std::time::Duration::from_secs({sleep_secs}));\n    \
         std::process::exit({exit_code});\n}}\n"
    ));
    fake_ssh_with_output(dir, tag, &[]) // ensures rustc works; then overwrite
        .and_then(|_| {
            let src = dir.join(format!("{tag}.rs"));
            std::fs::write(&src, program).ok()?;
            let exe = dir.join(if cfg!(windows) { format!("{tag}.exe") } else { tag.to_string() });
            let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
            let out = std::process::Command::new(rustc).arg(&src).arg("-o").arg(&exe).output().ok()?;
            if !out.status.success() {
                eprintln!("fixture build failed for {tag}: {}", String::from_utf8_lossy(&out.stderr));
                return None;
            }
            Some(exe)
        })
}

/// Compile a child that floods stdout then sleeps, to prove stdout is drained.
pub(crate) fn fake_chatty_child(dir: &Path, tag: &str, lines: u32, sleep_secs: u64, exit_code: i32) -> Option<PathBuf> {
    let program = format!(
        "fn main() {{\n    \
         for _ in 0..{lines} {{ println!(\"noise on stdout\"); }}\n    \
         std::thread::sleep(std::time::Duration::from_secs({sleep_secs}));\n    \
         std::process::exit({exit_code});\n}}\n"
    );
    compile_helper(dir, tag, &program)
}

/// Write `program` to `<dir>/<tag>.rs` and compile it, returning the exe path.
fn compile_helper(dir: &Path, tag: &str, program: &str) -> Option<PathBuf> {
    let src = dir.join(format!("{tag}.rs"));
    std::fs::write(&src, program).ok()?;
    let exe = dir.join(if cfg!(windows) { format!("{tag}.exe") } else { tag.to_string() });
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = std::process::Command::new(rustc).arg(&src).arg("-o").arg(&exe).output().ok()?;
    if !out.status.success() {
        eprintln!("fixture build failed for {tag}: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    Some(exe)
}

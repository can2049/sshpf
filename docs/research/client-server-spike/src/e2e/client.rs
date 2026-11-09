//! The spike's client process: connects, records what it observes, obeys
//! commands on stdin, and leaves when told.
//!
//! What belongs here: the client's own discipline — discard stale revisions and
//! record what it saw — plus saying goodbye with the right `last_exit` claim.
//! What does not: the server's bookkeeping (`e2e/server`).
//!
//! Every observation is written to a marker file, because the whole point of
//! this binary is that a *different* process can assert on real cross-process
//! behaviour instead of on its own memory.

use std::io::BufRead;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::common::frame::{read_frame, write_frame};
use crate::common::msg::{ClientMsg, Op};
use crate::common::wire::Snapshot;

/// How long to wait for the server's socket to appear.
const CONNECT_BUDGET: Duration = Duration::from_secs(10);

/// Connect, observe, then follow stdin commands. Returns the process exit code.
pub fn run(dir: &Path, name: &str) -> i32 {
    let socket = dir.join("server.sock");
    let Some(mut stream) = connect(&socket) else {
        return 4;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));

    // Introduce ourselves.
    let hello = ClientMsg::Hello { proto_version: 1, client_pid: std::process::id() };
    let Ok(text) = toml::to_string(&hello) else {
        return 5;
    };
    if write_frame(&mut stream, &text).is_err() {
        return 5;
    }

    // Reader thread: persistent observations go to disk as they happen, so a
    // kill cannot hide what was already seen.
    let reader = {
        let dir = dir.to_path_buf();
        let name = name.to_string();
        let read_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return 5,
        };
        std::thread::spawn(move || observe(read_stream, &dir, &name))
    };

    // Command loop: the parent drives the interleavings through stdin.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        match line.trim() {
            // Let the pending snapshots land before the parent inspects markers.
            "wait" => std::thread::sleep(Duration::from_millis(350)),
            "bye" => {
                send_bye(&mut stream, false);
                break;
            }
            // The claim the design treats as a *proposal*: the user was told that
            // quitting stops every forward.
            "bye_last" => {
                send_bye(&mut stream, true);
                break;
            }
            "stop_all" => {
                let cmd = ClientMsg::Command { req_id: 1, expect_rev: None, op: Op::StopAll { all: true } };
                if let Ok(text) = toml::to_string(&cmd) {
                    let _ = write_frame(&mut stream, &text);
                }
            }
            // Ask for one more state change, then let the reader settle so the
            // snapshot on disk is final.
            "hold" => {
                let cmd = ClientMsg::Command { req_id: 2, expect_rev: None, op: Op::Start { id: 1 } };
                if let Ok(text) = toml::to_string(&cmd) {
                    let _ = write_frame(&mut stream, &text);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            _ => {}
        }
    }

    // Give the reader a moment to persist the final observation, then mark done.
    std::thread::sleep(Duration::from_millis(150));
    drop(stream);
    let _ = reader.join();
    let _ = std::fs::write(dir.join(format!("{name}.done")), "1");
    0
}

/// Send the goodbye, then pause so the server counts us out before the socket
/// closes — otherwise the parent races against the kernel delivering the EOF.
fn send_bye(stream: &mut UnixStream, last_exit: bool) {
    if let Ok(text) = toml::to_string(&ClientMsg::Bye { last_exit }) {
        let _ = write_frame(stream, &text);
    }
    std::thread::sleep(Duration::from_millis(120));
}

/// Read snapshots until the server closes the connection.
///
/// A frame that is not newer than what was already applied is discarded. That
/// single rule is what makes a duplicated or reordered frame harmless.
fn observe(mut stream: UnixStream, dir: &Path, name: &str) {
    let mut last_rev = 0u64;
    let mut strictly_increasing = true;
    let mut applied_any = false;

    loop {
        match read_frame(&mut stream) {
            Ok(Some(payload)) => {
                let Ok(snap): Result<Snapshot, _> = toml::from_str(&payload) else {
                    continue;
                };
                // Stale or duplicate: ignore it rather than regress the view.
                if applied_any && snap.revision <= last_rev {
                    if snap.revision < last_rev {
                        strictly_increasing = false;
                    }
                    continue;
                }
                last_rev = snap.revision;
                applied_any = true;

                let digest = snap.digest();
                let _ = std::fs::write(dir.join(format!("{name}.snap")), &digest);
                let _ = std::fs::write(
                    dir.join(format!("{name}.incr")),
                    if strictly_increasing { "true" } else { "false" },
                );
                // Marker files per observed client count: this is how "I was the
                // only client when I opened the dialog" survives the process
                // boundary and cannot be confused with "I am the last one now".
                if snap.client_count == 1 {
                    let _ = std::fs::write(dir.join(format!("{name}.solo")), "1");
                }
                if snap.client_count >= 2 {
                    let _ = std::fs::write(dir.join(format!("{name}.saw2")), "1");
                }
                if snap.client_count >= 3 {
                    let _ = std::fs::write(dir.join(format!("{name}.saw3")), "1");
                }
            }
            // Clean EOF: the server stopped, or dropped us as a slow consumer.
            Ok(None) => break,
            // A read timeout is not an error; keep waiting for the next frame.
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
}

/// Wait for the server's socket to appear, then connect.
fn connect(socket: &Path) -> Option<UnixStream> {
    let deadline = Instant::now() + CONNECT_BUDGET;
    while Instant::now() < deadline {
        if let Ok(s) = UnixStream::connect(socket) {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    UnixStream::connect(socket).ok()
}

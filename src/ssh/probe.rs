//! Discover the ports that are **listening on the peer machine**.
//!
//! The create form asks the user for a remote port. Typing a number from memory
//! is the one step in "make a tunnel" that requires knowledge the machine already
//! has, so this module asks the host instead: one command over the tunnel's own
//! SSH path, parsed into [`RemotePort`]s.
//!
//! # What is run, and why that shape
//!
//! ```text
//! command -v ss      → ss -ltnH
//! else command -v netstat → netstat -ltn
//! else             → the sentinel __SSHPF_NO_TOOL__
//! ```
//!
//! Three properties matter and each is load-bearing:
//!
//! * **Fixed text, no interpolation.** The command contains no host name, port or
//!   any other user-supplied value, so there is nothing for a hostile alias to
//!   inject into. [`list_command`] is the only place it is written.
//! * **A sentinel instead of silence.** Without it, "the peer has neither `ss`
//!   nor `netstat`" and "the peer has nothing listening" are the same empty
//!   string — and those are opposite answers to give a user who is about to pick
//!   a port.
//! * **Listening only.** `ss -ltn` / `netstat -ltn` restrict to LISTEN sockets, and
//!   [`is_listening_line`] requires the state to be visible as well, so a peer
//!   whose `-l` flag is unsupported cannot fill the list with established
//!   connections.
//!
//! Belongs here: the command, the sentinel handling, the "unparseable output"
//! verdict, and the wire to [`crate::ssh::client::probe`].
//! Does not belong: parsing the text into ports (`crate::model::parse_listen_ports`),
//! the thread that runs this (`crate::app::port_scan`), rendering.

use anyhow::{Result, bail};

use crate::model::{RemotePort, parse_listen_ports};
use crate::ssh::known_hosts::HostKeyPolicy;

/// Marker the remote command prints when neither tool exists.
///
/// Chosen so no real `ss`/`netstat` line can contain it, and so it has no colon
/// (a port parser scanning tokens therefore ignores it even if it leaks through).
pub const NO_TOOL_SENTINEL: &str = "__SSHPF_NO_TOOL__";

/// The command whose stdout is parsed for listening ports.
///
/// Written once, here: a second copy is how the shell quoting drifts from the
/// parser. `-H` suppresses `ss`'s header (its `Local Address:Port` column is the
/// one line a lenient token scan could misread); `netstat`'s header is harmless
/// because it has no numeric `addr:port` token, but it is filtered by the LISTEN
/// rule anyway.
pub fn list_command() -> &'static str {
    concat!(
        "if command -v ss >/dev/null 2>&1; then ss -ltnH; ",
        "elif command -v netstat >/dev/null 2>&1; then netstat -ltn; ",
        "else printf '%s\\n' ",
        "\"__SSHPF_NO_TOOL__\"; fi"
    )
}

/// Whether a line reports a listening socket.
///
/// The state must be visible as a `LISTEN` token. This is what keeps a peer whose
/// `netstat` ignores `-l` from contributing its established connections: every
/// port in this list is offered as a place to forward *to*, and a client socket's
/// ephemeral port would be a wrong answer that looks plausible.
fn is_listening_line(line: &str) -> bool {
    line.split_whitespace().any(|token| token == "LISTEN")
}

/// The verdict of one probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    /// Ports are listening; empty means nothing is listening, which is a real
    /// answer worth showing.
    Found(Vec<RemotePort>),
    /// The peer has no tool we know how to read.
    NoTool,
    /// The tool produced text this parser does not recognise (an unsupported
    /// `netstat` dialect, typically a BSD/macOS one that prints `127.0.0.1.631`
    /// instead of `127.0.0.1:631`). Reported instead of "nothing listening",
    /// because telling the user there is nothing to pick when there is plenty is
    /// worse than saying the output was not understood.
    Unparsed,
}

impl ScanOutcome {
    pub fn ports(&self) -> &[RemotePort] {
        match self {
            ScanOutcome::Found(ports) => ports,
            _ => &[],
        }
    }
}

/// Turn one probe's stdout into a verdict.
///
/// Pure, so every branch (sentinel, empty, unparseable, real rows) is unit-tested
/// without a socket.
pub fn interpret(stdout: &str) -> ScanOutcome {
    if stdout.contains(NO_TOOL_SENTINEL) {
        return ScanOutcome::NoTool;
    }
    let listening: Vec<&str> = stdout.lines().filter(|line| is_listening_line(line)).collect();
    if listening.is_empty() {
        // No listener line at all: either nothing is listening (empty output) or
        // the text is in a dialect this parser does not handle.
        return if stdout.trim().is_empty() { ScanOutcome::Found(Vec::new()) } else { ScanOutcome::Unparsed };
    }
    let ports = parse_listen_ports(&listening.join("\n"));
    if ports.is_empty() { ScanOutcome::Unparsed } else { ScanOutcome::Found(ports) }
}

/// Scan `ssh_host` for listening ports.
///
/// Async because it runs on an SSH session; the app calls it from a dedicated
/// thread (see `crate::app::port_scan`), never from the UI thread.
pub async fn scan_listen_ports(ssh_host: &str, policy: HostKeyPolicy) -> Result<ScanOutcome> {
    let opts = crate::ssh::client::Options { host_key_policy: policy };
    let raw = crate::ssh::client::probe::exec_capture(ssh_host, &opts, list_command()).await?;
    // Remote output is bytes, not necessarily UTF-8: a locale or a binary banner
    // can put invalid sequences in it, and `String::from_utf8` would throw the
    // whole answer away. Lossy decoding keeps the ASCII part, which is all the
    // parser needs.
    let text = String::from_utf8_lossy(&raw).to_string();
    let outcome = interpret(&text);
    if outcome == ScanOutcome::Unparsed {
        bail!("远端返回的端口列表无法解析（仅支持 ss / netstat -l 格式）");
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_fixed_and_mentions_both_tools() {
        let cmd = list_command();
        assert!(cmd.contains("ss -ltnH"), "{cmd}");
        assert!(cmd.contains("netstat -ltn"), "{cmd}");
        assert!(cmd.contains(NO_TOOL_SENTINEL), "the sentinel must be printed when neither tool exists: {cmd}");
        // Nothing user-supplied can reach the shell: no variable expansion, no
        // command substitution, no quoting of an interpolated value.
        for forbidden in ['$', '`', '\n'] {
            assert!(!cmd.contains(forbidden), "the probe command must stay a fixed literal: {cmd}");
        }
        assert_eq!(cmd, list_command(), "the command must be stable between calls");
    }

    #[test]
    fn a_real_ss_answer_becomes_ports() {
        let out = "\
LISTEN 0      4096         0.0.0.0:22         0.0.0.0:*
LISTEN 0      511          127.0.0.1:631      0.0.0.0:*
";
        let found = interpret(out).ports().to_vec();
        assert_eq!(found.iter().map(|p| p.port).collect::<Vec<_>>(), vec![22, 631]);
    }

    #[test]
    fn a_netstat_answer_with_a_header_is_understood() {
        let out = "\
Active Internet connections (only servers)
Proto Recv-Q Send-Q Local Address           Foreign Address         State
tcp        0      0 127.0.0.1:5432          0.0.0.0:*               LISTEN
";
        let found = interpret(out).ports().to_vec();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].port, 5432);
    }

    #[test]
    fn the_sentinel_reports_the_missing_tool_instead_of_an_empty_list() {
        // The distinction this test exists for: "no ss/netstat on the peer" must
        // not read as "nothing is listening".
        assert_eq!(interpret(&format!("{NO_TOOL_SENTINEL}\n")), ScanOutcome::NoTool);
        assert_eq!(interpret(""), ScanOutcome::Found(Vec::new()));
        assert_ne!(interpret(NEVER_LISTENING), ScanOutcome::Found(Vec::new()));
    }

    /// Output this parser does not understand: a BSD/macOS `netstat` prints
    /// `127.0.0.1.631` with a dot, so the colon split finds nothing.
    const NEVER_LISTENING: &str = "Active Internet connections\ntcp4 0 0 127.0.0.1.631 *.* LISTEN\n";

    #[test]
    fn unknown_output_is_reported_rather_than_shown_as_empty() {
        let outcome = interpret(NEVER_LISTENING);
        assert_eq!(outcome, ScanOutcome::Unparsed);
        assert!(outcome.ports().is_empty());
    }

    #[test]
    fn established_connections_are_not_offered_as_forward_targets() {
        // A peer whose `-l` is unsupported: the LISTEN rule is the only thing
        // keeping client sockets (and their ephemeral ports) out of the list.
        let out = "\
tcp 0 0 10.0.0.5:22 10.0.0.9:54321 ESTABLISHED
tcp 0 0 127.0.0.1:8080 0.0.0.0:* LISTEN
";
        let found = interpret(out).ports().to_vec();
        assert_eq!(found.iter().map(|p| p.port).collect::<Vec<_>>(), vec![8080], "only the listener");
    }

    #[test]
    fn a_line_listing_only_is_accepted() {
        // `ss -ltnH` with the header suppressed still carries the state token;
        // this pins that requirement, since a header-only answer must not become
        // a port list.
        assert!(is_listening_line("LISTEN 0 128 0.0.0.0:22 0.0.0.0:*"));
        assert!(!is_listening_line("State Recv-Q Send-Q Local Address:Port Peer Address:Port"));
        assert!(!is_listening_line(""));
    }

    #[test]
    fn duplicate_and_unsorted_rows_are_normalised() {
        let out = "\
LISTEN 0 128 [::]:8080 [::]:*
LISTEN 0 128 0.0.0.0:22 0.0.0.0:*
LISTEN 0 128 127.0.0.1:8080 0.0.0.0:*
";
        let found = interpret(out).ports().to_vec();
        assert_eq!(found.iter().map(|p| p.port).collect::<Vec<_>>(), vec![22, 8080], "sorted, merged");
        assert_eq!(found[1].addrs, vec!["[::]", "127.0.0.1"], "both bind addresses of 8080");
    }
}

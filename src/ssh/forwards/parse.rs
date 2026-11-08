//! One forwarding directive's arguments → [`SshForward`].
//!
//! Pure: no I/O, no scoping decisions, no knowledge of which block the line came
//! from. The accepted shapes are the ones OpenSSH documents:
//!
//! ```text
//! LocalForward   [bind_address:]port  host:hostport
//! RemoteForward  [bind_address:]port  host:hostport
//! DynamicForward [bind_address:]port
//! ```
//!
//! Sockets and anything that cannot become a tunnel still produce a row (port 0)
//! so the UI can list it and explain why: silently dropping a configured forward
//! reads as a bug in sshpf, not as an unsupported feature.

use crate::model::{ForwardKind, SshForward};

use super::scan::shell_words;

/// Parse the arguments of one forwarding directive.
///
/// Accepted shapes (the ones OpenSSH documents):
///
/// ```text
/// LocalForward  [bind_address:]port  host:hostport
/// RemoteForward [bind_address:]port  host:hostport
/// DynamicForward [bind_address:]port
/// ```
///
/// `[bind_address:]port` may also be written as two arguments (`8080` or
/// `127.0.0.1 8080`-style arguments are accepted where unambiguous). Sockets and
/// anything we cannot map onto a `TunnelConfig` produce a row with port 0 so the
/// UI can list it as un-importable instead of hiding it.
pub(super) fn parse_directive(kind: ForwardKind, rest: &str, alias: &str, line: usize) -> Option<SshForward> {
    let args = shell_words(rest);
    let mut fwd = SshForward {
        kind,
        listen_addr: String::new(),
        listen_port: 0,
        target_host: String::new(),
        target_port: 0,
        host_alias: alias.to_string(),
        line,
        raw: String::new(),
    };
    match kind {
        ForwardKind::Dynamic => apply_dynamic(&mut fwd, &args),
        ForwardKind::Local | ForwardKind::Remote => apply_forward(&mut fwd, &args),
    }
    Some(fwd)
}

/// `DynamicForward [bind:]port` — a listen endpoint and nothing else.
///
/// A spec that does not parse leaves port 0 on the row, which is how the table
/// knows to list it as un-importable.
fn apply_dynamic(fwd: &mut SshForward, args: &[String]) {
    if let Some((addr, port)) = args.first().and_then(|spec| split_host_port(spec)) {
        fwd.listen_addr = addr.unwrap_or_default();
        fwd.listen_port = port;
    }
}

/// `LocalForward` / `RemoteForward`, in the argument shapes OpenSSH accepts.
///
/// Three shapes, and each malformed one still yields a row (with whatever part
/// did parse) so the UI can list and explain it:
///
/// | args | meaning |
/// |---|---|
/// | `[]` | nothing usable — a bare directive |
/// | `[spec]` | a listen endpoint with no destination (an un-importable `-R`; still shows the port) |
/// | `[listen, target]` | the documented form |
/// | `[addr, port, target…]` | the same, written as three or more arguments |
fn apply_forward(fwd: &mut SshForward, args: &[String]) {
    let (listen_spec, target_spec) = match args.len() {
        0 => return,
        1 => {
            if let Some((addr, port)) = split_host_port(&args[0]) {
                fwd.listen_addr = addr.unwrap_or_default();
                fwd.listen_port = port;
            }
            return;
        }
        2 => (args[0].clone(), args[1].clone()),
        _ => {
            // `<addr> <port> <target…>`: the first two arguments are one endpoint.
            let listen = format!("{}:{}", args[0], args[1]);
            (listen, args[2..].join(" "))
        }
    };
    // A unix-socket path is not a host:port; leaving port 0 marks the row
    // unsupported instead of misparsing it.
    if let Some((addr, port)) = split_host_port(&listen_spec) {
        fwd.listen_addr = addr.unwrap_or_default();
        fwd.listen_port = port;
    }
    if let Some((host, port)) = split_host_port(&target_spec) {
        fwd.target_host = host.unwrap_or_default();
        fwd.target_port = port;
    }
}
/// Split `[host:]port` into `(host, port)`.
///
/// `None` when the port is not a number (a socket path, or a hostname that the
/// user left as `host:service`).
fn split_host_port(spec: &str) -> Option<(Option<String>, u16)> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    // `[::1]:8080` — bracketed IPv6.
    if let Some(rest) = spec.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':').and_then(|p| p.parse::<u16>().ok())?;
        return Some((Some(host.to_string()), port));
    }
    match spec.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse::<u16>().ok()?;
            Some((Some(host.to_string()), port))
        }
        // Bare `8080`: listen on the default bind address.
        None => Some((None, spec.parse::<u16>().ok()?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::forwards::scan::scan_config;

    /// Scan `text` as one config file, with a nonexistent `Include` base.
    fn scan(text: &str, alias: &str) -> Vec<SshForward> {
        let mut stack = Vec::new();
        let mut out = Vec::new();
        scan_config(text, alias, std::path::Path::new("/nonexistent"), &mut stack, &mut out, 0);
        out
    }

    #[test]
    fn parses_the_three_directive_kinds() {
        let text = "\
Host cloud_pc
  HostName 10.0.0.1
  LocalForward 18789 127.0.0.1:18789
  LocalForward 127.0.0.1:44897 127.0.0.1:44897
  DynamicForward 1080
  RemoteForward 7000 localhost:22
";
        let rows = scan(text, "cloud_pc");
        assert_eq!(rows.len(), 4, "{rows:#?}");

        assert_eq!(rows[0].kind, ForwardKind::Local);
        assert_eq!(rows[0].listen_addr, "");
        assert_eq!(rows[0].listen_port, 18789);
        assert_eq!(rows[0].target_host, "127.0.0.1");
        assert_eq!(rows[0].target_port, 18789);
        assert_eq!(rows[0].line, 3);
        assert_eq!(rows[0].host_alias, "cloud_pc");

        assert_eq!(rows[1].listen_addr, "127.0.0.1");
        assert_eq!(rows[1].listen_port, 44897);

        assert_eq!(rows[2].kind, ForwardKind::Dynamic);
        assert_eq!(rows[2].listen_port, 1080);
        assert_eq!(rows[2].target_display(), "(SOCKS5)");

        assert_eq!(rows[3].kind, ForwardKind::Remote);
        assert_eq!(rows[3].listen_port, 7000);
        assert_eq!(rows[3].target_port, 22);
    }

    #[test]
    fn unix_sockets_are_listed_but_unsupported() {
        let text = "\
Host web
  LocalForward /run/app.sock /run/remote.sock
  RemoteForward /tmp/x.sock
";
        let rows = scan(text, "web");
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.listen_port, 0);
            assert!(r.unsupported_reason().is_some(), "{r:?} should be unsupported");
        }
    }

    #[test]
    fn remote_forward_without_a_destination_is_unsupported() {
        let rows = scan("Host web\n  RemoteForward 7006\n", "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_port, 7006);
        assert!(rows[0].unsupported_reason().is_some(), "no destination means nothing to forward to");
    }

    #[test]
    fn three_argument_form_is_accepted() {
        let rows = scan("Host web\n  LocalForward 127.0.0.1 8080 10.0.0.1:80\n", "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_addr, "127.0.0.1");
        assert_eq!(rows[0].listen_port, 8080);
        assert_eq!(rows[0].target_host, "10.0.0.1");
        assert_eq!(rows[0].target_port, 80);
    }

    #[test]
    fn ipv6_listen_and_target_parse() {
        let rows = scan("Host web\n  LocalForward [::1]:8080 [2001:db8::1]:443\n", "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_addr, "::1");
        assert_eq!(rows[0].listen_port, 8080);
        assert_eq!(rows[0].target_host, "2001:db8::1");
        assert_eq!(rows[0].target_port, 443);
    }

    #[test]
    fn verb_spelling_and_case_do_not_matter() {
        let rows = scan("Host web\n  localforward 8081 127.0.0.1:81\n", "web");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].listen_port, 8081);
        let rows = scan("Host web\n  LocalForward=8082 127.0.0.1:82\n", "web");
        assert_eq!(rows.len(), 1, "`Verb=args` spelling must work");
    }

    #[test]
    fn raw_text_is_preserved_for_display() {
        let rows = scan("Host web\n  LocalForward 8080 127.0.0.1:80\n", "web");
        assert_eq!(rows[0].raw, "LocalForward 8080 127.0.0.1:80");
    }

    #[test]
    fn suggested_name_mentions_host_kind_and_port() {
        let rows = scan("Host cloud_pc\n  LocalForward 8080 127.0.0.1:80\n", "cloud_pc");
        assert_eq!(rows[0].suggested_name(), "cloud_pc-L-8080");
    }
}

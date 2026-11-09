//! Parsing `ss` / `netstat` output into the ports that are listening.
//!
//! Remote output is untrusted input, so this is deliberately a strict parser
//! rather than a permissive scan: it accepts the one token shape both tools agree
//! on (`addr:port` with a numeric `1..=65535` port and a plausible address) and
//! ignores everything else. A header line, a process column and a truncated line
//! therefore produce nothing, which is the safe direction — a wrong port in the
//! list is a tunnel that forwards nowhere.
//!
//! The two tools format a line differently but agree on the important part: the
//! **first** token that reads as `addr:port` is the *local* address
//! (`ss`: `LISTEN 0 128 0.0.0.0:22 0.0.0.0:*`; `netstat`: `tcp 0 0
//! 127.0.0.1:631 0.0.0.0:* LISTEN`).
//!
//! Belongs here: the text → [`RemotePort`] conversion and its token helpers.
//! Does not belong: the picker state (`super`), the SSH transport
//! (`crate::ssh::probe`), the ssh-config filter (`super::declared`).

use super::RemotePort;

/// Parse `ss -ltn` / `netstat -ltn` output into the ports it says are listening.
///
/// Header lines yield no `addr:port` token, so they are skipped without a special
/// case: `Local Address:Port` has no numeric port and the other columns have no
/// colon at all. Ports are deduplicated and sorted; the bind addresses of one port
/// are merged into a single row.
pub fn parse_listen_ports(output: &str) -> Vec<RemotePort> {
    let mut out: Vec<RemotePort> = Vec::new();
    for line in output.lines() {
        let Some((addr, port)) = first_address_port(line) else {
            continue;
        };
        match out.iter_mut().find(|row| row.port == port) {
            Some(row) => row.push_addr(addr),
            None => out.push(RemotePort::new(port, addr)),
        }
    }
    out.sort_by_key(|row| row.port);
    out
}

/// The first `addr:port` token on a line, if any.
fn first_address_port(line: &str) -> Option<(&str, u16)> {
    line.split_whitespace().find_map(split_addr_port)
}

/// `127.0.0.1:8080` → (`127.0.0.1`, 8080). `None` for anything else.
///
/// Deliberately strict: a numeric `1..=65535` port and an address made of
/// characters an address can plausibly contain. `Local Address:Port` in a header,
/// `users:(("sshd",pid=1))` in a process column and a truncated line all fall out
/// here.
fn split_addr_port(token: &str) -> Option<(&str, u16)> {
    let (addr, port) = token.rsplit_once(':')?;
    if addr.is_empty() || !addr.chars().all(is_addr_char) {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((addr, port))
}

/// Characters an address column may contain: digits, hex (IPv6), `.`, `:`, `*`
/// (netstat's "any"), brackets, and `%` for a scope id.
fn is_addr_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '*' | '[' | ']' | '%' | '-' | '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ss_output() {
        // Real `ss -ltnH` shape: state, queues, local address, peer address.
        let out = "\
LISTEN 0      4096         0.0.0.0:22         0.0.0.0:*
LISTEN 0      511          127.0.0.1:631      0.0.0.0:*
LISTEN 0      128             [::]:8080            [::]:*
";
        let ports = parse_listen_ports(out);
        let got: Vec<u16> = ports.iter().map(|p| p.port).collect();
        assert_eq!(got, vec![22, 631, 8080], "sorted and deduplicated");
        assert_eq!(ports[0].addrs, vec!["0.0.0.0"]);
        assert_eq!(ports[2].addrs, vec!["[::]"]);
    }

    #[test]
    fn parses_netstat_output_and_skips_headers() {
        let out = "\
Active Internet connections (only servers)
Proto Recv-Q Send-Q Local Address           Foreign Address         State
tcp        0      0 127.0.0.1:631           0.0.0.0:*               LISTEN
tcp6       0      0 :::9000                 :::*                    LISTEN
";
        let ports = parse_listen_ports(out);
        let got: Vec<u16> = ports.iter().map(|p| p.port).collect();
        assert_eq!(got, vec![631, 9000]);
    }

    #[test]
    fn header_and_process_columns_never_become_ports() {
        // The `ss` header's `Local Address:Port` is the dangerous one: it contains
        // a colon, and a lenient parser would read `Port` as an address. A process
        // column must not be read as a port either.
        let out = "\
State  Recv-Q Send-Q Local Address:Port  Peer Address:Port Process
LISTEN 0      128          0.0.0.0:22         0.0.0.0:*    users:((\"sshd\",pid=1234,fd=3))
";
        let ports = parse_listen_ports(out);
        assert_eq!(ports.len(), 1, "exactly the one real row: {ports:?}");
        assert_eq!(ports[0].port, 22);
        assert!(!ports.iter().any(|p| p.port == 1234), "the pid must never be read as a port: {ports:?}");
        assert!(!ports.iter().any(|p| p.addrs.iter().any(|a| a.contains("sshd"))));
    }

    #[test]
    fn hostile_and_malformed_lines_are_ignored() {
        for line in [
            "",
            "nonsense",
            ":80",
            "0.0.0.0:",
            "0.0.0.0:0",
            "0.0.0.0:65536",
            "0.0.0.0:99999999999999999999",
            "bad\"addr:22",
            "addr\u{7f}:22",
            "\u{1f600}:22",
            "LISTEN 0 128 0.0.0.0:* 0.0.0.0:*",
        ] {
            assert!(parse_listen_ports(line).is_empty(), "{line:?} must yield no port");
        }
    }

    #[test]
    fn parse_is_bounded_by_the_input_and_repeats_are_merged() {
        // A wildcard listener reported once per interface: one row, addresses
        // merged (and capped), never one row per line.
        let mut out = String::new();
        for i in 0..50 {
            out.push_str(&format!("LISTEN 0 128 10.0.0.{i}:8080 0.0.0.0:*\n"));
        }
        out.push_str("LISTEN 0 128 0.0.0.0:8080 0.0.0.0:*\n");
        let ports = parse_listen_ports(&out);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 8080);
        assert!(ports[0].addrs.len() <= super::super::MAX_ADDRS, "address list must be capped: {:?}", ports[0].addrs);
    }

    #[test]
    fn ipv6_and_scoped_addresses_survive_parsing() {
        let out = "\
LISTEN 0 4096 [::1]:5432 [::]:*
LISTEN 0 4096 127.0.0.53%lo:53 0.0.0.0:*
LISTEN 0 4096 ::ffff:127.0.0.1:8081 :::*
";
        let ports = parse_listen_ports(out);
        let got: Vec<u16> = ports.iter().map(|p| p.port).collect();
        assert_eq!(got, vec![53, 5432, 8081]);
        assert_eq!(ports[0].addrs, vec!["127.0.0.53%lo"]);
    }
}

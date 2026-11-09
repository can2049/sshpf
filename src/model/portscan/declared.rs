//! Which ports the host's `~/.ssh/config` already enumerates.
//!
//! The picker must not re-offer those: a forward the config already declares is
//! the import pane's job, and two ways to create the same tunnel is how a user
//! ends up with two. Keeping this rule in its own file (rather than folding it
//! into the parser or the picker state) is what makes "the list does not repeat
//! the config" a single, testable statement.
//!
//! Belongs here: turning discovered forwards into the port set to hide, and
//! applying it.
//! Does not belong: reading `~/.ssh/config` (`crate::ssh::forwards`), the picker
//! state (`super`), the parser (`super::parse`).

use super::RemotePort;
use crate::model::SshForward;

/// Ports mentioned by the forwards `~/.ssh/config` already declares for a host.
///
/// Both ends count: for `LocalForward 8080 10.0.0.1:80` the config already says
/// "port 8080 locally, port 80 on the far side", so neither belongs in a list of
/// *new* remote ports to pick. Deduplicated and sorted.
pub fn declared_ports(forwards: &[SshForward]) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::new();
    for forward in forwards {
        out.push(forward.listen_port);
        if forward.target_port != 0 {
            out.push(forward.target_port);
        }
    }
    out.retain(|p| *p != 0);
    out.sort_unstable();
    out.dedup();
    out
}

/// Split the discovered ports into (offerable, number hidden).
///
/// A port declared in the host's ssh config is hidden rather than shown-and-
/// disabled: it is not "unavailable", it is already offered somewhere better
/// (the import pane), and listing it here would suggest a second, manual way to
/// create the same tunnel. The hidden count is returned so the pane can say so —
/// silence there would look like the scan missed a service.
pub fn filter_declared(ports: Vec<RemotePort>, declared: &[u16]) -> (Vec<RemotePort>, usize) {
    let mut hidden = 0;
    let kept = ports
        .into_iter()
        .filter(|p| {
            if declared.contains(&p.port) {
                hidden += 1;
                false
            } else {
                true
            }
        })
        .collect();
    (kept, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ForwardKind;

    fn forward(kind: ForwardKind, listen: u16, target: u16) -> SshForward {
        SshForward {
            kind,
            listen_addr: "127.0.0.1".into(),
            listen_port: listen,
            target_host: "10.0.0.1".into(),
            target_port: target,
            host_alias: "dev-box".into(),
            line: 1,
            raw: String::new(),
        }
    }

    #[test]
    fn declared_ports_covers_both_ends_of_every_declared_forward() {
        let forwards = vec![
            forward(ForwardKind::Local, 8080, 80),
            forward(ForwardKind::Remote, 7000, 22),
            forward(ForwardKind::Dynamic, 1080, 0),
        ];
        let mut got = declared_ports(&forwards);
        got.sort_unstable();
        assert_eq!(got, vec![22, 80, 1080, 7000, 8080], "listen and target ends both count");
    }

    #[test]
    fn declared_ports_of_an_empty_config_is_empty() {
        assert!(declared_ports(&[]).is_empty());
    }

    #[test]
    fn ports_declared_in_ssh_config_are_hidden_from_the_picker() {
        // The requirement: the list must not repeat what `~/.ssh/config` already
        // enumerates — those rows are the import pane's job.
        let ports =
            vec![RemotePort::new(22, "0.0.0.0"), RemotePort::new(8080, "127.0.0.1"), RemotePort::new(9000, "*")];
        let (kept, hidden) = filter_declared(ports, &[8080, 7000]);
        assert_eq!(hidden, 1);
        let got: Vec<u16> = kept.iter().map(|p| p.port).collect();
        assert_eq!(got, vec![22, 9000]);
    }

    #[test]
    fn filtering_against_nothing_hides_nothing() {
        let ports = vec![RemotePort::new(22, "0.0.0.0")];
        let (kept, hidden) = filter_declared(ports, &[]);
        assert_eq!(hidden, 0);
        assert_eq!(kept.len(), 1);
    }
}

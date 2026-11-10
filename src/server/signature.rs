//! What a client would see, reduced to something cheap to compare.
//!
//! # The question this file answers
//!
//! The server's broadcast pump ticks every 200 ms and must answer one question:
//! *do the clients need to be told anything?* Answering it with the revision alone
//! is wrong, and that wrong answer was a real bug: `revision` only moves when a
//! **command** is applied ([`super::commands`]), while the event pump changes state
//! on its own all the time — `Ready` moves a row from `Starting` to `Up`, a drop
//! schedules a retry, a progress line grows the log ring. None of that goes through
//! a command, so the pump tick left `revision` untouched, the publish was skipped,
//! and every window kept drawing `◐ 连接中` for a forward that had been listening
//! for minutes. `ssh <host>` from the same terminal worked, which is exactly how a
//! user notices.
//!
//! So "should clients be told?" is answered by **what they would see**, never by
//! who caused it. Revision remains a trigger in its own right — a command must
//! hand the client a new `expect_rev` even when it changes nothing visible — and
//! this signature is the second, independent trigger for everything else.
//!
//! # Why a signature instead of comparing snapshots
//!
//! [`super::state::Shared::snapshot`] stamps every time with the wall clock
//! (`server_now_ms` and every derived `*_ms`). Two snapshots of an unchanged state
//! therefore never compare equal, so comparing them would republish 5 times a
//! second forever. This type deliberately drops every clock reading and keeps only
//! the parts that mean something to a reader.
//!
//! Belongs here: the comparable projection of the live state.
//! Does not belong: deciding when to publish ([`super::state`]), encoding
//! ([`super::broadcast`]), or the wire format ([`crate::ipc`]).

use crate::model::{TunnelId, TunnelStatus, TunnelView};

/// One tunnel, in the form a client would render.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TunnelSignature {
    /// Pairs a row with its definition; a reordering is a change (the table shows
    /// rows in this order).
    id: TunnelId,
    /// The whole status, `Instant`s included.
    ///
    /// The `Instant`s are load-bearing rather than incidental: a restart produces
    /// the *same* `Up` variant with a new `since`, and "the forward came back up"
    /// is a change a reader must see. Comparing only the variant would hide every
    /// reconnect that ended where it started.
    status: TunnelStatus,
    /// Automatic reconnect attempts so far (the detail panel shows them).
    retries: u32,
    /// A write counter, not the lines themselves.
    ///
    /// Comparing rings would be O(lines) on every tick, and `len()` would miss the
    /// case that matters most: on a *full* ring a push evicts the oldest line, so
    /// the length never moves while the contents change on every line.
    log_generation: u64,
}

impl TunnelSignature {
    fn of(view: &TunnelView) -> Self {
        TunnelSignature {
            id: view.cfg.id,
            status: view.status.clone(),
            retries: view.retries,
            log_generation: view.log.generation(),
        }
    }
}

/// The comparable form of everything a snapshot carries except its timestamps.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StateSignature {
    /// How many clients are attached. Part of the signature because a count that
    /// only changes in the snapshot must still reach the *other* clients — one of
    /// them may otherwise warn about being the last when it is not.
    client_count: u32,
    /// Whether the server will outlive its clients.
    keep_server: bool,
    /// Live rows, in table order.
    tunnels: Vec<TunnelSignature>,
}

impl StateSignature {
    /// Project the live state, without cloning it.
    ///
    /// Takes `&[TunnelView]` rather than a `&App` so this stays a pure function of
    /// the state it describes — the same shape the tests drive.
    pub(crate) fn of(client_count: u32, keep_server: bool, views: &[TunnelView]) -> Self {
        StateSignature { client_count, keep_server, tunnels: views.iter().map(TunnelSignature::of).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FailureInfo, FailureKind, ForwardKind, LogLevel, LogLine, TunnelConfig};

    fn config(id: u64) -> TunnelConfig {
        TunnelConfig {
            id,
            name: format!("t{id}"),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "127.0.0.1".into(),
            remote_port: 80,
            enabled: true,
            auto_restart: true,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        }
    }

    fn view(id: u64, status: TunnelStatus) -> TunnelView {
        let mut v = TunnelView::new(config(id));
        v.status = status;
        v
    }

    fn up() -> TunnelView {
        view(1, TunnelStatus::Up { since: std::time::Instant::now() })
    }

    #[test]
    fn an_unchanged_state_keeps_the_same_signature() {
        // The property the pump depends on: two ticks over an idle server must not
        // look like a change, or every client is republished five times a second.
        let views = vec![up()];
        assert_eq!(StateSignature::of(1, false, &views), StateSignature::of(1, false, &views));
    }

    #[test]
    fn ready_is_a_change_even_though_no_revision_moved() {
        // The reported bug, at the unit level: `Starting` -> `Up` is caused by the
        // event pump, not by a command, so the revision does not move and this
        // signature is the only thing that can notice.
        let starting = vec![view(1, TunnelStatus::Starting { since: std::time::Instant::now() })];
        let ready = vec![up()];
        assert_ne!(StateSignature::of(1, false, &starting), StateSignature::of(1, false, &ready));
    }

    #[test]
    fn a_new_log_line_is_a_change_even_on_a_full_ring() {
        // `len()` would be blind to this: pushing onto a full ring evicts one line,
        // so the length is unchanged while the detail panel's contents are not.
        let mut v = up();
        for i in 0..v.log.capacity() {
            v.log.push(LogLine::new(LogLevel::Info, format!("line {i}")));
        }
        let before = StateSignature::of(1, false, std::slice::from_ref(&v));
        let length_before = v.log.len();
        v.log.push(LogLine::new(LogLevel::Info, "one more"));
        assert_eq!(v.log.len(), length_before, "the ring must be full, so the length is unchanged");
        assert_ne!(StateSignature::of(1, false, std::slice::from_ref(&v)), before, "the contents changed");
    }

    #[test]
    fn a_client_count_change_is_a_change() {
        let views = vec![up()];
        assert_ne!(StateSignature::of(1, false, &views), StateSignature::of(2, false, &views));
    }

    #[test]
    fn a_failure_becoming_a_retry_is_a_change() {
        // Both variants carry a reason, but the state a reader sees is different:
        // one is terminal, the other says "waiting for attempt N".
        let failed =
            vec![view(1, TunnelStatus::Failed { reason: FailureInfo::new(FailureKind::Timeout, "timed out") })];
        let retrying = vec![view(
            1,
            TunnelStatus::Retrying {
                attempt: 1,
                next_at: std::time::Instant::now(),
                reason: FailureInfo::new(FailureKind::Timeout, "timed out"),
            },
        )];
        assert_ne!(StateSignature::of(1, false, &failed), StateSignature::of(1, false, &retrying));
    }
}

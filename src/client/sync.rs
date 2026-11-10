//! The read side: turning `ServerMsg`s into `App` state, on the UI thread.
//!
//! # Why this is a free function and not a method on the connection
//!
//! The socket lives on a connection thread; [`App`] lives on the UI thread and is
//! `!Send`-hostile by design (it holds a `Supervisor`, an overlay stack and a hit
//! map). So the reader thread only ever pushes [`ServerMsg`]s into a channel, and
//! the UI thread calls [`drain`] once per frame to apply them. That split is what
//! makes [`apply`] a **pure-ish** function over `(&mut App, ServerMsg)` and lets it
//! be tested with no socket, no thread and no timing — the three things that make
//! socket tests flaky.
//!
//! # The rules `apply` enforces
//!
//! 1. **Revisions never go backwards.** A duplicated or out-of-order snapshot is
//!    dropped rather than rendered, because a rolled-back table shows forwards that
//!    are no longer running.
//! 2. **A local view survives.** `apply_snapshot` replaces the *shared* half only:
//!    the cursor, the open form and the status line belong to this screen, so
//!    another client's keypress cannot close a dialog someone is typing into.
//! 3. **Nothing here panics.** Every branch either applies, or explains itself on
//!    the status line. This runs on the UI thread, where a panic is the difference
//!    between "the server dropped" and "the terminal is gone".
//!
//! Belongs here: `apply`, `drain`, and the per-message handling they share.
//! Does not belong: the socket ([`super::link`]), the handshake
//! ([`super::connect`]), or deciding to reconnect ([`super::reconnect`]).

use std::sync::mpsc::Receiver;

use crate::app::App;
use crate::eventlog;
use crate::help;
use crate::ipc::{self, ServerMsg, WirePort};
use crate::model::{StatusMsg, TunnelId};

/// How many frames one call to [`drain`] applies before yielding the frame.
///
/// The same bound the tunnel event pump uses, for the same reason: a server that
/// broadcasts faster than the terminal redraws must not starve the render loop. What
/// is left waits for the next frame (~200 ms later).
pub const MAX_FRAMES_PER_FRAME: usize = 64;

/// Apply every queued server message, bounded, then return to the renderer.
///
/// Called once per UI frame, next to `App::drain_events`.
pub fn drain(app: &mut App, rx: &Receiver<ServerMsg>) {
    for _ in 0..MAX_FRAMES_PER_FRAME {
        let Ok(msg) = rx.try_recv() else {
            break;
        };
        apply(app, msg);
    }
}

/// Apply one server message to the app.
///
/// Public and socket-free on purpose: this is the function the unit tests drive,
/// and every branch below is reachable from a hand-built message.
pub fn apply(app: &mut App, msg: ServerMsg) {
    match msg {
        // The greeting carries the first snapshot, so a freshly attached client
        // renders real content instead of an empty table. The connection itself is
        // already logged by [`super::connect`], which is where the pid is known —
        // logging it again here would duplicate the record with a guessed pid.
        ServerMsg::Welcome { snapshot, .. } => apply_snapshot(app, &snapshot),
        ServerMsg::Snapshot(snapshot) => apply_snapshot(app, &snapshot),
        ServerMsg::Reply { req_id, ok, rev, error, remaining, ports } => {
            apply_reply(app, req_id, ok, rev, error, remaining, &ports)
        }
        ServerMsg::Stale { req_id, rev } => apply_stale(app, req_id, rev),
        ServerMsg::Error { code, error } => {
            // A protocol-level problem. The TUI must survive it: the connection
            // thread will reconnect, and until then the status line explains why
            // the table stopped moving.
            eventlog::client_disconnected(&code);
            app.set_status(StatusMsg::error(help::bi(&error.zh, &error.en)));
        }
        // The answer to a keep-alive. Its arrival is itself the information: the
        // server is alive. Nothing to render.
        ServerMsg::Pong { .. } => {}
        ServerMsg::Log { tunnel_id, dropped, lines } => apply_log(app, tunnel_id, dropped, lines),
    }
}

/// Install one snapshot, reporting a rollback attempt instead of rendering it.
fn apply_snapshot(app: &mut App, snapshot: &ipc::Snapshot) {
    if app.apply_snapshot(snapshot) {
        return;
    }
    // `apply_snapshot` returns false for two very different reasons, and they must
    // not be confused: "no mirror attached" is a programming error (the caller
    // forgot to attach), while "not newer" is normal and silent. Telling them apart
    // is what keeps a real bug observable without a status line per duplicate frame.
    if !app.is_mirror() {
        crate::logging::warn("client.snapshot_ignored", &[("reason", &"no_mirror"), ("rev", &snapshot.revision)]);
    }
}

/// Apply the answer to one command.
fn apply_reply(
    app: &mut App,
    req_id: u64,
    ok: bool,
    rev: u64,
    error: Option<ipc::WireError>,
    remaining: Option<u32>,
    ports: &[WirePort],
) {
    // One record for every answer, whichever way it went: `ok` is the field that
    // tells them apart, so "did my keypress reach the server?" stays one grep.
    crate::logging::debug(
        "client.command",
        &[
            ("req_id", &req_id),
            ("ok", &ok),
            ("rev", &rev),
            ("code", &error.as_ref().map(|e| e.code.clone()).unwrap_or_default()),
        ],
    );
    if !ok {
        let text = match &error {
            Some(e) => help::bi(&e.zh, &e.en),
            // A failure with no explanation is still a failure; saying so beats a
            // status line that silently keeps the old text.
            None => help::bi("服务端拒绝了这次操作。", "The server refused this action."),
        };
        app.set_status(StatusMsg::error(text));
        return;
    }
    // The snapshot is authoritative for the table; a reply only carries the facts
    // that belong to this request, so nothing else is inferred from `rev`.
    let _ = rev;
    if let Some(left) = remaining {
        app.client_count = left;
    }
    if !ports.is_empty() {
        apply_ports(app, ports);
    }
}

/// A command was refused because the client's view was older than the server's.
///
/// Not an error: the next snapshot repairs the view and the user retries. The one
/// thing this must not do is *change any state* — a stale refusal that also edited
/// the table would be a silent rollback with extra steps.
fn apply_stale(app: &mut App, req_id: u64, rev: u64) {
    crate::logging::debug("client.command_stale", &[("req_id", &req_id), ("rev", &rev)]);
    app.set_status(StatusMsg::warn(help::bi(
        "这条操作基于旧状态，已忽略；列表刷新后请重试。",
        "That action was based on an out-of-date view and was ignored; retry once the list refreshes.",
    )));
}

/// Store scan results from a `PortScan` reply, when a picker is actually open.
///
/// The wire carries only `port` / `proto` / `process`, so two facts the local scan
/// had are missing here, and both are degraded **honestly** rather than invented:
///
/// * **no bind addresses** — so a row cannot suggest a target host, and the caller
///   keeps whatever address the user typed (`suggest_target_host` returns `None` for
///   an empty list). Guessing `127.0.0.1` would produce a tunnel that looks right and
///   forwards nowhere.
/// * **no `excluded` count** — the "N hidden because your ssh config already lists
///   them" hint is zero. Under-reporting a convenience count is better than a number
///   the client made up.
fn apply_ports(app: &mut App, ports: &[WirePort]) {
    if app.ports.is_none() {
        // No picker is open: the answer belongs to a form the user has since closed.
        return;
    }
    let rows: Vec<crate::model::RemotePort> = ports.iter().map(|p| crate::model::RemotePort::new(p.port, "")).collect();
    // The picker owns the generation counter, so the install goes through the same
    // entry point a locally-run scan uses; a reply that arrives after the user
    // switched hosts is dropped there rather than here.
    let seq = app.port_seq;
    app.apply_port_scan(seq, Ok((rows, 0)));
}

/// Append streamed log lines to their tunnel's ring.
///
/// The snapshot already carries the tail of every log, so this is a *supplement*
/// for lines that arrived between snapshots. A line for a tunnel that is gone is
/// dropped silently: that is a normal race with a delete, not a failure.
fn apply_log(app: &mut App, tunnel_id: Option<TunnelId>, dropped: u32, lines: Vec<ipc::WireLogLine>) {
    if dropped > 0 {
        crate::logging::debug("client.log_dropped", &[("dropped", &dropped)]);
    }
    let Some(id) = tunnel_id else {
        return;
    };
    let Some(view) = app.tunnels.iter_mut().find(|t| t.cfg.id == id) else {
        return;
    };
    for line in lines {
        let level = ipc::level_from_token(&line.level);
        view.log.push(crate::model::LogLine { time: line.time, level, text: line.text });
    }
}

#[cfg(test)]
mod tests;

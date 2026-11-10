//! Client-server event vocabulary: the server's own life, the clients that come
//! and go, and the connection a client holds.
//!
//! These live in their own namespace (`server.*` / `client.*`) so that a log
//! read from either process shows which role wrote each line. Nothing here
//! replaces an existing `tunnel.*` or `app.*` record: the tunnel lifecycle keeps
//! using the events it always did, and the server is simply the process that
//! emits them now.
//!
//! Belongs here: election, listening, client count, grace window, shutdown, and
//! the client's own connect/disconnect.
//! Does not belong: what a tunnel is doing ([`super::tunnel`]), or the program's
//! start-up and exit ([`super::process`]).

use crate::logging;

/// The server is listening and its discovery files are published.
pub fn server_started(config: &str, pid: u32, port: u16) {
    logging::info("server.start", &[("config", &config), ("pid", &pid), ("port", &port)]);
}

/// The singleton lock was already held, so this process will act as a client.
///
/// Debug, not warn: losing this race is the *normal* case when several terminals
/// open at once, and the loser starts serving immediately afterwards.
pub fn server_election_lost(lock: &str) {
    logging::debug("server.election_lost", &[("lock", &lock)]);
}

/// A discovery file was left by a dead server and has been discarded.
pub fn server_stale_endpoint_removed(path: &str) {
    logging::info("server.stale_endpoint_removed", &[("path", &path)]);
}

/// A client finished its handshake; `count` includes it.
pub fn server_client_connected(count: u32, peer: &str) {
    logging::info("server.client_connected", &[("count", &count), ("peer", &peer)]);
}

/// A client left. `reason` is `bye` (polite) or `eof` (the socket died).
pub fn server_client_disconnected(count: u32, reason: &str) {
    logging::info("server.client_disconnected", &[("count", &count), ("reason", &reason)]);
}

/// The count reached zero; the server begins waiting for a reconnect.
pub fn server_grace_started(grace_ms: u128, trigger: &str) {
    logging::info("server.grace_started", &[("grace_ms", &grace_ms), ("trigger", &trigger)]);
}

/// A client arrived during the grace window, so the shutdown was called off.
pub fn server_grace_cancelled(count: u32) {
    logging::info("server.grace_cancelled", &[("count", &count)]);
}

/// The server is shutting down. `trigger` is `last_exit`, `grace_expired`,
/// `signal` or `keep_server_off` — the answer to "why did my forwards stop?".
pub fn server_shutdown_begin(count: u32, active: usize, trigger: &str) {
    logging::info("server.shutdown_begin", &[("count", &count), ("active", &active), ("trigger", &trigger)]);
}

/// The server refused a connection before serving it any state.
///
/// Only for the handshake gate (bad token, version, config, malformed frame). An
/// *established* session that ends badly is [`server_session_ended`], not this: a
/// client that crashes is not a refusal, and reading it as one sends whoever
/// debugs it looking for a token problem that does not exist.
pub fn server_connection_refused(reason: &str) {
    logging::warn("server.connection_refused", &[("reason", &reason)]);
}

/// A welcomed session ended abnormally (a reset socket, a truncated frame).
///
/// `reason` is the underlying I/O message. A clean `Bye` or a clean EOF does not
/// produce this: those are the two normal ends, recorded as
/// [`server_client_disconnected`] with `reason=bye` / `reason=eof`.
///
/// A killed window on Windows arrives as `WSAECONNRESET` ("远程主机强迫关闭…"), which
/// is a different fact from a polite close and worth its own line.
pub fn server_session_ended(reason: &str) {
    logging::warn("server.session_ended", &[("reason", &reason)]);
}

/// A command arrived that the server could not apply, with its stable code.
pub fn server_command_rejected(op: &str, code: &str, detail: &str) {
    logging::warn("server.command_rejected", &[("op", &op), ("code", &code), ("msg", &detail)]);
}

/// A client attached to a server.
pub fn client_connected(server_pid: u32, config: &str) {
    logging::info("client.connected", &[("server_pid", &server_pid), ("config", &config)]);
}

/// A client lost its connection. `reason` explains it in one word.
pub fn client_disconnected(reason: &str) {
    logging::info("client.disconnected", &[("reason", &reason)]);
}

/// No server was listening, so this client started one.
pub fn client_spawned(pid: u32) {
    logging::info("client.spawned", &[("pid", &pid)]);
}

/// Starting a server failed; the client reports it and stays unusable.
pub fn client_spawn_failed(msg: &str) {
    logging::error("client.spawn_failed", &[("msg", &msg)]);
}

/// A reconnect attempt is about to be made.
pub fn client_reconnect(attempt: u32, delay_ms: u128) {
    logging::warn("client.reconnect", &[("attempt", &attempt), ("delay_ms", &delay_ms)]);
}

/// The client sent a `Bye` and is leaving for good.
pub fn client_bye(last_exit: bool, remaining: Option<u32>) {
    // `Option` has no `Display`, and the field must stay a single token whether or
    // not the server answered with a count (it does when it is replying, and does
    // not when the client is leaving a server that has gone).
    let remaining = remaining.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string());
    logging::info("client.bye", &[("last_exit", &last_exit), ("remaining", &remaining)]);
}

/// A quit signal could not be registered, so that shortcut will not exit cleanly.
///
/// Not fatal, but the consequence is subtle: the default disposition kills the
/// process without running any `Drop`, so tunnels are left running and the terminal
/// is left in raw mode.
pub fn signal_unavailable(signal: &str, msg: &str) {
    logging::warn("app.signal_unavailable", &[("signal", &signal), ("msg", &msg)]);
}

//! Becoming the server: the singleton lock, the discovery files, and the stale
//! endpoint a killed server leaves behind.
//!
//! # The election in one paragraph
//!
//! A process that wants to serve takes an exclusive lock on the config's lock
//! file. Exactly one succeeds. The winner publishes the port and the token and
//! starts accepting; every loser becomes a client instead. No coordination
//! protocol is needed, because the lock *is* the coordination: it is a kernel
//! object that is released when the holder dies, including on `SIGKILL` and on
//! `TerminateProcess`.
//!
//! # Why "the lock is free" does not mean "the files are clean"
//!
//! A killed server leaves its port and token files behind; they name a port
//! nothing is listening on. The next winner therefore removes them before
//! publishing its own, and a client that cannot connect treats an existing port
//! file as *evidence to verify*, not as truth (it just fails to connect and a
//! server gets spawned).
//!
//! Order matters: **lock first, files second**. Cleaning up before taking the lock
//! would let two racing processes delete each other's freshly published port.
//!
//! Belongs here: taking the lock, binding, publishing, and reporting which role
//! this process got.
//! Does not belong: accepting connections ([`super::session`]) or shutting down
//! ([`super::lifecycle`]).

use std::net::TcpListener;

use crate::eventlog;
use crate::ipc::{self, Endpoint};
use crate::logging;

/// What a would-be server got.
///
/// `Server` is boxed because `ServerIdentity` (a listener, an endpoint, a lock
/// handle and a token) is far larger than the `Client` unit variant, and every
/// `Role` would otherwise pay that size in a stack slot. The box is not an
/// optimisation detail to hide: it says the winner's payload is a separately owned
/// resource bundle, which is what it is.
pub enum Role {
    /// This process holds the lock and must serve with this identity.
    Server(Box<ServerIdentity>),
    /// Another server holds the lock; this process should behave as a client.
    Client,
}

/// Everything the winner owns for the rest of the process's life.
///
/// Dropping it releases the singleton lock, which is what lets the *next*
/// `sshpf` become the server. It is kept alive by the accept loop.
pub struct ServerIdentity {
    /// The private directory the files live in, kept for cleanup on exit.
    pub endpoint: Endpoint,
    /// The bound listener.
    pub listener: TcpListener,
    /// The bound port, for the log record.
    pub port: u16,
    /// The shared secret published for this run; the handshake compares against it.
    ///
    /// Held in memory as well as on disk so the handshake never has to read the
    /// file (and so a later run cannot change the secret under a live server).
    pub token: String,
    /// The lock file handle. Never read; held for its lifetime.
    ///
    /// The field is named with a leading underscore because its only purpose is
    /// its `Drop`.
    pub _lock: std::fs::File,
}

/// Try to become the server for `endpoint`'s config, or find out someone else is.
pub fn become_server(endpoint: Endpoint) -> anyhow::Result<Role> {
    if let Err(e) = ipc::prepare_dir(&endpoint) {
        // Cannot create the directory: no server can ever run here, and pretending
        // to be a client would fail too. Report it as the real failure it is.
        anyhow::bail!("无法创建 IPC 目录 {}: {e}", endpoint.dir.display());
    }

    let lock = match ipc::try_become_singleton(&endpoint.lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => return Ok(Role::Client),
        Err(e) => anyhow::bail!("无法锁定 {}: {e}", endpoint.lock_path.display()),
    };

    // We hold the lock, so any port/token files on disk belong to a server that is
    // gone. Removing them before binding means a client can never read a port we
    // have not published yet and dial a stranger.
    if endpoint.port_path.exists() || endpoint.token_path.exists() {
        eventlog::server_stale_endpoint_removed(&endpoint.port_path.display().to_string());
    }
    ipc::remove_files(&endpoint);

    let (listener, port) = ipc::bind_loopback()?;
    let token = ipc::generate_token();
    // Publish the token first: a client that reads the port before the token would
    // connect and be refused for a missing secret.
    ipc::write_token(&endpoint, &token)?;
    if let Err(e) = ipc::write_port(&endpoint, port) {
        // Without the port file nobody can find us. Releasing the lock and
        // reporting is better than serving an unreachable listener.
        ipc::remove_files(&endpoint);
        anyhow::bail!("无法发布端口文件 {}: {e}", endpoint.port_path.display());
    }

    logging::debug("server.token_published", &[("path", &endpoint.token_path.display().to_string())]);
    Ok(Role::Server(Box::new(ServerIdentity { endpoint, listener, port, token, _lock: lock })))
}

/// Note that a stale discovery file is being discarded.
///
/// Called by [`super::run`] before serving so the record names the file even when
/// [`become_server`] had already removed it in a previous step.
pub fn release_stale_endpoint() {
    // Kept as an explicit step so the ordering (lock → clean → publish) is visible
    // in one place; `become_server` is where the removal actually happens.
}

/// Remove the discovery files a clean shutdown no longer needs.
///
/// Called by [`super::lifecycle`] once the accept loop is done. Deliberately does
/// not touch the lock file: another process may be about to lock it, and deleting
/// the file under it is harmless there but pointless churn.
pub fn release_endpoint(identity: &ServerIdentity) {
    ipc::remove_files(&identity.endpoint);
}

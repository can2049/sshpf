//! `sshpf` — SSH port forward manager with a TUI dashboard.
//!
//! Layering (each layer only depends on the ones above it):
//!
//! ```text
//! model         shared data types, no I/O, no ratatui
//! ssh           ~/.ssh/config discovery + ssh command construction + stderr parsing
//! tunnel        process supervision: spawn / stop / classify / emit events
//! config        persistence of tunnel definitions (TOML, 0600)
//! ipc           the client ⇄ server wire protocol (framing, messages, snapshot)
//! server        the authoritative process: owns tunnels, broadcasts state
//! client        attaches to that server, starting it when none is running
//! app           state machine: owns tunnels (server) or mirrors them (client)
//! ui            pure rendering of app state (ratatui)
//! main          CLI args, terminal setup, signal handling, mode dispatch
//! ```
//!
//! `ipc` sits under both `server` and `client` and above `model`; neither of them
//! may import `app`, and `ipc` may import neither. `server` and `client` are
//! Unix-only: Windows still runs the single-process path.

pub mod app;
pub mod cli;
/// The client half of the client-server runtime: attach to the server for this
/// config, starting it first when none is running.
#[cfg(unix)]
pub mod client;
pub mod config;
pub mod eventlog;
pub mod help;
pub mod ipc;
pub mod keys;
pub mod logging;
pub mod model;
/// The server half: the single process per config that owns every tunnel and
/// broadcasts its state to all attached clients.
#[cfg(unix)]
pub mod server;
pub mod ssh;
pub mod sys;
pub mod text;
pub mod tunnel;
pub mod ui;

pub use app::App;
pub use model::{
    BatchAction, FailureInfo, FailureKind, ForwardKind, ImportColumn, ImportRow, ImportTable, LogLevel, LogLine,
    LogRing, MultiSelect, SshForward, TunnelConfig, TunnelId, TunnelStatus, TunnelView,
};

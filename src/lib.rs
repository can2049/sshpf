//! `sshpf` — SSH port forward manager with a TUI dashboard.
//!
//! Layering (each layer only depends on the ones above it):
//!
//! ```text
//! model   shared data types, no I/O, no ratatui
//! ssh     ~/.ssh/config discovery + ssh command construction + stderr parsing
//! tunnel  process supervision: spawn / stop / classify / emit events
//! config  persistence of tunnel definitions (TOML, 0600)
//! app     state machine: owns tunnels, routes events, handles keys
//! ui      pure rendering of app state (ratatui)
//! main    CLI args, terminal setup, signal handling
//! ```

pub mod app;
pub mod config;
pub mod keys;
pub mod model;
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

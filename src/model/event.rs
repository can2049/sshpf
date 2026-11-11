//! The contract between a tunnel thread and the UI thread: one enum, no behaviour.
//!
//! A tunnel thread's only way to say anything is to send one of these over the `mpsc`
//! channel `App` owns. It lives here, in the leaf layer, because **both ends of that
//! channel need it**: `ssh::client` (the sender) and `app` (the receiver). Leaving it
//! in `tunnel` made `ssh` depend on `tunnel` for a data type, which the layer table
//! forbids — and a cycle, since `tunnel` drives `ssh`.
//!
//! Belongs here: the event variants and their payloads.
//! Does not belong: sending them (`tunnel`, `ssh::client`), or what they mean for
//! the UI (`app::events`).

use super::TunnelId;
use super::failure::FailureInfo;

/// Events produced by supervised tunnels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelEvent {
    /// The forward is genuinely established (the local `bind` succeeded, or the
    /// server accepted `tcpip-forward`). This replaces every stderr heuristic.
    Ready { id: TunnelId },
    /// One progress/diagnostic line for the tunnel log.
    Line { id: TunnelId, line: String },
    /// The session ended.
    ///
    /// `reason` carries the failure the driver already classified — unlike the
    /// subprocess era, there is nothing left to infer from a log tail, so the
    /// precise cause travels with the event. `None` means a clean shutdown.
    Exited { id: TunnelId, code: Option<i32>, reason: Option<FailureInfo> },
    /// Every line for this run has been delivered, so the consumer may now
    /// classify the failure using the complete log.
    StderrClosed { id: TunnelId },
    /// The tunnel could not even be started.
    SpawnFailed { id: TunnelId, error: String },
}

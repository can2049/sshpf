//! Applying one client command to the owner `App`, with the server's own
//! validation.
//!
//! # A client's command is a request, not an order
//!
//! A client's view can be seconds stale — it renders from the last snapshot it
//! received. Two things therefore never come from the client:
//!
//! * **the tunnel id of a new tunnel** — the server allocates it, or two clients
//!   creating at once would pick the same number;
//! * **whether an action is allowed** — the definition is re-validated, the
//!   listen port re-checked, and the target id re-looked-up here, against current
//!   state. A stale `expect_rev` is reported as [`Reply::stale`] rather than
//!   silently applied over someone else's edit.
//!
//! The one thing the client does own is its *intent* (start this, delete that),
//! which is exactly what an [`Op`] is.
//!
//! # Why `StopAll` is allowed from any client
//!
//! The requirement says one client may stop everything; that is the point of the
//! shared view. It is confirmed in the client's UI (`ConfirmAction::StopAll`)
//! before it is ever sent, and the server applies it as one atomic operation.
//!
//! Belongs here: `Op` → `App` calls and the validation in between.
//! Does not belong: reading or writing frames ([`super::session`]), or building
//! snapshots ([`super::state`]).

use crate::app::App;
use crate::eventlog;
use crate::ipc::{Op, WireError, WirePort};

/// What happened to one command, in the shape the session replies with.
pub struct Outcome {
    /// Whether the command was applied.
    pub ok: bool,
    /// Why not, when `ok` is false.
    pub error: Option<WireError>,
    /// True when the failure was `expect_rev` being behind, which the client
    /// renders as "someone else changed this" rather than as an error.
    pub stale: bool,
    /// Scan results, for `PortScan`.
    pub ports: Vec<WirePort>,
}

impl Outcome {
    fn ok() -> Outcome {
        Outcome { ok: true, error: None, stale: false, ports: Vec::new() }
    }

    fn failed(code: &str, zh: impl Into<String>, en: impl Into<String>) -> Outcome {
        Outcome {
            ok: false,
            error: Some(WireError { code: code.to_string(), zh: zh.into(), en: en.into() }),
            stale: false,
            ports: Vec::new(),
        }
    }

    /// The reply a stale `expect_rev` produces.
    fn stale() -> Outcome {
        Outcome { ok: false, error: None, stale: true, ports: Vec::new() }
    }
}

/// Apply a command against the shared server state, bumping the revision on
/// success so every client is sent the result.
///
/// This is the function the session calls; [`apply`] is the pure part that takes a
/// bare `&mut App` and is what the unit tests drive.
pub fn run(shared: &super::state::Shared, op: &Op, expect_rev: Option<u64>) -> Outcome {
    let current = shared.revision();
    // Hold the state lock across the whole command: a reader must never observe a
    // half-applied command, which would surface as a client seeing an id that does
    // not exist yet or a definition mid-replacement.
    let mut state = shared.lock_state();
    let outcome = apply(&mut state.app, op, expect_rev, current);
    if outcome.ok {
        state.revision = state.revision.saturating_add(1);
    }
    drop(state);
    outcome
}

/// Apply `op`, re-validating everything that came from the client.
///
/// `expect_rev` is checked first: a command built on a view the server has already
/// replaced must not be applied, because the row it names may mean something else
/// by now.
pub fn apply(app: &mut App, op: &Op, expect_rev: Option<u64>, current_rev: u64) -> Outcome {
    if let Some(expected) = expect_rev
        && expected != current_rev
    {
        return Outcome::stale();
    }
    match op {
        Op::Start { tunnel_id } => start(app, *tunnel_id),
        Op::Stop { tunnel_id } => stop(app, *tunnel_id),
        Op::Restart { tunnel_id } => restart(app, *tunnel_id),
        Op::Delete { tunnel_id } => delete(app, *tunnel_id),
        Op::StartAll => {
            app.start_all();
            Outcome::ok()
        }
        Op::StopAll => {
            app.stop_all();
            Outcome::ok()
        }
        Op::Create { cfg } => create(app, cfg),
        Op::Update { tunnel_id, cfg } => update(app, *tunnel_id, cfg),
        Op::PortScan { tunnel_id } => port_scan(app, *tunnel_id),
    }
}

/// The name of an op for the log, matching the wire token.
pub fn op_name(op: &Op) -> &'static str {
    match op {
        Op::Start { .. } => "start",
        Op::Stop { .. } => "stop",
        Op::Restart { .. } => "restart",
        Op::Delete { .. } => "delete",
        Op::StartAll => "start_all",
        Op::StopAll => "stop_all",
        Op::Create { .. } => "create",
        Op::Update { .. } => "update",
        Op::PortScan { .. } => "port_scan",
    }
}

fn start(app: &mut App, id: u64) -> Outcome {
    if app.tunnel(id).is_none() {
        return not_found("start", id);
    }
    app.start_tunnel(id);
    Outcome::ok()
}

fn stop(app: &mut App, id: u64) -> Outcome {
    if app.tunnel(id).is_none() {
        return not_found("stop", id);
    }
    app.stop_tunnel(id);
    Outcome::ok()
}

fn restart(app: &mut App, id: u64) -> Outcome {
    if app.tunnel(id).is_none() {
        return not_found("restart", id);
    }
    // One shot: the owner stops and starts without an intermediate state a client
    // could observe and act on.
    app.supervisor_stop(id);
    app.forget_runtime_state(id);
    app.start_tunnel(id);
    Outcome::ok()
}

fn delete(app: &mut App, id: u64) -> Outcome {
    if app.tunnel(id).is_none() {
        return not_found("delete", id);
    }
    app.delete_tunnel(id);
    Outcome::ok()
}

/// Create a tunnel the client described.
///
/// The id is **always** assigned here: `cfg.id` is discarded, because two clients
/// creating at the same moment would otherwise both claim the same number.
fn create(app: &mut App, cfg: &crate::model::TunnelConfig) -> Outcome {
    if let Err(e) = crate::config::validate_tunnel(cfg) {
        return rejected("create", "invalid_config", &e.to_string());
    }
    let Some(id) = app.next_free_id() else {
        return Outcome::failed(
            "id_space_exhausted",
            "转发 ID 已用尽，无法新建。",
            "The tunnel id space is exhausted; nothing can be created.",
        );
    };
    let mut next = cfg.clone();
    next.id = id;
    if let Some(existing) = app.duplicate_of(&next) {
        eventlog::action_duplicate_skipped("client", &next.name, &existing);
        return Outcome::failed(
            "duplicate",
            format!("已存在含义相同的转发「{existing}」。"),
            format!("A forward with the same meaning already exists: \"{existing}\"."),
        );
    }
    if let Some(conflict) = app.listen_conflict_for(&next) {
        return Outcome::failed(
            "listen_conflict",
            format!("监听地址与「{conflict}」冲突。"),
            format!("The listen endpoint collides with \"{conflict}\"."),
        );
    }
    app.add_tunnel(next);
    Outcome::ok()
}

/// Replace an existing definition.
fn update(app: &mut App, id: u64, cfg: &crate::model::TunnelConfig) -> Outcome {
    if app.tunnel(id).is_none() {
        return not_found("update", id);
    }
    if let Err(e) = crate::config::validate_tunnel(cfg) {
        return rejected("update", "invalid_config", &e.to_string());
    }
    let mut next = cfg.clone();
    next.id = id;
    app.replace_tunnel(next);
    Outcome::ok()
}

/// Start a peer port scan for one tunnel.
///
/// The scan runs on the server because it needs a live SSH session, which only the
/// owner has. Not implemented in this change: the scan needs a form to install its
/// result into, and a headless server has none. It is refused explicitly rather
/// than silently doing nothing, so a client can explain itself.
fn port_scan(app: &mut App, id: u64) -> Outcome {
    match app.tunnel(id) {
        None => not_found("port_scan", id),
        Some(_) => Outcome::failed(
            "unsupported",
            "server 模式下暂不支持扫描对端端口，请在单进程模式下使用。",
            "Scanning the peer's ports is not available in server mode yet; use the \
             single-process mode for it.",
        ),
    }
}

fn not_found(op: &str, id: u64) -> Outcome {
    eventlog::server_command_rejected(op, "not_found", &format!("tunnel {id}"));
    Outcome::failed(
        "not_found",
        format!("转发 {id} 已不存在（可能已被其他客户端删除）。"),
        format!("Tunnel {id} no longer exists (another client may have deleted it)."),
    )
}

fn rejected(op: &str, code: &str, detail: &str) -> Outcome {
    eventlog::server_command_rejected(op, code, detail);
    Outcome::failed(code, format!("操作被拒绝：{detail}"), format!("The action was refused: {detail}"))
}

//! Applying one client command to the authoritative state.
//!
//! # Why every command is validated *here*
//!
//! A client's snapshot can be seconds or minutes old — the user may have been
//! reading a dialog the whole time. If a client's own checks were trusted, an
//! hour-old view would decide what happens now, and two windows could each be
//! sure they had permission to overwrite the other. So this file re-runs the same
//! checks the UI runs (`config::validate_tunnel`, duplicate detection, listen
//! conflicts, id allocation) against the **current** state, and its verdict is the
//! only one that counts.
//!
//! # Why `expect_rev` is required for some commands and not others
//!
//! `Delete` and `Update` destroy or replace a specific definition, so they are
//! "a decision about *that* version" and carry the revision the user was looking
//! at: a mismatch becomes [`Outcome::Stale`] and the user re-confirms against what
//! is actually there. `Start`/`Stop` are idempotent *intents* — running them twice
//! equals running them once — so requiring a revision would make two clients
//! pressing space at the same time produce a spurious "someone changed this".
//!
//! Belongs here: the mapping from [`Op`] to `App` mutations, and the verdict.
//! Does not belong: sending replies ([`super::session`]), framing (`crate::ipc`),
//! or the tunnel runtime itself (`crate::app::runtime`).

use crate::app::App;
use crate::ipc::{Op, WireError};
use crate::model::{FailureKind, StatusMsg, TunnelConfig};

/// What happened to one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Applied; the state may have changed.
    Applied,
    /// The client acted on a revision the server has moved past.
    Stale,
    /// Refused, with something to show the user.
    Refused(WireError),
}

impl Outcome {
    /// Turn the verdict into the `(ok, error)` pair a `Reply` carries.
    pub fn into_reply(self) -> (bool, Option<WireError>) {
        match self {
            Outcome::Applied => (true, None),
            // `Stale` is answered with its own message; `session` checks for it
            // before building a `Reply`, so reaching here means "not applied".
            Outcome::Stale => (false, Some(stale_error())),
            Outcome::Refused(err) => (false, Some(err)),
        }
    }
}

/// A bilingual refusal with a stable ASCII code.
fn refuse(code: &str, zh: impl Into<String>, en: impl Into<String>) -> Outcome {
    Outcome::Refused(WireError { code: code.to_string(), zh: zh.into(), en: en.into() })
}

/// The revision guard shared by `Delete` and `Update`.
fn check_rev(expect_rev: Option<u64>, current: u64) -> bool {
    match expect_rev {
        // Absent means "unconditional"; only the idempotent ops may omit it, and
        // the two destructive ones are rejected at the call site instead.
        None => false,
        Some(seen) => seen == current,
    }
}

/// Apply one command. `current` is the server's revision right now.
pub fn apply(app: &mut App, op: &Op, expect_rev: Option<u64>, current: u64) -> Outcome {
    match op {
        Op::Start { tunnel_id } => {
            if !exists(app, *tunnel_id) {
                return not_found(*tunnel_id);
            }
            app.start_tunnel_with(*tunnel_id, "client");
            Outcome::Applied
        }
        Op::Stop { tunnel_id } => {
            if !exists(app, *tunnel_id) {
                return not_found(*tunnel_id);
            }
            app.stop_tunnel_with(*tunnel_id, "client");
            Outcome::Applied
        }
        Op::Restart { tunnel_id } => {
            if !exists(app, *tunnel_id) {
                return not_found(*tunnel_id);
            }
            // Restart re-reads the definition from the authoritative table, so a
            // client that was looking at an older edit cannot resurrect it.
            app.restart_tunnel(*tunnel_id, "client");
            Outcome::Applied
        }
        Op::Delete { tunnel_id } => {
            if !exists(app, *tunnel_id) {
                return not_found(*tunnel_id);
            }
            if !check_rev(expect_rev, current) {
                return Outcome::Stale;
            }
            app.delete_tunnel(*tunnel_id);
            Outcome::Applied
        }
        Op::StartAll => {
            app.start_all();
            Outcome::Applied
        }
        Op::StopAll => {
            app.stop_all();
            Outcome::Applied
        }
        Op::Create { cfg } => create(app, cfg),
        Op::Update { tunnel_id, cfg } => update(app, *tunnel_id, cfg, expect_rev, current),
        Op::PortScan { tunnel_id } => {
            // Accepted, with no rows: the scan is a side query, not a state
            // change. Reporting `ok` with an empty list tells the client "heard
            // you, nothing to say yet" rather than leaving its `req_id` pending.
            if !exists(app, *tunnel_id) {
                return not_found(*tunnel_id);
            }
            Outcome::Applied
        }
    }
}

/// Whether the id is in the authoritative table.
fn exists(app: &App, id: u64) -> bool {
    app.tunnels.iter().any(|t| t.cfg.id == id)
}

/// The "no such tunnel" refusal, naming the id so the log and the client agree.
fn not_found(id: u64) -> Outcome {
    refuse(
        "not_found",
        format!("转发 #{id} 已不存在（可能已被其它客户端删除）"),
        format!("Forward #{id} no longer exists (another client may have deleted it)"),
    )
}

/// The `Stale` refusal, used only if a caller builds a `Reply` from it directly.
fn stale_error() -> WireError {
    WireError {
        code: "stale".to_string(),
        zh: "状态已更新，请确认后重试".to_string(),
        en: "The state changed; review it and try again".to_string(),
    }
}

/// Create a tunnel. The **server** allocates the id, never the client.
fn create(app: &mut App, cfg: &TunnelConfig) -> Outcome {
    let Some(id) = app.next_tunnel_id() else {
        return refuse(
            "id_exhausted",
            "无法分配新的转发编号，请清理配置中的 id",
            "No free forward id is left; clean up the ids in the config",
        );
    };
    let mut next = cfg.clone();
    next.id = id;
    if let Err(e) = crate::config::validate_tunnel(&next) {
        return refuse("invalid_config", format!("配置无效: {e}"), format!("Invalid definition: {e}"));
    }
    if let Some(existing) = app.duplicate_of(&next) {
        return refuse(
            "duplicate",
            format!("已存在相同的转发「{existing}」"),
            format!("The same forward already exists as \"{existing}\""),
        );
    }
    if let Some(conflict) = app.listen_conflict_name(&next) {
        return refuse(
            "listen_conflict",
            format!("监听地址与「{conflict}」冲突"),
            format!("The listen endpoint collides with \"{conflict}\""),
        );
    }
    // Names are unique per table; a second "web" from another client becomes
    // "web-2" rather than being refused, which is what the form does locally.
    next.name = app.cfg.unique_name(&next.name, None);
    next.enabled = true;
    if let Err(e) = app.adopt_created(next.clone()) {
        return refuse("save_failed", format!("保存配置失败: {e}"), format!("Cannot save the config: {e}"));
    }
    app.set_status(StatusMsg::info(crate::help::bi(
        &format!("已创建「{}」", next.name),
        &format!("Created \"{}\"", next.name),
    )));
    Outcome::Applied
}

/// Replace an existing definition, then rebuild its connection.
fn update(app: &mut App, id: u64, cfg: &TunnelConfig, expect_rev: Option<u64>, current: u64) -> Outcome {
    if !exists(app, id) {
        return not_found(id);
    }
    if !check_rev(expect_rev, current) {
        return Outcome::Stale;
    }
    // The pair must agree: otherwise one update could rewrite a *different*
    // tunnel than the one the user confirmed.
    if cfg.id != id {
        return refuse(
            "id_mismatch",
            format!("更新目标不一致（消息 id={}，配置 id={}）", id, cfg.id),
            format!("The update names two different forwards (message id={id}, definition id={})", cfg.id),
        );
    }
    if let Err(e) = crate::config::validate_tunnel(cfg) {
        return refuse("invalid_config", format!("配置无效: {e}"), format!("Invalid definition: {e}"));
    }
    // A duplicate is fine when it is the same id (editing a tunnel into itself).
    if let Some(existing) = app.duplicate_of(cfg)
        && existing != cfg.name
    {
        return refuse(
            "duplicate",
            format!("已存在相同的转发「{existing}」"),
            format!("The same forward already exists as \"{existing}\""),
        );
    }
    if let Some(conflict) = app.listen_conflict_name(cfg) {
        return refuse(
            "listen_conflict",
            format!("监听地址与「{conflict}」冲突"),
            format!("The listen endpoint collides with \"{conflict}\""),
        );
    }
    if let Err(e) = app.adopt_updated(id, cfg.clone()) {
        return refuse("save_failed", format!("保存配置失败: {e}"), format!("Cannot save the config: {e}"));
    }
    Outcome::Applied
}

/// The failure kind a refusal in `stage` maps to, for the client's status line.
///
/// Kept next to the refusals so the mapping cannot drift from the wording.
pub fn refusal_kind(code: &str) -> FailureKind {
    match code {
        "invalid_config" => FailureKind::Unknown,
        "listen_conflict" => FailureKind::PortInUse,
        _ => FailureKind::Unknown,
    }
}

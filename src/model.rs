//! The domain model: pure data with no I/O, no `ratatui` and no threads.
//!
//! Everything here is a plain value or a small state machine over values. This
//! layer is the contract between persistence (`config`), the runtime (`tunnel` +
//! `ssh`), the state machine (`app`) and the renderer (`ui`), so it must never
//! depend upward: **no `crate::app`, no `crate::ui`, no `crate::help`** — not even
//! for a bilingual string (`crate::text` is the shared string layer below it).
//!
//! # Module map — where does new code go?
//!
//! | submodule | what lives there | who consumes it | put a new thing here |
//! |---|---|---|---|
//! | `tunnel` | `ForwardKind`, `TunnelConfig`: the persisted definition of one forward | `config`, `ssh`, `tunnel`, `app`, `ui` | a new config field, a new `-L/-R/-D` flavour |
//! | `import` | `SshForward` (a directive found in `~/.ssh/config`) + `plan_local_port`/`col_match` rules | `ssh::forwards`, `app::import`, `app::form`, `ui::import` | a new discovery rule or local-port rule |
//! | `import::row` | `ImportColumn`, `ImportRow`: how one discovered forward is shown and ticked | `app::import`, `ui::import` | a filterable column or a per-row flag |
//! | `import::table` | `ImportTable`: candidate rows, cursor, scroll, selection | `app::import`, `ui::import`, `app::mouse` | row/selection state and navigation |
//! | `import::filter` | `ImportTable`'s per-column filters + the undo stack | `app::import`, `keys` | filter/undo behaviour |
//! | `batch` | `MultiSelect`, `BatchAction`: the marked-set and the batch verbs | `app`, `keys`, `ui`, `ui::hit` | a batch verb or selection rule |
//! | `dedup` | `fold_duplicates`, `Folded`: folding saved entries that mean one forward | `config` | which duplicate survives, how it is reported |
//! | `failure` | `FailureKind`, `FailureInfo`: classified ssh failures | `ssh::client`, `tunnel`, `app`, `ui` | a new failure class or its wording |
//! | `status` | `TunnelStatus`, `LogLevel`, `LogLine`, `LogRing`, `TunnelView`: live state | `tunnel`, `app`, `ui` | a lifecycle state or log-ring behaviour |
//! | `form` | `FormField`, `FormMode`, `InputBuf`: the create/edit form's state | `app::form`, `app::import`, `ui` | a field, or text-editing behaviour |
//! | `picker` | `HostPicker`: the SSH-HOST dropdown of the form | `app::picker`, `ui` | dropdown state/scroll behaviour |
//! | `msg` | `StatusMsg` + `now_hms`/`now_unix`: the transient status line and clocks | `app`, `ui`, `config` | status levels/lifetime, another clock helper |
//! | `match` | `fuzzy_match`: case-insensitive subsequence match | `app::picker`, `app` | a matching rule (nothing else) |
//!
//! Two rules keep this split honest:
//!
//! 1. **A file only ever holds one responsibility** — if a type needs a new
//!    concern, that is a new file, not another 100 lines here.
//! 2. **Values are language-neutral.** Anything the user reads is composed one
//!    layer up (`ui`), except where a phrase is part of the data contract
//!    (`FailureKind::short`, `BatchAction::prompt`), because tests and logs key
//!    off it.

mod batch;
mod dedup;
mod event;
mod failure;
mod form;
mod import;
mod r#match;
mod msg;
mod picker;
mod portscan;
mod status;
mod tunnel;

#[cfg(test)]
mod fixtures;

pub type TunnelId = u64;

// Re-exports keep `crate::model::X` working for every caller that predates the
// split. New code may name the submodule directly (`crate::model::status::…`);
// these are the stable surface.
pub use batch::{BatchAction, MultiSelect};
pub use dedup::{Folded, fold_duplicates};
pub use event::TunnelEvent;
pub use failure::{FailureInfo, FailureKind};
pub use form::{FormField, FormMode, InputBuf};
pub use import::{
    ForwardEnds, ImportColumn, ImportRow, ImportTable, LocalPortReason, PORT_SCAN_LIMIT, SshForward, col_match,
    plan_local_port,
};
pub use r#match::fuzzy_match;
pub use msg::{StatusMsg, now_hms, now_unix};
pub use picker::HostPicker;
pub use portscan::{
    PORT_ROWS_VISIBLE, PortPick, PortRow, PortScanState, RemotePort, declared_ports, filter_declared,
    parse_listen_ports,
};
pub use status::{LogLevel, LogLine, LogRing, TunnelStatus, TunnelView};
pub use tunnel::{ForwardKind, TunnelConfig, identity_of};

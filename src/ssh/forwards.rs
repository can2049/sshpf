//! Forwards declared in `~/.ssh/config` (`LocalForward` / `RemoteForward` /
//! `DynamicForward`).
//!
//! ssh config already says how to reach a host *and* which ports the user wanted
//! forwarded on it. `sshpf` never writes to `~/.ssh/config`, but it can read
//! those directives and offer them as ready-made tunnels: that is the whole
//! point of this module.
//!
//! # Why not `ssh2-config`'s own fields
//!
//! The parser in use models `RemoteForward` but treats `LocalForward` and
//! `DynamicForward` as *unsupported* fields: with `ParseRule::ALLOW_UNKNOWN_FIELDS`
//! they land in `HostParams.unsupported_fields` (or are silently dropped), one
//! argument list per directive name — which loses both the order and the count
//! when a host declares several of them. So this module scans the config text
//! itself for the three forwarding verbs, and uses `ssh2-config` only for the
//! connection parameters (`ssh/config.rs`).
//!
//! Scanning is deliberately line-based and quote-aware rather than a full config
//! parser: the directives we care about are single-line and, unlike `Host`,
//! `Match` and `Include`, their scoping rules are simple (applies to the `Host`
//! block it appears in; `Host *` blocks apply to everyone).
//!
//! # Scoping, the rules we implement
//!
//! * Every `Host` block whose pattern matches the alias contributes its forwards,
//!   in file order — this mirrors OpenSSH, which *accumulates* forwarding
//!   directives from every matching block (`Host web` plus `Host *`).
//! * Negated clauses (`!prod`) win over positive ones, as in OpenSSH.
//! * `Include`d files are followed, because users do put forwards in a separate
//!   `config.d/` file.
//! * Two rows with the same [`SshForward::key`] are de-duplicated: `Host *`
//!   repeating a forward that a specific block already declared must not produce
//!   two tunnels for one port.
//!
//! Unix-domain-socket forms (`LocalForward /run/a.sock /run/b.sock`) and `-R`
//! without a destination are *not* silently dropped: [`SshForward`] carries
//! enough to render them, and `unsupported_reason()` explains why they cannot be
//! imported.

//! # Layout
//!
//! | file | responsibility |
//! |---|---|
//! | `forwards.rs` (this file) | [`forwards_for_host`]: the include walk, de-duplication, the depth guard |
//! | [`scan`] | one config file's text → rows: `Host` / `Match` / `Include` scoping, verb dispatch, `Host` pattern matching, `Include` glob expansion |
//! | [`parse`] | one directive's arguments → [`SshForward`]: `[addr:]port host:hostport`, quoting, IPv6, and the forms we deliberately list as un-importable |
//!
//! Where to add things: a new forwarding verb → `scan` (dispatch) + `parse`
//! (argument shapes); a new scoping rule → `scan` only; a new public entry point →
//! here. [`forwards_for_host`] is the only public item.

use super::config::{default_config_paths, strip_match_blocks};
use crate::model::SshForward;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Largest number of `Include` hops we follow, so a cyclic include cannot hang
/// the UI thread.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Read every forwarding directive that applies to `alias`.
///
/// The result is in file order (matching block order, then line order inside a
/// block), de-duplicated by [`SshForward::key`].
pub fn forwards_for_host(alias: &str) -> Result<Vec<SshForward>> {
    // `first()` rather than `paths[0]`: the emptiness check makes the index safe
    // today, but this parses user-supplied files and the index would be a panic if
    // the check ever moved.
    let Some(main) = default_config_paths().first().cloned() else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    let mut stack = vec![main];
    let mut depth = 0;
    let mut seen_files: Vec<PathBuf> = Vec::new();
    while let Some(path) = stack.pop() {
        if depth > MAX_INCLUDE_DEPTH || seen_files.contains(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        seen_files.push(path.clone());
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        scan_config(&strip_match_blocks(&text), alias, &base, &mut stack, &mut found, 0);
        depth += 1;
    }
    Ok(scan::dedup(found))
}

mod parse;
mod scan;

use scan::scan_config;

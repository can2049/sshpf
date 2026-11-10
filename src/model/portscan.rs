//! Ports discovered on the *peer* machine (the SSH host), and the picker state
//! that lets the user tick one or more of them as the remote port.
//!
//! Typing a remote port is the one step in "make a tunnel" that asks the user for
//! something the machine next to them already knows. This module is the data
//! half: what one discovered port is ([`RemotePort`]), what the scan is doing
//! ([`PortScanState`]), and the row/cursor/tick state the form drives
//! ([`PortPick`]).
//!
//! # Layout
//!
//! | file | responsibility |
//! |---|---|
//! | this file | [`RemotePort`], [`PortPick`] and their invariants |
//! | [`parse`] | `ss` / `netstat` text → [`RemotePort`]s (untrusted input) |
//! | [`declared`] | which ports the host's `~/.ssh/config` already enumerates |
//!
//! Belongs here: the shape of a discovered port and the picker's state machine.
//! Does not belong: connecting to the host (`crate::ssh::probe`), spawning the
//! probe thread (`crate::app::port_scan`), rendering (`crate::ui::ports`), key
//! dispatch (`crate::keys::ports`).

use std::time::Instant;

pub mod declared;
pub mod parse;

pub use declared::{declared_ports, filter_declared};
pub use parse::parse_listen_ports;

/// How many rows the port list shows at once.
///
/// Shared by the key handler (scroll window) and the renderer (pane height), the
/// same way `IMPORT_ROWS_VISIBLE` is: two independent numbers would let the
/// cursor leave the visible window.
pub const PORT_ROWS_VISIBLE: usize = 8;

/// Most bind addresses remembered per port.
///
/// One service usually listens on one or two addresses (`0.0.0.0` + `[::]`), but
/// a wildcard listener can produce one line per interface. The cap keeps a
/// hostile or unusual peer from growing a row without bound.
pub(crate) const MAX_ADDRS: usize = 4;

/// One port that is listening on the peer machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePort {
    /// The listening port.
    pub port: u16,
    /// Bind addresses as reported (`127.0.0.1`, `0.0.0.0`, `[::]`, `*`).
    /// Bounded by [`MAX_ADDRS`]; can be empty only for a hand-built row.
    pub addrs: Vec<String>,
}

impl RemotePort {
    /// A row with one address, or an empty address list when `addr` is empty.
    pub fn new(port: u16, addr: impl Into<String>) -> Self {
        let addr = addr.into();
        let addrs = if addr.is_empty() { Vec::new() } else { vec![addr] };
        RemotePort { port, addrs }
    }

    /// `8080` — what the picker shows in the port column.
    pub fn port_text(&self) -> String {
        self.port.to_string()
    }

    /// `127.0.0.1, 0.0.0.0` (or `—` when the peer reported no address).
    pub fn addrs_text(&self) -> String {
        if self.addrs.is_empty() { "—".to_string() } else { self.addrs.join(", ") }
    }

    /// Whether one of the bind addresses is a wildcard, i.e. the service is
    /// reachable on every interface of the peer.
    ///
    /// `0.0.0.0` is the IPv4 wildcard, `*` is what `netstat` prints for "any"
    /// and `[::]` / `::` is the IPv6 one. This matters only for the suggested
    /// target address: dialling `0.0.0.0` is not the same thing as dialling the
    /// loopback.
    pub fn is_wildcard(&self) -> bool {
        self.addrs.iter().any(|a| matches!(a.as_str(), "0.0.0.0" | "*" | "[::]" | "::"))
    }

    /// The target address to suggest for a `-L` forward landing on this port.
    ///
    /// A service bound to a single concrete address is best reached at *that*
    /// address (a daemon listening only on `192.168.1.5` is not at
    /// `127.0.0.1`). Anything ambiguous — no address, several, a wildcard, or a
    /// scope-qualified link-local address such as `127.0.0.53%lo` that another
    /// machine may not resolve — returns `None`, and the caller keeps whatever the
    /// user typed. Guessing there would produce a tunnel that looks right and
    /// forwards nowhere.
    pub fn suggest_target_host(&self) -> Option<&str> {
        let concrete = self.addrs.iter().filter(|a| !a.contains('%'));
        let mut iter = concrete.filter(|a| !matches!(a.as_str(), "0.0.0.0" | "*" | "[::]" | "::"));
        let only = iter.next()?;
        if iter.next().is_some() {
            return None;
        }
        Some(only.as_str())
    }

    /// Add a bind address if it is new, up to [`MAX_ADDRS`].
    pub(crate) fn push_addr(&mut self, addr: &str) {
        if addr.is_empty() || self.addrs.iter().any(|a| a == addr) || self.addrs.len() >= MAX_ADDRS {
            return;
        }
        self.addrs.push(addr.to_string());
    }
}

/// What the scan for this picker is currently doing.
///
/// `Ready` carries only the count: the rows live in [`PortPick::rows`], where the
/// tick marks change without touching the phase. Keeping only the *phase* here is
/// what makes "scanning…" and "3 ports, one ticked" two independent facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PortScanState {
    /// Never scanned for this host / reset.
    #[default]
    Idle,
    /// A probe is in flight.
    Scanning,
    /// The probe returned; `count` is how many rows survived the ssh-config
    /// filter (`0` is a valid, reportable answer: nothing is listening).
    Ready { count: usize },
    /// The probe failed; `reason` is the bilingual sentence to show.
    Failed { reason: String },
}

impl PortScanState {
    pub fn is_scanning(&self) -> bool {
        matches!(self, PortScanState::Scanning)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, PortScanState::Failed { .. })
    }

    /// The failure sentence, when there is one.
    pub fn failure(&self) -> Option<&str> {
        match self {
            PortScanState::Failed { reason } => Some(reason),
            _ => None,
        }
    }
}

/// One selectable row of the port list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRow {
    pub port: RemotePort,
    /// Ticked by the user. Always `false` for a read-only picker (a `-R` forward
    /// lists ports that are *occupied*, so none of them may be picked).
    pub checked: bool,
}

/// The remote-port picker: the scan phase, the rows and the tick marks.
///
/// Lives beside [`crate::model::HostPicker`] and for the same reason: it is a
/// sub-state of the form (rendered inside it, not a modal), and keeping it pure
/// means the navigation and selection rules are testable without a terminal or a
/// network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortPick {
    /// Whether the list is expanded. A collapsed picker keeps its rows and ticks:
    /// the user may close it to read a field, and re-opening must not lose their
    /// choices. The submit path deliberately refuses to act on an invisible list
    /// (see `app::port_batch`), so "closed" cannot mean "create tunnels nobody can
    /// see".
    pub open: bool,
    pub state: PortScanState,
    pub rows: Vec<PortRow>,
    /// Index into `rows` of the highlighted row.
    pub cursor: usize,
    /// First visible row (scroll window).
    pub scroll: u16,
    /// Ports hidden because the host's ssh config already declares them.
    pub excluded: usize,
    /// Generation counter: a scan result is only applied when its sequence
    /// number is still current, so a slow probe for a previous host can never
    /// overwrite the rows of the host the user is looking at now.
    pub seq: u64,
    /// Whether ticking rows can produce tunnels. `true` for a `-L` forward (the
    /// listed ports are candidate *target* ports); `false` for `-R`, where the
    /// listed ports are the ones already taken on the peer — picking one would
    /// fail at bind time, so the list is informational there.
    pub multi: bool,
    /// The host these rows belong to.
    pub host: String,
    /// When the successful scan landed, for the "scanned at" hint.
    pub scanned_at: Option<Instant>,
}

impl PortPick {
    /// A picker that is expanded and waiting for its first result.
    pub fn scanning(host: impl Into<String>, multi: bool, seq: u64) -> Self {
        PortPick {
            open: true,
            state: PortScanState::Scanning,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            excluded: 0,
            seq,
            multi,
            host: host.into(),
            scanned_at: None,
        }
    }

    /// Whether the pane has anything worth drawing.
    pub fn is_visible(&self) -> bool {
        self.open && (!self.rows.is_empty() || !matches!(self.state, PortScanState::Idle))
    }

    /// Store the ports a scan returned for generation `seq`.
    ///
    /// Returns `false` when `seq` is stale, which is not an error: it means the
    /// user switched hosts (or re-scanned) while this probe was in flight.
    /// Ticks start **clear**: discovering a service is not a request to forward
    /// it, and one keypress must never open several listeners.
    pub fn install(&mut self, seq: u64, ports: Vec<RemotePort>, excluded: usize) -> bool {
        if seq != self.seq {
            return false;
        }
        let count = ports.len();
        self.rows = ports.into_iter().map(|port| PortRow { port, checked: false }).collect();
        self.excluded = excluded;
        self.state = PortScanState::Ready { count };
        self.cursor = 0;
        self.scroll = 0;
        self.scanned_at = Some(Instant::now());
        true
    }

    /// Record a failed scan for generation `seq`; `false` when stale.
    pub fn fail(&mut self, seq: u64, reason: impl Into<String>) -> bool {
        if seq != self.seq {
            return false;
        }
        self.rows.clear();
        self.cursor = 0;
        self.scroll = 0;
        self.state = PortScanState::Failed { reason: reason.into() };
        true
    }

    /// Reset for a new scan of the same host, keeping the ticked ports.
    ///
    /// A rescan is a *refresh*, not a new decision: ports the user ticked stay
    /// ticked while the probe is in flight, and [`Self::install`] then replaces the
    /// rows with the fresh answer.
    pub fn begin_rescan(&mut self, seq: u64) {
        self.seq = seq;
        self.state = PortScanState::Scanning;
        self.rows.retain(|row| row.checked);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Ports the user ticked, in ascending order so batch creation is
    /// deterministic.
    pub fn checked_ports(&self) -> Vec<u16> {
        let mut out: Vec<u16> = self.rows.iter().filter(|r| r.checked).map(|r| r.port.port).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub fn checked_count(&self) -> usize {
        self.rows.iter().filter(|r| r.checked).count()
    }

    /// The row under the cursor.
    pub fn cursor_row(&self) -> Option<&PortRow> {
        self.rows.get(self.cursor)
    }

    /// Whether a tick is currently meaningful at all.
    pub fn can_tick(&self) -> bool {
        self.multi && !matches!(self.state, PortScanState::Scanning) && !self.rows.is_empty()
    }

    /// Move the cursor, clamped to the row list.
    ///
    /// `delta` is an `i32` from a key handler: `unsigned_abs` keeps `i32::MIN`
    /// from panicking on `-delta`, and the loop stops as soon as the cursor stops
    /// moving, so a huge delta is bounded by the list.
    pub fn move_cursor(&mut self, delta: i32) {
        let last = self.rows.len().saturating_sub(1);
        for _ in 0..delta.unsigned_abs() {
            let before = self.cursor;
            if delta > 0 {
                self.cursor = (self.cursor + 1).min(last);
            } else {
                self.cursor = self.cursor.saturating_sub(1);
            }
            if self.cursor == before {
                break;
            }
        }
        self.ensure_visible(PORT_ROWS_VISIBLE);
    }

    pub fn home(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.rows.len().saturating_sub(1);
        self.ensure_visible(PORT_ROWS_VISIBLE);
    }

    /// Toggle the row under the cursor. A no-op on a read-only picker.
    pub fn toggle_cursor(&mut self) {
        if !self.can_tick() {
            return;
        }
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.checked = !row.checked;
        }
    }

    /// Tick every row (`a`). No-op on a read-only picker.
    pub fn check_all(&mut self) {
        if !self.can_tick() {
            return;
        }
        for row in &mut self.rows {
            row.checked = true;
        }
    }

    /// Untick every row (`U`).
    pub fn uncheck_all(&mut self) {
        for row in &mut self.rows {
            row.checked = false;
        }
    }

    /// Invert every tick (`I`). No-op on a read-only picker.
    pub fn invert(&mut self) {
        if !self.can_tick() {
            return;
        }
        for row in &mut self.rows {
            row.checked = !row.checked;
        }
    }

    /// Clip `scroll` so the cursor stays inside a `visible`-row viewport.
    ///
    /// `scroll` is a public `u16` that a caller may have set to `u16::MAX`, so
    /// the arithmetic goes through `usize`: `scroll + visible` in `u16` would
    /// overflow and panic in a debug build.
    pub fn ensure_visible(&mut self, visible: usize) {
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        if visible == 0 {
            self.scroll = self.cursor.min(u16::MAX as usize) as u16;
            return;
        }
        let scroll = self.scroll as usize;
        if self.cursor < scroll {
            self.scroll = self.cursor.min(u16::MAX as usize) as u16;
        } else if self.cursor >= scroll.saturating_add(visible) {
            self.scroll = (self.cursor + 1 - visible).min(u16::MAX as usize) as u16;
        }
    }
}

#[cfg(test)]
mod tests;

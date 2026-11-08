//! Shared domain model. Pure data types, no `ratatui`, no I/O.
//!
//! This module is the contract between the persistence layer (`config`),
//! the runtime layer (`tunnel` + `ssh`) and the presentation layer (`ui`).

use std::collections::VecDeque;
use std::time::Instant;

pub type TunnelId = u64;

/// Which OpenSSH forwarding flavour a tunnel uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardKind {
    /// `-L`: listen locally, forward to an address reachable from the SSH server.
    #[default]
    Local,
    /// `-R`: listen on the SSH server, forward back to an address local to us.
    Remote,
    /// `-D`: local SOCKS5 proxy.
    Dynamic,
}

impl ForwardKind {
    pub fn tag(self) -> char {
        match self {
            ForwardKind::Local => 'L',
            ForwardKind::Remote => 'R',
            ForwardKind::Dynamic => 'D',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ForwardKind::Local => "本地转发 -L",
            ForwardKind::Remote => "远程转发 -R",
            ForwardKind::Dynamic => "动态转发 -D",
        }
    }

    /// English half of [`ForwardKind::label`], for the bilingual UI.
    pub fn label_en(self) -> &'static str {
        match self {
            ForwardKind::Local => "Local -L",
            ForwardKind::Remote => "Remote -R",
            ForwardKind::Dynamic => "Dynamic -D",
        }
    }

    pub fn all() -> [ForwardKind; 3] {
        [ForwardKind::Local, ForwardKind::Remote, ForwardKind::Dynamic]
    }
}

/// A persisted tunnel definition (the user's intent).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TunnelConfig {
    pub id: TunnelId,
    pub name: String,
    #[serde(default)]
    pub kind: ForwardKind,
    /// ssh config alias (e.g. `cloud_pc`) or `user@host:port`.
    pub ssh_host: String,
    /// Local listen address for `-L`/`-D`, local target address for `-R`.
    #[serde(default = "default_local_addr")]
    pub local_addr: String,
    pub local_port: u16,
    /// Remote target address for `-L`, remote listen address for `-R`.
    #[serde(default = "default_local_addr")]
    pub remote_host: String,
    pub remote_port: u16,
    /// User intent: should this tunnel be running? Restored on start-up.
    #[serde(default)]
    pub enabled: bool,
    /// Auto reconnect on network-class failures.
    #[serde(default)]
    pub auto_restart: bool,
    /// Extra raw `ssh -o KEY=VALUE` options.
    #[serde(default)]
    pub ssh_options: Vec<String>,
    /// Free-form note shown in the detail panel.
    #[serde(default)]
    pub note: String,
    /// Unix timestamp of creation, informational only.
    #[serde(default)]
    pub created_at: i64,
}

fn default_local_addr() -> String {
    "127.0.0.1".to_string()
}

impl TunnelConfig {
    /// `127.0.0.1:8080`
    pub fn local_display(&self) -> String {
        format!("{}:{}", self.local_addr, self.local_port)
    }

    /// `127.0.0.1:22`, or `(SOCKS5)` for dynamic forwards.
    pub fn remote_display(&self) -> String {
        match self.kind {
            ForwardKind::Dynamic => "(SOCKS5)".to_string(),
            _ => format!("{}:{}", self.remote_host, self.remote_port),
        }
    }

    /// Spec string passed to `ssh -L/-R/-D`.
    pub fn forward_spec(&self) -> String {
        match self.kind {
            ForwardKind::Local => {
                format!("{}:{}:{}:{}", self.local_addr, self.local_port, self.remote_host, self.remote_port)
            }
            ForwardKind::Remote => {
                format!("{}:{}:{}:{}", self.remote_host, self.remote_port, self.local_addr, self.local_port)
            }
            ForwardKind::Dynamic => format!("{}:{}", self.local_addr, self.local_port),
        }
    }

    /// 0..=33 are privileged ports on Linux.
    pub fn uses_privileged_port(&self) -> bool {
        let listen_port = match self.kind {
            ForwardKind::Local | ForwardKind::Dynamic => self.local_port,
            ForwardKind::Remote => self.remote_port,
        };
        listen_port <= 1023
    }
}

// ---------------------------------------------------------------------------
// Forwards declared in ~/.ssh/config (`LocalForward` / `RemoteForward` /
// `DynamicForward`), and the "import these into the form" table that lets a
// user pick which of them a new tunnel batch should start with.
// ---------------------------------------------------------------------------

/// One forward directive read out of a `Host` block in `~/.ssh/config`.
///
/// This is *discovery* data: it is never persisted as-is. The form turns the
/// rows the user keeps into `TunnelConfig`s, which is where `id`, `enabled`,
/// `auto_restart` and the rest of the user intent live.
///
/// Addresses are kept as written in the config (`""` / `"*"` / `"0.0.0.0"` /
/// a hostname) because OpenSSH gives them distinct meanings; the form decides
/// how to normalise them, and the table shows exactly what was configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshForward {
    /// Which directive it came from, so the table can show `L` / `R` / `D`.
    pub kind: ForwardKind,
    /// Listen address as written (`""` when the directive omitted it).
    pub listen_addr: String,
    /// Listen port (the local port for `-L`/`-D`, the remote one for `-R`).
    pub listen_port: u16,
    /// Target host as written (`-L`: reachable from the server; `-R`: local).
    /// Always empty for `-D`.
    pub target_host: String,
    /// Target port (0 for `-D`).
    pub target_port: u16,
    /// `Host` alias this directive belongs to.
    pub host_alias: String,
    /// 1-based line number in the config file, for "where did this come from".
    pub line: usize,
    /// The directive verbatim, e.g. `LocalForward 8080 127.0.0.1:80`. Shown in
    /// the table so the user can recognise a row without decoding it.
    pub raw: String,
}

impl SshForward {
    /// Stable identity of the *forward itself*, independent of where it was
    /// declared. Used to de-duplicate rows that several matching `Host` blocks
    /// contribute, and to remember which rows the user unticked across
    /// re-filtering.
    pub fn key(&self) -> String {
        match self.kind {
            ForwardKind::Dynamic => format!("D|{}|{}", self.listen_addr, self.listen_port),
            _ => format!(
                "{:?}|{}|{}|{}|{}",
                self.kind, self.listen_addr, self.listen_port, self.target_host, self.target_port
            ),
        }
    }

    /// `LocalForward` / `RemoteForward` / `DynamicForward`.
    pub fn directive(&self) -> &'static str {
        match self.kind {
            ForwardKind::Local => "LocalForward",
            ForwardKind::Remote => "RemoteForward",
            ForwardKind::Dynamic => "DynamicForward",
        }
    }

    /// `listen` or `listen → target`, for the 目标 column.
    pub fn target_display(&self) -> String {
        match self.kind {
            ForwardKind::Dynamic => "(SOCKS5)".to_string(),
            _ => format!("{}:{}", self.target_host, self.target_port),
        }
    }

    /// `addr:port`, with an empty address rendered as the OpenSSH default.
    pub fn listen_display(&self) -> String {
        let addr = if self.listen_addr.is_empty() { "localhost" } else { self.listen_addr.as_str() };
        format!("{addr}:{}", self.listen_port)
    }

    /// Whether this row can be imported at all. Unix-domain-socket forwards and
    /// non-numeric ports cannot be expressed as a `TunnelConfig`, so they are
    /// listed but never selectable (the table renders them dimmed with a
    /// reason) instead of silently disappearing.
    pub fn unsupported_reason(&self) -> Option<&'static str> {
        if self.listen_port == 0 {
            return Some("端口无法解析");
        }
        if self.kind != ForwardKind::Dynamic && (self.target_host.is_empty() || self.target_port == 0) {
            return Some("目标地址无法解析");
        }
        None
    }

    /// English half of [`SshForward::unsupported_reason`], for the bilingual UI.
    ///
    /// Mirrors the same two conditions rather than matching on the Chinese
    /// string: the reason and its translation then cannot drift apart, and a
    /// third reason added above becomes a compile-visible edit here too.
    pub fn unsupported_reason_en(&self) -> Option<&'static str> {
        if self.listen_port == 0 {
            return Some("Unparseable port");
        }
        if self.kind != ForwardKind::Dynamic && (self.target_host.is_empty() || self.target_port == 0) {
            return Some("Unparseable target");
        }
        None
    }

    /// Suggested tunnel name, e.g. `cloud_pc-L-8080` — unique enough to sort,
    /// short enough for the NAME column.
    pub fn suggested_name(&self) -> String {
        format!("{}-{}-{}", self.host_alias, self.kind.tag(), self.listen_port)
    }
}

/// Why [`plan_local_port`] picked the port it did, for the form's hint text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPortReason {
    /// Same number as the remote port (the intuitive case).
    SameAsRemote,
    /// The remote port is privileged, so this is a friendly unprivileged alias
    /// (`22 -> 2222`).
    PrivilegedAlias,
    /// The natural choice was taken, so the scan walked upwards.
    Bumped,
}

/// Candidates the upward scan may probe before giving up.
pub const PORT_SCAN_LIMIT: usize = 200;

/// Choose a local listen port for a forward landing on `remote`.
///
/// This is the *one* implementation of the local-port rules; the form and the
/// ssh-config importer both call it, because they already drifted once. The
/// rules, in order:
///
/// 1. `remote` itself, when it is unprivileged and free — "the same number on
///    both ends" is what people expect.
/// 2. For a privileged `remote`, a friendly alias (`22 -> 2222`). Scanning up
///    from a privileged port instead would be futile: binding one needs root, so
///    `available` reports every candidate below 1024 as busy and the whole scan
///    budget burns inside the privileged range. That bug was caught on the form
///    path and is pinned by `privileged_remote_gets_an_alias_not_a_privileged_scan`;
///    the importer must not reintroduce it.
/// 3. Otherwise walk upwards **from 1024**, skipping anything `taken`.
///
/// `available` is a closure so this stays pure and unit-testable without binding
/// real sockets.
pub fn plan_local_port(remote: u16, taken: &[u16], available: impl Fn(u16) -> bool) -> Option<(u16, LocalPortReason)> {
    if remote == 0 {
        return None;
    }
    let usable = |c: u16| c >= 1024 && !taken.contains(&c) && available(c);

    if remote >= 1024 && usable(remote) {
        return Some((remote, LocalPortReason::SameAsRemote));
    }
    if remote < 1024 {
        // The conventional alias: 22 -> 2222, 80 -> 2280, 443 -> 2643.
        let alias = 2000u16.saturating_add(remote).saturating_add(200);
        if usable(alias) {
            return Some((alias, LocalPortReason::PrivilegedAlias));
        }
    }
    (remote.max(1024)..=u16::MAX)
        .take(PORT_SCAN_LIMIT)
        .find(|candidate| usable(*candidate))
        .map(|port| (port, LocalPortReason::Bumped))
}

/// Case-insensitive substring match used by the per-column filters.
///
/// Substring rather than the fuzzy subsequence matcher used by the main table:
/// a column filter is a *scope narrowing* tool (`8080`, `localhost`, `-R`), and
/// fuzzy matching on a port would happily return unrelated rows.
pub fn col_match(needle: &str, haystack: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return true;
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// A column of the import table.
///
/// The checkbox column is selection state, not data, so it is deliberately not
/// a variant here: a filter on it would be a second way to express "select
/// all", which is what the row toggles are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportColumn {
    Kind,
    Listen,
    Target,
    Raw,
}

impl ImportColumn {
    pub const COUNT: usize = 4;
    /// Columns that accept a filter, left to right.
    pub const ALL: [ImportColumn; ImportColumn::COUNT] =
        [ImportColumn::Kind, ImportColumn::Listen, ImportColumn::Target, ImportColumn::Raw];

    pub fn label(self) -> &'static str {
        match self {
            ImportColumn::Kind => "类型",
            ImportColumn::Listen => "监听",
            ImportColumn::Target => "目标",
            ImportColumn::Raw => "原始指令",
        }
    }

    /// English half of [`ImportColumn::label`], for the bilingual UI.
    pub fn label_en(self) -> &'static str {
        match self {
            ImportColumn::Kind => "Kind",
            ImportColumn::Listen => "Listen",
            ImportColumn::Target => "Target",
            ImportColumn::Raw => "Directive",
        }
    }

    /// Position in [`ImportColumn::ALL`], i.e. the index into
    /// [`ImportTable::filters`].
    ///
    /// Derived from `ALL` rather than hand-numbered: the filter array is sized
    /// `COUNT`, so a variant missing from `ALL` must not silently get a
    /// hand-written index that is out of range.
    pub fn index(self) -> usize {
        ImportColumn::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }

    /// `value` as the filter sees it, so the table and the matcher cannot
    /// disagree about what a column contains.
    pub fn cell(self, f: &SshForward) -> String {
        match self {
            ImportColumn::Kind => f.kind.tag().to_string(),
            ImportColumn::Listen => f.listen_display(),
            ImportColumn::Target => f.target_display(),
            ImportColumn::Raw => f.raw.clone(),
        }
    }

    pub fn next(self) -> ImportColumn {
        let i = self.index();
        ImportColumn::ALL.get(i + 1).or_else(|| ImportColumn::ALL.first()).copied().unwrap_or(self)
    }

    pub fn prev(self) -> ImportColumn {
        match self.index() {
            0 => ImportColumn::ALL.last().copied().unwrap_or(self),
            i => ImportColumn::ALL.get(i - 1).copied().unwrap_or(self),
        }
    }
}

/// One candidate row: a forward found in `~/.ssh/config`, plus whether the user
/// wants it created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRow {
    pub forward: SshForward,
    pub checked: bool,
}

impl ImportRow {
    /// `false` for rows we can list but cannot import (unix sockets, a `-R`
    /// with no destination): the table renders them dimmed with the reason.
    pub fn importable(&self) -> bool {
        self.forward.unsupported_reason().is_none()
    }
}

/// How many filter snapshots the undo stack keeps.
const FILTER_HISTORY_MAX: usize = 64;

/// The "forwards found for this host" table shown when creating a tunnel.
///
/// Deliberately a *pure* state machine: it owns the candidate rows, the tick
/// marks, the per-column filters and the filter undo stack, and knows nothing
/// about `App`, SSH or rendering. That is what makes the filtering/selection
/// rules unit-testable without a terminal.
///
/// Default policy (requirement: "默认勾选该 host 在 ~/.ssh/config 中的所有现有
/// 端口转发"): every importable row starts checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportTable {
    pub rows: Vec<ImportRow>,
    /// Index into [`ImportTable::visible`], not into `rows`.
    pub cursor: usize,
    /// First visible row (scroll window).
    pub scroll: u16,
    /// One filter per [`ImportColumn`], indexed by `ImportColumn::index()`.
    pub filters: [String; ImportColumn::COUNT],
    /// The column filter currently being typed into, if any.
    pub editing: Option<ImportColumn>,
    /// Filter snapshots for undo (`z`). Newest last.
    history: Vec<[String; ImportColumn::COUNT]>,
}

impl ImportTable {
    /// Build a table from the discovered forwards: everything importable is
    /// checked by default.
    pub fn new(forwards: Vec<SshForward>) -> Self {
        let rows = forwards
            .into_iter()
            .map(|forward| {
                let checked = forward.unsupported_reason().is_none();
                ImportRow { forward, checked }
            })
            .collect();
        ImportTable {
            rows,
            cursor: 0,
            scroll: 0,
            filters: std::array::from_fn(|_| String::new()),
            editing: None,
            history: Vec::new(),
        }
    }

    /// An empty table (`~/.ssh/config` has nothing for this host).
    pub fn empty() -> Self {
        ImportTable::new(Vec::new())
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Row indices that survive every column filter, in file order.
    pub fn visible(&self) -> Vec<usize> {
        let active: Vec<ImportColumn> =
            ImportColumn::ALL.into_iter().filter(|c| !self.filter(*c).trim().is_empty()).collect();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| active.iter().all(|col| col_match(self.filter(*col), &col.cell(&row.forward))))
            .map(|(i, _)| i)
            .collect()
    }

    /// The row under the cursor.
    pub fn cursor_row(&self) -> Option<&ImportRow> {
        self.visible().get(self.cursor).and_then(|i| self.rows.get(*i))
    }

    pub fn cursor_forward(&self) -> Option<SshForward> {
        self.cursor_row().map(|r| r.forward.clone())
    }

    /// Keep `cursor` inside the filtered view. Called after any filter or row
    /// change, because a shrinking view must never leave a dangling cursor.
    pub fn clamp_cursor(&mut self) {
        let n = self.visible().len();
        self.cursor = if n == 0 { 0 } else { self.cursor.min(n - 1) };
    }

    pub fn move_cursor(&mut self, delta: i32) {
        self.clamp_cursor();
        let n = self.visible().len();
        if n == 0 {
            return;
        }
        let next = self.cursor as i64 + delta as i64;
        self.cursor = next.clamp(0, n as i64 - 1) as usize;
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.visible().len().saturating_sub(1);
    }

    /// Clip `scroll` so the cursor row is inside a `height`-row viewport.
    ///
    /// `scroll`/`height` are `u16` and `scroll` can be written directly, so the
    /// comparisons use `usize` arithmetic: `self.scroll + height` in `u16` would
    /// overflow (and panic in a debug build) for a scroll left at `u16::MAX`.
    pub fn ensure_visible(&mut self, height: usize) {
        let sel = self.cursor;
        if height == 0 {
            self.scroll = sel.min(u16::MAX as usize) as u16;
            return;
        }
        let scroll = self.scroll as usize;
        if sel < scroll {
            self.scroll = sel.min(u16::MAX as usize) as u16;
        } else if sel >= scroll.saturating_add(height) {
            self.scroll = (sel + 1 - height).min(u16::MAX as usize) as u16;
        }
    }

    // -- selection ---------------------------------------------------------

    pub fn toggle_cursor(&mut self) {
        if let Some(&idx) = self.visible().get(self.cursor)
            && let Some(row) = self.rows.get_mut(idx)
            && row.importable()
        {
            row.checked = !row.checked;
        }
    }

    /// Tick every importable row in the *filtered* view.
    ///
    /// Filtered, not all: with a filter active the user is looking at a subset,
    /// and a "全选" that silently checked hidden rows would be a footgun.
    pub fn check_all_visible(&mut self) {
        for idx in self.visible() {
            if let Some(row) = self.rows.get_mut(idx)
                && row.importable()
            {
                row.checked = true;
            }
        }
    }

    pub fn uncheck_all_visible(&mut self) {
        for idx in self.visible() {
            if let Some(row) = self.rows.get_mut(idx) {
                row.checked = false;
            }
        }
    }

    pub fn invert_visible(&mut self) {
        for idx in self.visible() {
            if let Some(row) = self.rows.get_mut(idx)
                && row.importable()
            {
                row.checked = !row.checked;
            }
        }
    }

    pub fn checked_count(&self) -> usize {
        self.rows.iter().filter(|r| r.checked && r.importable()).count()
    }

    pub fn importable_count(&self) -> usize {
        self.rows.iter().filter(|r| r.importable()).count()
    }

    pub fn any_checked(&self) -> bool {
        self.checked_count() > 0
    }

    /// The checked forwards, in file order — what the form turns into tunnels.
    pub fn checked_forwards(&self) -> Vec<SshForward> {
        self.rows.iter().filter(|r| r.checked && r.importable()).map(|r| r.forward.clone()).collect()
    }

    // -- filters -----------------------------------------------------------

    pub fn has_filters(&self) -> bool {
        self.filters.iter().any(|f| !f.trim().is_empty())
    }

    pub fn filter(&self, col: ImportColumn) -> &str {
        // The array is sized `ImportColumn::COUNT` and indexed by
        // `ImportColumn::index()`; `get` makes the coupling explicit instead of
        // relying on the two staying in step.
        self.filters.get(col.index()).map(String::as_str).unwrap_or("")
    }

    fn snapshot(&mut self) {
        self.history.push(self.filters.clone());
        if self.history.len() > FILTER_HISTORY_MAX {
            self.history.remove(0);
        }
    }

    /// Replace one column's filter, recording an undo point.
    pub fn set_filter(&mut self, col: ImportColumn, text: impl Into<String>) {
        self.snapshot();
        if let Some(slot) = self.filters.get_mut(col.index()) {
            *slot = text.into();
        }
        self.after_filter_change();
    }

    pub fn filter_char(&mut self, col: ImportColumn, c: char) {
        self.snapshot();
        if let Some(slot) = self.filters.get_mut(col.index()) {
            slot.push(c);
        }
        self.after_filter_change();
    }

    pub fn filter_backspace(&mut self, col: ImportColumn) {
        let Some(index) = self.filter_index(col) else {
            return;
        };
        if self.filters[index].is_empty() {
            return;
        }
        self.snapshot();
        self.filters[index].pop();
        self.after_filter_change();
    }

    /// Delete one column's filter (undoable).
    pub fn clear_filter(&mut self, col: ImportColumn) {
        let Some(index) = self.filter_index(col) else {
            return;
        };
        if self.filters[index].is_empty() {
            return;
        }
        self.snapshot();
        self.filters[index].clear();
        self.after_filter_change();
    }

    /// `col`'s slot in `filters`, or `None` when the array and the column list
    /// disagree about how many columns exist.
    fn filter_index(&self, col: ImportColumn) -> Option<usize> {
        let index = col.index();
        (index < self.filters.len()).then_some(index)
    }

    /// Delete every filter at once (undoable).
    pub fn clear_all_filters(&mut self) {
        if !self.has_filters() {
            return;
        }
        self.snapshot();
        self.filters = std::array::from_fn(|_| String::new());
        self.after_filter_change();
    }

    /// Roll back the most recent filter change. Returns true when something was
    /// undone, so the caller can say so instead of failing silently.
    pub fn undo_filter(&mut self) -> bool {
        match self.history.pop() {
            Some(prev) => {
                self.filters = prev;
                self.after_filter_change();
                true
            }
            None => false,
        }
    }

    pub fn can_undo_filter(&self) -> bool {
        !self.history.is_empty()
    }

    fn after_filter_change(&mut self) {
        // The filtered view changed size: re-anchor the cursor instead of
        // leaving it pointing at a row that is no longer shown.
        self.cursor = 0;
        self.scroll = 0;
        self.clamp_cursor();
    }

    /// Begin typing into a column filter.
    pub fn begin_filter(&mut self, col: ImportColumn) {
        self.editing = Some(col);
    }

    pub fn end_filter(&mut self) {
        self.editing = None;
    }

    /// The status line: `3/5 条勾选 · 筛选: 类型=R, 监听=8080`.
    ///
    /// Chinese only on purpose: §5.2 of the i18n contract keeps the model layer
    /// language-neutral and lets the UI compose the English half from the
    /// `*_en()` accessors (`src/ui/import.rs`).
    pub fn summary(&self) -> String {
        let mut s = format!("{}/{} 条勾选", self.checked_count(), self.importable_count());
        if self.has_filters() {
            let parts: Vec<String> = ImportColumn::ALL
                .into_iter()
                .map(|c| (c, self.filter(c)))
                .filter(|(_, f)| !f.trim().is_empty())
                .map(|(c, f)| format!("{}={f}", c.label()))
                .collect();
            s.push_str(&format!(" · 筛选: {}", parts.join(", ")));
        }
        s
    }
}

/// One or more rows of the main table marked for a batch operation.
///
/// Selection is by `TunnelId`, not by row index: rows move when the filter
/// changes or a tunnel is deleted, and a marked set that silently retargeted
/// another tunnel would be dangerous for a batch delete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultiSelect {
    /// Whether the table is in selection mode at all.
    pub active: bool,
    /// Marked tunnel ids, ascending for stable prompts.
    pub marked: std::collections::BTreeSet<TunnelId>,
    /// Row the mark was last extended from (an index into `visible_indices`),
    /// for shift+click / shift+j range marking.
    pub anchor: Option<usize>,
}

impl MultiSelect {
    pub fn new() -> Self {
        MultiSelect::default()
    }

    /// Enter selection mode with nothing marked.
    ///
    /// Deliberately empty rather than pre-marking the cursor row: `space` must
    /// mean the same thing before and after entering (toggle the row under the
    /// cursor). If entering already marked it, the first `space` would *unmark*
    /// the row the user just pointed at, which reads as a bug.
    pub fn enter(&mut self) {
        self.active = true;
        self.marked.clear();
        self.anchor = None;
    }

    pub fn exit(&mut self) {
        self.active = false;
        self.marked.clear();
        self.anchor = None;
    }

    pub fn is_marked(&self, id: TunnelId) -> bool {
        self.marked.contains(&id)
    }

    pub fn len(&self) -> usize {
        self.marked.len()
    }

    pub fn is_empty(&self) -> bool {
        self.marked.is_empty()
    }

    pub fn toggle(&mut self, id: TunnelId) {
        if !self.marked.remove(&id) {
            self.marked.insert(id);
        }
    }

    pub fn set(&mut self, id: TunnelId, on: bool) {
        if on {
            self.marked.insert(id);
        } else {
            self.marked.remove(&id);
        }
    }

    pub fn clear(&mut self) {
        self.marked.clear();
        self.anchor = None;
    }

    /// Mark every id in `ids` (used by 全选 on the filtered view).
    pub fn mark_all(&mut self, ids: &[TunnelId]) {
        self.marked.extend(ids.iter().copied());
    }

    /// Mark the inclusive range `from..=to` in `ids`.
    pub fn mark_range(&mut self, ids: &[TunnelId], from: usize, to: usize) {
        // `lo`/`hi` are caller-supplied indices; clamp both to the view before
        // slicing, and use `get` so an empty view stays a no-op rather than a
        // `len() - 1` underflow.
        let Some(last) = ids.len().checked_sub(1) else {
            return;
        };
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        let lo = lo.min(last);
        let hi = hi.min(last);
        if let Some(range) = ids.get(lo..=hi) {
            self.marked.extend(range.iter().copied());
        }
        self.anchor = Some(hi);
    }

    /// Drop marks for tunnels that no longer exist (after a batch delete).
    pub fn retain_existing(&mut self, exists: impl Fn(TunnelId) -> bool) {
        self.marked.retain(|id| exists(*id));
        if self.marked.is_empty() {
            self.anchor = None;
        }
    }

    /// Marked ids as a `Vec`, for prompts and dispatch.
    pub fn ids(&self) -> Vec<TunnelId> {
        self.marked.iter().copied().collect()
    }
}

/// An operation a multi-selection can perform at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAction {
    Start,
    Stop,
    Restart,
    Delete,
}

impl BatchAction {
    pub const ALL: [BatchAction; 4] =
        [BatchAction::Start, BatchAction::Stop, BatchAction::Restart, BatchAction::Delete];

    pub fn label(self) -> &'static str {
        match self {
            BatchAction::Start => "启用",
            BatchAction::Stop => "停用",
            BatchAction::Restart => "重启",
            BatchAction::Delete => "删除",
        }
    }

    /// English half of [`BatchAction::label`], for the bilingual UI.
    pub fn label_en(self) -> &'static str {
        match self {
            BatchAction::Start => "Start",
            BatchAction::Stop => "Stop",
            BatchAction::Restart => "Restart",
            BatchAction::Delete => "Delete",
        }
    }

    /// ASCII name, used for log fields (and therefore stable across UI wording).
    pub fn name(self) -> &'static str {
        match self {
            BatchAction::Start => "start",
            BatchAction::Stop => "stop",
            BatchAction::Restart => "restart",
            BatchAction::Delete => "delete",
        }
    }

    /// Every batch operation goes through the confirmation gate: even "enable"
    /// opens N listeners at once, which is exactly what the single-row flow
    /// already warns about.
    /// Bilingual: the Chinese sentence stays first and unchanged (tests and the
    /// UI key off it), the English sentence follows after a space.
    pub fn prompt(self, n: usize) -> String {
        match self {
            BatchAction::Start => crate::help::bi(
                &format!("确认启用选中的 {n} 条转发？将新开 {n} 个监听端口。"),
                &format!("Start the {n} selected forwards? {n} new listening ports will open."),
            ),
            BatchAction::Stop => crate::help::bi(
                &format!("确认停用选中的 {n} 条转发？将断开它们的连接。"),
                &format!("Stop the {n} selected forwards? Their connections will drop."),
            ),
            BatchAction::Restart => crate::help::bi(
                &format!("确认重启选中的 {n} 条转发？连接会短暂中断。"),
                &format!("Restart the {n} selected forwards? The connections will briefly drop."),
            ),
            BatchAction::Delete => crate::help::bi(
                &format!("确认删除选中的 {n} 条转发？此操作不可撤销。"),
                &format!("Delete the {n} selected forwards? This cannot be undone."),
            ),
        }
    }
}

/// Coarse failure classification, rendered as a short phrase in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// `bind ...: Address already in use`
    PortInUse,
    /// `bind ...: Permission denied` — privileged port without CAP_NET_BIND_SERVICE
    PermissionDenied,
    /// `Permission denied (publickey)` — no usable key / agent
    AuthFailed,
    /// `Could not resolve hostname`
    DnsFailure,
    /// `Connection refused`
    ConnectionRefused,
    /// `Connection timed out` / `Operation timed out`
    Timeout,
    /// `Host key verification failed` / `REMOTE HOST IDENTIFICATION HAS CHANGED`
    HostKeyMismatch,
    /// `remote port forwarding failed`
    RemoteForwardFailed,
    /// Target host:port behind the tunnel refused/reset on first use.
    TargetUnreachable,
    /// Could not spawn the `ssh` binary at all.
    SpawnFailed,
    /// ssh exited without a more specific classification.
    Exited,
    /// Feature not implemented for this forward kind yet.
    Unsupported,
    /// Anything else; `detail` carries the raw line.
    Unknown,
}

impl FailureKind {
    /// Short Chinese phrase for the table's LAST ERROR column (no newlines).
    pub fn short(self) -> &'static str {
        match self {
            FailureKind::PortInUse => "本地端口被占用",
            FailureKind::PermissionDenied => "端口权限不足(<1024)",
            FailureKind::AuthFailed => "认证失败(公钥/agent)",
            FailureKind::DnsFailure => "域名解析失败",
            FailureKind::ConnectionRefused => "连接被拒绝",
            FailureKind::Timeout => "连接超时",
            FailureKind::HostKeyMismatch => "主机密钥校验失败",
            FailureKind::RemoteForwardFailed => "远端转发失败",
            FailureKind::TargetUnreachable => "目标不可达",
            FailureKind::SpawnFailed => "无法启动 ssh",
            FailureKind::Exited => "ssh 异常退出",
            FailureKind::Unsupported => "该类型暂未支持",
            FailureKind::Unknown => "未知错误",
        }
    }

    /// English half of [`FailureKind::short`], for the bilingual UI.
    pub fn short_en(self) -> &'static str {
        match self {
            FailureKind::PortInUse => "Local port in use",
            FailureKind::PermissionDenied => "Port needs root (<1024)",
            FailureKind::AuthFailed => "Authentication failed",
            FailureKind::DnsFailure => "DNS lookup failed",
            FailureKind::ConnectionRefused => "Connection refused",
            FailureKind::Timeout => "Connection timed out",
            FailureKind::HostKeyMismatch => "Host key verification failed",
            FailureKind::RemoteForwardFailed => "Remote forward rejected",
            FailureKind::TargetUnreachable => "Target unreachable",
            FailureKind::SpawnFailed => "Cannot start ssh",
            FailureKind::Exited => "ssh exited unexpectedly",
            FailureKind::Unsupported => "Not supported yet",
            FailureKind::Unknown => "Unknown error",
        }
    }

    /// ASCII name, used for log fields where a stable greppable token beats the
    /// UI phrasing (`short()` is Chinese and may be reworded).
    pub fn name(self) -> &'static str {
        match self {
            FailureKind::PortInUse => "port_in_use",
            FailureKind::PermissionDenied => "permission_denied",
            FailureKind::AuthFailed => "auth_failed",
            FailureKind::DnsFailure => "dns_failure",
            FailureKind::ConnectionRefused => "connection_refused",
            FailureKind::Timeout => "timeout",
            FailureKind::HostKeyMismatch => "host_key_mismatch",
            FailureKind::RemoteForwardFailed => "remote_forward_failed",
            FailureKind::TargetUnreachable => "target_unreachable",
            FailureKind::SpawnFailed => "spawn_failed",
            FailureKind::Exited => "exited",
            FailureKind::Unsupported => "unsupported",
            FailureKind::Unknown => "unknown",
        }
    }

    /// Config-class failures are terminal: retrying cannot help until the user
    /// edits the definition. Network-class failures may be retried.
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            FailureKind::ConnectionRefused
                | FailureKind::Timeout
                | FailureKind::DnsFailure
                | FailureKind::TargetUnreachable
                | FailureKind::Exited
                | FailureKind::Unknown
        )
    }
}

/// A classified failure: short phrase for the table, raw text for the detail view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureInfo {
    pub kind: FailureKind,
    /// Human readable short reason, may be more specific than `kind.short()`.
    pub short: String,
    /// Raw ssh stderr line(s) — shown verbatim in the detail panel.
    pub detail: String,
}

impl FailureInfo {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        FailureInfo { kind, short: kind.short().to_string(), detail }
    }

    pub fn with_short(kind: FailureKind, short: impl Into<String>, detail: impl Into<String>) -> Self {
        FailureInfo { kind, short: short.into(), detail: detail.into() }
    }
}

/// Live status of a tunnel. Rebuilt from scratch on every start/stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelStatus {
    /// Not running (user intent off, or stopped by hand).
    Stopped,
    /// ssh spawned, waiting for the forwarding-ready marker.
    Starting { since: Instant },
    /// Forwarding is listening and the session is established.
    Up { since: Instant },
    /// Waiting for the next automatic reconnect attempt.
    Retrying { attempt: u32, next_at: Instant, reason: FailureInfo },
    /// Terminal failure; no automatic retry.
    Failed { reason: FailureInfo },
}

impl TunnelStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, TunnelStatus::Up { .. } | TunnelStatus::Starting { .. })
    }

    pub fn is_running_or_pending(&self) -> bool {
        !matches!(self, TunnelStatus::Stopped)
    }

    /// Stable lowercase key used by the UI for styling.
    pub fn key(&self) -> &'static str {
        match self {
            TunnelStatus::Stopped => "stopped",
            TunnelStatus::Starting { .. } => "starting",
            TunnelStatus::Up { .. } => "up",
            TunnelStatus::Retrying { .. } => "retrying",
            TunnelStatus::Failed { .. } => "failed",
        }
    }

    pub fn failure(&self) -> Option<&FailureInfo> {
        match self {
            TunnelStatus::Failed { reason } => Some(reason),
            TunnelStatus::Retrying { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// One line in a tunnel's per-tunnel log ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Pre-formatted `HH:MM:SS`.
    pub time: String,
    pub level: LogLevel,
    pub text: String,
}

impl LogLine {
    pub fn new(level: LogLevel, text: impl Into<String>) -> Self {
        LogLine { time: now_hms(), level, text: text.into() }
    }
}

/// Bounded FIFO of log lines.
#[derive(Debug, Clone)]
pub struct LogRing {
    buf: VecDeque<LogLine>,
    cap: usize,
}

impl LogRing {
    pub fn new(cap: usize) -> Self {
        // A zero-capacity ring would never evict: `push` pops only when
        // `len == cap`, so `cap == 0` grows without bound. The bound is the
        // whole point of this type, so enforce it here rather than at every
        // call site.
        let cap = cap.max(1);
        LogRing { buf: VecDeque::with_capacity(cap.min(1024)), cap }
    }

    pub fn push(&mut self, line: LogLine) {
        while self.buf.len() >= self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(line);
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &LogLine> {
        self.buf.iter()
    }

    pub fn last(&self) -> Option<&LogLine> {
        self.buf.back()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<LogLine> {
        let skip = self.buf.len().saturating_sub(n);
        self.buf.iter().skip(skip).cloned().collect()
    }
}

impl Default for LogRing {
    fn default() -> Self {
        LogRing::new(1000)
    }
}

/// A tunnel as presented by the UI: definition + live state + its log.
#[derive(Debug, Clone)]
pub struct TunnelView {
    pub cfg: TunnelConfig,
    pub status: TunnelStatus,
    pub log: LogRing,
    /// Number of automatic reconnect attempts made so far (for the detail panel).
    pub retries: u32,
}

impl TunnelView {
    pub fn new(cfg: TunnelConfig) -> Self {
        let running = cfg.enabled;
        TunnelView {
            cfg,
            status: if running { TunnelStatus::Starting { since: Instant::now() } } else { TunnelStatus::Stopped },
            log: LogRing::new(1000),
            retries: 0,
        }
    }

    /// Short reason shown in the LAST ERROR column, `None` when healthy.
    pub fn last_error(&self) -> Option<String> {
        self.status.failure().map(|f| f.short.clone())
    }
}

/// Format the current wall clock as `HH:MM:SS` in local time.
pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Unix timestamp right now.
pub fn now_unix() -> i64 {
    chrono::Local::now().timestamp()
}

// ---------------------------------------------------------------------------
// UI-visible transient state (form / host picker). Kept here so the `ui`
// module can render it without depending on the `app` module.
// ---------------------------------------------------------------------------

/// Editable fields of the create/edit form, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    Name,
    Kind,
    SshHost,
    LocalAddr,
    LocalPort,
    RemoteHost,
    RemotePort,
    AutoRestart,
}

impl FormField {
    /// Field order, chosen so the form reads top-to-bottom in the order a user
    /// actually thinks about a forward: pick the machine, name it, then say
    /// *where* the traffic goes (remote port) before *where it lands locally*
    /// (local port) — because the remote port is what the user knows, and the
    /// local port is derived from it as a suggestion.
    pub const ORDER: [FormField; 8] = [
        FormField::SshHost,
        FormField::Name,
        FormField::Kind,
        FormField::RemoteHost,
        FormField::RemotePort,
        FormField::LocalAddr,
        FormField::LocalPort,
        FormField::AutoRestart,
    ];

    /// Fields the user types into (as opposed to toggles/selects).
    pub fn is_text(self) -> bool {
        matches!(
            self,
            FormField::Name
                | FormField::SshHost
                | FormField::LocalAddr
                | FormField::LocalPort
                | FormField::RemoteHost
                | FormField::RemotePort
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            FormField::Name => "名称",
            FormField::Kind => "类型",
            FormField::SshHost => "SSH 主机",
            FormField::LocalAddr => "本地地址",
            FormField::LocalPort => "本地端口",
            FormField::RemoteHost => "远端地址",
            FormField::RemotePort => "远端端口",
            FormField::AutoRestart => "自动重连",
        }
    }

    /// English half of [`FormField::label`], for the bilingual UI.
    pub fn label_en(self) -> &'static str {
        match self {
            FormField::Name => "Name",
            FormField::Kind => "Type",
            FormField::SshHost => "SSH host",
            FormField::LocalAddr => "Local addr",
            FormField::LocalPort => "Local port",
            FormField::RemoteHost => "Remote host",
            FormField::RemotePort => "Remote port",
            FormField::AutoRestart => "Auto-retry",
        }
    }

    pub fn next(self) -> FormField {
        // `ORDER` is a fixed array, but the "position may be missing" case is
        // real: a field added without being listed would make `position` return
        // `None`. Fall back to the first field instead of indexing blind.
        let i = FormField::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        FormField::ORDER.get(i + 1).or_else(|| FormField::ORDER.first()).copied().unwrap_or(self)
    }

    pub fn prev(self) -> FormField {
        let i = FormField::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        match i.checked_sub(1) {
            Some(prev) => FormField::ORDER.get(prev).copied().unwrap_or(self),
            None => FormField::ORDER.last().copied().unwrap_or(self),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    Create,
    Edit(TunnelId),
}

/// A text input buffer with a cursor position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBuf {
    pub text: String,
    pub cursor: usize,
}

impl InputBuf {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        InputBuf { text, cursor }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.chars().count();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        // `cursor` is a public field and can be stale (past the end of the text).
        // Clamping first keeps `cursor += 1` from overflowing and puts the new
        // character where the user is looking rather than after a `usize::MAX`.
        let len = self.text.chars().count();
        if self.cursor > len {
            self.cursor = len;
        }
        let byte = self.byte_index(self.cursor);
        self.text.insert(byte, c);
        self.cursor = self.cursor.saturating_add(1);
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // A stale `cursor` past the end means "the caret is after the last
        // character", so clamp and delete that character rather than bailing
        // out — backspace at the end is the common case.
        let len = self.text.chars().count();
        if self.cursor > len {
            self.cursor = len;
        }
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_index(self.cursor);
        let start = self.byte_index(self.cursor - 1);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        let len = self.text.chars().count();
        if self.cursor >= len {
            return;
        }
        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        let len = self.text.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    fn byte_index(&self, char_idx: usize) -> usize {
        self.text.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(self.text.len())
    }
}

/// The SSH-HOST dropdown of the create/edit form.
///
/// Rendered as an inline list attached to the SSH HOST field (not a separate
/// modal), so choosing a host from `~/.ssh/config` is the obvious path rather
/// than a hidden shortcut. `open` controls whether the list is expanded.
#[derive(Debug, Clone, Default)]
pub struct HostPicker {
    pub open: bool,
    pub query: String,
    /// Indices into the caller-supplied host list, best match first.
    pub matches: Vec<usize>,
    pub selected: usize,
    /// First visible row in the dropdown (scrolling window).
    pub scroll: u16,
}

impl HostPicker {
    pub fn new() -> Self {
        HostPicker::default()
    }

    /// `DOWN` arrow marker shown inside the field when the list is collapsed.
    pub fn marker(&self) -> &'static str {
        if self.open { "▴" } else { "▾" }
    }

    pub fn query_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn query_backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.scroll = 0;
    }

    /// Move down, keeping the selection inside a `visible`-row window.
    pub fn down(&mut self, visible: usize) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
            self.ensure_visible(visible);
        }
    }

    /// Move up, keeping the selection inside a `visible`-row window.
    pub fn up(&mut self, visible: usize) {
        self.selected = self.selected.saturating_sub(1);
        self.ensure_visible(visible);
    }

    pub fn home(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn end(&mut self, visible: usize) {
        self.selected = self.matches.len().saturating_sub(1);
        self.ensure_visible(visible);
    }

    /// Clip `scroll` so the selected row is inside the viewport.
    ///
    /// `selected` is clamped to the match list first: it is a public field and
    /// the query can shrink the list at any time, which would otherwise let the
    /// `u16` arithmetic below overflow.
    pub fn ensure_visible(&mut self, visible: usize) {
        let last = self.matches.len().saturating_sub(1);
        let sel = self.selected.min(last);
        self.selected = sel;
        if visible == 0 {
            self.scroll = sel.min(u16::MAX as usize) as u16;
            return;
        }
        let scroll = self.scroll as usize;
        if sel < scroll {
            self.scroll = sel.min(u16::MAX as usize) as u16;
        } else if sel >= scroll.saturating_add(visible) {
            self.scroll = (sel + 1 - visible).min(u16::MAX as usize) as u16;
        }
    }

    pub fn current(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }
}

/// Non-modal single-line message shown at the bottom of the screen.
#[derive(Debug, Clone)]
pub struct StatusMsg {
    pub text: String,
    pub level: LogLevel,
    pub at: Instant,
}

impl StatusMsg {
    pub fn info(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Info, at: Instant::now() }
    }

    pub fn ok(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Info, at: Instant::now() }
    }

    pub fn warn(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Warn, at: Instant::now() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Error, at: Instant::now() }
    }

    /// Messages fade out after ~4s.
    pub fn expired(&self) -> bool {
        self.at.elapsed().as_millis() > 4000
    }
}

/// Simple case-insensitive subsequence matcher used by the host picker and the
/// table filter. Empty needle matches everything.
pub fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut hay = haystack.chars().flat_map(|c| c.to_lowercase());
    for nc in needle.chars().flat_map(|c| c.to_lowercase()) {
        if nc == ' ' {
            continue;
        }
        loop {
            match hay.next() {
                Some(hc) if hc == nc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_spec_local() {
        let cfg = TunnelConfig {
            id: 1,
            name: "x".into(),
            kind: ForwardKind::Local,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "10.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        };
        assert_eq!(cfg.forward_spec(), "127.0.0.1:8080:10.0.0.1:80");
        assert_eq!(cfg.local_display(), "127.0.0.1:8080");
        assert_eq!(cfg.remote_display(), "10.0.0.1:80");
    }

    #[test]
    fn input_buf_edits_multibyte() {
        let mut b = InputBuf::new("中文");
        assert_eq!(b.cursor, 2);
        b.left();
        b.insert('X');
        assert_eq!(b.value(), "中X文");
        b.backspace();
        assert_eq!(b.value(), "中文");
        b.delete();
        assert_eq!(b.value(), "中");
    }

    #[test]
    fn fuzzy() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("dbs", "db-staging"));
        assert!(fuzzy_match("DB", "db-staging"));
        assert!(!fuzzy_match("zz", "db-staging"));
    }

    #[test]
    fn log_ring_bounded() {
        let mut r = LogRing::new(3);
        for i in 0..10 {
            r.push(LogLine::new(LogLevel::Info, format!("{i}")));
        }
        assert_eq!(r.len(), 3);
        let tail: Vec<_> = r.iter().map(|l| l.text.clone()).collect();
        assert_eq!(tail, vec!["7", "8", "9"]);
    }

    fn sample(kind: ForwardKind) -> TunnelConfig {
        TunnelConfig {
            id: 1,
            name: "t".into(),
            kind,
            ssh_host: "h".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 8080,
            remote_host: "10.0.0.1".into(),
            remote_port: 80,
            enabled: false,
            auto_restart: false,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        }
    }

    #[test]
    fn forward_kind_tags_and_labels() {
        assert_eq!(ForwardKind::Local.tag(), 'L');
        assert_eq!(ForwardKind::Remote.tag(), 'R');
        assert_eq!(ForwardKind::Dynamic.tag(), 'D');
        assert!(ForwardKind::Local.label().contains("-L"));
        assert_eq!(ForwardKind::all().len(), 3);
        assert_eq!(ForwardKind::default(), ForwardKind::Local);
    }

    #[test]
    fn forward_spec_for_each_kind() {
        // -L: local listen -> remote target
        assert_eq!(sample(ForwardKind::Local).forward_spec(), "127.0.0.1:8080:10.0.0.1:80");
        // -R reverses the two ends: remote listen -> local target
        assert_eq!(sample(ForwardKind::Remote).forward_spec(), "10.0.0.1:80:127.0.0.1:8080");
        // -D only needs a listen address
        assert_eq!(sample(ForwardKind::Dynamic).forward_spec(), "127.0.0.1:8080");
    }

    #[test]
    fn displays_are_readable_for_each_kind() {
        let local = sample(ForwardKind::Local);
        assert_eq!(local.local_display(), "127.0.0.1:8080");
        assert_eq!(local.remote_display(), "10.0.0.1:80");

        // A dynamic forward has no remote target: say so rather than printing
        // a stale host:port.
        assert_eq!(sample(ForwardKind::Dynamic).remote_display(), "(SOCKS5)");
    }

    #[test]
    fn privileged_port_detection_follows_the_listen_side() {
        // -L and -D listen locally, -R listens remotely.
        let mut local = sample(ForwardKind::Local);
        local.local_port = 80;
        assert!(local.uses_privileged_port());

        let mut remote = sample(ForwardKind::Remote);
        remote.remote_port = 80;
        assert!(remote.uses_privileged_port(), "-R binds the remote port");
        // Once the remote listen port is unprivileged we are fine again, even
        // though the local *target* port is 8080 (we never bind it for -R).
        remote.remote_port = 9999;
        assert!(!remote.uses_privileged_port());

        let mut dynamic = sample(ForwardKind::Dynamic);
        dynamic.local_port = 1080;
        assert!(!dynamic.uses_privileged_port());
        dynamic.local_port = 1023;
        assert!(dynamic.uses_privileged_port());
        dynamic.local_port = 1024;
        assert!(!dynamic.uses_privileged_port(), "1024 is the first unprivileged port");
    }

    #[test]
    fn every_failure_kind_has_a_non_empty_short_phrase() {
        let kinds = [
            FailureKind::PortInUse,
            FailureKind::PermissionDenied,
            FailureKind::AuthFailed,
            FailureKind::DnsFailure,
            FailureKind::ConnectionRefused,
            FailureKind::Timeout,
            FailureKind::HostKeyMismatch,
            FailureKind::RemoteForwardFailed,
            FailureKind::TargetUnreachable,
            FailureKind::SpawnFailed,
            FailureKind::Exited,
            FailureKind::Unsupported,
            FailureKind::Unknown,
        ];
        for k in kinds {
            assert!(!k.short().is_empty(), "{k:?} has no short phrase");
            assert!(!k.short().contains('\n'), "{k:?} must stay single-line for the table");
        }
    }

    #[test]
    fn retryable_covers_network_class_only() {
        // Network hiccups may heal on their own.
        for k in [
            FailureKind::ConnectionRefused,
            FailureKind::Timeout,
            FailureKind::DnsFailure,
            FailureKind::TargetUnreachable,
            FailureKind::Exited,
            FailureKind::Unknown,
        ] {
            assert!(k.is_retryable(), "{k:?} should be retryable");
        }
        // Configuration problems need the user to change something.
        for k in [
            FailureKind::PortInUse,
            FailureKind::PermissionDenied,
            FailureKind::AuthFailed,
            FailureKind::HostKeyMismatch,
            FailureKind::RemoteForwardFailed,
            FailureKind::SpawnFailed,
            FailureKind::Unsupported,
        ] {
            assert!(!k.is_retryable(), "{k:?} must not auto-retry");
        }
    }

    #[test]
    fn status_helpers_agree_with_the_variant() {
        let up = TunnelStatus::Up { since: Instant::now() };
        assert!(up.is_active());
        assert!(up.is_running_or_pending());
        assert_eq!(up.key(), "up");
        assert!(up.failure().is_none());

        let starting = TunnelStatus::Starting { since: Instant::now() };
        assert!(starting.is_active());
        assert_eq!(starting.key(), "starting");

        let stopped = TunnelStatus::Stopped;
        assert!(!stopped.is_active());
        assert!(!stopped.is_running_or_pending());
        assert_eq!(stopped.key(), "stopped");

        let info = FailureInfo::new(FailureKind::AuthFailed, "detail");
        let failed = TunnelStatus::Failed { reason: info.clone() };
        assert!(!failed.is_active());
        // A failed tunnel is not "running", but it is not "off" either: the user
        // asked for it to run and it is not running, so retry/stop apply.
        assert!(failed.is_running_or_pending());
        assert_eq!(failed.key(), "failed");
        assert_eq!(failed.failure(), Some(&info));

        let retrying = TunnelStatus::Retrying { attempt: 3, next_at: Instant::now(), reason: info };
        assert_eq!(retrying.key(), "retrying");
        assert!(retrying.failure().is_some());
    }

    #[test]
    fn failure_info_constructors() {
        let f = FailureInfo::new(FailureKind::Timeout, "raw stderr");
        assert_eq!(f.short, FailureKind::Timeout.short());
        assert_eq!(f.detail, "raw stderr");

        let g = FailureInfo::with_short(FailureKind::PortInUse, "端口 8080 被占用", "bind ...");
        assert_eq!(g.short, "端口 8080 被占用");
        assert_eq!(g.kind, FailureKind::PortInUse);
    }

    #[test]
    fn tunnel_view_has_no_error_when_healthy() {
        let mut cfg = sample(ForwardKind::Local);
        cfg.enabled = false;
        let mut v = TunnelView::new(cfg);
        v.status = TunnelStatus::Up { since: Instant::now() };
        assert!(v.last_error().is_none());

        v.status =
            TunnelStatus::Failed { reason: FailureInfo::with_short(FailureKind::AuthFailed, "认证失败", "stderr") };
        assert_eq!(v.last_error().as_deref(), Some("认证失败"));
    }

    #[test]
    fn log_ring_tail_and_clear() {
        let mut r = LogRing::new(5);
        for i in 0..8 {
            r.push(LogLine::new(LogLevel::Info, format!("{i}")));
        }
        assert_eq!(r.len(), 5);
        let tail: Vec<_> = r.tail(2).into_iter().map(|l| l.text).collect();
        assert_eq!(tail, vec!["6", "7"]);
        // Asking for more than we have returns everything, oldest first.
        assert_eq!(r.tail(100).len(), 5);
        assert_eq!(r.last().map(|l| l.text.as_str()), Some("7"));
        r.clear();
        assert!(r.is_empty());
        assert!(r.last().is_none());
        assert_eq!(r.tail(3).len(), 0);
    }

    #[test]
    fn log_line_timestamps_are_hms() {
        let l = LogLine::new(LogLevel::Warn, "x");
        assert_eq!(l.time.len(), 8, "expected HH:MM:SS, got {}", l.time);
        assert_eq!(l.time.matches(':').count(), 2);
    }

    #[test]
    fn input_buf_cursor_navigation() {
        let mut b = InputBuf::new("abc");
        assert_eq!(b.cursor, 3, "cursor starts at the end");
        b.home();
        assert_eq!(b.cursor, 0);
        b.insert('X');
        assert_eq!(b.value(), "Xabc");
        // Currently "Xabc" with the cursor after "X".
        b.end();
        assert_eq!(b.cursor, 4);
        b.left();
        b.left();
        assert_eq!(b.cursor, 2); // between 'a' and 'b'
        b.delete(); // delete the char under the cursor
        assert_eq!(b.value(), "Xac");
        b.right();
        b.right();
        b.right(); // no-op past the end
        assert_eq!(b.cursor, 3);
        b.clear();
        assert!(b.value().is_empty());
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn input_buf_backspace_at_start_is_a_noop() {
        let mut b = InputBuf::new("ab");
        b.home();
        b.backspace();
        assert_eq!(b.value(), "ab");
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn input_buf_set_moves_cursor_to_end() {
        let mut b = InputBuf::new("abc");
        b.home();
        b.set("xyz");
        assert_eq!(b.value(), "xyz");
        assert_eq!(b.cursor, 3);
    }

    #[test]
    fn form_field_metadata() {
        // Every field is reachable by cycling, and text fields are flagged.
        for f in FormField::ORDER {
            assert!(!f.label().is_empty());
        }
        assert!(FormField::Name.is_text());
        assert!(FormField::SshHost.is_text());
        assert!(FormField::LocalPort.is_text());
        assert!(!FormField::Kind.is_text());
        assert!(!FormField::AutoRestart.is_text());
        // Host comes first: it is the field the user fills first.
        assert_eq!(FormField::ORDER[0], FormField::SshHost);
    }

    #[test]
    fn host_picker_scroll_window() {
        let mut p = HostPicker::new();
        p.matches = (0..20).collect();
        assert!(!p.open);
        assert_eq!(p.marker(), "▾");
        p.open = true;
        assert_eq!(p.marker(), "▴");

        // Walking down past the window pushes the viewport along.
        for _ in 0..10 {
            p.down(5);
        }
        assert_eq!(p.selected, 10);
        assert!(p.scroll <= 10);
        assert!(10 < p.scroll as usize + 5, "selection must stay visible");

        p.home();
        assert_eq!((p.selected, p.scroll), (0, 0));
        p.end(5);
        assert_eq!(p.selected, 19);
        assert_eq!(p.current(), Some(19));

        // A shrinking list must re-clamp the viewport.
        p.matches.truncate(3);
        p.ensure_visible(5);
        assert!(p.scroll as usize <= p.selected);
    }

    #[test]
    fn host_picker_queries_reset_the_cursor() {
        let mut p = HostPicker::new();
        p.matches = (0..5).collect();
        p.selected = 3;
        p.scroll = 2;
        p.query_char('a');
        assert_eq!((p.selected, p.scroll), (0, 0));
        p.selected = 2;
        p.query_backspace();
        assert_eq!((p.selected, p.scroll), (0, 0));
        assert_eq!(p.query, "");
    }

    #[test]
    fn fuzzy_match_edge_cases() {
        assert!(fuzzy_match("", ""), "empty needle matches empty haystack");
        assert!(fuzzy_match("", "abc"));
        assert!(!fuzzy_match("abc", ""), "non-empty needle cannot match nothing");
        assert!(fuzzy_match("abc", "abc"));
        assert!(fuzzy_match("ac", "abc"), "subsequence, not substring");
        assert!(!fuzzy_match("ca", "abc"), "order matters");
        assert!(fuzzy_match(" ssh ", "my-ssh-host"), "spaces in the needle are ignored");
        assert!(fuzzy_match("SSH", "ssh-box"), "case insensitive");
    }

    #[test]
    fn status_msg_expires() {
        let fresh = StatusMsg::info("hi");
        assert!(!fresh.expired(), "a just-created message is visible");
        let mut old = StatusMsg::ok("hi");
        old.at = Instant::now() - std::time::Duration::from_secs(10);
        assert!(old.expired(), "messages fade out");
        // All constructors carry a level the UI can colour by.
        assert_eq!(StatusMsg::error("e").level, LogLevel::Error);
        assert_eq!(StatusMsg::warn("w").level, LogLevel::Warn);
        assert_eq!(StatusMsg::ok("o").level, LogLevel::Info);
    }

    // -- shared local-port planning ----------------------------------------

    #[test]
    fn a_free_unprivileged_remote_port_is_reused_verbatim() {
        let (port, reason) = plan_local_port(8080, &[], |_| true).unwrap();
        assert_eq!(port, 8080);
        assert_eq!(reason, LocalPortReason::SameAsRemote);
    }

    #[test]
    fn a_privileged_remote_port_gets_an_unprivileged_alias() {
        // The trap this pins: scanning up from 80 would stay below 1024, where
        // every bind needs root. The old importer code did exactly that and
        // produced another privileged port (280 for a requested 80).
        for (remote, alias) in [(22u16, 2222u16), (80, 2280), (443, 2643)] {
            let (port, reason) = plan_local_port(remote, &[], |_| true).unwrap();
            assert_eq!(port, alias, "remote {remote} should alias to {alias}");
            assert!(port >= 1024, "alias for {remote} must be unprivileged, got {port}");
            assert_eq!(reason, LocalPortReason::PrivilegedAlias);
        }
    }

    #[test]
    fn a_privileged_remote_never_yields_a_privileged_result() {
        // Even when the alias is taken, the fallback scan must stay >= 1024.
        for remote in 1u16..=1023 {
            let (port, _) = plan_local_port(remote, &[], |_| true).expect("should always find something");
            assert!(port >= 1024, "remote {remote} produced privileged local {port}");
        }
    }

    #[test]
    fn a_taken_port_is_bumped_upwards() {
        // 8080 taken by another tunnel -> the next free one, never a lower port.
        let (port, reason) = plan_local_port(8080, &[8080], |c| c != 8081).unwrap();
        assert_eq!(port, 8082, "must skip both the taken and the unavailable port");
        assert_eq!(reason, LocalPortReason::Bumped);
        assert!(port > 8080, "bumping must not wrap downwards");
    }

    #[test]
    fn taken_ports_below_the_unprivileged_floor_are_irrelevant() {
        // A privileged port in `taken` cannot affect the >=1024 result.
        let (port, _) = plan_local_port(8080, &[22, 80], |_| true).unwrap();
        assert_eq!(port, 8080);
    }

    #[test]
    fn port_zero_is_not_plannable() {
        assert!(plan_local_port(0, &[], |_| true).is_none(), "port 0 means 'unknown', not 'pick one'");
    }

    #[test]
    fn planning_gives_up_rather_than_hanging_when_nothing_is_free() {
        // A closure that rejects everything must terminate, not spin.
        assert!(plan_local_port(8080, &[], |_| false).is_none());
    }

    #[test]
    fn planning_never_lands_inside_the_taken_set() {
        // Take a contiguous block and check the plan lands outside it.
        let taken: Vec<u16> = (8080..8085).collect();
        let (port, _) = plan_local_port(8080, &taken, |_| true).unwrap();
        assert!(!taken.contains(&port), "{port} is in the taken set");
        assert_eq!(port, 8085);
    }

    // -- ssh-config import table -------------------------------------------

    fn fwd(kind: ForwardKind, listen: u16, target: (&str, u16)) -> SshForward {
        SshForward {
            kind,
            listen_addr: "127.0.0.1".into(),
            listen_port: listen,
            target_host: target.0.into(),
            target_port: target.1,
            host_alias: "dev-box".into(),
            line: 7,
            raw: format!("{:?}Forward {listen} {}:{}", kind, target.0, target.1),
        }
    }

    fn sample_table() -> ImportTable {
        ImportTable::new(vec![
            fwd(ForwardKind::Local, 8080, ("10.0.0.1", 80)),
            fwd(ForwardKind::Local, 9090, ("10.0.0.2", 90)),
            fwd(ForwardKind::Remote, 7000, ("127.0.0.1", 22)),
            fwd(ForwardKind::Dynamic, 1080, ("", 0)),
        ])
    }

    #[test]
    fn every_importable_row_starts_checked() {
        // Requirement: the host's existing forwards are the default, so creating
        // a tunnel should already carry them without any clicking.
        let t = sample_table();
        assert_eq!(t.checked_count(), 4);
        assert_eq!(t.importable_count(), 4);
        assert!(t.rows.iter().all(|r| r.checked));
        assert_eq!(t.checked_forwards().len(), 4);
    }

    #[test]
    fn unimportable_rows_are_never_checked_by_default() {
        let mut bad = fwd(ForwardKind::Local, 0, ("", 0));
        bad.listen_port = 0;
        let t = ImportTable::new(vec![bad, fwd(ForwardKind::Local, 8080, ("10.0.0.1", 80))]);
        assert_eq!(t.len(), 2);
        assert_eq!(t.importable_count(), 1, "the broken row is listed but not importable");
        assert!(!t.rows[0].checked);
        assert!(t.rows[1].checked);
        assert_eq!(t.checked_forwards().len(), 1);
    }

    #[test]
    fn toggling_ignores_unimportable_rows() {
        let mut bad = fwd(ForwardKind::Local, 0, ("", 0));
        bad.listen_port = 0;
        let mut t = ImportTable::new(vec![bad]);
        t.toggle_cursor();
        assert!(!t.rows[0].checked, "a row that cannot be imported must not become checked");
        t.check_all_visible();
        assert!(!t.rows[0].checked, "select-all must skip it too");
    }

    #[test]
    fn column_filters_are_anded_and_case_insensitive() {
        let mut t = sample_table();
        t.set_filter(ImportColumn::Kind, "L");
        assert_eq!(t.visible(), vec![0, 1], "only Local rows");
        t.set_filter(ImportColumn::Listen, "9090");
        assert_eq!(t.visible(), vec![1], "both filters must hold");
        // Substring, not prefix: "0.0." only appears in the middle of the
        // rendered listen address.
        t.set_filter(ImportColumn::Listen, "0.1:9");
        assert_eq!(t.visible(), vec![1], "a substring match is enough");
        t.set_filter(ImportColumn::Listen, "127.0.0.1");
        assert_eq!(t.visible(), vec![0, 1], "every Local row listens on loopback");
        t.set_filter(ImportColumn::Target, "10.0.0.2");
        assert_eq!(t.visible(), vec![1]);
        t.clear_all_filters();
        assert_eq!(t.visible().len(), 4);
    }

    #[test]
    fn filters_match_case_insensitively_and_ignore_padding() {
        let t = ImportTable::new(vec![SshForward {
            kind: ForwardKind::Local,
            listen_addr: String::new(),
            listen_port: 8080,
            target_host: "LocalHost".into(),
            target_port: 80,
            host_alias: "h".into(),
            line: 1,
            raw: "LocalForward 8080 LocalHost:80".into(),
        }]);
        assert!(col_match("localhost", &ImportColumn::Target.cell(&t.rows[0].forward)));
        assert!(col_match("  LOCAL  ", "localhost"), "surrounding spaces must not break a filter");
        assert!(col_match("", "anything"), "an empty filter matches everything");
    }

    #[test]
    fn an_empty_listen_addr_renders_as_the_openssh_default() {
        let f = SshForward {
            kind: ForwardKind::Local,
            listen_addr: String::new(),
            listen_port: 8080,
            target_host: "10.0.0.1".into(),
            target_port: 80,
            host_alias: "h".into(),
            line: 1,
            raw: String::new(),
        };
        assert_eq!(f.listen_display(), "localhost:8080", "an omitted bind address is localhost");
    }

    #[test]
    fn filter_undo_rolls_back_one_step_at_a_time() {
        let mut t = sample_table();
        assert!(!t.can_undo_filter());
        t.set_filter(ImportColumn::Kind, "L");
        t.set_filter(ImportColumn::Listen, "8080");
        assert_eq!(t.visible(), vec![0]);
        assert!(t.undo_filter());
        assert_eq!(t.filter(ImportColumn::Listen), "", "the newest change is undone first");
        assert_eq!(t.visible(), vec![0, 1]);
        assert!(t.undo_filter());
        assert!(!t.has_filters());
        assert!(!t.undo_filter(), "nothing left to undo must report false, not panic");
    }

    #[test]
    fn clearing_an_already_empty_filter_is_not_an_undo_step() {
        // Otherwise `F` on an unfiltered column would push a snapshot and make
        // `z` look broken (it would appear to do nothing).
        let mut t = sample_table();
        t.clear_filter(ImportColumn::Kind);
        t.clear_all_filters();
        assert!(!t.can_undo_filter());
    }

    #[test]
    fn filter_history_is_bounded() {
        let mut t = sample_table();
        for i in 0..(FILTER_HISTORY_MAX + 20) {
            t.set_filter(ImportColumn::Kind, i.to_string());
        }
        let mut undoable = 0;
        while t.undo_filter() {
            undoable += 1;
        }
        assert_eq!(undoable, FILTER_HISTORY_MAX, "the undo stack must not grow without bound");
    }

    #[test]
    fn changing_a_filter_reanchors_a_dangling_cursor() {
        // A filter that hides everything must not leave the cursor pointing at
        // row 5 of a one-row view.
        let mut t = sample_table();
        t.cursor = 3;
        t.set_filter(ImportColumn::Listen, "8080");
        assert_eq!(t.cursor, 0);
        assert!(t.cursor_row().is_some());
        t.set_filter(ImportColumn::Listen, "zzz-no-match");
        assert_eq!(t.visible(), Vec::<usize>::new());
        assert!(t.cursor_row().is_none(), "no match means no cursor row, not a panic");
        t.move_cursor(1);
        assert_eq!(t.cursor, 0, "moving in an empty view is a no-op");
    }

    #[test]
    fn select_all_only_touches_the_filtered_view() {
        let mut t = sample_table();
        t.uncheck_all_visible();
        assert_eq!(t.checked_count(), 0);
        t.set_filter(ImportColumn::Kind, "D");
        t.check_all_visible();
        assert_eq!(t.visible(), vec![3]);
        assert_eq!(t.checked_count(), 1, "only the visible row may be ticked");
        // Clearing the filter shows the others are still unticked.
        t.clear_all_filters();
        assert_eq!(t.checked_count(), 1);
    }

    #[test]
    fn invert_flips_only_the_visible_importable_rows() {
        let mut t = sample_table();
        t.set_filter(ImportColumn::Kind, "L");
        t.invert_visible();
        assert_eq!(t.checked_count(), 2, "the two Local rows flipped on... to off; the others stay on");
        // Initially 4 checked; flipping 2 visible ones leaves 2 checked.
        assert!(!t.rows[0].checked && !t.rows[1].checked);
        assert!(t.rows[2].checked && t.rows[3].checked, "hidden rows must not change");
    }

    #[test]
    fn checked_forwards_keep_file_order() {
        let t = sample_table();
        let ports: Vec<u16> = t.checked_forwards().iter().map(|f| f.listen_port).collect();
        assert_eq!(ports, vec![8080, 9090, 7000, 1080]);
    }

    #[test]
    fn summary_reports_counts_and_active_filters() {
        let mut t = sample_table();
        assert_eq!(t.summary(), "4/4 条勾选");
        t.uncheck_all_visible();
        t.set_filter(ImportColumn::Kind, "L");
        let s = t.summary();
        assert!(s.contains("0/4"), "{s}");
        assert!(s.contains("类型=L"), "{s}");
    }

    #[test]
    fn cursor_movement_clamps_instead_of_wrapping() {
        let mut t = sample_table();
        t.move_cursor(99);
        assert_eq!(t.cursor, 3, "clamps at the last row");
        t.move_cursor(-99);
        assert_eq!(t.cursor, 0);
        t.end();
        assert_eq!(t.cursor, 3);
        t.home();
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn cursor_stays_inside_a_scrolled_window() {
        let mut t = sample_table();
        t.ensure_visible(2);
        t.move_cursor(3);
        t.ensure_visible(2);
        // cursor 3 in a 2-row window must put the window at rows 2..=3.
        assert_eq!(t.scroll, 2);
        t.home();
        t.ensure_visible(2);
        assert_eq!(t.scroll, 0, "scrolling back up must follow the cursor");
    }

    #[test]
    fn import_columns_cycle_and_expose_their_cells() {
        let mut col = ImportColumn::Kind;
        for _ in 0..ImportColumn::COUNT {
            assert!(!col.label().is_empty());
            col = col.next();
        }
        assert_eq!(col, ImportColumn::Kind, "cycling wraps");
        assert_eq!(ImportColumn::Kind.prev(), ImportColumn::Raw);

        let f = fwd(ForwardKind::Dynamic, 1080, ("", 0));
        assert_eq!(ImportColumn::Kind.cell(&f), "D");
        assert_eq!(ImportColumn::Target.cell(&f), "(SOCKS5)", "a SOCKS row has no target");
    }

    // -- multi-select ------------------------------------------------------

    #[test]
    fn multi_select_enter_marks_the_cursor_and_exit_clears() {
        let mut ms = MultiSelect::new();
        assert!(!ms.active);
        ms.enter();
        assert!(ms.active);
        assert!(ms.is_empty(), "entering must not pre-mark anything");
        ms.mark_all(&[7]);
        assert_eq!(ms.ids(), vec![7]);
        ms.exit();
        assert!(!ms.active);
        assert!(ms.is_empty());
        assert_eq!(ms.anchor, None);
    }

    #[test]
    fn multi_select_toggle_and_set() {
        let mut ms = MultiSelect::new();
        ms.enter();
        ms.toggle(1);
        ms.toggle(2);
        assert_eq!(ms.len(), 2);
        ms.toggle(1);
        assert_eq!(ms.ids(), vec![2], "a second toggle removes");
        ms.set(2, false);
        assert!(ms.is_empty());
        ms.set(3, true);
        assert_eq!(ms.ids(), vec![3]);
    }

    #[test]
    fn multi_select_range_is_inclusive_and_order_insensitive() {
        let ids: Vec<TunnelId> = (1..=6).collect();
        let mut ms = MultiSelect::new();
        ms.mark_range(&ids, 1, 3);
        assert_eq!(ms.ids(), vec![2, 3, 4]);
        // Dragging backwards marks the same set.
        ms.enter();
        ms.mark_range(&ids, 4, 2);
        assert_eq!(ms.ids(), vec![3, 4, 5]);
        // Out-of-range indices are clamped, not a panic.
        ms.enter();
        ms.mark_range(&ids, 0, 99);
        assert_eq!(ms.len(), 6);
        ms.enter();
        ms.mark_range(&[], 0, 2);
        assert!(ms.is_empty(), "an empty view marks nothing");
    }

    #[test]
    fn retain_existing_drops_marks_for_deleted_tunnels() {
        let mut ms = MultiSelect::new();
        ms.enter();
        ms.mark_all(&[1, 2, 3]);
        ms.retain_existing(|id| id != 2);
        assert_eq!(ms.ids(), vec![1, 3]);
        ms.retain_existing(|_| false);
        assert!(ms.is_empty());
        assert_eq!(ms.anchor, None, "an empty selection must not keep an anchor");
    }

    #[test]
    fn marked_ids_are_sorted_for_stable_prompts() {
        let mut ms = MultiSelect::new();
        ms.enter();
        for id in [9, 3, 5] {
            ms.mark_all(&[id]);
        }
        assert_eq!(ms.ids(), vec![3, 5, 9]);
    }

    #[test]
    fn every_batch_action_has_a_label_and_a_counted_prompt() {
        for action in BatchAction::ALL {
            assert!(!action.label().is_empty());
            let prompt = action.prompt(3);
            assert!(prompt.contains('3'), "{action:?} prompt must state the count: {prompt}");
            assert!(!prompt.contains('\n'), "prompts must stay single-line");
        }
        assert_eq!(BatchAction::ALL.len(), 4);
    }

    #[test]
    fn form_field_cycles() {
        let mut f = FormField::SshHost;
        for _ in 0..FormField::ORDER.len() {
            f = f.next();
        }
        assert_eq!(f, FormField::SshHost);
        assert_eq!(FormField::SshHost.prev(), FormField::AutoRestart);
    }

    // -- panic hardening: index/scroll invariants --------------------------
    //
    // These pin the "the scalar came from somewhere untrusted" paths: a column
    // list that can drift from the filter array, a scroll offset written
    // directly, and cursor indices supplied by a caller.

    #[test]
    fn every_import_column_index_is_a_valid_filter_slot() {
        // `ImportTable::filters` is `[String; ImportColumn::COUNT]` and is
        // indexed by `ImportColumn::index()`. The two used to be hand-numbered,
        // so a new variant could describe a slot the array does not have.
        let table = ImportTable::empty();
        for col in ImportColumn::ALL {
            let index = col.index();
            assert!(index < ImportColumn::COUNT, "{col:?} maps to {index}, past COUNT");
            assert!(index < table.filters.len(), "{col:?} would index outside the filter array");
        }
    }

    #[test]
    fn import_column_cycling_visits_every_column_from_every_start() {
        // `next`/`prev` used `% COUNT` on a hand-written index; verify the wrap
        // holds for every starting column, in both directions.
        for start in ImportColumn::ALL {
            let mut seen = Vec::new();
            let mut col = start;
            for _ in 0..ImportColumn::COUNT {
                seen.push(col);
                col = col.next();
            }
            assert_eq!(col, start, "next must come back to {start:?}");
            assert_eq!(seen.len(), ImportColumn::COUNT);
            let mut seen = Vec::new();
            let mut col = start;
            for _ in 0..ImportColumn::COUNT {
                seen.push(col);
                col = col.prev();
            }
            assert_eq!(col, start, "prev must come back to {start:?}");
            assert_eq!(seen.len(), ImportColumn::COUNT);
        }
    }

    #[test]
    fn filter_accessors_tolerate_a_null_scroll_target() {
        // `filter()` returning "" for an unknown slot is what lets `visible()` /
        // `summary()` use it without a bounds check that could be stale.
        let mut table = sample_table();
        for col in ImportColumn::ALL {
            assert!(table.filter(col).is_empty());
            table.set_filter(col, "x");
            assert_eq!(table.filter(col), "x");
            table.clear_filter(col);
            assert!(table.filter(col).is_empty());
        }
    }

    #[test]
    fn import_ensure_visible_survives_a_scroll_left_at_u16_max() {
        // `scroll` is a public `u16` field. The old code compared `cursor` and
        // `scroll + height` in `u16`, so a scroll near 65535 overflowed (panic in
        // a debug build — which is what CI runs).
        let mut table = sample_table();
        table.scroll = u16::MAX;
        table.cursor = 0;
        table.ensure_visible(5); // must not overflow
        assert_eq!(table.scroll, 0, "the window must follow the cursor back up");

        // The actual overflowing pair: both ends at the top of the `u16` range.
        table.scroll = u16::MAX - 1;
        table.cursor = u16::MAX as usize;
        table.ensure_visible(2); // must not overflow either
        assert!(table.scroll as usize <= table.cursor);
    }

    #[test]
    fn import_ensure_visible_handles_a_zero_height_viewport() {
        // A pane squeezed to nothing must not divide or subtract by zero.
        let mut table = sample_table();
        table.cursor = 2;
        table.ensure_visible(0);
        assert_eq!(table.scroll, 2);
    }

    #[test]
    fn import_ensure_visible_with_a_cursor_past_the_end_is_clamped() {
        let mut table = sample_table();
        table.cursor = 99;
        table.ensure_visible(2);
        assert!(table.scroll as usize <= table.cursor);
    }

    #[test]
    fn picker_ensure_visible_survives_a_scroll_left_at_u16_max() {
        // Same `u16` overflow as the import table: `scroll + visible`.
        let mut p = HostPicker::new();
        p.matches = (0..20).collect();
        p.scroll = u16::MAX;
        p.selected = 0;
        p.ensure_visible(5);
        assert_eq!(p.scroll, 0);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn picker_selection_is_clamped_to_the_match_list() {
        // `selected` is public and the query can shrink `matches` at any time;
        // an out-of-range selection must degrade to the last row, not panic.
        let mut p = HostPicker::new();
        p.matches = (0..3).collect();
        p.selected = 99;
        p.ensure_visible(5);
        assert_eq!(p.selected, 2);
        assert_eq!(p.current(), Some(2));

        p.matches.clear();
        p.selected = 5;
        p.ensure_visible(5);
        assert_eq!(p.selected, 0);
        assert_eq!(p.scroll, 0);
        assert_eq!(p.current(), None);
    }

    #[test]
    fn picker_end_keeps_the_last_row_inside_the_window() {
        // `end` used to move the selection without moving the viewport, leaving
        // the highlighted row scrolled out of sight.
        let mut p = HostPicker::new();
        p.matches = (0..20).collect();
        p.end(5);
        assert_eq!(p.selected, 19);
        assert!(p.selected < p.scroll as usize + 5, "the last row must be visible: scroll={}", p.scroll);
    }

    #[test]
    fn mark_range_with_extreme_indices_marks_a_clamped_range() {
        // `from`/`to` come from a cursor index; `usize::MAX` must clamp instead
        // of slicing out of bounds (and the empty case must not underflow
        // `len() - 1`).
        let ids: Vec<TunnelId> = (1..=3).collect();
        let mut ms = MultiSelect::new();
        ms.enter();
        ms.mark_range(&ids, usize::MAX, usize::MAX);
        assert_eq!(ms.ids(), vec![3], "both ends clamp to the last row");
        ms.enter();
        ms.mark_range(&ids, 0, usize::MAX);
        assert_eq!(ms.ids(), vec![1, 2, 3]);
        ms.enter();
        ms.mark_range(&[], usize::MAX, 0);
        assert!(ms.is_empty());
        assert_eq!(ms.anchor, None, "an empty view must not record an anchor");
    }

    #[test]
    fn a_zero_capacity_log_ring_still_evicts() {
        // `push` evicted only when `len == cap`, so a `cap` of 0 never evicted
        // and the "bounded" ring grew without limit.
        let mut r = LogRing::new(0);
        for i in 0..10 {
            r.push(LogLine::new(LogLevel::Info, format!("{i}")));
        }
        assert_eq!(r.len(), 1, "an empty-cap ring must still hold at most one line");
        assert_eq!(r.last().map(|l| l.text.as_str()), Some("9"));
    }

    #[test]
    fn a_log_ring_push_with_a_cap_of_one_keeps_the_newest() {
        let mut r = LogRing::new(1);
        r.push(LogLine::new(LogLevel::Info, "a"));
        r.push(LogLine::new(LogLevel::Info, "b"));
        assert_eq!(r.len(), 1);
        assert_eq!(r.last().map(|l| l.text.as_str()), Some("b"));
    }

    #[test]
    fn an_input_buffer_with_a_stale_cursor_does_not_panic() {
        // `cursor` is a public field: a value past the end must clamp, not slice
        // at a byte offset the text does not have (and not overflow on `+= 1`).
        let mut b = InputBuf::new("abc");
        b.cursor = usize::MAX;
        b.insert('X');
        assert!(b.value().ends_with('X'), "the character must land at the end: {:?}", b.value());
        assert_eq!(b.value(), "abcX");

        b.cursor = usize::MAX;
        b.backspace();
        assert_eq!(b.value(), "abc");

        b.cursor = usize::MAX;
        b.delete();
        assert_eq!(b.value(), "abc", "delete at the end is a no-op");

        b.cursor = usize::MAX;
        b.right();
        b.left();
        b.home();
        b.end();
        assert_eq!(b.cursor, b.value().chars().count());
    }

    #[test]
    fn an_input_buffer_with_a_stale_cursor_edits_multibyte_text_safely() {
        let mut b = InputBuf::new("中文");
        b.cursor = 99;
        b.insert('X');
        assert_eq!(b.value(), "中文X");
        b.cursor = 1;
        b.insert('Y');
        assert_eq!(b.value(), "中Y文X");
    }

    #[test]
    fn form_field_navigation_wraps_in_both_directions() {
        // `next`/`prev` index a fixed array; a field missing from ORDER would
        // make `position` return `None`, and the old code then indexed blind.
        for field in FormField::ORDER {
            let next = field.next();
            let prev = field.prev();
            assert!(FormField::ORDER.contains(&next), "{field:?}.next() = {next:?} is not in ORDER");
            assert!(FormField::ORDER.contains(&prev), "{field:?}.prev() = {prev:?} is not in ORDER");
            assert_eq!(next.prev(), field, "next/prev must be inverses for {field:?}");
        }
        assert_eq!(FormField::ORDER.first().copied().unwrap().prev(), FormField::AutoRestart);
        assert_eq!(FormField::ORDER.last().copied().unwrap().next(), FormField::SshHost);
    }
}

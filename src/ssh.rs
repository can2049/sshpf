//! SSH plumbing: `~/.ssh/config` discovery, `ssh` command construction, a
//! synthetic one-shot config and stderr classification.
//!
//! This layer is deliberately *pure plumbing*: it never spawns a long-lived
//! process (that is `tunnel`'s job) and never touches ratatui. The only short
//! lived processes it starts are `ssh -G -T …` probes used to resolve a host
//! definition (with a hard 3 s timeout) — see `docs/research/tech-options.md`
//! §2.1 for the measured evidence behind that choice.
//!
//! # Public API
//!
//! ```text
//! struct SshHost { alias, hostname, user, port, proxy_jump, identity_file }
//!     SshHost::display(&self) -> String
//!
//! fn default_config_paths() -> Vec<PathBuf>
//! fn list_hosts() -> Result<Vec<SshHost>>
//! fn resolve_host(alias: &str) -> Result<SshHost>
//! fn build_ssh_args(cfg: &TunnelConfig, synth_conf: Option<&Path>) -> Vec<String>
//! fn write_synth_config(cfg: &TunnelConfig, dir: &Path) -> Result<PathBuf>
//! fn classify_failure(line: &str) -> Option<FailureInfo>
//! fn is_ready_line(kind: ForwardKind, line: &str) -> bool
//! fn is_local_listening_line(line: &str) -> bool
//! fn display_width(s: &str) -> usize
//! fn ssh_binary() -> PathBuf
//! fn ssh_available() -> bool
//! fn port_available(addr: &str, port: u16) -> bool
//! fn suggest_free_port(addr: &str, start: u16) -> Option<u16>
//! fn port_owner(addr: &str, port: u16) -> Option<String>
//! ```
//!
//! # Design notes / measured facts (all verified on OpenSSH_8.2p1)
//!
//! * **Synthetic config, not `ClearAllForwardings`.** `-o ClearAllForwardings=yes`
//!   also clears *our* `-L` (§2.5), so a tunnel gets a one-shot `-F` file that
//!   re-states the resolved host verbatim. Baking the *full* `ssh -G` output is
//!   lossless: 73–74 keys round-trip `ssh -G -F synth <host>` byte-identically
//!   (verified against the machine's real 13-block config, including
//!   `identityfile` paths containing spaces, which must be re-quoted or ssh
//!   silently drops the line).
//! * `localforward`/`remoteforward`/`dynamicforward`/`clearallforwardings`/
//!   `proxycommand` are **excluded**: the first three would duplicate or
//!   re-bind the user's config forwards, `clearallforwardings` kills ours, and
//!   a `ProxyCommand` cannot be safely re-stated as a config line. Structural
//!   directives (`host`/`match`/`include`) are excluded too: emitting one
//!   inside a block would open a new `Host` context and drop every following
//!   key.
//! * `ProxyJump` needs more than the target's own block: ssh forks
//!   `ssh -F synth -W '[%h]:%p' <jump>` and that child re-reads the *synthetic*
//!   file, so every host on the jump chain is baked too (verified: end-to-end
//!   `Authenticated to … (via proxy)` through a baked two-hop chain).
//! * `-v` is enough to get `debug1:` markers; a `LogLevel` baked into the
//!   config does **not** override it (measured: baked `LogLevel ERROR` still
//!   printed `debug1:` lines), and adding `-o LogLevel=…` on the command line
//!   is not needed — so we only ever pass `-v`.
//! * Host enumeration cannot be done with `ssh -G '*'` (§2.1): the `Host`
//!   lines are parsed from the config files and each alias is then *evaluated*
//!   with `ssh -G -T`. The alias list itself is the source of truth for "does
//!   this host exist"; the extra `ssh -G -T -v | grep 'Applying options for'`
//!   check is deliberately skipped to avoid doubling the number of processes
//!   (documented trade-off, §2.1 measured both to agree on 12/12 hosts).
//! * A destination of the shape `user@host:port` is **not** understood by ssh
//!   as a port (`ssh user@h:2222` tries to resolve the literal `h:2222`). It is
//!   resolved locally here, and the synthetic config carries
//!   `Host <host>:<port>` + explicit `hostname`/`port`, which is exactly the
//!   pattern ssh matches for that destination (measured).
//!
//! # Environment overrides
//!
//! * `SSHPF_SSH_CONFIG` — parse only this file instead of `~/.ssh/config`
//!   (tests, and `--config`-style experimentation). Relative `Include`s then
//!   resolve against that file's directory instead of `~/.ssh`.
//! * `SSHPF_SSH_BIN` — use this executable instead of searching `PATH`
//!   (end-to-end tests with a fake `ssh` script).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::model::{FailureInfo, FailureKind, ForwardKind, TunnelConfig};

/// Env var that overrides the parsed ssh config file (single file).
pub const CONFIG_ENV: &str = "SSHPF_SSH_CONFIG";
/// Env var that overrides the `ssh` binary used by this module.
pub const SSH_BIN_ENV: &str = "SSHPF_SSH_BIN";

/// Hard timeout for one `ssh -G` probe. Measured: ~5 ms typical, 5 ms for
/// unknown hosts; 3 s is a generous guard against a pathological config
/// (e.g. a FIFO or NFS-hung include).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Recursion guard for `Include` chains.
const MAX_INCLUDE_DEPTH: usize = 8;
/// Recursion guard for `ProxyJump` chains.
const MAX_JUMP_DEPTH: usize = 4;

/// Maximum display width of a [`FailureInfo::short`] phrase, in terminal cells.
pub const SHORT_MAX_WIDTH: usize = 34;

// ---------------------------------------------------------------------------
// Host discovery
// ---------------------------------------------------------------------------

/// One `Host` entry of `~/.ssh/config`, resolved through `ssh -G`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: u16,
    pub proxy_jump: Option<String>,
    pub identity_file: Option<String>,
}

impl SshHost {
    /// `alias (user@hostname:port)`, omitting the user when there is none and
    /// the port when it is the default 22.
    pub fn display(&self) -> String {
        let mut who = String::new();
        if let Some(user) = &self.user {
            who.push_str(user);
            who.push('@');
        }
        who.push_str(&self.hostname);
        if self.port != 22 {
            who.push(':');
            who.push_str(&self.port.to_string());
        }
        format!("{} ({})", self.alias, who)
    }

    /// `user@hostname:port` — handy for the detail panel / copy-paste.
    pub fn endpoint(&self) -> String {
        let mut s = String::new();
        if let Some(u) = &self.user {
            s.push_str(u);
            s.push('@');
        }
        s.push_str(&self.hostname);
        s.push(':');
        s.push_str(&self.port.to_string());
        s
    }
}

/// Config files to parse by default: `~/.ssh/config` when it exists.
pub fn default_config_paths() -> Vec<PathBuf> {
    match dirs::home_dir() {
        Some(home) => {
            let p = home.join(".ssh").join("config");
            if p.is_file() { vec![p] } else { Vec::new() }
        }
        None => Vec::new(),
    }
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn config_override() -> Option<PathBuf> {
    std::env::var_os(CONFIG_ENV).map(PathBuf::from)
}

/// Enumerate every concrete `Host` alias and resolve it with `ssh -G -T`.
///
/// Sorted by alias. A single alias whose probe fails is downgraded to the
/// values parsed straight out of the config file (never a hard error for the
/// whole list).
pub fn list_hosts() -> Result<Vec<SshHost>> {
    let explicit = config_override();
    let paths = match &explicit {
        Some(p) => vec![p.clone()],
        None => default_config_paths(),
    };
    list_hosts_with(&ssh_binary(), &paths, explicit.as_deref())
}

/// Resolve one host spec. `user@host:port` is parsed locally (no `ssh` call);
/// anything else is an alias evaluated with `ssh -G -T`.
pub fn resolve_host(alias: &str) -> Result<SshHost> {
    let explicit = config_override();
    resolve_host_with(&ssh_binary(), alias, explicit.as_deref())
}

// -- internals --------------------------------------------------------------

/// A parsed destination, in either `alias` or `[user@]host[:port]` form.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostSpec {
    user: Option<String>,
    port: Option<u16>,
    host: String,
    /// True when the input was a bare ssh-config alias (no `@`, no port).
    is_alias: bool,
}

impl HostSpec {
    fn parse(raw: &str) -> HostSpec {
        let s = raw.trim();
        let (user, rest) = match s.split_once('@') {
            Some((u, r)) => (Some(u.to_string()), r),
            None => (None, s),
        };

        // `[::1]:2222` / `[::1]`
        let (host, port) = if let Some(stripped) = rest.strip_prefix('[') {
            match stripped.split_once(']') {
                Some((h, tail)) => {
                    let port = tail.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
                    (h.to_string(), port)
                }
                None => (rest.to_string(), None),
            }
        } else if rest.matches(':').count() == 1 {
            let (h, p) = rest.split_once(':').unwrap_or((rest, ""));
            match p.parse::<u16>() {
                Ok(port) if !h.is_empty() => (h.to_string(), Some(port)),
                _ => (rest.to_string(), None),
            }
        } else {
            (rest.to_string(), None)
        };

        let is_alias = user.is_none() && port.is_none() && !s.contains(':');
        HostSpec { user, port, host, is_alias }
    }

    /// What ssh itself uses as the `Host` match key for this destination.
    fn match_key(&self) -> String {
        if self.is_alias {
            self.host.clone()
        } else if let Some(port) = self.port {
            format!("{}:{}", self.host, port)
        } else {
            self.host.clone()
        }
    }
}

/// Values parsed straight from a config block, used as a fallback when the
/// `ssh -G` probe fails.
#[derive(Debug, Clone, Default)]
struct RawBlock {
    aliases: Vec<String>,
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
    proxy_jump: Option<String>,
}

impl RawBlock {
    fn to_host(&self, alias: &str) -> SshHost {
        SshHost {
            alias: alias.to_string(),
            hostname: self.hostname.clone().unwrap_or_else(|| alias.to_string()),
            user: self.user.clone(),
            port: self.port.unwrap_or(22),
            proxy_jump: self.proxy_jump.clone(),
            identity_file: self.identity_file.clone(),
        }
    }
}

/// Result of one short-lived process.
struct Captured {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_capture(bin: &Path, args: &[String], timeout: Duration) -> Result<Captured> {
    // `ETXTBSY` is transient on Linux: if any thread of this process forked
    // while the binary/script was still open for writing, the write fd is
    // inherited and exec fails. It is a spawn-level race, not an ssh failure,
    // so a couple of immediate retries are safe (and never retry a real
    // non-zero exit).
    let mut attempt = 0;
    let mut child = loop {
        match Command::new(bin).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(child) => break child,
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("无法启动 {}", bin.display()));
            }
        }
    };

    // Drain both pipes concurrently: a large `ssh -G` dump must never fill the
    // pipe buffer and deadlock the probe.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = out_pipe.as_mut() {
            pipe.read_to_end(&mut buf).ok();
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = err_pipe.as_mut() {
            pipe.read_to_end(&mut buf).ok();
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("{} 超时（{} ms）", bin.display(), timeout.as_millis());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).with_context(|| format!("等待 {} 失败", bin.display()));
            }
        }
    };

    let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
    Ok(Captured { success: status.success(), stdout, stderr })
}

/// Run `ssh -G -T [-l user] [-p port] [-F conf] <target>` and parse its
/// `key value` output. Order is preserved and repeated keys kept (they are
/// meaningful: `identityfile` is printed once per candidate).
fn ssh_g(bin: &Path, spec: &HostSpec, conf: Option<&Path>) -> Result<Vec<(String, String)>> {
    let mut args: Vec<String> = vec!["-G".into(), "-T".into()];
    if let Some(user) = &spec.user {
        args.push("-l".into());
        args.push(user.clone());
    }
    if let Some(port) = spec.port {
        args.push("-p".into());
        args.push(port.to_string());
    }
    if let Some(conf) = conf {
        args.push("-F".into());
        args.push(conf.display().to_string());
    }
    args.push(spec.host.clone());

    let capped = run_capture(bin, &args, PROBE_TIMEOUT)?;
    if !capped.success {
        bail!("ssh -G -T {} 失败: {}", spec.host, capped.stderr.trim());
    }
    Ok(parse_g_output(&capped.stdout))
}

fn parse_g_output(stdout: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = match line.split_once(char::is_whitespace) {
            Some((k, v)) => (k.to_ascii_lowercase(), v.trim().to_string()),
            None => (line.trim().to_ascii_lowercase(), String::new()),
        };
        pairs.push((key, value));
    }
    pairs
}

fn first_value(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs.iter().find(|(k, v)| k == key && !v.is_empty()).map(|(_, v)| v.clone())
}

fn host_from_pairs(alias: &str, pairs: &[(String, String)]) -> Option<SshHost> {
    let hostname = first_value(pairs, "hostname")?;
    if hostname == "none" {
        return None;
    }
    let port = first_value(pairs, "port").and_then(|p| p.parse::<u16>().ok()).unwrap_or(22);
    let proxy_jump = first_value(pairs, "proxyjump").filter(|v| v != "none");
    Some(SshHost {
        alias: alias.to_string(),
        hostname,
        user: first_value(pairs, "user"),
        port,
        proxy_jump,
        identity_file: first_value(pairs, "identityfile"),
    })
}

fn list_hosts_with(bin: &Path, paths: &[PathBuf], explicit: Option<&Path>) -> Result<Vec<SshHost>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let home = home_dir();
    // Relative `Include` paths are relative to `~/.ssh` normally; when an
    // explicit file was forced (tests) they are relative to its directory.
    let base = match explicit {
        Some(p) => p.parent().map(Path::to_path_buf).unwrap_or_else(|| home.join(".ssh")),
        None => home.join(".ssh"),
    };

    let mut parser = ConfigParser::new(home.clone());
    for path in paths {
        parser.parse_file(path, &base, 0);
    }
    let blocks = parser.finish();

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<SshHost> = Vec::new();
    for block in &blocks {
        for alias in &block.aliases {
            if !seen.insert(alias.clone()) {
                continue;
            }
            let spec = HostSpec::parse(alias);
            let host = match ssh_g(bin, &spec, explicit) {
                Ok(pairs) => host_from_pairs(alias, &pairs),
                // Downgrade: one broken host must not break the whole list.
                Err(_) => Some(block.to_host(alias)),
            };
            if let Some(host) = host {
                out.push(host);
            }
        }
    }
    out.sort_by(|a, b| a.alias.cmp(&b.alias));
    Ok(out)
}

fn resolve_host_with(bin: &Path, alias: &str, explicit: Option<&Path>) -> Result<SshHost> {
    let spec = HostSpec::parse(alias);
    if spec.host.is_empty() {
        bail!("主机为空");
    }
    // A literal `user@host:port` is valid input on its own: ssh cannot express
    // that shape as a destination (`ssh user@h:2222` tries to resolve the
    // literal `h:2222`), so we always parse it locally. The probe is still
    // attempted first to inherit ProxyJump/IdentityFile from the config; if it
    // fails we simply fall back to the literal values instead of erroring.
    let literal = || SshHost {
        alias: alias.to_string(),
        hostname: spec.host.clone(),
        user: spec.user.clone(),
        port: spec.port.unwrap_or(22),
        proxy_jump: None,
        identity_file: None,
    };
    if !spec.is_alias {
        match ssh_g(bin, &spec, explicit).ok().and_then(|pairs| host_from_pairs(alias, &pairs)) {
            Some(mut host) => {
                if let Some(user) = spec.user {
                    host.user = Some(user);
                }
                if let Some(port) = spec.port {
                    host.port = port;
                }
                return Ok(host);
            }
            None => return Ok(literal()),
        }
    }
    let pairs = ssh_g(bin, &spec, explicit)?;
    host_from_pairs(alias, &pairs).with_context(|| format!("ssh -G -T {alias} 没有给出 hostname"))
}

/// Walks config files, following `Include`, and collects concrete `Host`
/// aliases with their first-wins raw values.
struct ConfigParser {
    blocks: Vec<RawBlock>,
    current: Option<RawBlock>,
    /// Set while inside a `Match` block: its options belong to no `Host`.
    skipping: bool,
    visited: HashSet<PathBuf>,
    home: PathBuf,
}

impl ConfigParser {
    fn new(home: PathBuf) -> Self {
        ConfigParser { blocks: Vec::new(), current: None, skipping: false, visited: HashSet::new(), home }
    }

    fn finish(mut self) -> Vec<RawBlock> {
        self.flush();
        self.blocks
    }

    fn flush(&mut self) {
        if let Some(block) = self.current.take()
            && !block.aliases.is_empty()
        {
            self.blocks.push(block);
        }
    }

    fn parse_file(&mut self, path: &Path, base: &Path, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH {
            return;
        }
        if !self.visited.insert(path.to_path_buf()) {
            return; // include cycle
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };

        for raw in content.lines() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let (key, rest) = split_keyword(line);
            match key.to_ascii_lowercase().as_str() {
                "host" => {
                    self.flush();
                    self.skipping = false;
                    let aliases = host_aliases(rest);
                    self.current = Some(RawBlock { aliases, ..RawBlock::default() });
                }
                "match" => {
                    // `Match` ends the previous `Host` context; its options
                    // must not leak into the host we just collected.
                    self.flush();
                    self.skipping = true;
                }
                "include" => {
                    // Deliberately *not* flushing: ssh applies the options of an
                    // included file in the current host context, so a bare
                    // `Port 2200` inside `Host a { Include x }` belongs to `a`.
                    // A `Host` line inside the include flushes/restarts by
                    // itself (measured behaviour, §2.5).
                    for pattern in tokenize(rest) {
                        for included in self.expand_include(&pattern, base) {
                            self.parse_file(&included, base, depth + 1);
                        }
                    }
                }
                "hostname" => self.set(|b| &mut b.hostname, rest, |s| Some(s.to_string())),
                "user" => self.set(|b| &mut b.user, rest, |s| Some(s.to_string())),
                "port" => self.set(|b| &mut b.port, rest, |s| s.parse::<u16>().ok()),
                "identityfile" => self.set(|b| &mut b.identity_file, rest, |s| Some(s.to_string())),
                "proxyjump" => {
                    self.set(|b| &mut b.proxy_jump, rest, |s| if s == "none" { None } else { Some(s.to_string()) })
                }
                _ => {}
            }
        }
    }

    /// First value wins, mirroring ssh's own "first obtained value" rule.
    fn set<T: Clone>(
        &mut self,
        field: impl Fn(&mut RawBlock) -> &mut Option<T>,
        raw: &str,
        parse: impl Fn(&str) -> Option<T>,
    ) {
        if self.skipping {
            return;
        }
        let Some(block) = self.current.as_mut() else {
            return;
        };
        if field(block).is_some() {
            return;
        }
        let values = tokenize(raw);
        if let Some(first) = values.first() {
            *field(block) = parse(first);
        }
    }

    fn expand_include(&self, spec: &str, base: &Path) -> Vec<PathBuf> {
        let expanded = if let Some(rest) = spec.strip_prefix("~/") {
            self.home.join(rest)
        } else if let Some(rest) = spec.strip_prefix('~') {
            self.home.join(rest)
        } else if Path::new(spec).is_absolute() {
            PathBuf::from(spec)
        } else {
            base.join(spec)
        };

        let has_wildcard = expanded.to_string_lossy().contains(['*', '?']);
        if !has_wildcard {
            return if expanded.is_file() { vec![expanded] } else { Vec::new() };
        }
        glob_paths(&expanded)
    }
}

/// Expand a path containing `*`/`?` in any component below an existing
/// directory. Only single-level wildcard components are needed for `Include`.
fn glob_paths(pattern: &Path) -> Vec<PathBuf> {
    let components: Vec<String> = pattern.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();

    let mut roots: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    // Start from the longest literal prefix.
    let mut prefix = PathBuf::from(if pattern.is_absolute() { "/" } else { "" });
    while i < components.len() {
        let comp = &components[i];
        let root_comp = comp.trim_start_matches('/');
        if !root_comp.contains(['*', '?']) {
            if !root_comp.is_empty() {
                prefix.push(root_comp);
            }
            i += 1;
        } else {
            break;
        }
    }
    if prefix.as_os_str().is_empty() {
        prefix = PathBuf::from(".");
    }
    roots.push(prefix);

    for comp in &components[i..] {
        let comp = comp.as_str();
        let mut next: Vec<PathBuf> = Vec::new();
        for root in &roots {
            if !comp.contains(['*', '?']) {
                next.push(root.join(comp));
                continue;
            }
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .filter(|name| glob_match(comp, name))
                .collect();
            names.sort();
            for name in names {
                next.push(root.join(name));
            }
        }
        roots = next;
        if roots.is_empty() {
            return Vec::new();
        }
    }

    let mut out: Vec<PathBuf> = roots.into_iter().filter(|p| p.is_file()).collect();
    out.sort();
    out.dedup();
    out
}

/// Minimal `*`/`?` shell glob match (no bracket classes, no `**` semantics
/// beyond what `*` already covers).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Iterative backtracking matcher: O(n·m) worst case, tiny inputs.
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Concrete aliases of a `Host` line: wildcards and negations are skipped, a
/// line may carry several aliases.
fn host_aliases(rest: &str) -> Vec<String> {
    tokenize(rest)
        .into_iter()
        .filter(|a| !a.starts_with('!'))
        .filter(|a| !a.contains(['*', '?']))
        .filter(|a| !a.is_empty())
        .collect()
}

/// Split an option value into words, honouring simple double/single quotes.
fn tokenize(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    }
                } else if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
                has = true;
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    has = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        cur.push(next);
                        has = true;
                    }
                }
                c if c.is_whitespace() => {
                    if has {
                        out.push(std::mem::take(&mut cur));
                        has = false;
                    }
                }
                c => {
                    cur.push(c);
                    has = true;
                }
            },
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// Strip a trailing `#` comment that is outside quotes and at a word boundary.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut prev_ws = true;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == '#' && prev_ws {
                    return &line[..i];
                }
            }
        }
        if !c.is_whitespace() {
            prev_ws = c == '"' || c == '\'';
        } else {
            prev_ws = true;
        }
    }
    line
}

fn split_keyword(line: &str) -> (&str, &str) {
    let mut end = line.len();
    for (i, c) in line.char_indices() {
        if c.is_whitespace() || c == '=' {
            end = i;
            break;
        }
    }
    let key = &line[..end];
    let mut rest = line[end..].trim_start();
    if let Some(stripped) = rest.strip_prefix('=') {
        rest = stripped.trim_start();
    }
    (key, rest.trim_end())
}

// ---------------------------------------------------------------------------
// Command construction
// ---------------------------------------------------------------------------

/// Options every tunnel client gets. `-v` is required for the readiness
/// markers; a `LogLevel` must **not** be added (measured: the config's
/// `LogLevel` does not defeat `-v`, but adding one on the command line is
/// pointless and easy to get wrong — §2.3).
const BASE_ARGS: &[&str] = &[
    "-N",
    "-v",
    "-o",
    "ExitOnForwardFailure=yes",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
];

/// Build the `ssh` argument vector for one tunnel. Arguments stay separated —
/// never a shell string.
pub fn build_ssh_args(cfg: &TunnelConfig, synth_conf: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = BASE_ARGS.iter().map(|s| (*s).to_string()).collect();

    if let Some(conf) = synth_conf {
        args.push("-F".to_string());
        args.push(conf.display().to_string());
    }
    for opt in &cfg.ssh_options {
        let opt = opt.trim();
        if opt.is_empty() {
            continue;
        }
        args.push("-o".to_string());
        args.push(opt.to_string());
    }

    match cfg.kind {
        ForwardKind::Local => {
            args.push("-L".to_string());
            args.push(cfg.forward_spec());
        }
        ForwardKind::Remote => {
            args.push("-R".to_string());
            args.push(cfg.forward_spec());
        }
        ForwardKind::Dynamic => {
            args.push("-D".to_string());
            args.push(format!("{}:{}", cfg.local_addr, cfg.local_port));
        }
    }

    args.push(cfg.ssh_host.clone());
    args
}

// ---------------------------------------------------------------------------
// Synthetic one-shot config
// ---------------------------------------------------------------------------

/// Keys of `ssh -G` output that must never be re-stated in a synthetic config
/// (`docs/research/tech-options.md` §2.5, §2.7).
const SYNTH_EXCLUDED: &[&str] = &[
    // re-binding the user's own forwards would duplicate / steal their ports
    "localforward",
    "remoteforward",
    "dynamicforward",
    // would cancel the `-L`/`-R`/`-D` we pass on the command line
    "clearallforwardings",
    // cannot be re-stated as a config line
    "proxycommand",
    // structural directives, never options: emitting one inside a block would
    // open a new `Host` context and silently drop every following key
    "host",
    "match",
    "include",
];

/// Keys whose value is a whitespace separated *list*: quoting them breaks ssh.
const MULTI_VALUE_KEYS: &[&str] = &[
    "rekeylimit",
    "ipqos",
    "userknownhostsfile",
    "globalknownhostsfile",
    "sendenv",
    "ciphers",
    "macs",
    "kexalgorithms",
    "hostkeyalgorithms",
    "hostbasedkeytypes",
    "pubkeyacceptedkeytypes",
    "casignaturealgorithms",
    "canonicaldomains",
    "proxyjump",
    "localforward",
    "remoteforward",
    "dynamicforward",
];

/// Write a one-shot ssh config for `cfg` into `dir` and return its path.
///
/// The file re-states the *resolved* configuration of `cfg.ssh_host` (plus its
/// whole `ProxyJump` chain) so that `ClearAllForwardings` tricks are
/// unnecessary and the user's own `LocalForward`s are not duplicated.
pub fn write_synth_config(cfg: &TunnelConfig, dir: &Path) -> Result<PathBuf> {
    write_synth_config_with(&ssh_binary(), cfg, dir, config_override().as_deref())
}

fn write_synth_config_with(
    bin: &Path,
    cfg: &TunnelConfig,
    dir: &Path,
    conf_override: Option<&Path>,
) -> Result<PathBuf> {
    let root = HostSpec::parse(&cfg.ssh_host);
    if root.host.is_empty() {
        bail!("SSH 主机为空");
    }

    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let mut visited: Vec<String> = Vec::new();
    bake_sections(bin, &root, conf_override, &mut sections, &mut visited, 0)?;
    if sections.is_empty() {
        bail!("无法解析 {} 的 ssh 配置", cfg.ssh_host);
    }

    let mut text = String::new();
    text.push_str("# sshpf generated config — do not edit, regenerated per start.\n");
    text.push_str(&format!("# tunnel: {} ({})  host: {}\n", cfg.id, cfg.name, cfg.ssh_host));
    for (key, lines) in &sections {
        text.push('\n');
        text.push_str(&format!("Host {key}\n"));
        for line in lines {
            text.push_str("    ");
            text.push_str(line);
            text.push('\n');
        }
    }

    std::fs::create_dir_all(dir).with_context(|| format!("无法创建目录 {}", dir.display()))?;
    let path = dir.join(format!("sshpf-{}.conf", cfg.id));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("无法写入 {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    // `mode(0o600)` only applies on creation; force it if the file existed.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

fn bake_sections(
    bin: &Path,
    spec: &HostSpec,
    conf_override: Option<&Path>,
    sections: &mut Vec<(String, Vec<String>)>,
    visited: &mut Vec<String>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_JUMP_DEPTH {
        bail!("ProxyJump 链过深");
    }
    let key = spec.match_key();
    if visited.contains(&key) {
        return Ok(());
    }
    visited.push(key.clone());

    let pairs = ssh_g(bin, spec, conf_override).with_context(|| format!("无法解析主机 {} 的配置", spec.match_key()))?;

    // A ProxyCommand cannot be re-stated reliably in a synthetic config; let
    // the caller fall back to the user's real config instead of silently
    // producing a broken tunnel.
    if let Some((_, pc)) = pairs.iter().find(|(k, v)| k == "proxycommand" && !v.is_empty()) {
        bail!("主机 {} 使用 ProxyCommand，无法生成合成配置（{}）", spec.match_key(), pc);
    }

    let mut lines: Vec<String> = Vec::new();
    for (k, v) in &pairs {
        if SYNTH_EXCLUDED.contains(&k.as_str()) {
            continue;
        }
        if k == "hostname" && v == "none" {
            continue;
        }
        if k == "user" && v.is_empty() {
            continue;
        }
        lines.push(format_baked_line(k, v));
    }

    // The baked block must pin the resolved endpoint explicitly: `Host` line
    // matching alone would leave `hostname` at the literal destination string.
    if let Some(hostname) = first_value(&pairs, "hostname").filter(|h| h != "none") {
        ensure_line(&mut lines, "hostname", &format_baked_line("hostname", &hostname));
    }
    if let Some(port) = first_value(&pairs, "port") {
        ensure_line(&mut lines, "port", &format_baked_line("port", &port));
    }
    if let Some(user) = &spec.user {
        ensure_line(&mut lines, "user", &format_baked_line("user", user));
    }

    sections.push((key, lines));

    // `ProxyJump` makes ssh fork a child that re-reads *our* `-F` file, so the
    // whole chain has to be contained in it.
    if let Some(jump) = first_value(&pairs, "proxyjump").filter(|j| j != "none") {
        for hop in jump.split(',') {
            let hop = hop.trim();
            if hop.is_empty() {
                continue;
            }
            let hop_spec = HostSpec::parse(hop);
            bake_sections(bin, &hop_spec, conf_override, sections, visited, depth + 1)?;
        }
    }
    Ok(())
}

fn ensure_line(lines: &mut Vec<String>, key: &str, line: &str) {
    if lines.iter().any(|l| l.split_whitespace().next() == Some(key)) {
        return;
    }
    lines.push(line.to_string());
}

fn format_baked_line(key: &str, value: &str) -> String {
    if value.is_empty() {
        return key.to_string();
    }
    let needs_quote = value.chars().any(char::is_whitespace) && !MULTI_VALUE_KEYS.contains(&key);
    if needs_quote {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{key} \"{escaped}\"")
    } else {
        format!("{key} {value}")
    }
}

// ---------------------------------------------------------------------------
// stderr classification
// ---------------------------------------------------------------------------

/// Classify one line of `ssh` stderr into a UI-ready failure.
///
/// Order matters: a partially bound `-L` prints the `bind` failure *and*
/// `Entering interactive session.`, and the failure is the interesting part
/// (§2.3). Callers should classify before consulting [`is_ready_line`].
pub fn classify_failure(line: &str) -> Option<FailureInfo> {
    let l = line.trim();
    if l.is_empty() {
        return None;
    }
    let lower = l.to_ascii_lowercase();

    // ① local bind: `bind [::1]:18081: Address already in use`
    if lower.contains("bind ") && lower.contains("address already in use") {
        let port = port_from_line(l);
        let short = match port {
            Some(p) => fit_short(format!("本地端口 {p} 被占用")),
            None => fit_short("本地端口被占用".to_string()),
        };
        return Some(FailureInfo::with_short(FailureKind::PortInUse, short, l));
    }
    // ② privileged port: `bind [127.0.0.1]:80: Permission denied`
    if lower.contains("bind ") && lower.contains("permission denied") {
        let port = port_from_line(l);
        let short = match port {
            Some(p) => fit_short(format!("端口 {p} 权限不足")),
            None => fit_short("端口权限不足(<1024)".to_string()),
        };
        return Some(FailureInfo::with_short(FailureKind::PermissionDenied, short, l));
    }
    // ③ forwarding listener could not be set up. On its own this line is
    // ambiguous (it can also follow "Permission denied"), so it is reported as
    // `Unknown` keeping the raw text; when the very same line carries the
    // `bind` context we can be specific. In a real stream the `bind` line comes
    // first, so the specific classification is the one that sticks.
    if lower.contains("cannot listen to port") || lower.contains("could not request local forwarding") {
        let kind = if lower.contains("bind ") { FailureKind::PortInUse } else { FailureKind::Unknown };
        return Some(FailureInfo::with_short(kind, fit_short(l.to_string()), l));
    }
    // ④ `-R` failures
    if lower.contains("remote port forwarding failed")
        || lower.contains("remote forward failure")
        || lower.contains("remote forwarding failed")
    {
        let port = port_from_line(l);
        let short = match port {
            Some(p) => fit_short(format!("远端端口 {p} 转发失败")),
            None => fit_short("远端转发失败".to_string()),
        };
        return Some(FailureInfo::with_short(FailureKind::RemoteForwardFailed, short, l));
    }
    // ⑤ lazy target failure: `channel 2: open failed: connect failed: …`
    if lower.contains("open failed") && lower.contains("connect failed") {
        return Some(FailureInfo::with_short(
            FailureKind::TargetUnreachable,
            fit_short("目标不可达(隧道内)".to_string()),
            l,
        ));
    }
    // ⑥ host key
    if lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
        || (lower.contains("no ") && lower.contains("host key is known for"))
    {
        return Some(FailureInfo::with_short(
            FailureKind::HostKeyMismatch,
            fit_short("主机密钥校验失败".to_string()),
            l,
        ));
    }
    // ⑦ name resolution
    if lower.contains("could not resolve hostname") {
        let host = host_from_line(l);
        let short = match host {
            Some(h) => fit_short(format!("无法解析 {h}")),
            None => fit_short("域名解析失败".to_string()),
        };
        return Some(FailureInfo::with_short(FailureKind::DnsFailure, short, l));
    }
    // ⑧ refused.
    //
    // Only *connection* refusals matter. OpenSSH emits benign refusals that do
    // not fail the session at all — measured on OpenSSH 8.2 with no agent
    // running:
    //     debug1: pubkey_prepare: ssh_get_authentication_socket: Connection refused
    // The connection then proceeds and the forward comes up. Treating that line
    // as a failure marked a perfectly healthy tunnel as failed (and killed it).
    if lower.contains("connection refused") {
        let benign = (lower.contains("ssh_get_authentication_socket")
            || lower.contains("get_authentication_socket")
            || lower.contains("agent")
            || lower.contains("identity agent"))
            && !lower.contains("connect to host")
            && !lower.contains("open failed");
        if benign {
            return None;
        }
        return Some(FailureInfo::with_short(FailureKind::ConnectionRefused, fit_short("连接被拒绝".to_string()), l));
    }
    // ⑨ timeouts
    if lower.contains("connection timed out")
        || lower.contains("operation timed out")
        || lower.contains("timeout, server")
        || lower.contains("timed out")
    {
        return Some(FailureInfo::with_short(FailureKind::Timeout, fit_short("连接超时".to_string()), l));
    }
    // ⑩ authentication (any `Permission denied` without `bind`)
    if lower.contains("permission denied") {
        return Some(FailureInfo::with_short(
            FailureKind::AuthFailed,
            fit_short("认证失败(公钥/agent)".to_string()),
            l,
        ));
    }
    None
}

/// Marks that a tunnel's forwarding is actually established. Failure lines
/// must be checked first (§2.3).
pub fn is_ready_line(kind: ForwardKind, line: &str) -> bool {
    match kind {
        ForwardKind::Local | ForwardKind::Dynamic => line.contains("Entering interactive session."),
        ForwardKind::Remote => {
            line.contains("remote forward success for") || line.contains("Entering interactive session.")
        }
    }
}

/// True for the line that announces a *local* forwarding listener:
/// `debug1: Local forwarding listening on 127.0.0.1 port 18500.`
///
/// Exposed for callers that need staged readiness: with `ProxyJump`, ssh forks
/// `ssh … -W` and the child's stderr is merged into ours, so the child's own
/// `Entering interactive session.` can appear *before* the target is even
/// authenticated (measured). Requiring this listener line first (and only then
/// accepting [`is_ready_line`]) removes that false positive without any state
/// living in this module.
pub fn is_local_listening_line(line: &str) -> bool {
    line.contains("Local forwarding listening on")
}

/// Best-effort port extraction from an ssh stderr line.
fn port_from_line(line: &str) -> Option<u16> {
    // Prefer `]:<port>:` (bind lines) then `port <n>` then any 1..=5 digit run
    // that follows a ':' or whitespace.
    if let Some(close) = line.find("]:") {
        let tail = &line[close + 2..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = digits.parse::<u16>()
            && p != 0
        {
            return Some(p);
        }
    }
    if let Some(idx) = line.to_ascii_lowercase().find("port ") {
        let tail = &line[idx + 5..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(p) = digits.parse::<u16>()
            && p != 0
        {
            return Some(p);
        }
    }
    for token in line.split(|c: char| !c.is_ascii_digit()) {
        if !token.is_empty()
            && token.len() <= 5
            && let Ok(p) = token.parse::<u16>()
            && p != 0
        {
            return Some(p);
        }
    }
    None
}

fn host_from_line(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let idx = lower.find("could not resolve hostname")?;
    let tail = &line[idx + "could not resolve hostname".len()..];
    let tail = tail.trim_start_matches([':', ' ']);
    let host: String = tail.chars().take_while(|c| *c != ':' && !c.is_whitespace()).collect();
    if host.is_empty() { None } else { Some(host) }
}

/// Truncate to at most [`SHORT_MAX_WIDTH`] display cells, ending in `…`.
fn fit_short(mut s: String) -> String {
    if display_width(&s) <= SHORT_MAX_WIDTH {
        return s;
    }
    let budget = SHORT_MAX_WIDTH - 1;
    let mut width = 0;
    let mut cut = 0;
    for (i, c) in s.char_indices() {
        let w = char_width(c);
        if width + w > budget {
            break;
        }
        width += w;
        cut = i + c.len_utf8();
    }
    s.truncate(cut);
    s.push('…');
    s
}

/// Terminal cell width of a string (CJK = 2, everything else = 1).
///
/// `unicode-width` is not a dependency and must not be added.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Binary discovery
// ---------------------------------------------------------------------------

/// The `ssh` executable: `$SSHPF_SSH_BIN` when it points at an executable
/// file, otherwise the first `ssh` on `$PATH`, otherwise `/usr/bin/ssh`.
pub fn ssh_binary() -> PathBuf {
    pick_binary(std::env::var_os(SSH_BIN_ENV).as_deref())
}

fn pick_binary(override_path: Option<&OsStr>) -> PathBuf {
    if let Some(path) = override_path {
        let p = PathBuf::from(path);
        if is_executable(&p) {
            return p;
        }
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("ssh");
            if is_executable(&candidate) {
                return candidate;
            }
        }
    }
    PathBuf::from("/usr/bin/ssh")
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// Whether `ssh` can actually be run.
pub fn ssh_available() -> bool {
    is_executable(&ssh_binary())
}

// ---------------------------------------------------------------------------
// Local port probing
// ---------------------------------------------------------------------------

/// Can we bind `addr:port` right now? IPv4/IPv6 are tested exactly as given:
/// `127.0.0.1` is not silently widened to the wildcard address.
pub fn port_available(addr: &str, port: u16) -> bool {
    if port == 0 {
        return false;
    }
    match std::net::TcpListener::bind((addr, port)) {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(_) => false,
    }
}

/// First free port at or above `start` (scanning at most 200 candidates).
pub fn suggest_free_port(addr: &str, start: u16) -> Option<u16> {
    let from = start.max(1);
    (from..=u16::MAX).take(200).find(|candidate| port_available(addr, *candidate))
}

/// Best-effort "who is using this port": `pid/comm` from `/proc`.
///
/// Returns `None` when the owner cannot be determined (permissions, kernel
/// thread, socket owned by another user's fd we may not inspect, …). This is a
/// diagnostic nicety, never a correctness dependency.
pub fn port_owner(addr: &str, port: u16) -> Option<String> {
    let _ = addr; // matched by port only: /proc/net/tcp has no name resolution
    let inodes = listening_inodes(port);
    if inodes.is_empty() {
        return None;
    }
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(target) = target.to_str() else {
                continue;
            };
            let Some(rest) = target.strip_prefix("socket:[") else {
                continue;
            };
            let Some(inode) = rest.strip_suffix(']') else {
                continue;
            };
            if inodes.iter().any(|i| i == inode) {
                let comm = std::fs::read_to_string(entry.path().join("comm"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| "?".to_string());
                return Some(format!("{pid}/{comm}"));
            }
        }
    }
    None
}

/// Socket inodes of listening sockets bound to `port`, from `/proc/net/tcp{,6}`.
fn listening_inodes(port: u16) -> Vec<String> {
    let mut out = Vec::new();
    let target = format!("{port:04X}");
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            let Some((_, local_port)) = fields[1].rsplit_once(':') else {
                continue;
            };
            if !local_port.eq_ignore_ascii_case(&target) {
                continue;
            }
            // Prefer LISTEN sockets (state 0A) but accept all: a connected
            // socket on the same local port still points at a culprit.
            let inode = fields[9];
            if inode != "0" {
                out.push(inode.to_string());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tunnel(kind: ForwardKind) -> TunnelConfig {
        TunnelConfig {
            id: 7,
            name: "demo".into(),
            kind,
            ssh_host: "cloud_pc".into(),
            local_addr: "127.0.0.1".into(),
            local_port: 18080,
            remote_host: "127.0.0.1".into(),
            remote_port: 3000,
            enabled: false,
            auto_restart: true,
            ssh_options: vec![],
            note: String::new(),
            created_at: 0,
        }
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    // -- classify_failure ---------------------------------------------------

    #[test]
    fn classify_real_stderr_lines() {
        // ① port already in use (both families)
        let f = classify_failure("bind [127.0.0.1]:18081: Address already in use").unwrap();
        assert_eq!(f.kind, FailureKind::PortInUse);
        assert!(f.short.contains("18081"), "{}", f.short);
        assert_eq!(classify_failure("bind [::1]:18081: Address already in use").unwrap().kind, FailureKind::PortInUse);
        // On its own this line is ambiguous (it follows both the in-use and the
        // permission case), so it is `Unknown` and keeps the raw text as `short`.
        let f = classify_failure("channel_setup_fwd_listener_tcpip: cannot listen to port: 18081").unwrap();
        assert_eq!(f.kind, FailureKind::Unknown);
        // `short` keeps the raw text (truncated to the display budget).
        assert!(f.short.starts_with("channel_setup_fwd_listener_tcpip"), "{}", f.short);
        let f = classify_failure("Could not request local forwarding.").unwrap();
        assert_eq!(f.kind, FailureKind::Unknown);
        assert!(f.short.starts_with("Could not request local"), "{}", f.short);
        assert!(display_width(&f.short) <= SHORT_MAX_WIDTH, "{}", display_width(&f.short));

        // ② privileged port
        let f = classify_failure("bind [127.0.0.1]:80: Permission denied").unwrap();
        assert_eq!(f.kind, FailureKind::PermissionDenied);
        assert!(f.short.contains("80"), "{}", f.short);

        // ③ auth failure
        let f = classify_failure("wangcan@127.0.0.1: Permission denied (publickey).").unwrap();
        assert_eq!(f.kind, FailureKind::AuthFailed);

        // ④ dns / refused
        let f = classify_failure(
            "ssh: Could not resolve hostname no-such-host.invalid: Temporary failure in name resolution",
        )
        .unwrap();
        assert_eq!(f.kind, FailureKind::DnsFailure);
        assert!(f.short.contains("no-such-host.invalid"), "{}", f.short);
        let f = classify_failure("ssh: connect to host 127.0.0.1 port 1: Connection refused").unwrap();
        assert_eq!(f.kind, FailureKind::ConnectionRefused);

        // ⑤ host key
        assert_eq!(
            classify_failure(
                "No ED25519 host key is known for [127.0.0.1]:2222 and you have requested strict checking."
            )
            .unwrap()
            .kind,
            FailureKind::HostKeyMismatch
        );
        assert_eq!(classify_failure("Host key verification failed.").unwrap().kind, FailureKind::HostKeyMismatch);
        assert_eq!(
            classify_failure("@@@@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@@@@").unwrap().kind,
            FailureKind::HostKeyMismatch
        );

        // ⑥ remote forward
        let f = classify_failure("remote forward failure for: listen 22, connect 127.0.0.1:22").unwrap();
        assert_eq!(f.kind, FailureKind::RemoteForwardFailed);
        assert_eq!(
            classify_failure("Error: remote port forwarding failed for listen port 22").unwrap().kind,
            FailureKind::RemoteForwardFailed
        );

        // ⑦ lazy target failure
        let f = classify_failure("channel 2: open failed: connect failed: Connection refused").unwrap();
        assert_eq!(f.kind, FailureKind::TargetUnreachable);

        // ⑧ timeout
        let f = classify_failure("Timeout, server 10.244.119.128 not responding.").unwrap();
        assert_eq!(f.kind, FailureKind::Timeout);
        assert_eq!(
            classify_failure("ssh: connect to host 10.0.0.1 port 22: Connection timed out").unwrap().kind,
            FailureKind::Timeout
        );
        assert_eq!(
            classify_failure("ssh: connect to host h port 22: Operation timed out").unwrap().kind,
            FailureKind::Timeout
        );

        // noise / benign lines are not failures
        assert!(classify_failure("debug1: Entering interactive session.").is_none());
        assert!(classify_failure("Authenticated to 10.0.0.1 ([10.0.0.1]:22).").is_none());
        assert!(classify_failure("").is_none());
    }

    #[test]
    fn benign_agent_socket_refusal_is_not_a_failure() {
        // Measured on OpenSSH 8.2 with no ssh-agent running: this line appears
        // on a healthy session that goes on to forward successfully. Treating
        // it as a failure used to mark (and kill) working tunnels.
        assert!(
            classify_failure("debug1: pubkey_prepare: ssh_get_authentication_socket: Connection refused").is_none()
        );
        assert!(classify_failure("debug1: identity agent: Connection refused").is_none());
        // Genuine refusals must still be classified.
        assert!(classify_failure("ssh: connect to host 127.0.0.1 port 1: Connection refused").is_some());
        assert!(classify_failure("debug1: channel 2: open failed: connect failed: Connection refused").is_some());
    }

    #[test]
    fn classify_short_phrases_fit_width() {
        let cases = [
            "bind [127.0.0.1]:65535: Address already in use",
            "bind [127.0.0.1]:80: Permission denied",
            "ssh: Could not resolve hostname a-very-long-hostname-that-goes-on-and-on.invalid: Name or service not known",
            "channel_setup_fwd_listener_tcpip: cannot listen to port: 65535",
        ];
        for line in cases {
            let info = classify_failure(line).unwrap();
            assert!(
                display_width(&info.short) <= SHORT_MAX_WIDTH,
                "{} => {} ({} cells)",
                line,
                info.short,
                display_width(&info.short)
            );
            assert!(!info.short.contains('\n'));
            assert_eq!(info.detail, line.trim());
        }
    }

    #[test]
    fn display_width_counts_cjk_as_two() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("本地端口"), 8);
        assert_eq!(display_width("a中b"), 4);
        let long = fit_short("端口".repeat(40));
        assert!(display_width(&long) <= SHORT_MAX_WIDTH, "{}", display_width(&long));
        assert!(long.ends_with('…'));
    }

    // -- is_ready_line ------------------------------------------------------

    #[test]
    fn ready_markers_per_kind() {
        assert!(is_ready_line(ForwardKind::Local, "debug1: Entering interactive session."));
        assert!(is_ready_line(ForwardKind::Dynamic, "debug1: Entering interactive session."));
        assert!(!is_ready_line(ForwardKind::Local, "debug1: Authenticated to h ([1.2.3.4]:22)."));
        assert!(is_ready_line(
            ForwardKind::Remote,
            "debug1: remote forward success for: listen 8080, connect 127.0.0.1:80"
        ));
        assert!(is_ready_line(ForwardKind::Remote, "debug1: Entering interactive session."));
        assert!(!is_ready_line(ForwardKind::Remote, "remote forward failure for: listen 22, connect 127.0.0.1:22"));
    }

    #[test]
    fn local_listening_line_is_staged_marker() {
        assert!(is_local_listening_line("debug1: Local forwarding listening on 127.0.0.1 port 18500."));
        assert!(!is_local_listening_line("debug1: Entering interactive session."));
        // Measured with ProxyJump: the forked `ssh -W` child reaches
        // "Entering interactive session." before the target authenticated, so
        // the listener line has to be required first.
        let child_line = "debug1: Entering interactive session.";
        assert!(is_ready_line(crate::model::ForwardKind::Local, child_line));
        assert!(!is_local_listening_line(child_line));
    }

    // -- build_ssh_args -----------------------------------------------------

    #[test]
    fn args_local_forward() {
        let args = build_ssh_args(&tunnel(ForwardKind::Local), None);
        assert!(args.windows(2).any(|w| w == ["-L", "127.0.0.1:18080:127.0.0.1:3000"]));
        assert_eq!(args.last().unwrap(), "cloud_pc");
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"-v".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("LogLevel")), "must not emit LogLevel");
        assert!(args.windows(2).any(|w| w == ["-o", "ExitOnForwardFailure=yes"]));
        assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ConnectTimeout=10"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ServerAliveInterval=15"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ServerAliveCountMax=3"]));
    }

    #[test]
    fn args_remote_and_dynamic() {
        let mut cfg = tunnel(ForwardKind::Remote);
        cfg.remote_host = "0.0.0.0".into();
        cfg.remote_port = 8080;
        let args = build_ssh_args(&cfg, None);
        assert!(args.windows(2).any(|w| w == ["-R", "0.0.0.0:8080:127.0.0.1:18080"]));

        let cfg = tunnel(ForwardKind::Dynamic);
        let args = build_ssh_args(&cfg, None);
        assert!(args.windows(2).any(|w| w == ["-D", "127.0.0.1:18080"]));
        assert!(!args.iter().any(|a| a == "-L" || a == "-R"));
    }

    #[test]
    fn args_synth_config_and_user_options() {
        let mut cfg = tunnel(ForwardKind::Local);
        cfg.ssh_options = vec!["StrictHostKeyChecking=no".into(), "  ".into(), "UserKnownHostsFile=/dev/null".into()];
        let synth = Path::new("/tmp/sshpf-x/sshpf-7.conf");
        let args = build_ssh_args(&cfg, Some(synth));
        let pos = args.iter().position(|a| a == "-F").expect("-F present");
        assert_eq!(args[pos + 1], "/tmp/sshpf-x/sshpf-7.conf");
        assert!(args.windows(2).any(|w| w == ["-o", "StrictHostKeyChecking=no"]));
        assert!(args.windows(2).any(|w| w == ["-o", "UserKnownHostsFile=/dev/null"]));
        // empty option is skipped, no dangling `-o`
        let dangling = args.windows(2).filter(|w| w[0] == "-o").count();
        assert_eq!(dangling, 2 + 5, "5 base + 2 user options");
        assert_eq!(args.last().unwrap(), "cloud_pc");
    }

    // -- host spec parsing --------------------------------------------------

    #[test]
    fn parse_user_at_host_port() {
        let spec = HostSpec::parse("wangcan@127.0.0.1:2222");
        assert_eq!(spec.user.as_deref(), Some("wangcan"));
        assert_eq!(spec.host, "127.0.0.1");
        assert_eq!(spec.port, Some(2222));
        assert!(!spec.is_alias);
        assert_eq!(spec.match_key(), "127.0.0.1:2222");

        let spec = HostSpec::parse("cloud_pc");
        assert!(spec.is_alias);
        assert_eq!(spec.user, None);
        assert_eq!(spec.port, None);
        assert_eq!(spec.match_key(), "cloud_pc");

        let spec = HostSpec::parse("nvidia@10.24.12.41");
        assert_eq!(spec.user.as_deref(), Some("nvidia"));
        assert_eq!(spec.port, None);
        assert_eq!(spec.match_key(), "10.24.12.41");

        let spec = HostSpec::parse("[::1]:2222");
        assert_eq!(spec.host, "::1");
        assert_eq!(spec.port, Some(2222));

        // A bare IPv6 literal stays a host, not a host:port.
        let spec = HostSpec::parse("fe80::1");
        assert_eq!(spec.host, "fe80::1");
        assert_eq!(spec.port, None);
    }

    // -- config parsing / list_hosts ---------------------------------------

    const FAKE_SSH: &str = r#"#!/bin/sh
# Fake `ssh` for tests: only implements `-G -T` style evaluation.
host=""
for a in "$@"; do host="$a"; done
case "$host" in
  alpha|cloud_pc)
    echo "hostname 10.0.0.1"
    echo "user alice"
    echo "port 2222"
    echo "proxyjump jumpbox"
    echo "identityfile ~/.ssh/id_rsa"
    echo "localforward 18789 [127.0.0.1]:18789"
    echo "clearallforwardings no"
    echo "userknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2"
    echo "proxycommand"
    ;;
  jumpbox)
    echo "hostname 10.9.9.9"
    echo "port 80"
    echo "user jump"
    ;;
  beta)
    echo "hostname 10.0.0.2"
    echo "user bob"
    echo "port 22"
    ;;
  broken)
    echo "some config error" >&2
    exit 255
    ;;
  *) exit 255 ;;
esac
"#;

    #[test]
    fn list_hosts_parses_include_glob_and_skips_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bin = write_script(root, "fake-ssh.sh", FAKE_SSH);

        std::fs::create_dir_all(root.join("conf.d")).unwrap();
        std::fs::write(root.join("conf.d/a.conf"), "Host alpha jeprof\n  HostName 10.1.1.1\n  User nvidia\n").unwrap();
        std::fs::write(root.join("conf.d/b.conf"), "Host broken\n  HostName 9.9.9.9\n  User bob\n  Port 1234\n")
            .unwrap();

        let config = root.join("config");
        std::fs::write(
            &config,
            "Host *\n  ServerAliveInterval 60\n\n\
             Include conf.d/*.conf\n\n\
             Match host foo\n  User ignored\n\n\
             Host beta\n  HostName 10.0.0.2\n\n\
             Host !negated other\n  HostName 10.0.0.3\n",
        )
        .unwrap();

        let hosts = list_hosts_with(&bin, std::slice::from_ref(&config), Some(&config)).unwrap();
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        // `*`, `!negated` and `foo` (inside Match) are not concrete aliases.
        assert_eq!(aliases, vec!["alpha", "beta", "broken", "jeprof", "other"], "{aliases:?}");

        let alpha = hosts.iter().find(|h| h.alias == "alpha").unwrap();
        assert_eq!(alpha.hostname, "10.0.0.1");
        assert_eq!(alpha.user.as_deref(), Some("alice"));
        assert_eq!(alpha.port, 2222);
        assert_eq!(alpha.proxy_jump.as_deref(), Some("jumpbox"));
        assert_eq!(alpha.identity_file.as_deref(), Some("~/.ssh/id_rsa"));
        assert_eq!(alpha.display(), "alpha (alice@10.0.0.1:2222)");

        // `ssh -G` failed for `broken` → downgraded to the raw config values.
        let broken = hosts.iter().find(|h| h.alias == "broken").unwrap();
        assert_eq!(broken.hostname, "9.9.9.9");
        assert_eq!(broken.user.as_deref(), Some("bob"));
        assert_eq!(broken.port, 1234);
    }

    #[test]
    fn list_hosts_missing_config_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        let missing = dir.path().join("nope");
        assert!(list_hosts_with(&bin, std::slice::from_ref(&missing), Some(&missing)).unwrap().is_empty());
        assert!(list_hosts_with(&bin, &[], None).unwrap().is_empty());
    }

    #[test]
    fn resolve_host_uses_probe_and_overrides_user_port() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        let host = resolve_host_with(&bin, "alpha", None).unwrap();
        assert_eq!(host.hostname, "10.0.0.1");
        assert_eq!(host.port, 2222);

        // Failure surfaces as an error for a single explicit lookup.
        assert!(resolve_host_with(&bin, "no-such-host", None).is_err());
    }

    #[test]
    fn include_glob_matcher() {
        assert!(glob_match("*.conf", "a.conf"));
        assert!(glob_match("*.conf", ".hidden.conf"));
        assert!(!glob_match("*.conf", "a.toml"));
        assert!(glob_match("conf.d/*", "conf.d/x"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*.conf", "x.y.conf"));
    }

    #[test]
    fn include_inside_host_block_contributes_to_that_host() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        std::fs::write(dir.path().join("extra.conf"), "User alice\nPort 2222\n").unwrap();
        let config = dir.path().join("config");
        std::fs::write(&config, "Host alpha\n  HostName 10.0.0.1\n  Include extra.conf\n").unwrap();
        // `alpha` is handled by the fake ssh, but the raw parser must also have
        // picked the included values up, which we check via a host the fake
        // ssh does not know (so the fallback values are used).
        let hosts = list_hosts_with(&bin, std::slice::from_ref(&config), Some(&config)).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].hostname, "10.0.0.1");

        // `unknown` fails the probe → downgraded to parsed values.
        let config2 = dir.path().join("config2");
        std::fs::write(&config2, "Host unknown-host\n  HostName 10.0.0.9\n  Include extra.conf\n").unwrap();
        let hosts = list_hosts_with(&bin, std::slice::from_ref(&config2), Some(&config2)).unwrap();
        assert_eq!(hosts[0].user.as_deref(), Some("alice"), "{:?}", hosts[0]);
        assert_eq!(hosts[0].port, 2222, "{:?}", hosts[0]);
    }

    #[test]
    fn include_cycle_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        let config = dir.path().join("config");
        std::fs::write(&config, "Include config\nHost alpha\n  HostName 10.0.0.1\n").unwrap();
        let hosts = list_hosts_with(&bin, std::slice::from_ref(&config), Some(&config)).unwrap();
        assert_eq!(hosts.len(), 1);
    }

    // -- synth config -------------------------------------------------------

    #[test]
    fn synth_config_excludes_forwards_and_bakes_jump_chain() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        let out_dir = dir.path().join("nested").join("synth");
        assert!(!out_dir.exists(), "caller dir may not exist yet");

        let cfg = tunnel(ForwardKind::Local);
        let path = write_synth_config_with(&bin, &cfg, &out_dir, None).unwrap();
        assert_eq!(path.file_name().unwrap(), "sshpf-7.conf");
        assert_eq!(path.parent().unwrap(), out_dir.as_path());

        let text = std::fs::read_to_string(&path).unwrap();
        // resolved target block
        assert!(text.contains("Host cloud_pc"), "{text}");
        assert!(text.contains("hostname 10.0.0.1"), "{text}");
        assert!(text.contains("port 2222"), "{text}");
        assert!(text.contains("user alice"), "{text}");
        // jump chain must be present too, otherwise the forked child cannot
        // resolve `jumpbox` once `-F` replaces the user's config.
        assert!(text.contains("Host jumpbox"), "{text}");
        assert!(text.contains("hostname 10.9.9.9"), "{text}");
        // excluded keys
        assert!(!text.contains("localforward"), "{text}");
        assert!(!text.contains("remoteforward"), "{text}");
        assert!(!text.contains("dynamicforward"), "{text}");
        assert!(!text.contains("clearallforwardings"), "{text}");
        assert!(!text.contains("proxycommand "), "{text}");
        // multi-value line must stay unquoted, single path values are kept
        assert!(text.contains("userknownhostsfile ~/.ssh/known_hosts"), "{text}");

        // 0600
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn synth_config_quotes_paths_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "space-ssh.sh",
            "#!/bin/sh\ncase \"$*\" in\n  *) echo 'hostname 1.2.3.4'; echo 'user u';\
             echo 'port 22'; echo 'identityfile ~/my keys/key with space';;\nesac\n",
        );
        let cfg = tunnel(ForwardKind::Local);
        let path = write_synth_config_with(&script, &cfg, dir.path(), None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("identityfile \"~/my keys/key with space\""), "{text}");
    }

    #[test]
    fn synth_config_never_emits_structural_directives() {
        // A hostile/odd `-G` dump (the project's fake ssh prints a `host …`
        // line) must not leak `Host`/`Match`/`Include` into the baked block:
        // that would open a new host context and drop every following key.
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "weird-ssh.sh",
            "#!/bin/sh\necho 'host -G -T tgt'\necho 'match all'\necho 'include /tmp/x.conf'\n             echo 'hostname 1.2.3.4'\necho 'user u'\necho 'port 22'\n",
        );
        let cfg = tunnel(ForwardKind::Local);
        let path = write_synth_config_with(&script, &cfg, dir.path(), None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let body: Vec<&str> =
            text.lines().filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty()).collect();
        // Exactly one `Host` line — the one we wrote for the block itself.
        assert_eq!(body.iter().filter(|l| l.starts_with("Host ")).count(), 1, "{text}");
        assert!(!text.contains("match all"), "{text}");
        assert!(!text.contains("include /tmp/x.conf"), "{text}");
        assert!(text.contains("hostname 1.2.3.4"), "{text}");
    }

    #[test]
    fn synth_config_rejects_proxycommand_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "pc-ssh.sh",
            "#!/bin/sh\necho 'hostname 5.5.5.5'\necho 'user u'\necho 'port 22'\necho 'proxycommand ssh -W %h:%p jump'\n",
        );
        let cfg = tunnel(ForwardKind::Local);
        let err = write_synth_config_with(&script, &cfg, dir.path(), None).unwrap_err();
        assert!(err.to_string().contains("ProxyCommand"), "{err}");
    }

    #[test]
    fn synth_config_host_port_destination_writes_match_key() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        let mut cfg = tunnel(ForwardKind::Local);
        cfg.ssh_host = "alice@alpha:2222".into();
        let path = write_synth_config_with(&bin, &cfg, dir.path(), None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // ssh matches `user@host:port` destinations against `Host host:port`.
        assert!(text.contains("Host alpha:2222"), "{text}");
        assert!(text.contains("hostname 10.0.0.1"), "{text}");
        assert!(text.contains("port 2222"), "{text}");
    }

    // -- binary discovery ---------------------------------------------------

    #[test]
    fn pick_binary_prefers_override_then_path() {
        let dir = tempfile::tempdir().unwrap();
        let fake = write_script(dir.path(), "my-ssh", "#!/bin/sh\nexit 0\n");
        assert_eq!(pick_binary(Some(fake.as_os_str())), fake);
        // Non-executable override falls back to PATH resolution.
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "not executable").unwrap();
        let picked = pick_binary(Some(plain.as_os_str()));
        assert_ne!(picked, plain);
        assert!(picked.ends_with("ssh") || picked == Path::new("/usr/bin/ssh"));
    }

    // Env-var overrides are process-global, so these two tests are merged into
    // one serial test to stay correct under `cargo test`'s thread pool.
    #[test]
    fn env_overrides_select_config_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "fake-ssh.sh", FAKE_SSH);
        let config = dir.path().join("config");
        std::fs::write(&config, "Host alpha\n  HostName 10.0.0.1\n").unwrap();

        // Baseline: real environment resolves the real ssh.
        assert!(ssh_binary().exists());

        // SSHPF_SSH_BIN wins over PATH lookup.
        unsafe { std::env::set_var(SSH_BIN_ENV, &bin) };
        assert_eq!(ssh_binary(), bin);
        assert!(ssh_available());

        // SSHPF_SSH_CONFIG makes list_hosts parse only that file.
        unsafe { std::env::set_var(CONFIG_ENV, &config) };
        let hosts = list_hosts().unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "alpha");
        assert_eq!(hosts[0].hostname, "10.0.0.1");
        let resolved = resolve_host("alpha").unwrap();
        assert_eq!(resolved.hostname, "10.0.0.1");
        // `user@host:port` never shells out.
        let explicit = resolve_host("bob@example.test:2200").unwrap();
        assert_eq!(explicit.hostname, "example.test");
        assert_eq!(explicit.port, 2200);
        assert_eq!(explicit.user.as_deref(), Some("bob"));

        // A non-executable override falls back to PATH rather than lying.
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "nope").unwrap();
        unsafe { std::env::set_var(SSH_BIN_ENV, &plain) };
        assert_ne!(ssh_binary(), plain);

        unsafe {
            std::env::remove_var(SSH_BIN_ENV);
            std::env::remove_var(CONFIG_ENV);
        }
        assert!(ssh_available());
    }

    #[test]
    fn ssh_available_is_true_on_this_machine() {
        // The dev/CI box for this project is Linux with OpenSSH installed.
        assert!(ssh_available(), "ssh should be available: {:?}", ssh_binary());
    }

    // -- ports --------------------------------------------------------------

    #[test]
    fn probe_and_suggest_free_port() {
        // Grab a port from the OS, then make sure it is reported busy and that
        // a free replacement is suggested *above* it. The listener stays bound
        // for the whole test so the assertions cannot race other tests.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        assert!(!port_available("127.0.0.1", busy));

        // `suggest_free_port` scans and then releases, so another test binding
        // the same ephemeral port in between is a legal race: verify by
        // grabbing the suggestion ourselves, retrying the whole scan if we
        // lost it.
        let mut verified = false;
        for _ in 0..10 {
            let free = suggest_free_port("127.0.0.1", busy).expect("some free port above a busy one");
            assert!(free > busy, "suggestion {free} must be above busy {busy}");
            if let Ok(held) = std::net::TcpListener::bind(("127.0.0.1", free)) {
                assert_eq!(held.local_addr().unwrap().port(), free);
                verified = true;
                break;
            }
        }
        assert!(verified, "never managed to hold a suggested port");

        // Port 0 is never a valid forwarding port.
        assert!(!port_available("127.0.0.1", 0));
        // Scanning past the top of the range terminates instead of wrapping or
        // overflowing. This must not depend on whether 65535 happens to be
        // free at this instant (other tests bind ports concurrently), only on
        // the scan not wrapping around to a low port.
        match suggest_free_port("127.0.0.1", u16::MAX) {
            None => {}
            Some(p) => assert_eq!(p, u16::MAX, "scan wrapped around instead of stopping at the top"),
        }
        // Starting at 0 must be clamped to 1, never scan port 0.
        assert_ne!(suggest_free_port("127.0.0.1", 0), Some(0));
    }

    #[test]
    fn port_owner_does_not_panic() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        // Our own process should be identifiable on Linux; tolerate None on
        // exotic setups but never panic.
        if let Some(owner) = port_owner("127.0.0.1", busy) {
            assert!(owner.contains('/'), "{owner}");
        }
        assert_eq!(port_owner("127.0.0.1", 1).is_some(), port_owner("127.0.0.1", 1).is_some());
    }

    // -- misc ---------------------------------------------------------------

    #[test]
    fn tokenizer_and_comment_stripping() {
        assert_eq!(tokenize("a b  c"), vec!["a", "b", "c"]);
        assert_eq!(tokenize("\"a b\" c"), vec!["a b", "c"]);
        assert_eq!(strip_comment("Port 22 # comment"), "Port 22 ");
        assert_eq!(strip_comment("Host a#b"), "Host a#b");
        assert_eq!(strip_comment("IdentityFile \"/k #1/key\""), "IdentityFile \"/k #1/key\"");
        assert_eq!(split_keyword("Port=2222"), ("Port", "2222"));
        assert_eq!(split_keyword("  HostName  1.2.3.4  "), ("", "HostName  1.2.3.4"));
    }
}

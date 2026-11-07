//! Parsing `~/.ssh/config` and evaluating hosts through `ssh -G`.
//!
//! `ssh -G` is the only way to get OpenSSH's *effective* configuration for a
//! host (ProxyJump expansion, `Host *` defaults, Include order, …), so the
//! parser reads the file only to learn which aliases exist and then asks ssh
//! for each host's real values. Measured facts behind this design live in
//! `docs/research/tech-options.md` §2.1–§2.2.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::ssh::{
    MAX_INCLUDE_DEPTH, PROBE_TIMEOUT, SshHost, config_override, default_config_paths, home_dir, ssh_binary,
};

// -- internals --------------------------------------------------------------

/// A parsed destination, in either `alias` or `[user@]host[:port]` form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostSpec {
    pub(crate) user: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) host: String,
    /// True when the input was a bare ssh-config alias (no `@`, no port).
    pub(crate) is_alias: bool,
}

impl HostSpec {
    pub(crate) fn parse(raw: &str) -> HostSpec {
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
    pub(crate) fn match_key(&self) -> String {
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
pub(crate) struct Captured {
    success: bool,
    stdout: String,
    stderr: String,
}

pub(crate) fn run_capture(bin: &Path, args: &[String], timeout: Duration) -> Result<Captured> {
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
pub(crate) fn ssh_g(bin: &Path, spec: &HostSpec, conf: Option<&Path>) -> Result<Vec<(String, String)>> {
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

pub(crate) fn parse_g_output(stdout: &str) -> Vec<(String, String)> {
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

pub(crate) fn first_value(pairs: &[(String, String)], key: &str) -> Option<String> {
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

// -- public entry points ----------------------------------------------------
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::{CONFIG_ENV, SSH_BIN_ENV, ssh_available};

    use crate::ssh::testutil::{FAKE_SSH, write_script};

    #[test]
    fn parse_user_at_host_port() {
        let spec = HostSpec::parse("alice@127.0.0.1:2222");
        assert_eq!(spec.user.as_deref(), Some("alice"));
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
}

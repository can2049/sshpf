//! Command-line surface: argument parsing, the help text, and the non-TUI modes.
//!
//! Everything here runs *before* (or instead of) the terminal: `main` owns the
//! process life cycle, `run_tui` owns the screen, and this module owns what the
//! user asked for on the command line. Keeping it apart is what lets the TUI code
//! stay a readable loop instead of being buried under 200 lines of help text.
//!
//! Two rules this file exists to keep:
//!
//! * **`USAGE` is the single source of truth** for the help text. It is bilingual
//!   and its wording is load-bearing (users grep it, `docs/keymap.md` mirrors it),
//!   so it lives in one const and is never assembled from pieces.
//! * **A broken pipe is not an error worth a panic.** `--help | head -1` and
//!   `> /dev/full` are legal; every write here goes through [`write_stdout`] and
//!   reports failure instead of unwrapping.
//!
//! The three modes are mutually exclusive in intent but not in parsing: `--check`
//! and `--list-hosts` can both be given, and `main` decides which to run (in the
//! same order this file documents).

use std::io::Write as _;
use std::process::ExitCode;

use crate::config::{self, Paths};
use crate::eventlog;
use crate::logging;
use crate::ssh;

const USAGE: &str = "\
sshpf — SSH 端口转发 TUI 管理器 / SSH port-forward manager for the terminal

用法 / Usage:
  sshpf [选项 / options]

选项 / Options:
  -c, --config <PATH>   使用指定的配置文件（默认 Linux ~/.config/sshpf/config.toml，
                        Windows %APPDATA%\\sshpf\\config.toml）
                        Use this config file (defaults: Linux ~/.config/sshpf/config.toml,
                        Windows %APPDATA%\\sshpf\\config.toml)
      --no-restore      启动时不自动恢复上次启用的转发
                        Do not restore the forwards enabled in the last run
      --server          在前台作为 server 运行（拥有全部转发，不显示界面）
                        Run as the server in the foreground (owns every forward,
                        draws nothing). 通常由 client 自动拉起，手动启动仅用于排障。
                        Normally started by a client; run it by hand to debug.
      --keep-server     最后一个 client 退出后 server 继续运行
                        Keep the server up after its last client leaves
      --list-hosts      打印 ~/.ssh/config 中的主机后退出
                        Print the hosts in ~/.ssh/config, then exit
      --check           校验配置文件后退出（不启动 UI）
                        Validate the config file, then exit (no UI)
      --ignore-host-key-changes
                        接受已改变的主机密钥（不写入 known_hosts，仅告警）
                        Accept a changed host key (warns; known_hosts is untouched)
                        用于开发机 / 跳板机密钥轮换后 ssh 能连、本工具报
                        host key verification failed 的情况。
                        For a dev or jump host whose key rotates: `ssh` connects but
                        this tool reports \"host key verification failed\".
                        更推荐在 ~/.ssh/config 里写 StrictHostKeyChecking no
                        （见 README）：那个设置只影响该主机，本开关影响所有主机。
                        Prefer `StrictHostKeyChecking no` in ~/.ssh/config for one
                        host (see README): that setting is per host, this flag is global.
  -h, --help            显示本帮助 / Show this help
  -V, --version         显示版本 / Show the version

日志 / Logging:
  每次运行都会向日志文件追加结构化日志（启动/关闭、隧道启停与重连、异常与
  失败原因、panic 与致命信号）。默认位置：
  Linux   ~/.local/state/sshpf/logs/sshpf.log
  Windows %LOCALAPPDATA%\\sshpf\\logs\\sshpf.log
  Every run appends structured logs to a file (start/exit, tunnel up/down/reconnect,
  failures, panics and fatal signals). Default location:
  Linux   ~/.local/state/sshpf/logs/sshpf.log
  Windows %LOCALAPPDATA%\\sshpf\\logs\\sshpf.log
  SSHPF_LOG_DIR=<目录/dir>     改日志目录 / Change the log directory
  SSHPF_LOG_LEVEL=<级别/level> off|error|warn|info|debug（默认/default info）
  SSHPF_IPC_DIR=<目录/dir>     改 client/server 端点目录（端口/令牌/锁）
                               Change where the client/server endpoint files live
  超过 2 MiB 自动轮转，保留 3 个归档（sshpf.log.1 … .3）。
  Rotates above 2 MiB, keeping 3 archives (sshpf.log.1 … .3).

界面快捷键 / Interface keys:
  j / ↓     下移选择 / Move selection down
  k / ↑     上移选择 / Move selection up
  g / home  跳到首行 / Jump to the first row
  G / end   跳到末行 / Jump to the last row
  space     启用 / 停用当前转发（运行中的会先确认）
            Start / stop the current forward (asks first while it is running)
  r         重启当前转发 / Restart the current forward
  n         新建转发 / Create a forward
  e / E     编辑当前转发 / Edit the current forward
  d         删除当前转发（需确认）/ Delete the current forward (asks first)
  enter / l 查看详情与日志 / Open the detail + log view
  /         过滤列表（名称 / 主机）/ Filter the list (name / host)
  esc       清除过滤 / Clear the filter
  a         全部启用（需确认）/ Start all forwards (asks first)
  x         全部停用（需确认）/ Stop all forwards (asks first)
  v / V     进入 / 退出多选模式（批量启停、重启、删除）
            Enter / leave selection mode (batch start, restart, delete)
  ?         打开 / 关闭帮助浮层（全部按键的中英说明）
            Open / close the help overlay (every key, Chinese + English)
  q         退出（有运行中转发时确认）/ Quit (asks first while forwards run)
  ctrl+c    同 q / Same as q

导入面板 / Import pane:
  新建转发时选中主机，该主机在 ~/.ssh/config 里声明的
  LocalForward/RemoteForward/DynamicForward 会自动列进「从 ~/.ssh/config 导入转发」
  面板并默认全部勾选：取消掉不要的、或全不选（U），再 ctrl+s 保存。
  面板可用 space/i 收起与重新展开；面板内可逐列筛选、撤销筛选。
  In the create form, pick the SSH host: every LocalForward / RemoteForward /
  DynamicForward it declares is listed in the \"import from ~/.ssh/config\" pane and
  ticked by default — untick what you do not want (or press U for none), then
  ctrl+s to save. Toggle the pane with space / i; columns can be filtered and
  filters undone.
    space 勾选 / Tick a row          a / U / I  全选 / 全不选 / 反选 / all / none / invert
    f, ← →  选择 / 切换列            F          清除该列筛选 / Clear this column
    ctrl+r  清除全部筛选 / Clear all filters     z  撤销上次筛选改动 / Undo filter change
    ctrl+z  撤销（筛选输入中）/ Undo while typing a filter
    j / ↓  k / ↑  g / G  pgup / pgdn  移动光标 / Move the cursor
    enter   结束筛选输入 / Finish typing a filter
    esc     收起面板（先退出筛选）/ Close the pane (leaves the filter first)
    i       收起面板 / Close the pane
    ctrl+s  保存并创建 N 条勾选行对应的隧道 / Save, creating one tunnel per ticked row
  注意：面板打开后 space 归「勾选」，收起请用 i 或 esc；字面字符 f 不能输入到筛选里。
  Note: once open, space ticks rows — close the pane with i or esc. A literal
  \"f\" cannot be typed into a filter, because it always means \"next column\".

鼠标 / Mouse:
  点击行          选中该行（多选模式下为切换勾选）/ Select the row (toggles the mark in selection mode)
  shift+点击      从锚点扩选 / Extend the selection from the anchor
  点击勾选框      勾选 / 取消该行 / Tick or untick the row
  点击列头        筛选该列 / Start filtering that column
  点击筛选 chip 的 ×  删除该列筛选 / Remove that column's filter
  点击 全选 / 反选 / 全不选 以及 清除全部筛选 / 撤销筛选
                  Run the action (disabled ones are dim and not clickable)
  点击多选动作条  批量启用 / 停用 / 重启 / 删除（均需确认）
                  Batch start / stop / restart / delete (all ask first)
  滚轮            滚动列表（移动光标）/ Scroll the list (moves the cursor)
  点击落空        什么都不做；弹窗会吞掉点击，不会穿透
                  Does nothing; popups swallow clicks instead of leaking them

配置文件为 TOML，权限 0600；转发定义由本工具管理，
SSH 连接参数（ProxyJump/IdentityFile/ssh-agent 等）交给 ~/.ssh/config。
The config file is TOML with mode 0600. Tunnel definitions are owned by sshpf;
SSH connection parameters (ProxyJump, IdentityFile, ssh-agent, …) stay in
~/.ssh/config, which this tool reads but never rewrites.

多窗口 / Multiple windows:
  同时打开多个 sshpf 时，只有一个进程真正持有转发（server），其余窗口连上它，
  所有窗口看到同一份转发列表，任何一处改动都会同步到全部窗口。
  **最后一个窗口退出时，server 也随之退出，所有转发停止** —— 即使那些转发是
  别的窗口启动的。想让它常驻请用 --keep-server。
  When several sshpf windows are open, one process owns the forwards (the server)
  and the others attach to it: every window shows the same list, and a change in
  one appears in all. **Closing the last window stops the server and every
  forward**, including ones another window started. Use --keep-server to keep it
  running instead.
";

/// What the command line asked for.
///
/// Public so `main` can read the fields without a getter per flag; the struct is
/// inert data.
#[derive(Debug, Default)]
pub struct Args {
    /// Kept as an `OsString`: a config path need not be valid UTF-8.
    pub config: Option<std::ffi::OsString>,
    pub no_restore: bool,
    pub list_hosts: bool,
    pub check: bool,
    /// Accept a changed server host key instead of refusing the connection.
    pub ignore_host_key_changes: bool,
    /// Run as the owning server in the foreground instead of as a client.
    pub server: bool,
    /// Keep the server running after its last client leaves.
    pub keep_server: bool,
    /// The pid of the client that spawned this server, when one did.
    ///
    /// Diagnostic only: it is recorded so a log can answer "which window started
    /// this server, and when did it go away". It deliberately changes no
    /// behaviour — the server's lifetime is decided by its client *count*, never by
    /// whether the process that spawned it is still alive. A server that died with
    /// its spawner would stop everyone else's forwards when that one terminal
    /// closed, which is the opposite of the requirement.
    pub attach_from: Option<u32>,
}

impl Args {
    /// Process-wide host-key policy implied by the command line.
    pub fn host_key_policy(&self) -> crate::ssh::known_hosts::HostKeyPolicy {
        if self.ignore_host_key_changes {
            crate::ssh::known_hosts::HostKeyPolicy::Ignore
        } else {
            crate::ssh::known_hosts::HostKeyPolicy::default()
        }
    }
}

/// The config/state paths implied by `--config` (or the platform defaults).
pub fn paths_for(args: &Args) -> Paths {
    match &args.config {
        Some(p) => {
            let config = std::path::PathBuf::from(p);
            let state = config.with_file_name("state.toml");
            Paths { config, state }
        }
        None => Paths::resolve(),
    }
}

/// Parse the command line.
///
/// Reads `args_os`, not `args`: `std::env::args()` **panics** on an argument that
/// is not valid UTF-8, and a filename in a non-UTF-8 locale is a perfectly legal
/// thing to pass. Non-UTF-8 arguments are handled two ways, whichever is
/// truthful: as a path they are kept byte-for-byte (the OS API is `OsStr`), as an
/// option name they are rejected with a normal usage error.
///
/// Help and version output is written through `write!` rather than `print!` /
/// `println!`, which panic on a broken pipe or a full disk (`sshpf --help | head
/// -1`, `> /dev/full`). A help text that cannot be delivered is not an error
/// worth a panic; the exit code still says what happened.
pub fn parse_args() -> Result<Option<Args>, String> {
    parse_args_from(std::env::args_os().skip(1))
}

fn parse_args_from<I>(args: I) -> Result<Option<Args>, String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut parsed = Args {
        config: None,
        no_restore: false,
        list_hosts: false,
        check: false,
        ignore_host_key_changes: false,
        server: false,
        keep_server: false,
        attach_from: None,
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        // Lossy only for the *match*; a path is stored untouched below.
        let flag = arg.to_string_lossy().into_owned();
        match flag.as_str() {
            "-h" | "--help" => {
                let _ = write_stdout(USAGE);
                return Ok(None);
            }
            "-V" | "--version" => {
                let _ = writeln!(std::io::stdout(), "sshpf {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-c" | "--config" => match it.next() {
                Some(p) => parsed.config = Some(p),
                None => {
                    return Err("--config 需要一个路径参数 / --config needs a path argument".to_string());
                }
            },
            "--no-restore" => parsed.no_restore = true,
            "--list-hosts" => parsed.list_hosts = true,
            "--check" => parsed.check = true,
            "--ignore-host-key-changes" => parsed.ignore_host_key_changes = true,
            "--server" => parsed.server = true,
            "--keep-server" => parsed.keep_server = true,
            "--attach-from" => match it.next() {
                Some(pid) => parsed.attach_from = Some(parse_pid(&pid)?),
                None => {
                    return Err("--attach-from 需要一个进程号 / --attach-from needs a pid argument".to_string());
                }
            },
            other => {
                return Err(format!("未知参数 / Unknown option: {other}\n\n{USAGE}"));
            }
        }
    }
    Ok(Some(parsed))
}

/// Parse the pid carried by `--attach-from`.
///
/// A bad value is a usage error rather than a panic: this argument is produced by
/// `sshpf` itself, so a malformed one means a version mismatch or a hand-run typo,
/// and both deserve the usage text.
fn parse_pid(raw: &std::ffi::OsString) -> Result<u32, String> {
    match raw.to_string_lossy().trim().parse::<u32>() {
        Ok(pid) => Ok(pid),
        Err(_) => Err(format!(
            "--attach-from 需要一个正整数进程号 / --attach-from needs a positive pid, got {:?}",
            raw.to_string_lossy()
        )),
    }
}

/// Best-effort stdout write: returns whether it landed, never panics.
pub(crate) fn write_stdout(text: &str) -> bool {
    let mut out = std::io::stdout();
    out.write_all(text.as_bytes()).and_then(|()| out.flush()).is_ok()
}

pub fn list_hosts() -> ExitCode {
    match ssh::list_hosts() {
        Ok(hosts) if hosts.is_empty() => {
            if !write_stdout("未在 ~/.ssh/config 中找到任何主机 / No hosts found in ~/.ssh/config\n") {
                logging::warn("cli.stdout_failed", &[("mode", &"list_hosts")]);
            }
            ExitCode::SUCCESS
        }
        Ok(hosts) => {
            // One buffer, one write: `| head -1` closing the pipe mid-loop would
            // otherwise turn a successful listing into a panic on the next line.
            let mut out = format!("{} 个主机 / {} host(s):\n", hosts.len(), hosts.len());
            for h in hosts {
                out.push_str("  ");
                out.push_str(&h.display());
                out.push('\n');
            }
            if !write_stdout(&out) {
                // stdout is gone (closed pipe, full disk): the listing itself
                // succeeded and nothing can be shown, so say so in the log and
                // leave the exit status alone.
                logging::warn("cli.stdout_failed", &[("mode", &"list_hosts")]);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eventlog::cli_error("sshconfig.load_failed", &e.to_string());
            let _ = writeln!(std::io::stderr(), "读取 ~/.ssh/config 失败 / Failed to read ~/.ssh/config: {e}");
            ExitCode::FAILURE
        }
    }
}

pub fn check_config(paths: &Paths) -> ExitCode {
    match config::load(&paths.config) {
        Ok(cfg) => {
            let problems = cfg.validate();
            let mut out = format!(
                "{}: {} 条转发 / {} tunnel(s)\n",
                config::display_path(&paths.config),
                cfg.tunnels.len(),
                cfg.tunnels.len()
            );
            if problems.is_empty() {
                out.push_str("校验通过 / Config OK\n");
            } else {
                for p in &problems {
                    out.push_str("  警告 / Warning: ");
                    out.push_str(p);
                    out.push('\n');
                }
                eventlog::config_loaded(&config::display_path(&paths.config), cfg.tunnels.len(), &problems);
            }
            // The verdict is in the exit code and the file; a failed stdout write
            // only costs the echo, never the exit status.
            if !write_stdout(&out) {
                logging::warn("cli.stdout_failed", &[("mode", &"check")]);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eventlog::cli_error("config.invalid", &e.to_string());
            let _ = writeln!(std::io::stderr(), "配置错误 / Config error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests;

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
  -c, --config <PATH>   使用指定的配置文件（默认 ~/.config/sshpf/config.toml）
                        Use this config file (default ~/.config/sshpf/config.toml)
      --no-restore      启动时不自动恢复上次启用的转发
                        Do not restore the forwards enabled in the last run
      --list-hosts      打印 ~/.ssh/config 中的主机后退出
                        Print the hosts in ~/.ssh/config, then exit
      --check           校验配置文件后退出（不启动 UI）
                        Validate the config file, then exit (no UI)
      --server          作为后台服务进程运行：持有全部隧道，供多个客户端共享
                        Run as the background server that owns every tunnel so
                        several clients can share one state (a client does this
                        automatically when none is running)
      --keep-server     最后一个客户端退出后仍保留服务端
                        Keep the server running after the last client exits
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
  每次运行都会向 ~/.local/state/sshpf/logs/sshpf.log 追加结构化日志
  （启动/关闭、隧道启停与重连、异常与失败原因、panic 与致命信号）。
  Every run appends structured logs to ~/.local/state/sshpf/logs/sshpf.log
  (start/exit, tunnel up/down/reconnect, failures, panics and fatal signals).
  SSHPF_LOG_DIR=<目录/dir>     改日志目录 / Change the log directory
  SSHPF_LOG_LEVEL=<级别/level> off|error|warn|info|debug（默认/default info）
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
    /// Run as the authoritative server for this config instead of as a client.
    pub server: bool,
    /// Keep the server alive after the last client leaves.
    pub keep_server: bool,
    /// Accept a changed server host key instead of refusing the connection.
    pub ignore_host_key_changes: bool,
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
    let mut parsed =
        Args {
            config: None,
            no_restore: false,
            list_hosts: false,
            check: false,
            server: false,
            keep_server: false,
            ignore_host_key_changes: false,
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
            "--server" => parsed.server = true,
            "--keep-server" => parsed.keep_server = true,
            "--ignore-host-key-changes" => parsed.ignore_host_key_changes = true,
            other => {
                return Err(format!("未知参数 / Unknown option: {other}\n\n{USAGE}"));
            }
        }
    }
    Ok(Some(parsed))
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
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn parse(args: &[&str]) -> Result<Option<Args>, String> {
        parse_args_from(args.iter().map(OsString::from))
    }

    /// The error half of a parse, for the cases that must fail.
    fn parse_err(args: &[&str]) -> String {
        match parse(args) {
            Err(e) => e,
            Ok(_) => panic!("expected a usage error for {args:?}"),
        }
    }

    #[test]
    fn plain_flags_parse() {
        let args = parse(&["--check"]).unwrap().unwrap();
        assert!(args.check && !args.list_hosts && !args.no_restore);
        let args = parse(&["--list-hosts", "--no-restore"]).unwrap().unwrap();
        assert!(args.list_hosts && args.no_restore && !args.check);
    }

    #[test]
    fn config_takes_the_next_argument_verbatim() {
        let args = parse(&["-c", "/tmp/x.toml"]).unwrap().unwrap();
        assert_eq!(args.config.as_deref(), Some(std::ffi::OsStr::new("/tmp/x.toml")));
        let args = parse(&["--config", "rel/config.toml"]).unwrap().unwrap();
        assert_eq!(args.config.as_deref(), Some(std::ffi::OsStr::new("rel/config.toml")));
    }

    #[test]
    fn missing_config_value_is_a_usage_error_not_a_panic() {
        let err = parse_err(&["--config"]);
        assert!(err.contains("--config"), "{err}");
    }

    #[test]
    fn unknown_flags_are_rejected_with_the_usage_text() {
        let err = parse_err(&["--nope"]);
        assert!(err.contains("未知参数"), "{err}");
        assert!(err.contains("用法"), "the message must include the usage: {err}");
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["-h"]).unwrap().is_none());
        assert!(parse(&["-V"]).unwrap().is_none());
        assert!(parse(&["--version"]).unwrap().is_none());
        // Everything after `--help` is irrelevant, and must not be validated.
        assert!(parse(&["--help", "--totally-bogus"]).unwrap().is_none());
    }

    /// The bug: `std::env::args()` panics on a non-UTF-8 argument. A config path
    /// is an OS path, so it has to survive byte-for-byte.
    #[test]
    fn a_non_utf8_config_path_is_accepted_verbatim() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::{OsStrExt, OsStringExt};
            let raw = OsString::from_vec(b"/tmp/\xff\xfe.toml".to_vec());
            let parsed = parse_args_from(vec![OsString::from("--config"), raw.clone()]).unwrap().unwrap();
            assert_eq!(parsed.config.as_deref().map(|c| c.as_bytes()), Some(&b"/tmp/\xff\xfe.toml"[..]));
        }
        // The portable check: at minimum it must not panic, on any platform.
        let raw = OsString::from("/tmp/config.toml");
        assert!(parse_args_from(vec![OsString::from("--config"), raw]).is_ok());
    }

    /// A non-UTF-8 *option name* cannot be a known flag, so it is a usage error
    /// (exit 2) rather than a panic.
    #[test]
    fn a_non_utf8_flag_is_a_usage_error() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = OsString::from_vec(b"--\xff".to_vec());
            let err = match parse_args_from(vec![raw]) {
                Err(e) => e,
                Ok(_) => panic!("a non-UTF-8 flag must be a usage error"),
            };
            assert!(err.contains("未知参数"), "{err}");
        }
    }

    #[test]
    fn write_stdout_reports_instead_of_panicking() {
        // Not a tty here, so this exercises the buffered path end to end.
        assert!(write_stdout("sshpf test line\n"));
    }

    #[test]
    fn ignore_host_key_changes_is_off_by_default() {
        let args = parse(&[]).unwrap().unwrap();
        assert!(!args.ignore_host_key_changes, "the escape hatch must be opt-in, never the default");
        assert_eq!(args.host_key_policy(), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew);
    }

    #[test]
    fn ignore_host_key_changes_flag_parses_to_the_ignore_policy() {
        let args = parse(&["--ignore-host-key-changes"]).unwrap().unwrap();
        assert!(args.ignore_host_key_changes);
        assert_eq!(args.host_key_policy(), crate::ssh::known_hosts::HostKeyPolicy::Ignore);
    }

    /// The flag is additive: it must not swallow the other modes, because
    /// `--check --ignore-host-key-changes` is a legitimate "validate with my
    /// usual flags" invocation.
    #[test]
    fn ignore_host_key_changes_composes_with_the_other_flags() {
        let args = parse(&["--check", "--ignore-host-key-changes", "--no-restore"]).unwrap().unwrap();
        assert!(args.check && args.no_restore && args.ignore_host_key_changes);
        assert_eq!(args.host_key_policy(), crate::ssh::known_hosts::HostKeyPolicy::Ignore);
        // Position must not matter.
        let reordered = parse(&["--ignore-host-key-changes", "--check"]).unwrap().unwrap();
        assert!(reordered.check && reordered.ignore_host_key_changes);
    }

    /// Order after `--help` is irrelevant and must not be validated — the same
    /// rule the other flags follow.
    #[test]
    fn ignore_host_key_changes_after_help_is_never_reached() {
        assert!(parse(&["--help", "--ignore-host-key-changes"]).unwrap().is_none());
    }

    #[test]
    fn the_usage_text_documents_the_ignore_flag() {
        // Users grep the help text and `docs/keymap.md` mirrors it; a flag with
        // no help line is undiscoverable.
        assert!(USAGE.contains("--ignore-host-key-changes"), "USAGE must list the flag");
        assert!(USAGE.contains("Accept a changed host key"), "the flag needs an English explanation");
    }
}

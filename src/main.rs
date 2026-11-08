//! `sshpf` — SSH port forward manager with a TUI dashboard.
//!
//! Entry point: CLI parsing, terminal lifecycle, signal handling and the
//! event loop. All state lives in [`app::App`]; all drawing in [`ui::draw`].

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use ratatui::DefaultTerminal;

use sshpf::app::App;
use sshpf::app::log as eventlog;
use sshpf::config::{self, Paths};
use sshpf::logging;
use sshpf::model::StatusMsg;
use sshpf::{keys, ssh, ui};

/// Last quit signal received (`0` = none). Written by a signal handler, read
/// after the loop, so it is an atomic rather than shared state.
static RECEIVED_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Stable signal name for the log.
fn signal_name(sig: i32) -> String {
    signal_hook::low_level::signal_name(sig).unwrap_or("SIGUNKNOWN").to_string()
}

/// Poll interval: how often keys/events are checked and the UI redraws.
const TICK: Duration = Duration::from_millis(200);

const USAGE: &str = "\
sshpf — SSH 端口转发 TUI 管理器

用法:
  sshpf [选项]

选项:
  -c, --config <PATH>   使用指定的配置文件（默认 ~/.config/sshpf/config.toml）
      --no-restore      启动时不自动恢复上次启用的转发
      --list-hosts      打印 ~/.ssh/config 中的主机后退出
      --check           校验配置文件后退出（不启动 UI）
  -h, --help            显示本帮助
  -V, --version         显示版本

日志:
  每次运行都会向 ~/.local/state/sshpf/logs/sshpf.log 追加结构化日志
  （启动/关闭、隧道启停与重连、异常与失败原因、panic 与致命信号）。
  SSHPF_LOG_DIR=<目录>    改日志目录
  SSHPF_LOG_LEVEL=<级别>  off|error|warn|info|debug（默认 info）
  超过 2 MiB 自动轮转，保留 3 个归档（sshpf.log.1 … .3）。

界面快捷键:
  j/k ↑↓   移动        space  启用/停用    r  重启
  n        新建        e      编辑        d  删除（需确认）
  enter/l  详情+日志    /      过滤        a  全部启用   x  全部停止
  v        多选模式（标记多条后批量启停/重启/删除）
  ?        帮助        q      退出（有运行中转发时确认）

新建转发时选中主机，该主机在 ~/.ssh/config 里声明的
LocalForward/RemoteForward/DynamicForward 会自动列进「从 ~/.ssh/config 导入转发」
面板并默认全部勾选：取消掉不要的、或全不选（U），再 ctrl+s 保存。
面板可用 space/i 收起与重新展开；面板内可逐列筛选、撤销筛选。

鼠标：点击行选中、点击勾选框、点击列头筛选该列、点击筛选 chip 的 × 删除、
滚轮滚动列表、多选模式下点击动作条按钮、shift 点击扩选范围。

配置文件为 TOML，权限 0600；转发定义由本工具管理，
SSH 连接参数（ProxyJump/IdentityFile/ssh-agent 等）交给 ~/.ssh/config。
";

#[derive(Debug)]
struct Args {
    /// Kept as an `OsString`: a config path need not be valid UTF-8.
    config: Option<std::ffi::OsString>,
    no_restore: bool,
    list_hosts: bool,
    check: bool,
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
fn parse_args() -> Result<Option<Args>, String> {
    parse_args_from(std::env::args_os().skip(1))
}

fn parse_args_from<I>(args: I) -> Result<Option<Args>, String>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut parsed = Args { config: None, no_restore: false, list_hosts: false, check: false };
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
                None => return Err("--config 需要一个路径参数".to_string()),
            },
            "--no-restore" => parsed.no_restore = true,
            "--list-hosts" => parsed.list_hosts = true,
            "--check" => parsed.check = true,
            other => return Err(format!("未知参数: {other}\n\n{USAGE}")),
        }
    }
    Ok(Some(parsed))
}

/// Best-effort stdout write: returns whether it landed, never panics.
fn write_stdout(text: &str) -> bool {
    let mut out = std::io::stdout();
    out.write_all(text.as_bytes()).and_then(|()| out.flush()).is_ok()
}

fn main() -> ExitCode {
    // Logging first: everything below, including a failed `--config` parse, is
    // more useful with a record of it. A failure here is not fatal — sshpf works
    // without a log — but it is reported once the TUI is up (see `run_tui`).
    let log_path = logging::init_from_env().ok();
    logging::install_crash_hooks();
    let started = Instant::now();

    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eventlog::cli_error("cli.bad_args", &e);
            let _ = writeln!(std::io::stderr(), "{e}");
            return ExitCode::from(2);
        }
    };

    let paths = match &args.config {
        Some(p) => {
            let config = std::path::PathBuf::from(p);
            let state = config.with_file_name("state.toml");
            Paths { config, state }
        }
        None => Paths::resolve(),
    };

    eventlog::app_start(
        env!("CARGO_PKG_VERSION"),
        &config::display_path(&paths.config),
        &log_path.as_ref().map(|p| config::display_path(p)).unwrap_or_default(),
    );

    if args.list_hosts {
        eventlog::cli_mode("list_hosts");
        return list_hosts();
    }
    if args.check {
        eventlog::cli_mode("check");
        return check_config(&paths);
    }

    match run_tui(paths, args.no_restore, started) {
        Ok(code) => code,
        Err(e) => {
            // The TUI could not even start (terminal init, event loop I/O) — the
            // one failure with no UI left to explain it.
            logging::error("app.fatal", &[("stage", &"tui"), ("msg", &e.to_string())]);
            let _ = writeln!(std::io::stderr(), "错误: {e}");
            ExitCode::FAILURE
        }
    }
}

fn list_hosts() -> ExitCode {
    match ssh::list_hosts() {
        Ok(hosts) if hosts.is_empty() => {
            if !write_stdout("未在 ~/.ssh/config 中找到任何主机\n") {
                logging::warn("cli.stdout_failed", &[("mode", &"list_hosts")]);
            }
            ExitCode::SUCCESS
        }
        Ok(hosts) => {
            // One buffer, one write: `| head -1` closing the pipe mid-loop would
            // otherwise turn a successful listing into a panic on the next line.
            let mut out = format!("{} 个主机:\n", hosts.len());
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
            let _ = writeln!(std::io::stderr(), "读取 ~/.ssh/config 失败: {e}");
            ExitCode::FAILURE
        }
    }
}

fn check_config(paths: &Paths) -> ExitCode {
    match config::load(&paths.config) {
        Ok(cfg) => {
            let problems = cfg.validate();
            let mut out = format!("{}: {} 条转发\n", config::display_path(&paths.config), cfg.tunnels.len());
            if problems.is_empty() {
                out.push_str("校验通过\n");
            } else {
                for p in &problems {
                    out.push_str("  警告: ");
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
            let _ = writeln!(std::io::stderr(), "配置错误: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_tui(paths: Paths, no_restore: bool, started: Instant) -> std::io::Result<ExitCode> {
    // A signal only flips this flag: the main loop then exits normally so that
    // `Drop` handlers stop the child processes. `process::exit()` would skip them.
    let quit = Arc::new(AtomicBool::new(false));
    // Which signals exist differs per OS (no SIGHUP on Windows); `sys` knows.
    //
    // Two things have to happen per signal: flip the quit flag (so the loop exits
    // normally and every `Drop` runs), and remember *which* signal arrived — the
    // TUI is gone by then and the exit code is a plain 0, so otherwise there is no
    // way to tell Ctrl-C from a `kill`. Both are async-signal-safe: one atomic
    // store each. Nothing else may run in a handler here (logging takes a mutex).
    for sig in sshpf::sys::quit_signals() {
        let set_flag = quit.clone();
        let sig = *sig;
        // SAFETY: the closure only performs atomic stores.
        let registered = unsafe {
            signal_hook::low_level::register(sig, move || {
                RECEIVED_SIGNAL.store(sig, Ordering::Relaxed);
                set_flag.store(true, Ordering::SeqCst);
            })
        };
        if let Err(e) = registered {
            // Swallowing this used to be a silent trap: Ctrl-C would kill the
            // process without running any `Drop`, leaving tunnels unstopped and
            // the terminal in raw mode. Not fatal — the TUI still runs, and `q`
            // quits — but the user deserves to know which shortcut is gone.
            logging::warn("app.signal_unavailable", &[("signal", &signal_name(sig)), ("msg", &e.to_string())]);
        }
    }

    // Take the terminal **before** any tunnel is restored: if there is no tty
    // (`sshpf < /dev/null`, a CI runner, `ssh host sshpf` without a pty, a Windows
    // process with no console), this is the first thing that fails and there is
    // nothing to clean up yet.
    //
    // `ratatui::init()` is `try_init().expect("failed to initialize terminal")`, so
    // it **panics** in exactly those cases — bypassing the `app.fatal` handling
    // below, which was written for this failure. `try_init` returns the error
    // instead, so it reaches `main` as an `io::Error`, is recorded as `app.fatal`,
    // and exits 1 instead of 101.
    //
    // `try_init` installs ratatui's own panic hook (which restores the terminal)
    // *after* ours, so the two chain: ratatui restores the screen and then calls
    // our hook, which writes `crash.panic`. That is the documented ordering of both
    // libraries and the one the crash tests rely on.
    //
    // `try_init` can fail *after* it has already put the terminal in raw mode
    // (its steps are: install hook → enable raw mode → enter the alternate screen
    // → build the terminal). Unlike `init`, which panics and lets the hook restore
    // the screen, returning the error leaves that partial state in place — and the
    // shell that regains control would then be unusable. So undo it explicitly;
    // `restore` is best effort and never panics.
    let mut terminal: DefaultTerminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(e) => {
            // Only undo state we actually created: `is_raw_mode_enabled` is the
            // observable proof that `try_init` got past its first step. With no
            // tty at all it fails on that first step, and emitting escape
            // sequences at a redirected stdout would corrupt whatever is reading
            // it (a CI log, a file).
            if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
                ratatui::restore();
            }
            return Err(e);
        }
    };
    // From here on the crash handler must release the alternate screen before the
    // message lands in the user's shell.
    logging::set_tui_active(true);

    if config::perms_too_open(&paths.config) {
        // Group/world readable config: it names hosts and key paths, so this is a
        // real (if small) exposure and worth a record either way.
        let ok = config::tighten_perms(&paths.config).is_ok();
        eventlog::config_perms_tightened(&config::display_path(&paths.config), ok);
    }

    let mut app = App::new(paths);
    if let Some(reason) = logging::init_error() {
        // Silent logging loss is the worst failure mode for a diagnostic feature;
        // the user finds out through the status line instead of an empty file.
        app.set_status(StatusMsg::warn(format!("日志未启用: {reason}")));
        logging::error("log.unavailable", &[("msg", &reason)]);
    }
    app.load_hosts();
    if !no_restore {
        app.restore_enabled();
    } else {
        logging::info("app.restore_skipped", &[("reason", &"no-restore")]);
        app.set_status(StatusMsg::info("已跳过自动恢复（--no-restore）"));
    }

    // Mouse capture is on for the whole session: the import table's filtering and
    // the table's row selection are click-driven, and terminals cannot report a
    // click without it. It is best-effort — a terminal that refuses capture (some
    // multiplexers, some CI ptys) must still give a working keyboard-only UI, so
    // a failure is ignored rather than fatal.
    let mouse = execute!(std::io::stdout(), EnableMouseCapture).is_ok();
    if !mouse {
        eventlog::mouse_capture_failed();
    }
    let result = event_loop(&mut terminal, &mut app, &quit);
    let received = RECEIVED_SIGNAL.load(Ordering::Relaxed);
    if received != 0 {
        logging::info("app.signal", &[("signal", &signal_name(received))]);
    }
    logging::set_tui_active(false);
    // Stop children and remove scratch files before restoring the terminal, so
    // a stray panic cannot leave the user's terminal in raw mode.
    let was_quit = app.should_quit;
    app.shutdown();
    eventlog::app_exit(
        if was_quit { "user" } else { "signal" },
        started.elapsed().as_millis(),
        app.tunnels.iter().filter(|t| matches!(t.status, sshpf::model::TunnelStatus::Up { .. })).count(),
    );
    if mouse {
        // Releasing capture matters more than the return value: leaving it on
        // makes the shell that regains control print garbage on every move.
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    // `restore` prints to stderr (never panics) if the terminal cannot be put
    // back; the event loop's own `io::Result` is what decides the exit code.
    ratatui::restore();
    result.map(|()| ExitCode::SUCCESS)
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, quit: &Arc<AtomicBool>) -> std::io::Result<()> {
    while !quit.load(Ordering::Relaxed) {
        let frame_started = Instant::now();
        app.drain_events();
        terminal.draw(|frame| ui::draw(frame, app))?;
        // A frame that overruns the tick is the first symptom of a wedged UI
        // (a blocking call sneaking into the event pump). Logging it turns "the
        // TUI felt stuck" into a timestamp.
        let frame_ms = frame_started.elapsed().as_millis();
        if frame_ms >= logging::SLOW_FRAME_MS {
            eventlog::slow_frame(frame_ms);
        }
        if app.should_quit {
            logging::info("app.quit", &[("reason", &"user")]);
            break;
        }
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => keys::handle_key(app, key),
                // Mouse events are dispatched through the hit map the renderer
                // recorded on the frame just drawn, so `App` resolves the target
                // without `main` knowing anything about overlays or layout.
                Event::Mouse(m) => keys::handle_mouse(app, m),
                _ => {}
            }
        }
    }
    Ok(())
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

    /// `signal_name` must never panic for an out-of-range number: it is used in
    /// the log line for a signal we do not know.
    #[test]
    fn signal_name_is_total() {
        for sig in [i32::MIN, -1, 0, 9, 15, i32::MAX] {
            let name = signal_name(sig);
            assert!(!name.is_empty(), "sig {sig} produced an empty name");
        }
    }
}

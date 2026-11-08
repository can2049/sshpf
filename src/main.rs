//! `sshpf` — SSH port forward manager with a TUI dashboard.
//!
//! Entry point: CLI parsing, terminal lifecycle, signal handling and the
//! event loop. All state lives in [`app::App`]; all drawing in [`ui::draw`].

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

struct Args {
    config: Option<String>,
    no_restore: bool,
    list_hosts: bool,
    check: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args { config: None, no_restore: false, list_hosts: false, check: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("sshpf {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-c" | "--config" => match it.next() {
                Some(p) => args.config = Some(p),
                None => return Err("--config 需要一个路径参数".to_string()),
            },
            "--no-restore" => args.no_restore = true,
            "--list-hosts" => args.list_hosts = true,
            "--check" => args.check = true,
            other => return Err(format!("未知参数: {other}\n\n{USAGE}")),
        }
    }
    Ok(Some(args))
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
            eprintln!("{e}");
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
            eprintln!("错误: {e}");
            ExitCode::FAILURE
        }
    }
}

fn list_hosts() -> ExitCode {
    match ssh::list_hosts() {
        Ok(hosts) if hosts.is_empty() => {
            println!("未在 ~/.ssh/config 中找到任何主机");
            ExitCode::SUCCESS
        }
        Ok(hosts) => {
            println!("{} 个主机:", hosts.len());
            for h in hosts {
                println!("  {}", h.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eventlog::cli_error("sshconfig.load_failed", &e.to_string());
            eprintln!("读取 ~/.ssh/config 失败: {e}");
            ExitCode::FAILURE
        }
    }
}

fn check_config(paths: &Paths) -> ExitCode {
    match config::load(&paths.config) {
        Ok(cfg) => {
            let problems = cfg.validate();
            println!("{}: {} 条转发", config::display_path(&paths.config), cfg.tunnels.len());
            if problems.is_empty() {
                println!("校验通过");
                ExitCode::SUCCESS
            } else {
                for p in &problems {
                    println!("  警告: {p}");
                }
                eventlog::config_loaded(&config::display_path(&paths.config), cfg.tunnels.len(), &problems);
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eventlog::cli_error("config.invalid", &e.to_string());
            eprintln!("配置错误: {e}");
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
        unsafe {
            let _ = signal_hook::low_level::register(sig, move || {
                RECEIVED_SIGNAL.store(sig, Ordering::Relaxed);
                set_flag.store(true, Ordering::SeqCst);
            });
        }
    }

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

    let mut terminal: DefaultTerminal = ratatui::init();
    // From here on the crash handler must release the alternate screen before the
    // message lands in the user's shell.
    logging::set_tui_active(true);
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

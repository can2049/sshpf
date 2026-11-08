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
  新建转发时选中主机，按 space 或 i 展开「从 ~/.ssh/config 导入转发」面板：
  该主机已声明的 LocalForward/RemoteForward/DynamicForward 会全部列出并默认勾选，
  取消掉不要的再保存。面板内可逐列筛选、撤销筛选。
  In the create form, pick the SSH host and press space or i to expand the
  \"import forwards from ~/.ssh/config\" pane: every LocalForward / RemoteForward /
  DynamicForward that host declares is listed and ticked, so you only untick what
  you do not want, then save. Columns can be filtered, and filters undone.
    space 勾选 / Tick a row          a / U / I  全选 / 全不选 / 反选
                                                   all / none / invert (visible rows)
    f, ← →  选择 / 切换列            F          清除该列筛选 / Clear this column
    ctrl+r  清除全部筛选 / Clear all filters
    z       撤销上次筛选改动 / Undo the last filter change
    ctrl+z  撤销（筛选输入中）/ Undo (while typing a filter)
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
                None => return Err("--config 需要一个路径参数 / --config needs a path argument".to_string()),
            },
            "--no-restore" => args.no_restore = true,
            "--list-hosts" => args.list_hosts = true,
            "--check" => args.check = true,
            other => return Err(format!("未知参数 / Unknown option: {other}\n\n{USAGE}")),
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
            println!("未在 ~/.ssh/config 中找到任何主机 / No hosts found in ~/.ssh/config");
            ExitCode::SUCCESS
        }
        Ok(hosts) => {
            println!("{} 个主机 / {} host(s):", hosts.len(), hosts.len());
            for h in hosts {
                println!("  {}", h.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eventlog::cli_error("sshconfig.load_failed", &e.to_string());
            eprintln!("读取 ~/.ssh/config 失败 / Failed to read ~/.ssh/config: {e}");
            ExitCode::FAILURE
        }
    }
}

fn check_config(paths: &Paths) -> ExitCode {
    match config::load(&paths.config) {
        Ok(cfg) => {
            let problems = cfg.validate();
            println!(
                "{}: {} 条转发 / {} tunnel(s)",
                config::display_path(&paths.config),
                cfg.tunnels.len(),
                cfg.tunnels.len()
            );
            if problems.is_empty() {
                println!("校验通过 / Config OK");
                ExitCode::SUCCESS
            } else {
                for p in &problems {
                    println!("  警告 / Warning: {p}");
                }
                eventlog::config_loaded(&config::display_path(&paths.config), cfg.tunnels.len(), &problems);
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eventlog::cli_error("config.invalid", &e.to_string());
            eprintln!("配置错误 / Config error: {e}");
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

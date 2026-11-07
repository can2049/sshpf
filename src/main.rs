//! `sshpf` — SSH port forward manager with a TUI dashboard.
//!
//! Entry point: CLI parsing, terminal lifecycle, signal handling and the
//! event loop. All state lives in [`app::App`]; all drawing in [`ui::draw`].

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use ratatui::DefaultTerminal;

use sshpf::app::App;
use sshpf::config::{self, Paths};
use sshpf::model::StatusMsg;
use sshpf::{keys, ssh, ui};

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

界面快捷键:
  j/k ↑↓   移动        space  启用/停用    r  重启
  n        新建        e      编辑        d  删除（需确认）
  enter/l  详情+日志    /      过滤        a  全部启用   x  全部停止
  v        多选模式（标记多条后批量启停/重启/删除）
  ?        帮助        q      退出（有运行中转发时确认）

新建转发时选中主机，按 space 或 i 展开「从 ~/.ssh/config 导入转发」面板：
该主机已声明的 LocalForward/RemoteForward/DynamicForward 会全部列出并默认勾选，
取消掉不要的再保存。面板内可逐列筛选、撤销筛选。

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
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
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

    if args.list_hosts {
        return list_hosts();
    }
    if args.check {
        return check_config(&paths);
    }

    match run_tui(paths, args.no_restore) {
        Ok(code) => code,
        Err(e) => {
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
                for p in problems {
                    println!("  警告: {p}");
                }
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("配置错误: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_tui(paths: Paths, no_restore: bool) -> std::io::Result<ExitCode> {
    // A signal only flips this flag: the main loop then exits normally so that
    // `Drop` handlers stop the child processes. `process::exit()` would skip them.
    let quit = Arc::new(AtomicBool::new(false));
    // Which signals exist differs per OS (no SIGHUP on Windows); `sys` knows.
    for sig in sshpf::sys::quit_signals() {
        let _ = signal_hook::flag::register(*sig, quit.clone());
    }

    if config::perms_too_open(&paths.config) {
        let _ = config::tighten_perms(&paths.config);
    }

    let mut app = App::new(paths);
    app.load_hosts();
    if !no_restore {
        app.restore_enabled();
    } else {
        app.set_status(StatusMsg::info("已跳过自动恢复（--no-restore）"));
    }

    let mut terminal: DefaultTerminal = ratatui::init();
    // Mouse capture is on for the whole session: the import table's filtering and
    // the table's row selection are click-driven, and terminals cannot report a
    // click without it. It is best-effort — a terminal that refuses capture (some
    // multiplexers, some CI ptys) must still give a working keyboard-only UI, so
    // a failure is ignored rather than fatal.
    let mouse = execute!(std::io::stdout(), EnableMouseCapture).is_ok();
    let result = event_loop(&mut terminal, &mut app, &quit);
    // Stop children and remove scratch files before restoring the terminal, so
    // a stray panic cannot leave the user's terminal in raw mode.
    app.shutdown();
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
        app.drain_events();
        terminal.draw(|frame| ui::draw(frame, app))?;
        if app.should_quit {
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

# SSH 端口转发 TUI · 技术选型实测报告

> 环境 OpenSSH_8.2p1 / rustc 1.99.1 / ratatui 0.30.2 + crossterm 0.29.0 / rsproxy。实测用临时 sshd（2222/2223/2224，当前用户、临时 host key、仅公钥）端到端验证，测完已清理。

## 0. 结论

| 项 | 结论 |
|---|---|
| SSH 实现 | **系统 `ssh` 子进程**：唯一零成本复用 `~/.ssh/config`（ProxyJump/IdentityFile/agent/known_hosts/通配）的方案 |
| 否决 | `ssh2`（libssh2 无 ProxyJump，config 需自写）、`russh`（同，依赖 203 包 vs 0） |
| 就绪判定 | **不能靠进程存活**：`-v` stderr 中 `-L/-D` 认 `Entering interactive session.`，`-R` 认 `remote forward success for:`；**失败行必须优先** |
| 必备选项 | `-N -v -o ExitOnForwardFailure=yes -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=3`；`-L` 显式写 bind 地址 |
| TUI / 并发 | ratatui 0.30.2 + crossterm 0.29 兼容（已 build + pty 实跑，`init()/restore()`、`Frame::area()`、`row_highlight_style()`）；**单线程 UI 轮询 + 每转发一 stderr 读线程 + mpsc**，不要 tokio |
| 清理 | `process_group(0)` + `kill(-pgid)`（收 ProxyJump 子进程）+ `wait()`；信号只置 flag，走 Drop |
| ControlMaster | **不能当启停单元**（`-O forward` 绑失败仍 exit 0），仅可作免重复认证层 |

## 1. 三方案对比

| 维度 | a) 系统 ssh | b) ssh2 0.9.7 | c) russh 0.65.1 |
|---|---|---|---|
| 复用 ~/.ssh/config | **原生全支持** | 无（`ls src/ \| grep -i config` 空） | 无（`grep ssh_config src/` 零命中） |
| ProxyJump | **原生**（自动 fork `ssh -W`） | 协议层无此概念 | 无，需自写 |
| agent/known_hosts/失败原因 | **原生** / **stderr 原文最准** | 有 API 需自接 / `Error` 枚举 | `keys::agent` / `russh::Error` 枚举 |
| 密码/键盘交互 | 需自建 pty | `userauth_password` 直调 | `authenticate_password` 直调 |
| -L/-R/-D | **参数直传** | 手写 accept + `channel_direct_tcpip` | 手写 accept + `channel_open_direct_tcpip` |
| 跨平台 | Windows 需装 OpenSSH | 可，MSVC 需预编译 | **最好**（纯 Rust，无外部依赖） |
| 依赖包数（cargo tree 实测） / 编译依赖 | 0 / 无 | 20 / libssh2-sys 静态 + **系统 openssl** | 203 / 纯 Rust |

**推荐 a**：① `ssh -G cloud_pc` 直接给出 `proxyjump`+2 条 `localforward`+`identityfile`，自实现"首值优先/通配/Include"是长期负担；② ProxyJump 两层穿透实测成功；③ 失败原因即 stderr 原文，另两方案只有枚举；④ 零 SSH 依赖。
**否决 b（ssh2）的风险**：config 语义要自己实现（`ssh2-config` 也无 ProxyJump，libssh2 协议层就没有）；编译走 `cargo:rustc-link-lib=ssl/crypto`+`/usr/include`，**无 `libssl-dev` 直接失败**；每种转发都要手写 accept/半关闭/SOCKS5。能力本身没问题（实测 `channel_direct_tcpip` 读到 `SSH-2.0-OpenSSH_8.2p1`），但复杂度不该由 TUI 承担。
**否决 c（russh）的风险**：同样不读 config；必拉 tokio（203 包）。能力齐全（`channel_open_direct_tcpip`/`tcpip_forward`/`authenticate_password`/`keys::agent`），**是 Windows / 无 ssh 二进制 / 细粒度统计的最佳替换路径**，v1 预留 `trait SshBackend` 即可。

## 2. 实测验证

### 2.1 `ssh -G` 能解析 config，但不能枚举 Host

```text
$ ssh -G cloud_pc | grep -E '^(hostname|user|port|proxyjump|identityfile|localforward) '
user alice / hostname 203.0.113.10 / port 22
localforward 18789 [127.0.0.1]:18789 / localforward 44897 [127.0.0.1]:44897
identityfile ~/.ssh/id_rsa / proxyjump cloud_pc_jump
```
Include / 通配 / 含空格路径均正确（`identityfile ~/my keys/key with space`）；不联网（20 次 `ssh -G` 共 77ms，未知域名 5ms）。加 `-T` 抑制 pty 警告；`ssh -G '*'` 只是按通配求值，**无法枚举 Host**。**枚举替代**：awk 解析 `Host` 行（递归 `Include`、跳过含 `*?` 的 Pattern）→ 真实 config 得 **12 个，与人工计数一致**。再用 `-v` 校验是否真命中块：`ssh -G -T -v "$h" 2>&1 >/dev/null | grep -cF "Applying options for $h"` → `cloud_pc=1, jeprof=1, totally-unknown-host-xyz=0`。判"未知主机"必须靠此计数，**不能比对 hostname**（未知主机的 hostname 原样等于输入）。

### 2.2 真实失败 stderr 原文（7 类，均实测）

```text
① 本地端口被占用（双栈都占才致命，exit=255）
   bind [::1]:18081: Address already in use
   bind [127.0.0.1]:18081: Address already in use
   channel_setup_fwd_listener_tcpip: cannot listen to port: 18081
   Could not request local forwarding.
   ★ 只占 IPv4 时：bind [127.0.0.1]:18105: Address already in use，但 ssh 仍 ALIVE
     （IPv6 半边 ::1:18105 仍在服务）→ 必须 -L 127.0.0.1:... 显式单栈，或按实际监听地址判定
② 低端口权限不足（exit=255）
   bind [127.0.0.1]:80: Permission denied
   channel_setup_fwd_listener_tcpip: cannot listen to port: 80
   Could not request local forwarding.
③ 认证失败（exit=255）：alice@127.0.0.1: Permission denied (publickey).
   （BatchMode=yes 对纯密码服务器也立即失败，不挂起）
④ 拒绝/解析失败（exit=255）
   ssh: connect to host 127.0.0.1 port 1: Connection refused
   ssh: Could not resolve hostname no-such-host.invalid: Temporary failure in name resolution
⑤ 主机密钥（exit=255）
   No ED25519 host key is known for [127.0.0.1]:2222 and you have requested strict checking.
   Host key verification failed.
⑥ -R 失败（exit=255）：remote forward failure for: listen 22, connect 127.0.0.1:22
   Error: remote port forwarding failed for listen port 22
⑦ 目标端口不通（惰性失败，最易误判）：隧道"建成"并进入会话后才暴露
   debug1: Entering interactive session.                      ← 看着成功
   channel 2: open failed: connect failed: Connection refused  ← 真连一次才有
   客户端 socket 立即 recv()==b''（EOF），不是 connect 失败
```

### 2.3 就绪判定标记

| 类型 | 成功标记 | 失败标记（须先判） |
|---|---|---|
| -L / -D | `Entering interactive session.`（前有 `Local forwarding listening on <a> port <p>.` + `channel N: new [port listener]`） | `bind ...: Address already in use` / `Could not request local forwarding.` / `cannot listen to port:` |
| -R | `remote forward success for: listen <p>, connect ...` + `All remote forwarding requests processed` | `remote forward failure for:` / `Error: remote port forwarding failed` |

```text
$ ssh -N -v -L 127.0.0.1:18500:127.0.0.1:22 ...
38: Authenticated to 127.0.0.1 ([127.0.0.1]:2222).
39: debug1: Local connections to 127.0.0.1:18500 forwarded to remote address 127.0.0.1:22
40: debug1: Local forwarding listening on 127.0.0.1 port 18500.
41: debug1: channel 0: new [port listener]
43: debug1: Entering interactive session.       ← 就绪（-R 则等 45: remote forward success for）
```
- `-o LogLevel=VERBOSE` **不够**（只有 `Authenticated to`），必须 `-v` 或 `-o LogLevel=DEBUG1`。三个必备细节：**(1) 失败行优先**（bind 失败行在 `Entering` 之前但进程可能仍活）；**(2) stderr EOF 后必须 `wait()` 取退出码**（认证失败无 marker，探针第一版漏 wait 误报 "stderr closed"）；**(3) `-L` 成功 ≠ 目标可达**（见⑦）。
- `BatchMode=yes`+stdin=null 下密码服务器立即失败；`SSH_ASKPASS_REQUIRE` **8.2 不支持**（askpass 被调用但无效）。pty 实测可读 `alice@127.0.0.1's password:` 并写入，但 v1 建议默认 BatchMode、失败时提示手动登录。

### 2.4 进程树与清理（含 ProxyJump）

```text
$ ps -o pid,ppid,pgid,cmd -p <parent> --ppid <parent>
975671 975653 975653  ssh ... -N -L 18099:127.0.0.1:22 target-test
975673 975671 975653  ssh ... -W [127.0.0.1]:2224 jump-test      ← 同 PGID
$ process_group(0) + kill(-pgid, SIGTERM) → reaped: Some(Some(0)) / no leftover ssh / port released
```
- SIGTERM/SIGKILL 端口都能释放，但只 `child.kill()` 必漏 ProxyJump 子进程；父进程被 SIGKILL 时子进程约 1s 后才消失 → **必须自己清理**，别指望孤儿自灭。**`Drop` 在 panic 展开时执行、`process::exit()` 时不执行**（三模式实测）→ 退出用 flag+break。`ratatui::init()` 自带 panic hook 会 `restore()`（源码确认），须在装完自己的 hook **之后**调用。服务端异常：杀会话子进程 → `Connection to 127.0.0.1 closed by remote host.` exit 255；静默挂起（SIGSTOP）→ keepalive 5s 内 `Timeout, server ... not responding.` exit 255。**`-f` 不要用**：端口被占时 `exit=0`（stderr 有 bind 失败、可能残留 daemon），低端口/认证失败才 255；`$!` 也不是真 ssh pid。
**2.5 ClearAllForwardings 陷阱（必读）**：config 已有 `LocalForward` 时我们的 `-L` 是**追加**（实测 3 条同 bind）；该条被占只报 bind 错、进程仍活。而 `-o ClearAllForwardings=yes` 会**把我们自己的 `-L` 也清空**（实测 `Local connections to` 计数 0、无监听，顺序无关）。
→ **正解：生成一次性 `-F` 合成配置**（`Include ~/.ssh/config` 放**最前**，再写自己的 `Host` 块）。实测穿透 ProxyJump 成功：`ssh -F synth.conf -N -L 18422:127.0.0.1:22 target-test` → 读到 `SSH-2.0-OpenSSH_8.2p`。注意 `Include` 若写在 `Host` 块之后会被当作该主机上下文解析（实测 `ssh -G jump-test` 变成 `hostname jump-test`）。

## 3. TUI 栈与事件循环

```text
$ cargo new rattest && cargo build   # ratatui="0.30.2", crossterm="0.29"
   Compiling ratatui-crossterm v0.1.2 / ratatui v0.30.2 → Finished in 5.54s
Cargo.lock 只有 1 个 crossterm 0.29.0（ratatui-crossterm 默认 feature 即 crossterm_0_29）
```
MSRV：ratatui 0.30.2 需 **rustc ≥ 1.88**（edition 2024）。**0.30 API 变化**：`ratatui::init() -> DefaultTerminal`（含 raw mode + alt screen + panic hook）/ `ratatui::restore()` / `ratatui::run(closure)`；`Frame::area()`（`size()` 已 deprecated）；`Table::row_highlight_style()`（`highlight_style()` 已 deprecated，源码标注）；`TableState` 用法不变。**架构**：单线程 UI（`event::poll(200ms)`）+ 每转发一个 stderr 读线程 + `mpsc` + 每帧 `try_recv`/`try_wait`；**不需要 tokio**——ssh 已在做 I/O。**下框 66 行，`cargo build` 通过；pty 实跑两路径均 `exit=0`（正常 config → `up`；不存在主机 → `failed`；q 后无残留 ssh）。**

```rust
use std::{io::{BufRead,BufReader}, os::unix::process::CommandExt, process::{Child,Command,Stdio},
  sync::{atomic::{AtomicBool,Ordering}, mpsc, Arc}, thread, time::{Duration,Instant}};
use ratatui::{crossterm::event::{self,Event,KeyCode,KeyEventKind}, layout::Constraint,
  style::{Color,Style}, widgets::{Block,Row,Table,TableState}, DefaultTerminal};

struct W { child: Option<Child>, st: &'static str, up: Option<Instant> }
impl Drop for W { fn drop(&mut self) {                       // panic / 正常退出都会走
  if let Some(mut c) = self.child.take() {
    let p = c.id() as i32;
    unsafe { libc::kill(-p, libc::SIGTERM) };                 // 整组（含 ProxyJump 子进程）
    for _ in 0..20 { if matches!(c.try_wait(), Ok(Some(_))) { return } thread::sleep(Duration::from_millis(50)) }
    unsafe { libc::kill(-p, libc::SIGKILL) }; let _ = c.wait(); // wait 防僵尸
  }
}}
fn main() -> std::io::Result<()> {
  let quit = Arc::new(AtomicBool::new(false));
  for s in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
    signal_hook::flag::register(s, quit.clone()).ok();       // 只置 flag
  }
  let mut t: DefaultTerminal = ratatui::init();              // 含 raw+alt screen+panic hook
  let r = run(&mut t, quit);
  ratatui::restore();                                        // 0.30 显式恢复
  r
}
fn run(t: &mut DefaultTerminal, quit: Arc<AtomicBool>) -> std::io::Result<()> {
  let (tx, rx) = mpsc::channel::<String>();
  let mut c = Command::new("ssh");
  c.args(["-N","-v","-o","ExitOnForwardFailure=yes","-o","BatchMode=yes",
          "-L","127.0.0.1:18400:127.0.0.1:22","tvhost"])
   .stdin(Stdio::null()).stderr(Stdio::piped()).process_group(0);  // 新进程组
  let mut ch = c.spawn()?;
  let err = ch.stderr.take().unwrap();
  thread::spawn(move || for l in BufReader::new(err).lines().map_while(Result::ok) {  // UI 永不阻塞
    if tx.send(l).is_err() { break } });
  let mut w = W { child: Some(ch), st: "starting", up: None };
  let mut tb = TableState::default().with_selected(0);
  while !quit.load(Ordering::Relaxed) {
    while let Ok(l) = rx.try_recv() {                         // 非阻塞采样
      if l.starts_with("bind ") || l.starts_with("ssh: ") || l.contains("Permission denied") {
        w.st = "failed"; w.up = None;                         // 失败行优先
      } else if l.contains("Entering interactive session.") || l.contains("remote forward success for") {
        if w.st != "up" { w.st = "up"; w.up = Some(Instant::now()) }
      }
    }
    if let Some(c) = &mut w.child {                           // try_wait 兜底（认证失败无 marker）
      if let Ok(Some(s)) = c.try_wait() { w.st = if s.success() { "up" } else { "failed" }; w.child = None }
    }
    t.draw(|f| {
      let col = match w.st { "up" => Color::Green, "failed" => Color::Red, _ => Color::Yellow };
      let up = w.up.map(|x| format!("{}s", x.elapsed().as_secs())).unwrap_or_default();
      let row = Row::new(vec!["tvhost 18400→22".into(), w.st.into(), up]).style(Style::default().fg(col));
      let [top, _] = ratatui::layout::Layout::vertical([Constraint::Min(5), Constraint::Length(6)]).areas(f.area());
      let tbl = Table::new([row], [Constraint::Min(30), Constraint::Min(20), Constraint::Length(8)])
        .header(Row::new(["转发","状态 / 失败原因","时长"]))
        .row_highlight_style(Style::default().bg(Color::DarkGray))  // 0.30: highlight_style 已弃用
        .highlight_symbol("▶ ").block(Block::bordered().title(" SSH 端口转发 "));
      f.render_stateful_widget(tbl, top, &mut tb);
    })?;
    if event::poll(Duration::from_millis(200))? {              // crossterm 与后台线程并存
      if let Event::Key(k) = event::read()? { if k.kind == KeyEventKind::Press { match k.code {
        KeyCode::Char('q') | KeyCode::Esc => break,
        KeyCode::Down => tb.select_next(), KeyCode::Up => tb.select_previous(), _ => {} } } }
    }
  }
  Ok(())                                                      // w Drop -> 清理
}
```

## 4. 依赖清单

`/tmp/finaldep` 完整 `cargo build` 通过（97 包含传递依赖）：`ratatui = "0.30.2"`（Table/Paragraph/布局）、`crossterm = "0.29"`（事件+后端，与 ratatui-crossterm 默认 feature 对齐）、`serde = { version="1", features=["derive"] }` + `serde_json = "1"`（配置持久化）、`anyhow = "1"`（错误聚合，可换 thiserror）、`directories = "6"`（`~/.config` 定位）、`signal-hook = "0.4"`（进程清理，**0.4 用 `Arc<AtomicBool>`，0.3 是 `Arc<Mutex<bool>>`**）、`libc = "0.2"`（`kill(-pgid)`）。
**可砍** `directories`（手写 XDG 回退）/`signal-hook`（手写 libc::signal，不建议）/`serde_json`（换 toml）；**不建议**精简 ratatui feature（包数 88→79，失去 macros/layout-cache）；**明确不引入** ssh2/russh/tokio/openssl/libssh2-sys。实测 release 约 **0.46 MB**，clean debug build **4.57s**。

## 5. 进程管理要点

1. **杀树与防僵尸**：`process_group(0)` + `kill(-pgid,SIGTERM)` → 1s 内 `try_wait` → `kill(-pgid,SIGKILL)` → `wait()`；只 `child.kill()` 必漏 ProxyJump 子进程，每个 Child 必须走到 `wait()` 返回 `Some`（主循环每帧 `try_wait`，Drop 兜底）。
2. **panic/退出**：`Drop` 在 panic 展开时执行、`process::exit()` 不执行 → 统一 flag+break；先装自己 hook 再 `ratatui::init()`。信号用 `signal_hook::flag::register`（0.4 需 `Arc<AtomicBool>`），处理器只置 flag。
3. **ControlMaster**：实测第二次连接 0.00s、`-O forward/cancel/exit` 均可用、master 退出全关；但 **`-O forward` 绑端口被占仍 `exit 0`**（只有低端口/permission 才 255），且 stale socket（`kill -9` 后 `Control socket connect(...): Connection refused`，`-O` 子命令直接失败、普通连接会自动 fallback 直连成功）。**结论：v1 一对一转发=一进程；ControlMaster 只作认证复用层**（`ControlPath` 放 runtime dir、`ControlPersist=60`），转发仍由各自 client 进程持有。
4. **流量列**：`ssh -v` 只在退出时打一次 `Transferred: sent X, received Y bytes`；实时用 `/proc/<pid>/io` 的 `rchar/wchar` 增量（实测推 1MB：2370→1053500），ProxyJump 需递归子进程求和；`ss -tinp` 的 `bytes_sent/received` 同样准（实测 400KB→403246）但仅 TCP 级。

## 6. 落地清单

1. `ssh -G -T <host>` 求值 + awk 枚举 `Host`（支持 `Include`）+ `-v` 的 `Applying options for` 判是否真命中；生成一次性 `-F` 合成配置（`Include ~/.ssh/config` 最前 + 自己的 `Host` 块），**不用 `-o ClearAllForwardings`**；每条 `-L 127.0.0.1:<lp>:<rh>:<rp>` 显式 bind，并加上 §0 的必备选项。
2. stderr 逐行解析：失败行优先 → 命中 `Entering interactive session.`/`remote forward success for` 才置 UP；EOF 后 `wait()` 取 code。
3. 进程组 `process_group(0)` + `kill(-pgid)` + `wait()` + `Drop`；信号只置 flag；退出不用 `process::exit`；单线程 UI（poll 200ms）+ 每转发一 stderr 读线程 + mpsc + try_wait。
4. 持久化 `directories::ProjectDirs` → `~/.config/ssh-port-forwarder/forwards.json`；预留 `trait SshBackend` 便于将来切 `russh`（Windows / 无 ssh 二进制 / 细粒度统计）。

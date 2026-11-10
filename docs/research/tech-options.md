# v1 技术选型实测：系统 `ssh` 子进程 + TUI 栈

> **这份文档回答**：v1 为什么选系统 `ssh` 子进程、`ssh -v` 的失败文本长什么样、就绪怎么判、进程树怎么清、TUI 栈怎么选（**§3.3 是 `src/app.rs` 事件循环模型的出处**）。
> **不负责**：当前实现——v2 已改用 `russh`，见 [../design.md](../design.md) D1/D3/D5 与 [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md)。
> **相关代码路径**：`src/app.rs`（§3.3 的事件循环）、`src/tunnel.rs`。

> 历史实测（2026-11，OpenSSH_8.2p1 / rustc 1.99.1 / ratatui 0.30.2 + crossterm 0.29 / rsproxy）。
> 当时的临时 sshd 已清理。**这些方案大多已被 `russh` 路线取代**——本文的价值是"为什么那样判断"
> 与 stderr 原文样本，不是当前行为。被推翻的清单见 §7。

## 0. 当时的结论

| 项 | 结论 |
|---|---|
| SSH 实现 | **系统 `ssh` 子进程**：唯一零成本复用 `~/.ssh/config`（ProxyJump / IdentityFile / agent / known_hosts / 通配）的方案 |
| 否决 | `ssh2`（libssh2 无 ProxyJump，config 需自写）、`russh`（同，且依赖 203 包 vs 0）——**后来被推翻**，见 §7 |
| 就绪判定 | **不能靠进程存活**：`-L/-D` 认 `Entering interactive session.`，`-R` 认 `remote forward success for:`；**失败行必须优先** |
| 必备选项 | `-N -v -o ExitOnForwardFailure=yes -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=3`；`-L` 显式写 bind 地址 |
| TUI / 并发 | ratatui + crossterm **单线程 UI 轮询 + 每转发一 stderr 读线程 + mpsc**，不要 tokio |
| 清理 | `process_group(0)` + `kill(-pgid)`（收 ProxyJump 子进程）+ `wait()`；信号只置 flag，走 `Drop` |
| ControlMaster | **不能当启停单元**（`-O forward` 绑失败仍 `exit 0`），仅可作免重复认证层 |

## 1. 三方案对比（当时的判断）

| 维度 | a) 系统 ssh | b) ssh2 0.9.7 | c) russh 0.65.1 |
|---|---|---|---|
| 复用 `~/.ssh/config` | **原生全支持** | 无 | 无（需 `ssh2-config`） |
| ProxyJump | **原生** | 协议层无此概念 | 需自写（实测可行） |
| 失败原因 | **stderr 原文** | `Error` 枚举 | 结构化枚举（更准） |
| 跨平台 | Windows 需装 OpenSSH | MSVC 需预编译 | **最好**（纯 Rust） |
| 依赖包数 | 0 | 20 + 系统 openssl | 203（含 tokio `full`） |

选 a 的理由：`ssh -G cloud_pc` 直接给出 `proxyjump` + `localforward` + `identityfile`，自实现
"首值优先/通配/Include"是长期负担；ProxyJump 两层穿透实测成功；失败原因即 stderr 原文。

否决 b 的风险：config 语义要自己实现；编译依赖系统 `libssl-dev`，无则直接失败；每种转发都要手写
accept/半关闭/SOCKS5。

否决 c 的风险（**后被推翻**）：当时记为"同样不读 config；必拉 tokio（203 包）"。真正的原因是本文
**没有独立核实 russh 的转发能力**——后续实测证明三种转发协议层零缺口，见
[pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) 与 [pure-rust-ssh-russh.md](pure-rust-ssh-russh.md)。

## 2. `ssh -G` 与真实失败文本

### 2.1 `ssh -G` 能解析 config，但不能枚举 Host

```text
$ ssh -G cloud_pc | grep -E '^(hostname|user|port|proxyjump|identityfile|localforward) '
user alice / hostname 203.0.113.10 / port 22
localforward 18789 [127.0.0.1]:18789 / localforward 44897 [127.0.0.1]:44897
identityfile ~/.ssh/id_rsa / proxyjump cloud_pc_jump
```

- `Include` / 通配 / 含空格路径均正确；不联网（20 次共 77 ms，未知域名 5 ms）。加 `-T` 抑制 pty 警告。
- **`ssh -G '*'` 只是按通配求值，无法枚举 Host**。枚举替代：awk 解析 `Host` 行（递归 `Include`、
  跳过含 `*?` 的 Pattern）→ 真实 config 得 **12 个，与人工计数一致**。
- 判"未知主机"必须靠 `ssh -G -T -v "$h" 2>&1 >/dev/null | grep -cF "Applying options for $h"`，
  **不能比对 hostname**（未知主机的 hostname 原样等于输入）。

### 2.2 七类失败 stderr 原文（均实测）

```text
① 本地端口被占用（双栈都占才致命，exit=255）
   bind [::1]:18081: Address already in use
   bind [127.0.0.1]:18081: Address already in use
   channel_setup_fwd_listener_tcpip: cannot listen to port: 18081
   Could not request local forwarding.
   ★ 只占 IPv4 时：bind 失败但 ssh 仍 ALIVE（IPv6 半边仍在服务）
     → 必须 -L 127.0.0.1:... 显式单栈，或按实际监听地址判定
② 低端口权限不足（exit=255）：bind [127.0.0.1]:80: Permission denied  + cannot listen to port: 80
③ 认证失败（exit=255）：alice@127.0.0.1: Permission denied (publickey).
   （BatchMode=yes 对纯密码服务器也立即失败，不挂起）
④ 拒绝/解析失败（exit=255）
   ssh: connect to host 127.0.0.1 port 1: Connection refused
   ssh: Could not resolve hostname no-such-host.invalid: Temporary failure in name resolution
⑤ 主机密钥（exit=255）
   No ED25519 host key is known for [127.0.0.1]:2222 and you have requested strict checking.
   Host key verification failed.
⑥ -R 失败（exit=255）：remote forward failure for: listen 22, connect 127.0.0.1:22
⑦ 目标端口不通（惰性失败，最易误判）：隧道"建成"并进入会话后才暴露
   debug1: Entering interactive session.                      ← 看着成功
   channel 2: open failed: connect failed: Connection refused  ← 真连一次才有
   客户端 socket 立即 recv()==b''（EOF），不是 connect 失败
```

### 2.3 就绪标记

| 类型 | 成功标记 | 失败标记（须先判） |
|---|---|---|
| `-L` / `-D` | `Entering interactive session.`（前有 `Local forwarding listening on <a> port <p>.`） | `bind ...: Address already in use` / `Could not request local forwarding.` |
| `-R` | `remote forward success for: listen <p>, connect ...` | `remote forward failure for:` / `Error: remote port forwarding failed` |

三个必备细节：**(1) 失败行优先**（bind 失败行在 `Entering` 之前，但进程可能仍活）；
**(2) stderr EOF 后必须 `wait()` 取退出码**（认证失败无 marker）；**(3) `-L` 成功 ≠ 目标可达**（见⑦）。

`-o LogLevel=VERBOSE` **不够**（只有 `Authenticated to`），必须 `-v` 或 `-o LogLevel=DEBUG1`。
`SSH_ASKPASS_REQUIRE` **8.2 不支持**。

## 3. TUI 栈与事件循环

ratatui 0.30.2 需 **rustc ≥ 1.88**（edition 2024）。

### 3.1 0.30 API 变化

`ratatui::init() -> DefaultTerminal`（含 raw mode + alt screen + panic hook）/ `ratatui::restore()` /
`ratatui::run(closure)`；`Frame::area()`（`size()` 已 deprecated）；
`Table::row_highlight_style()`（`highlight_style()` 已 deprecated，源码标注）；`TableState` 用法不变。

### 3.2 依赖清单

当时 97 包含传递依赖：`ratatui`、`crossterm`、`serde` + `serde_json`、`anyhow`、`directories`、
`signal-hook = "0.4"`（**0.4 用 `Arc<AtomicBool>`，0.3 是 `Arc<Mutex<bool>>`**）、`libc`。
实测 release 约 0.46 MB，clean debug build 4.57s。**明确不引入** ssh2 / russh / tokio / openssl / libssh2-sys。

### 3.3 单线程 UI 轮询 + 每转发一线程 + mpsc

**这是 `src/app.rs` 里「deliberately no `tokio` here」的出处。**

- 单线程 UI（`event::poll(200ms)`）+ 每转发一个 stderr 读线程 + `mpsc` + 每帧 `try_recv`/`try_wait`。
- **不需要 tokio**——I/O 已经由 ssh 子进程在做，UI 线程只做非阻塞采样。
- 信号用 `signal_hook::flag::register`，处理器**只置 flag**；主循环 `while !quit.load()`；
  退出走 `Drop` 回收子进程，**不用 `process::exit`**（它会跳过 `Drop`，终端留在 raw mode）。
- **失败行优先**：先判 `bind` / `ssh:` / `Permission denied` 前缀 → `failed`；命中
  `Entering interactive session.` / `remote forward success for` → `up`；`try_wait` 兜底
  （认证失败没有 marker）。
- 状态色与表格布局见 [prior-art.md](prior-art.md) §7。

**pty 实跑两路径均 `exit=0`**（正常 config → `up`；不存在主机 → `failed`；`q` 后无残留 ssh）。

## 4. 进程树与清理（含 ProxyJump）

```text
$ ps -o pid,ppid,pgid,cmd -p <parent> --ppid <parent>
975671 975653 975653  ssh ... -N -L 18099:127.0.0.1:22 target-test
975673 975671 975653  ssh ... -W [127.0.0.1]:2224 jump-test      ← 同 PGID
$ process_group(0) + kill(-pgid, SIGTERM) → reaped: Some(Some(0)) / no leftover / port released
```

- 只 `child.kill()` 必漏 ProxyJump 子进程；父进程被 `SIGKILL` 时子进程约 1s 后才消失 →
  **必须自己清理**，别指望孤儿自灭。
- **`Drop` 在 panic 展开时执行、`process::exit()` 时不执行**（三模式实测）→ 退出用 flag+break。
- `ratatui::init()` 自带 panic hook 会 `restore()`，须在装完自己的 hook **之后**调用。
- 服务端异常：杀会话子进程 → `Connection to 127.0.0.1 closed by remote host.` exit 255；
  静默挂起（SIGSTOP）→ keepalive 5s 内 `Timeout, server ... not responding.` exit 255。
- **`-f` 不要用**：端口被占时 `exit=0`（stderr 有 bind 失败、可能残留 daemon），`$!` 也不是真 pid。

**`ClearAllForwardings` 陷阱**：config 已有 `LocalForward` 时我们的 `-L` 是**追加**（实测 3 条同 bind）；
而 `-o ClearAllForwardings=yes` 会**把我们自己的 `-L` 也清空**（实测 `Local connections to` 计数 0）。
→ **正解：生成一次性 `-F` 合成配置**（`Include ~/.ssh/config` 放**最前**，再写自己的 `Host` 块）。
`Include` 若写在 `Host` 块之后会被当作该主机上下文解析（实测 `ssh -G jump-test` 变成 `hostname jump-test`）。

## 5. 进程管理要点

1. **杀树与防僵尸**：`process_group(0)` + `kill(-pgid, SIGTERM)` → 1s 内 `try_wait` →
   `kill(-pgid, SIGKILL)` → `wait()`。每个 `Child` 必须走到 `wait()` 返回 `Some`。
2. **ControlMaster**：实测第二次连接 0.00s、`-O forward/cancel/exit` 均可用；但 **`-O forward`
   绑端口被占仍 `exit 0`**，stale socket 下 `-O` 子命令直接失败。**结论：一转发一进程；
   ControlMaster 只作认证复用层**。
3. **流量列**：`ssh -v` 只在退出时打一次 `Transferred: sent X, received Y bytes`；实时用
   `/proc/<pid>/io` 的 `rchar/wchar` 增量（实测推 1MB：2370→1053500），ProxyJump 需递归求和。

## 6. 落地清单（历史）

1. `ssh -G -T <host>` 求值 + awk 枚举 `Host` + `-v` 的 `Applying options for` 判是否真命中；
   生成一次性 `-F` 合成配置，**不用 `-o ClearAllForwardings`**；每条 `-L 127.0.0.1:<lp>:<rh>:<rp>`
   显式 bind，加上 §0 的必备选项。
2. stderr 逐行解析：失败行优先 → 命中 marker 才置 UP；EOF 后 `wait()` 取 code。
3. 进程组 + `kill(-pgid)` + `wait()` + `Drop`；信号只置 flag；不用 `process::exit`；
   单线程 UI（poll 200ms）+ 每转发一 stderr 读线程 + mpsc + try_wait。
4. 持久化 `directories::ProjectDirs` → `~/.config/ssh-port-forwarder/forwards.json`。

## 7. 哪些结论后来被推翻

| 当时结论 | 现在 | 推翻依据 |
|---|---|---|
| 系统 `ssh` 子进程是最佳选择 | `russh` 纯 Rust | [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md)：三种转发协议层零缺口 |
| 就绪靠解析 `ssh -v` 文本 | 二值的真实结果（`bind` 成功 / `tcpip_forward` 回复） | [../design.md](../design.md) D3 |
| 进程组 + `PR_SET_PDEATHSIG` + Job Object | tokio task；`sys` 层只剩权限与信号 | [../design.md](../design.md) D5、D28 |
| 不要 tokio | tokio 跑在每隧道一个后台线程里，UI 线程零 tokio（§3.3 的模型仍在） | [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) §13 |
| `serde_json` 持久化、合成 `-F` 配置 | `toml` 持久化；合成配置整个问题消失（无子进程） | [../design.md](../design.md) D7 |

**仍然成立的部分**：§3.3 的「单线程 UI 轮询 + mpsc」事件模型（`TunnelEvent` 语义原样保留）；
§0 的"失败原因要分类"需求本身；§2.1 的"`ssh -G` 不能枚举 Host"（v2 用 `ssh2-config` 枚举，判据相同）。
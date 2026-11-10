# sshpf

[![CI](https://github.com/can2049/sshpf/actions/workflows/ci.yml/badge.svg)](https://github.com/can2049/sshpf/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/sshpf.svg)](https://crates.io/crates/sshpf)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

SSH 端口转发管理器，命令行 TUI 表格界面，Linux 与 Windows 均支持。在不离开终端的情况下查看、新增、启停、删除端口转发，每条转发实时显示状态与失败原因；配置持久化，重启后自动恢复上次启用的条目。

> 本文是**用户视角**：装 / 用 / 风险 / 配置 / 排查。内部结构、设计理由、给 AI agent 的导航在 **[docs/index.md](docs/index.md)**。

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
│  sshpf 0.5.0  ·  ssh-config: 12 hosts  ·  cfg: ~/.config/sshpf/config.toml          13:47:03    │
│  ● 2 up   ◐ 0 starting   ◌ 0 retrying   ✗ 1 failed   ○ 1 off                                     │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│   名称          T 本地             远端             SSH 主机    状态        最后错误             │
│ ▶ web-ssh       L 127.0.0.1:19711  127.0.0.1:22    web-server  ● 已连通    —                    │
│   staging-db    L 127.0.0.1:15432  127.0.0.1:5432  staging-db  ✗ 失败      主机密钥校验失败      │
│   old-tunnel    L 127.0.0.1:18080  127.0.0.1:8080  dev-box     ○ 未启用    —                    │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│  就绪 · n 新建转发 · ? 帮助 · q 退出                                                              │
│  j/k ↑↓ 移动  space 启停  r 重启  n 新建  e 编辑  d 删除  enter 详情  l 日志                      │
│  / 过滤  a 全启  x 全停  g/G 首尾  v 多选  ? 帮助  q 退出                                          │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

`v` 进入多选模式：底部多出一条动作条，标记若干条后一次启停/重启/删除。

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
│  名称           T 本地              远端              SSH 主机       状态        最后错误         │
│  ✓ web-ssh      L 127.0.0.1:19711   127.0.0.1:22      web-server     ● 已连通    —              │
│  ✓ staging-db   L 127.0.0.1:15432   127.0.0.1:5432    staging-db     ✗ 失败      主机密钥校验失败 │
│ ▶✓ old-tunnel   L 127.0.0.1:18080   127.0.0.1:8080    dev-box        ○ 未启用    —              │
│  · socks        D 127.0.0.1:1080    (SOCKS5)          dev-box        ◐ 连接中    —              │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│ 已选 2 条  ·  s 启用  ·  x 停用  ·  R 重启  ·  D 删除  ·  a 全选  ·  v/esc 退出                    │
│ j/k ↑↓ 移动  space 勾选  J/K 扩选  g/G 首尾  a 全选  s/x/R/D 批量操作  esc 退出  ? 帮助           │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

## 安装 / Install

```bash
cargo install sshpf          # 需要 Rust 1.89+
```

预编译二进制从 [GitHub Releases](https://github.com/can2049/sshpf/releases) 下载（Linux x86_64 静态链接 / Windows x86_64），或 `cargo install --git https://github.com/can2049/sshpf`。从源码：

```bash
git clone https://github.com/can2049/sshpf.git && cd sshpf
cargo build --release && install -m755 target/release/sshpf ~/.local/bin/sshpf
```

**运行时零外部依赖**：不需要系统 `ssh`。认证走 `~/.ssh/config` 的 `IdentityFile` 或 ssh-agent。构建期需要 C 工具链（`ring` 加密后端，Windows 上即预装的 MSVC），产物自包含。

| 平台 | 状态 | 说明 |
|---|---|---|
| Linux | ✅ 完整支持 | 端到端测试跑真实 sshd |
| Windows | ✅ 支持 | 同一套纯 Rust 代码路径，无需安装 OpenSSH |
| macOS | ⚠️ 未测试 | 代码走 Unix 分支，预期可用 |

## 使用 / Usage

```bash
sshpf                       # 启动 TUI，自动恢复上次启用的转发
sshpf --no-restore          # 启动但不自动恢复
sshpf -c /path/config.toml  # 使用指定配置
sshpf --list-hosts          # 打印 ~/.ssh/config 里的主机后退出
sshpf --check               # 校验配置文件后退出
sshpf --ignore-host-key-changes   # 接受已改变的主机密钥（全局降级开关，见排障）
```

`--ignore-host-key-changes` 只放宽"密钥变了"这一种情况：不写入 `known_hosts`、每次接受都记一条 WARN，`@revoked` 吊销的密钥**仍然拒绝**。某台主机单独需要时，优先在 `~/.ssh/config` 的 `Host` 块里写 `StrictHostKeyChecking no`（作用域仅限该主机）。

### 键位 / Keys

界面里**任何时刻**按 `?` 都能看到当前上下文的按键说明，中英双语，且与文档一致。**完整键位参考（15 个上下文，含鼠标与状态图例）见 [docs/keymap.md](docs/keymap.md)。** 常见几个：

| 键 | 作用 |
|---|---|
| `j` / `k`、`↓` / `↑`、`g` / `G` | 移动光标 / 跳首尾 |
| `space` / `r` | 启用 / 停用、重启（运行中先确认） |
| `n` / `e` / `d` | 新建 / 编辑 / 删除（删除需确认） |
| `enter` / `l` | 详情 + 完整日志 |
| `/` | 过滤（名称 / SSH 主机）；`esc` 清除 |
| `a` / `x` | 全部启用 / 全部停用（均需确认） |
| `v` / `V` | 进入 / 退出多选模式 |
| `?` | 打开 / 关闭帮助 |
| `q` / `ctrl+c` | 退出（有运行中转发时确认；`ctrl+c` 等同 `q`） |

`?` 的帮助是**分区**的，内容不是手写散文而是 `src/help.rs` 里的数据，与页脚提示同源，所以"页脚说的"和"`?` 说的"不可能矛盾。

### 新增转发 / Create a tunnel

表单采用**两阶段编辑**，避免方向键误改内容：**导航态**（默认）方向键只移动高亮；按 `enter` 进入**编辑态**才接收输入；`ctrl+s` 随时保存。字段顺序**远端在前、本地在后**：`SSH 主机 → 名称 → 类型 → 远端地址 → 远端端口 → 本地地址 → 本地端口 → 自动重连`。

**本地端口自动推荐**（始终可编辑覆盖）：远端端口空闲且 ≥1024 → 同值；<1024 → `+2200` 的惯用别名（`22 → 2222`），因为绑定低位端口需要 root；被系统或其它条目占用（含**未运行**的）→ 向上顺延；你手改过 → 不再自动覆盖。

**远端端口可直接选**：把光标放在 `远端端口` 上按 `p`，sshpf 会通过**隧道同一条 SSH 路径**去问对端正在监听哪些端口（有 `ss` 用 `ss -ltnH`，否则 `netstat -ltn`，都没有会明确告诉你）。`enter` 填入单条；或 `space` 勾选多条（`a` 全选 / `U` 清空 / `I` 反选，`r` 重扫）后 `ctrl+s`，**每个勾选端口一条 `-L` 转发**。勾选默认**全空**——发现一个服务不等于要转发它。`-R` 的列表是**只读**的（那些端口已被占用，选一个必然撞车）。

### 多个窗口共享同一份状态 / Several windows, one state

**一份配置只有一个后台服务进程**（`sshpf --server`，由第一个窗口自动拉起），所有窗口都是它的客户端：开第二个窗口**不会再报「端口已被占用」**，而是显示同一份状态；任何窗口的操作都会广播给所有窗口；标题栏显示 `客户端 2/client 2`。**关掉最后一个窗口 = 停掉所有转发**，所以只有它会在退出前提示。想让它常驻用 `--keep-server`。

**Linux 与 Windows 走同一套代码路径**：传输层是回环 TCP（不是 Unix 域套接字，`std` 在 Windows 上没有 UDS API），端点目录在 Linux 是 `$XDG_RUNTIME_DIR/sshpf`、在 Windows 是 `%TEMP%\sshpf-ipc`（`dirs::runtime_dir()` 在 Windows 恒为 `None`，落到 per-user temp）。完整模型见 [docs/client-server.md](docs/client-server.md)。

### 从 `~/.ssh/config` 导入转发 / Import

在 `SSH 主机` 字段上按 `space` 或 `i` 展开该主机已声明的 `LocalForward` / `RemoteForward` / `DynamicForward`。**默认全部勾选**，取消不要的再 `ctrl+s`；一条勾选行创建一条隧道。勾选只是**意图**：不写盘、不 bind、不建线程。**编辑已有条目时没有这个面板。**

面板自动展开说的是"不用按任何键就有默认"，**不是**"它总能盖过你手填的东西"：面板没被动过且手填字段完整时，`ctrl+s` **只建手填那一条**（状态栏说明面板的 N 条未创建）；手填不完整、或动过面板（勾选/筛选/手动展开），走面板的勾选行；面板已收起，走手填那一条。

规则速记：`Host` 块**全部累积**（不是命中第一个就停）、`!prod-db` 否定整块排除、`Match` 块里的转发行**不导入也不列出**、注释行不列出、跨块重复的转发去重成一行、`-R` 的两端**对调**（指令监听端→远端，目标端→本地）、Unix socket 形式与无目标的 `-R` 列出但不可勾。完整规则见 [docs/import.md](docs/import.md)。

### 不会重复创建 / No duplicate forwards

新增转发时如果**已经有完全一样的**，新增的不生效：导入面板里那一行标成「已存在：<隧道名>」且**不可勾选**，手填路径在表单里报「已存在相同的转发「X」」。

"完全一样"比的是**意图**，不比端口：同一个 SSH 主机 + 同一个远端目标就是同一条 `-L` 转发，**即使本地监听端口不同**——本地端口是环境逼出来的（被占用就向上顺延，且顺延值会存进配置）。代价写在明处：`LocalForward 8080 host:80` 与 `LocalForward 9090 host:80` 被视为同一条，因为同一远端目标开两个本地监听是冗余的，而漏判会让你永远清不掉重复。

**已经重复的条目会在启动时合并**：载入 `config.toml` 时按同一套身份规则折叠，**保留表格里的第一条**，其余移除，每组记一条 WARN（`event=config.deduped`），状态栏说明「已合并 N 条重复转发」。不改 `~/.ssh/config`、不改留下来的本地端口——启动时悄悄改端口是比重复更坏的意外。详见 [docs/dedup.md](docs/dedup.md)。

### 鼠标与确认 / Mouse & confirmation

终端支持鼠标时（捕获是 best-effort，失败也不影响键盘）：点行移动光标、多选模式下同时切换标记、`shift` 点击扩选、点导入表复选框勾选、点列头筛选该列、点 chip 的 `×` 删除筛选、点动作条按钮执行批量操作（**仍走确认闸**）、滚轮滚动。点击落空**什么都不做**，不"透传"；弹窗**吞掉**点击。

所有破坏性或有副作用的操作先弹确认框（`y`/`enter` 执行，`n`/`esc` 取消）：删除、停用、重启、批量启停/重启/删除、保存编辑、放弃编辑、退出（含 `ctrl+c`）。**刻意不弹**（可逆、无副作用）：移动光标、过滤、帮助、详情、**启动**某条未启用的转发、**新建**转发。给高频操作加确认会让用户习惯无脑按 `y`，真正的危险确认就失效了。

## 风险 / Risks

| 风险 | 说明与对策 |
|---|---|
| 密码/2FA 登录不支持 | 只走 `IdentityFile` 或 ssh-agent；需要密码的主机会立刻失败并显示「认证失败」，**不会**卡在密码提示上 |
| 主机密钥变更 | 默认 `accept-new`：首次连接自动记录，**密钥变更则拒绝**（并指出 `known_hosts` 行号）；`@revoked` 同样拒绝。确需接受时用 `--ignore-host-key-changes` 或 `~/.ssh/config` 的 `StrictHostKeyChecking no`，两者都留 WARN 且不改写 `known_hosts`，见[排障](#ssh-能连但-sshpf-报-host-key-verification-failed) |
| 未做连接复用 | 同一主机多条转发各自建连（`ControlMaster -O forward` 绑定失败仍返回 exit 0，无法可靠判定成败，故不采用） |
| 端口占用反查进程 | 仅 Linux（依赖 `/proc`），其它平台该项为空，不影响转发本身 |
| 把错误搞静默 | 反过来的风险由设计规避：所有降级都有一条可 grep 的日志记录，见 [安全](docs/security.md) |

## 配置 / Configuration

路径按平台取（`SSHPF_CONFIG` 可覆盖）：

| 内容 | Linux | Windows |
|---|---|---|
| 配置 | `~/.config/sshpf/config.toml`（`$XDG_CONFIG_HOME` 生效） | `%APPDATA%\sshpf\config.toml` |
| 运行状态 | `~/.local/state/sshpf/state.toml`（`$XDG_STATE_HOME` 生效） | `%LOCALAPPDATA%\sshpf\state.toml` |

配置与状态分开，运行状态不污染用户编辑的配置。权限：Linux 上配置 `0600`、目录 `0700`；Windows 没有 mode bit，靠用户配置文件目录继承的 ACL（`SYSTEM` / `Administrators` / 本人）。

```toml
version = 1

[defaults]
bind = "127.0.0.1"        # 新转发的默认本地监听地址
restore_on_start = true   # 启动时恢复 enabled = true 的条目
auto_restart = true       # 网络类失败自动退避重连（2s→4s→…→30s）
retry_initial_secs = 2
retry_max_secs = 30
health_check_secs = 30    # 每 30s 主动探活一次；0 = 关闭

[[tunnels]]
id = 1
name = "cloud-pc-ssh"
kind = "local"            # local(-L) | remote(-R) | dynamic(-D, SOCKS5)
ssh_host = "cloud_pc"     # ~/.ssh/config 里的别名，或 user@host:port
local_addr = "127.0.0.1"
local_port = 19711
remote_host = "127.0.0.1"
remote_port = 22
enabled = true            # 上次是否在运行 → 启动时据此恢复
auto_restart = true
ssh_options = []          # 额外 ssh -o 选项，如 ["Compression=yes"]
note = ""
```

**不存任何密码或密钥口令。** `retry_initial_secs` / `retry_max_secs` 越界会被钳制到 `[1s, 24h]` 并记 `config.defaults_clamped`（`0` 会变成每帧重连，所以不能照做）。`health_check_secs` 同样记 `config.defaults_clamped`：`0` 保持不变（**0 = 关闭探活**），`1..=4` 抬到 `5`，超过 24h 压到 24h。

| 变量 | 作用 |
|---|---|
| `SSHPF_CONFIG` | 配置文件路径（`state.toml` 与它同目录） |
| `SSHPF_LOG_DIR` / `SSHPF_LOG_LEVEL` | 日志目录 / `off\|error\|warn\|info\|debug`（默认 `info`；`off` 仍写崩溃记录） |
| `SSHPF_IPC_DIR` | client-server 的端点目录（端口/令牌/锁；默认 Linux `$XDG_RUNTIME_DIR/sshpf`、Windows `%TEMP%\sshpf-ipc`）；测试用它隔离 |
| `SSHPF_SSH_CONFIG` / `SSHPF_KNOWN_HOSTS` | 只解析这个 ssh config / 使用这个 known_hosts 文件 |
| `NO_COLOR` | 关闭全部颜色（状态仍由符号和文字表达） |

## 排查 / Troubleshooting

```bash
sshpf --list-hosts          # 确认主机能被读到
sshpf --check               # 确认配置没写坏
tail -f ~/.local/state/sshpf/logs/sshpf.log   # 实时看结构化日志（Windows: %LOCALAPPDATA%\sshpf\logs\sshpf.log）
```

| 症状 | 先看什么 |
|---|---|
| 主机密钥校验失败 | 见下方[「ssh 能连但 sshpf 报 host key verification failed」](#ssh-能连但-sshpf-报-host-key-verification-failed) |
| 认证失败 | 先手动 `ssh <主机>` 确认能免密登录，或 `ssh-add` 加载密钥 |
| 端口被占用 | 详情页会显示占用进程的 `pid/进程名`。**如果占用者是 `sshpf` 自己**，说明有旧版本的单进程实例还在跑：退出它即可 |
| 第二个窗口连不上 / 状态不一致 | 看 `client.attach_failed`、`server.election_lost`、`client.disconnected`；`SSHPF_IPC_DIR` 不同会让两个窗口连到不同的服务进程 |
| 某条一直 `◌ 重试` | `grep 'event=tunnel.retry_scheduled'` 看排了几次，`tunnel.retry_abandoned` 表示已放弃 |
| 显示 `● 已连通` 但端口不通 / 休眠唤醒后不工作 | SSH 半开连接：`grep 'event=tunnel.health_failed'` 看 `cause`（`ping_timeout`/`ping_error`/`session_closed`），确认探活没被 `health_check_secs=0` 关掉；探活成功不写日志，所以**没有这条事件不等于探活没跑** |
| 界面某块不见了 | `grep 'event=ui.render_degraded'` |
| 进程崩了 / 卡死 | `grep -E 'event=crash\.|tunnel.thread_panic'`，判据见下 |

### ssh 能连但 sshpf 报 host key verification failed

最常见：终端里 `ssh cloud_pc` 正常，sshpf 建同样的隧道却拒绝。**根因通常不是 `cloud_pc`，而是它前面那一跳跳板机。**

```text
Host cloud_pc_jump
    HostName desk-ssh.srv.deeproute.cn
    Port 80
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
Host cloud_pc
    HostName 10.244.119.128
    ProxyJump cloud_pc_jump
```

OpenSSH 因为 `StrictHostKeyChecking no` 根本不校验跳板机；sshpf 用自己的 `known_hosts` 校验器，照样拒绝。日志里 `host=` 指出被拒的是**哪一跳**：

```bash
grep 'event=knownhosts.rejected' ~/.local/state/sshpf/logs/sshpf.log   # Windows 用 "$env:LOCALAPPDATA/sshpf/logs/sshpf.log"
# ERROR … event=knownhosts.rejected host=desk-ssh.srv.deeproute.cn port=80 policy=accept-new
```

| 顺序 | 修法 | 说明 |
|---|---|---|
| 1 | 首选：对应 `Host` 块加 `StrictHostKeyChecking no` | per-host，sshpf 会读到（`no`/`off` 或 `UserKnownHostsFile /dev/null` 均生效）；目标主机仍保持严格 |
| 2 | 确认真实指纹后用 `ssh-keygen -R '[desk-ssh.srv.deeproute.cn]:80'` 清理旧条目 | 服务器确实换密钥时最干净；sshpf 会重新学习 |
| 3 | 兜底：`sshpf --ignore-host-key-changes` | 全局，等价 `ssh -o StrictHostKeyChecking=no`；只在临时排障时用 |

**忽略是降级行为**：每次接受都在日志留一条 `knownhosts.ignored_changed`（WARN，带 SHA256 指纹），状态条也会提示；`known_hosts` **不会被改写**。`@revoked` 吊销的密钥在任何策略下都不接受。

**崩溃不再是裸 panic。** 输入不合法时程序正常退出并在 stderr 说一句人话，原因写进日志。历史崩点与现在的行为对照见 [docs/security.md](docs/security.md#三层防御--three-layers)：中文注释的 config、非 UTF-8 参数、写满的 stdout、id 溢出、窄终端、持锁时 panic、隧道线程 panic —— 全部已转为降级或正常退出。

**提 issue 请附整个 `logs/` 目录**（含 `.1`~`.3`）——重连风暴、配置被改的证据都在更早的位置。完整事件目录与排查配方见 [docs/logging.md](docs/logging.md)。

## 开发 / Development

```bash
cargo test                                     # 单元 + 集成（e2e 默认跳过）
cargo test --test help_coverage                # 帮助/键位机器校验
cargo test --test e2e -- --ignored --test-threads=1   # 真实 sshd 端到端
cargo clippy --all-targets                     # 零警告
cargo fmt --check
```

| 我想… | 读 |
|---|---|
| 找任何东西 | **[docs/index.md](docs/index.md)**——按「我想…」查表 |
| 改按键 / 界面 / 日志 / 隧道 / 导入 | [docs/keymap.md](docs/keymap.md)、[docs/tui.md](docs/tui.md)、[docs/logging.md](docs/logging.md)、[docs/runtime.md](docs/runtime.md)、[docs/import.md](docs/import.md) |
| 了解分层、安全政策、测试、改码约定 | [docs/architecture.md](docs/architecture.md)、[docs/security.md](docs/security.md)、[docs/testing.md](docs/testing.md)、[docs/agents.md](docs/agents.md) |

调研依据（**历史调研，非当前事实来源**，见 [docs/research/README.md](docs/research/README.md)）：[pure-rust-ssh-plan.md](docs/research/pure-rust-ssh-plan.md)、[pure-rust-ssh-spike.md](docs/research/pure-rust-ssh-spike.md)、[tech-options.md](docs/research/tech-options.md)、[prior-art.md](docs/research/prior-art.md)。

## 已知限制（v1）

- **端口占用反查进程**仅 Linux 可用（依赖 `/proc`）；其它平台上该列只是空着，不影响转发本身。**多窗口共享状态**两平台都支持（回环 TCP + 令牌）。
- 一份配置一个服务进程；用 `-c` 指向不同配置文件就是互相独立的两套状态（socket 路径由配置路径派生）。
- 只支持公钥 / ssh-agent 认证，TUI 内不接管密码输入；不统计流量（需按 channel 累计字节，留待后续）。
- 同一主机多条转发各自建连，未做连接复用；隧道既是 tokio task，杀进程即关闭 socket，不需要孤儿回收。
- **导入**只读 `LocalForward` / `RemoteForward` / `DynamicForward`；`Match` 块里的转发一律不导入；Unix socket 形式与 `-R` 无目标只列出不可导入；去重按**字面** key 不走地址归一化；`Include` 有总文件数预算。完整清单见 [docs/import.md](docs/import.md#已知边界--known-limits)。
- 鼠标是 best-effort；字面字符 `f` 无法输入到导入表筛选里（`f` 优先用于切换筛选列）。
- **崩溃日志在 Windows 上只覆盖 panic**（没有 `sigaction`/Unix 信号语义，SEH 层不在此列）；致命信号行用 epoch 秒（`ts=…`）而非格式化时间戳。见 [docs/security.md](docs/security.md#崩溃捕获--crash-capture)。

## 许可证

[MIT](LICENSE)

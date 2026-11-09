# sshpf

[![CI](https://github.com/can2049/sshpf/actions/workflows/ci.yml/badge.svg)](https://github.com/can2049/sshpf/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/sshpf.svg)](https://crates.io/crates/sshpf)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

SSH 端口转发管理器，命令行 TUI 表格界面。Linux 与 Windows 均支持。

在不离开终端的情况下查看、新增、启停、删除端口转发，每条转发实时显示状态与失败原因；配置持久化，重启后自动恢复上次启用的条目。

> 本文是**用户视角**：是什么 / 装 / 用 / 风险 / 配置 / 排查。内部结构、设计理由、给 AI agent 的导航在 **[docs/index.md](docs/index.md)**。

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
│  sshpf 0.4.0  ·  ssh-config: 12 hosts  ·  cfg: ~/.config/sshpf/config.toml          13:47:03    │
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
│ j/k ↑↓ 移动  space 勾选  J/K 扩选  g/G 首尾                                                       │
│ a 全选  s/x/R/D 批量操作  esc 退出  ? 帮助                                                        │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

## 是什么 / What it is

| 能力 | 说明 |
|---|---|
| 三种转发 | `-L` 本地、`-R` 远端、`-D` 动态（内置 SOCKS5 服务端） |
| 主机来源 | `~/.ssh/config`（`Include`、`Host` 通配、`!` 否定、`Host *` 继承、`ProxyJump`） |
| 转发定义 | 由 sshpf 自己管理，**从不写** `~/.ssh/config` |
| 状态可见 | 每条转发实时显示状态符号 + 失败原因短句，详情页有完整日志 |
| 自动重连 | 网络类失败指数退避（2s→…→30s），配置类失败直接停下并说明原因 |
| 导入 | 主机在 `~/.ssh/config` 里声明的转发可直接导入，默认全勾 |
| 风险控制 | 破坏性/有副作用的操作一律先弹确认框 |
| 双语 | 界面文案、帮助浮层、`--help` 全部中英并列 |

## 安装 / Install

```bash
cargo install sshpf          # 需要 Rust 1.89+
```

预编译二进制从 [GitHub Releases](https://github.com/can2049/sshpf/releases) 下载（Linux x86_64 静态链接 / Windows x86_64）。也可以装 git 最新版：

```bash
cargo install --git https://github.com/can2049/sshpf
```

从源码构建：

```bash
git clone https://github.com/can2049/sshpf.git && cd sshpf
cargo build --release
install -m755 target/release/sshpf ~/.local/bin/sshpf
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

`--ignore-host-key-changes` 只放宽"密钥变了"这一种情况：不写入 `known_hosts`、每次接受都记一条 WARN，`@revoked` 吊销的密钥**仍然拒绝**。某台主机单独需要时，优先在 `~/.ssh/config` 的 `Host` 块里写 `StrictHostKeyChecking no`（sshpf 会读取，作用域仅限该主机）。

### 键位 / Keys

界面里**任何时刻**按 `?` 都能看到当前上下文的按键说明，中英双语，且与文档一致。
**完整键位参考（按 14 个上下文分表，含鼠标与状态图例）见 [docs/keymap.md](docs/keymap.md)。**

常见几个：

| 键 | 作用 |
|---|---|
| `j` / `k`、`↓` / `↑`、`g` / `G` | 移动光标 / 跳首尾 |
| `space` | 启用 / 停用（运行中先确认） |
| `r` | 重启（运行中先确认） |
| `n` / `e` / `d` | 新建 / 编辑 / 删除（删除需确认） |
| `enter` / `l` | 详情 + 完整日志 |
| `/` | 过滤（名称 / SSH 主机）；`esc` 清除 |
| `a` / `x` | 全部启用 / 全部停用（均需确认） |
| `v` / `V` | 进入 / 退出多选模式 |
| `?` | 打开 / 关闭帮助 |
| `q` / `ctrl+c` | 退出（有运行中转发时确认；`ctrl+c` 等同 `q`） |

`?` 打开的帮助是**分区**的：主界面、多选、表单（导航/编辑）、主机下拉、导入面板、导入列筛选、确认框、详情、过滤、帮助自身、鼠标、状态图例、安全与诊断。帮助内容不是手写散文，而是 `src/help.rs` 里的数据，与页脚提示同源，所以"页脚说的"和"`?` 说的"不可能矛盾。

### 新增转发 / Create a tunnel

表单采用**两阶段编辑**，避免方向键误改内容：**导航态**（默认）方向键只移动高亮；按 `enter` 进入**编辑态**才接收输入；`ctrl+s` 随时保存。

字段顺序**远端在前、本地在后**：`SSH 主机 → 名称 → 类型 → 远端地址 → 远端端口 → 本地地址 → 本地端口 → 自动重连`。

**本地端口自动推荐**（始终可编辑覆盖）：

| 场景 | 推荐 |
|---|---|
| 远端端口空闲且 ≥1024 | 同值（`8080 → 8080`） |
| 远端端口 <1024 | `+2200` 的惯用别名（`22 → 2222`），因为绑定低位端口需要 root |
| 端口被系统占用，或被其它条目占用（含**未运行**的） | 向上顺延 |
| 你手改过本地端口 | 不再自动覆盖 |

### 从 `~/.ssh/config` 导入转发 / Import

在 `SSH 主机` 字段上按 `space` 或 `i` 展开该主机已声明的 `LocalForward` / `RemoteForward` / `DynamicForward`。**默认全部勾选**，取消不要的再 `ctrl+s`；一条勾选行创建一条隧道。

勾选只是**意图**：不写盘、不 bind、不建线程，隧道在你确认保存之后才创建。**编辑已有条目时没有这个面板**（把一次编辑悄悄变成批量新建是数据丢失级的问题）。

几个容易踩的规则：

| 情况 | 行为 |
|---|---|
| `Host cloud_pc` 和 `Host *` 都声明了转发 | **全部累积**（OpenSSH 语义） |
| `Host prod-* !prod-db`，别名是 `prod-db` | 否定命中 → 整块排除 |
| 转发写在 `Match` 块里 | **不导入、也不列出**（带连接上下文前置条件） |
| 注释掉的 `# LocalForward …` | 不列出 |
| `Host *` 重复了具体块的转发 | 去重成一行，否则两条隧道抢同一端口 |
| Unix socket 形式 / `-R` 没有目标 | 列出但不勾选，变暗并写明原因 |
| 供 `-R` 使用 | 两端**对调**：指令的监听端 → 远端，目标端 → 本地 |

完整规则（作用域累积、`Include` 预算、去重按字面 key、批量校验顺序、端口顺延）见 [docs/import-forwards.md](docs/import-forwards.md)。

### 鼠标 / Mouse

终端支持鼠标时（捕获是 best-effort，失败也不影响键盘）：点行移动光标、多选模式下同时切换标记、`shift` 点击扩选、点导入表复选框勾选、点列头开始过滤该列、点筛选 chip 的 `×` 删除、点动作条按钮执行批量操作（**仍走确认闸**）、滚轮滚动。

点击落空**什么都不做**，不"透传"给下面的控件；弹窗**吞掉**点击，不会穿透到下面的表格。

### 风险操作确认

所有破坏性或有副作用的操作先弹确认框，`y`/`enter` 执行，`n`/`esc` 取消：删除、停用、重启、批量启停/重启/删除、保存编辑、放弃编辑、退出（含 `ctrl+c`）。

**刻意不弹确认**（可逆、无副作用）：移动光标、过滤、打开帮助、查看详情、**启动**某条未启用的转发、**新建**转发。给高频操作加确认会让用户习惯无脑按 `y`，真正的危险确认就失效了。

## 风险 / Risks

| 风险 | 说明与对策 |
|---|---|
| 密码/2FA 登录不支持 | 只走 `IdentityFile` 或 ssh-agent；需要密码的主机会立刻失败并显示「认证失败」，**不会**卡在密码提示上 |
| 主机密钥变更 | 默认策略 `accept-new`：首次连接自动记录，**密钥变更则拒绝**（并指出 `known_hosts` 行号）；被 `@revoked` 吊销的密钥同样拒绝。确需接受变更时用 `--ignore-host-key-changes` 或 `~/.ssh/config` 的 `StrictHostKeyChecking no`，两者都留 WARN 且不改写 `known_hosts`，见[排障](#ssh-能连但-sshpf-报-host-key-verification-failed) |
| 未做连接复用 | 同一主机多条转发各自建连（`ControlMaster -O forward` 绑定失败仍返回 exit 0，无法可靠判定成败，故不采用） |
| 端口占用反查进程 | 仅 Linux（依赖 `/proc`），其它平台该项为空，不影响转发本身 |
| 把错误搞静默 | 反过来的风险由设计规避：所有降级都有一条可 grep 的日志记录，见 [安全](docs/security.md) |

## 配置 / Configuration

`~/.config/sshpf/config.toml`（`$XDG_CONFIG_HOME` 生效；`SSHPF_CONFIG` 可覆盖），权限 `0600`。运行状态单独放 `~/.local/state/sshpf/state.toml`，不污染用户编辑的配置。

```toml
version = 1

[defaults]
bind = "127.0.0.1"        # 新转发的默认本地监听地址
restore_on_start = true   # 启动时恢复 enabled = true 的条目
auto_restart = true       # 网络类失败自动退避重连（2s→4s→…→30s）
retry_initial_secs = 2
retry_max_secs = 30

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

**不存任何密码或密钥口令。** `retry_initial_secs` / `retry_max_secs` 越界会被钳制到 `[1s, 24h]` 并记 `config.defaults_clamped`（`0` 会变成每帧重连，所以不能照做）。

## 环境变量 / Environment

| 变量 | 作用 |
|---|---|
| `SSHPF_CONFIG` | 配置文件路径（`state.toml` 与它同目录） |
| `SSHPF_LOG_DIR` | 日志目录（默认 `~/.local/state/sshpf/logs`） |
| `SSHPF_LOG_LEVEL` | `off\|error\|warn\|info\|debug`，默认 `info`；`off` 仍写崩溃记录 |
| `SSHPF_SSH_CONFIG` | 只解析这个文件代替 `~/.ssh/config` |
| `SSHPF_KNOWN_HOSTS` | 使用这个 known_hosts 文件 |
| `NO_COLOR` | 关闭全部颜色（状态仍由符号和文字表达） |

## 排查 / Troubleshooting

```bash
sshpf --list-hosts          # 确认主机能被读到
sshpf --check               # 确认配置没写坏
tail -f ~/.local/state/sshpf/logs/sshpf.log   # 实时看结构化日志
```

| 症状 | 先看什么 |
|---|---|
| 主机密钥校验失败 | 见下方[「ssh 能连但 sshpf 报 host key verification failed」](#ssh-能连但-sshpf-报-host-key-verification-failed) |
| 认证失败 | 先手动 `ssh <主机>` 确认能免密登录，或 `ssh-add` 加载密钥 |
| 端口被占用 | 详情页会显示占用进程的 `pid/进程名` |
| 某条一直 `◌ 重试` | 网络类失败在退避；`grep 'event=tunnel.retry_scheduled'` 看排了几次，`tunnel.retry_abandoned` 表示已放弃 |
| 界面某块不见了 | `grep 'event=ui.render_degraded'`——渲染层主动跳过的块会留一条记录 |
| 进程崩了 / 卡死 | `grep -E 'event=crash\.|tunnel.thread_panic'`，判据见下 |

### ssh 能连但 sshpf 报 host key verification failed

最常见的情况：终端里 `ssh cloud_pc` 正常登录，sshpf 建同样的隧道却拒绝。**根因通常不是 `cloud_pc`，而是它前面那一跳跳板机。**典型 `~/.ssh/config`：

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

OpenSSH 因为 `StrictHostKeyChecking no` 根本不校验跳板机；sshpf 用自己的 `known_hosts` 校验器，照样拒绝。打开日志能看到是哪一跳：

```bash
grep 'event=knownhosts.rejected' ~/.local/state/sshpf/logs/sshpf.log
# ERROR … event=knownhosts.rejected host=desk-ssh.srv.deeproute.cn port=80 policy=accept-new
#   msg="[desk-ssh.srv.deeproute.cn]:80 的主机密钥已改变（known_hosts 第 34 行），可能存在中间人攻击"
```

`host=` 字段指出被拒的是**哪一跳**——上例里是跳板机 `desk-ssh.srv.deeproute.cn:80`，不是 `10.244.119.128`。

| 顺序 | 修法 | 说明 |
|---|---|---|
| 1 | 首选：在 `~/.ssh/config` 对应 `Host` 块加 `StrictHostKeyChecking no` | per-host，sshpf 会读到（`no`/`off` 或 `UserKnownHostsFile /dev/null` 均生效）；目标主机仍保持严格 |
| 2 | 或确认真实指纹后用 `ssh-keygen -R '[desk-ssh.srv.deeproute.cn]:80'` 清理旧条目 | 确实是服务器换了密钥时最干净的做法；`sshpf` 会重新学习 |
| 3 | 兜底：`sshpf --ignore-host-key-changes` | 全局，等价 `ssh -o StrictHostKeyChecking=no`；只在临时排障时用 |

**忽略是降级行为**：每次接受都在日志留一条 `knownhosts.ignored_changed`（WARN，带服务器实际呈现的 SHA256 指纹），状态条也会提示；`known_hosts` **不会被改写**，所以下次启动仍会看到同一处不一致，直到你按上表第 2 步确认或清理。`@revoked` 吊销的密钥在任何策略下都不接受。

**崩溃不再是裸 panic。** 输入不合法时程序正常退出并在 stderr 说一句人话，原因写进日志，而不是甩一段 Rust 栈给用户：

| 以前 | 现在 |
|---|---|
| `~/.ssh/config` 首行是中文注释 → panic | 正常解析，主机照常列出 |
| 无终端（CI / `ssh host sshpf` 不带 `-t`）→ panic | 退出 1 + `app.fatal stage=tui` |
| 参数里有非 UTF-8 字节 → panic | 路径按字节接受；参数名则退出 2 |
| `--list-hosts > /dev/full` → panic | 退出 0 + `cli.stdout_failed` |
| `id = 18446744073709551615` 后新建转发 → 溢出 panic | 回落到空闲 id |
| 窄终端 / 超长值 → 除零、`u16` 溢出 panic | 降级渲染 + `ui.render_degraded` |
| panic 时日志锁被持有 → 进程永久僵住 | `crash.log_reentrant`，照常退出 |
| 隧道线程 panic → 该行永远"启动中" | `tunnel.thread_panic`，转失败态，UI 继续 |

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
| 了解分层与模块地图 | [docs/architecture.md](docs/architecture.md) |
| 改按键 / 界面 / 日志 / 隧道 | [docs/keymap.md](docs/keymap.md)、[docs/tui.md](docs/tui.md)、[docs/logging.md](docs/logging.md)、[docs/runtime.md](docs/runtime.md) |
| 了解安全政策与测试 | [docs/security.md](docs/security.md)、[docs/testing.md](docs/testing.md) |
| 改码约定（给 AI agent） | [docs/agents.md](docs/agents.md) |

调研依据（**历史调研，非当前事实来源**，见 [docs/research/README.md](docs/research/README.md)）：
[pure-rust-ssh-plan.md](docs/research/pure-rust-ssh-plan.md)、
[pure-rust-ssh-spike.md](docs/research/pure-rust-ssh-spike.md)、
[tech-options.md](docs/research/tech-options.md)、
[prior-art.md](docs/research/prior-art.md)。

## 已知限制（v1）

- **端口占用反查进程**仅 Linux 可用（依赖 `/proc`）。
- 隧道既是 tokio task，杀进程即关闭 socket，会话随之结束——不需要孤儿回收；macOS 走 Unix 分支。
- 只支持公钥 / ssh-agent 认证，TUI 内不接管密码输入。
- 不统计流量（需按 channel 累计字节，留待后续）。
- 同一主机多条转发各自建连，未做连接复用。
- **导入只读** `LocalForward` / `RemoteForward` / `DynamicForward`；`Match` 块里的转发一律不导入；Unix socket 形式与 `-R` 无目标只列出不可导入。
- `Include` 有**总文件数**预算（含主文件共 9 个）；`Include` 写在 `Host` 块内部时，被包含文件里的裸指令会被当成全局指令。
- 导入去重按**字面** key，不做地址归一化（`8090` 与 `127.0.0.1:8090` 是两行）；端口冲突在创建时兜底。
- 鼠标是 best-effort；字面字符 `f` 无法输入到导入表筛选里（`f` 优先用于切换筛选列）。
- **崩溃日志在 Windows 上只覆盖 panic**：没有 `sigaction`/Unix 信号语义，SEH 层（访问违例）不在此列。
- 崩溃记录里的致命信号行用 epoch 秒（`ts=…`）而非格式化时间戳（信号处理器只能用 async-signal-safe 调用）。

## 许可证

[MIT](LICENSE)

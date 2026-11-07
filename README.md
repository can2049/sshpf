# sshpf

[![CI](https://github.com/can2049/sshpf/actions/workflows/ci.yml/badge.svg)](https://github.com/can2049/sshpf/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

SSH 端口转发管理器，命令行 TUI 表格界面。Linux 与 Windows 均支持。

在不离开终端的情况下查看、新增、启停、删除端口转发，每条转发实时显示状态与失败原因；配置持久化，重启后自动恢复上次启用的条目。

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
│  sshpf 0.1.0  ·  ssh-config: 12 hosts  ·  cfg: ~/.config/sshpf/config.toml          13:47:03    │
│  ● 2 已连通   ◐ 0 连接中   ◌ 0 重试   ✗ 1 失败   ○ 1 未启用                                      │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│   名称          T 本地             远端             SSH 主机    状态        最后错误       时长   │
│ ▶ cloud-pc-ssh  L 127.0.0.1:19711  127.0.0.1:22    cloud_pc    ● 已连通    —            04:12   │
│   jeprof-db     L 127.0.0.1:15432  127.0.0.1:5432  jeprof      ✗ 失败      主机密钥校验失败  —    │
│   old-tunnel    L 127.0.0.1:18080  127.0.0.1:8080  dev-box     ○ 未启用    —             —     │
├──────────────────────────────────────────────────────────────────────────────────────────────────┤
│  ✓ cloud-pc-ssh 已连通                                                                            │
│  j/k ↑↓ 移动  space 启停  r 重启  n 新建  e 编辑  d 删除  enter 详情  l 日志  / 过滤  a 全启  x 全停 │
│  g/G 首尾  ? 帮助  esc 关闭弹窗  q 退出                                                           │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

## 设计要点

**SSH 由 Rust 直接实现，不再调用系统 `ssh` 子进程。** 用的是 [`russh`](https://crates.io/crates/russh)（纯 Rust SSH 协议栈）。这样 Linux 和 Windows 走**完全相同的代码路径**，行为一致；而且失败原因来自结构化错误，不再靠正则解析 `ssh -v` 的 stderr 文本。

**主机来自 `~/.ssh/config`，转发条目由本工具管理。** 职责分离：ssh config 负责「怎么连」，sshpf 负责「转发什么」。不会往 `~/.ssh/config` 写入任何内容。`Include`、`Host` 通配、否定、`Host *` 继承、`ProxyJump` 都由 [`ssh2-config`](https://crates.io/crates/ssh2-config) 解析（它名字里的 "ssh2" 有误导性，实际是纯 Rust、零 C 依赖）。

**每条转发 = 一个后台线程 + 独立的 tokio runtime。** UI 线程保持同步（`crossterm::event::poll(200ms)`），事件通过 `std::sync::mpsc` 回灌，所以界面永不被网络阻塞。三种转发都由 SSH 协议原语实现：

- `-L`：本地 `TcpListener` + `direct-tcpip` channel
- `-R`：`tcpip-forward` 请求 + `forwarded-tcpip` 回调
- `-D`：内置 SOCKS5 服务端（自己写的，约 150 行，含 IPv4/域名/IPv6）

**就绪判定不看日志，看真实结果。** `-L`/`-D` 的"就绪"是 `TcpListener::bind` 真的成功了；`-R` 的"就绪"是 `tcpip_forward()` 等到了服务端的协议回复。旧版本靠 stderr 里的 `Entering interactive session.` / `remote forward success for` 判断，有两个实测过的误判场景（ProxyJump 子进程的行会混入、`-R` 的成功标记晚于 `Entering`）——换实现后这类 bug 从根上消失。

**主机密钥校验自己实现。** `russh` 自带的 known_hosts 校验实测有缺陷（不支持通配符、Tab 分隔漏判，且 `@revoked` 与正常条目并存时**会放行被吊销的密钥**），所以匹配逻辑自己写：支持 `*`/`?` 通配、`!` 否定、`[host]:port`、`|1|` 哈希条目、`@revoked` 优先拒绝。默认策略 `accept-new`（首次连接自动记录，密钥变更则拒绝）。

**失败原因分类来自结构化错误。** 不再解析 stderr：`ConnectionRefused`/`Timeout`/`AddrInUse`/`PermissionDenied`/`RequestDenied` 等直接来自 `russh::Error` 变体。主机密钥被拒时会明确显示是"已改变"还是"已吊销"，并指向 known_hosts 的行号。

**转发被拒绝时主动停止会话。** 会话建立但转发请求被拒（`-R` 端口被占等）时，隧道是没用的；主动停掉，界面和现实才一致。

## 安装

从源码构建（需要 Rust 1.89+）：

```bash
git clone https://github.com/can2049/sshpf.git
cd sshpf
cargo build --release
install -m755 target/release/sshpf ~/.local/bin/sshpf
```

或直接用 cargo 安装：

```bash
cargo install --git https://github.com/can2049/sshpf
```

**运行时零外部依赖**：不再需要系统 `ssh`。认证走 `~/.ssh/config` 里的 `IdentityFile` 或 ssh-agent。

构建期需要 C 工具链（`ring` 加密后端，Windows 上即预装的 MSVC），因为密码学汇编需要编译；产物本身是自包含的。

| 平台 | 状态 | 说明 |
|---|---|---|
| Linux | ✅ 完整支持 | 端到端测试跑真实 sshd |
| Windows | ✅ 支持 | 同一套纯 Rust 代码路径，无需安装 OpenSSH |
| macOS | ⚠️ 未测试 | 代码走 Unix 分支，预期可用 |

CI 在 `ubuntu-latest` 与 `windows-latest` 上分别编译并运行单元测试。

## 使用

```bash
sshpf                       # 启动 TUI，自动恢复上次启用的转发
sshpf --no-restore          # 启动但不自动恢复
sshpf -c /path/config.toml  # 使用指定配置
sshpf --list-hosts          # 打印 ~/.ssh/config 里的主机后退出
sshpf --check               # 校验配置文件后退出
```

### 快捷键

| 键 | 作用 | 键 | 作用 |
|---|---|---|---|
| `j`/`k` `↑`/`↓` | 移动光标 | `space` | 启用 / 停用 |
| `g`/`G` | 跳到首 / 尾行 | `r` | 重启 |
| `n` | 新建转发 | `e` | 编辑 |
| `d` | 删除（需确认） | `enter` / `l` | 详情 + 完整日志 |
| `/` | 过滤 | `a` / `x` | 全部启用 / 全部停用（需确认） |
| `?` | 帮助 | `q` | 退出（有运行中转发时确认） |

表单内（两阶段编辑）：`↑↓`/`tab` 移动高亮 → `enter` 进入编辑 → `enter`/`esc` 结束编辑；`ctrl+s` 保存。SSH 主机字段按 `enter` 展开下拉。

### 风险操作确认

所有**破坏性或有副作用的操作**都会先弹确认框，必须按 `y` 或 `enter` 才执行，`n` 或 `esc` 取消：

| 操作 | 触发 | 确认提示 |
|---|---|---|
| 删除转发 | `d` | 确认删除「名称」？此操作不可撤销。 |
| 停用转发 | `space`（运行中时） | 确认停用「名称」？将断开其现有连接。 |
| 重启转发 | `r`（运行中时） | 确认重启「名称」？连接会短暂中断。 |
| 停用全部 | `x` | 确认停用全部 N 条运行中的转发？将断开所有连接。 |
| 启用全部 | `a` | 确认启用全部转发？将新开 N 个监听端口。 |
| 保存修改 | 编辑已有条目后 `ctrl+s` | 确认保存对「名称」的修改？运行中的连接会重建。 |
| 放弃编辑 | 表单里 `esc`（有未保存内容） | 确认放弃未保存的修改？已输入的内容会丢失。 |
| 退出程序 | `q` / `ctrl+c` | 确认退出 sshpf？（有运行中转发时提示会停止几条） |

```
╭ 确认 ────────────────────────────────────╮
│ ⚠ 确认停用「db-prod」？将断开其现有连接。│
│                                          │
│  y / enter 确认     n / esc 取消         │
╰──────────────────────────────────────────╯
```

**刻意不弹确认的操作**（可逆、无副作用，弹窗只会碍事）：

- 移动光标 `j/k`/`↑↓`/`g/G`、过滤 `/`、打开帮助 `?`、查看详情 `enter`
- **启动**某条未启用的转发（新增/启动是可逆的，停用才需要确认）
- **新建**转发并保存（新增是增量操作，删掉即可撤销；编辑才算覆盖）
- 表单里在两阶段导航态下移动高亮（不会改动任何值）

确认框的设计取舍：批量操作与单项操作共用同一套对话框，但文案会区分「一条」和「N 条」，
避免用户以为 `x` 只停当前行。

### 新增转发：两阶段编辑 + 自动推荐本地端口

表单采用**两阶段编辑**，避免方向键误改内容：

- **导航态**（默认）：`↑↓`/`tab` 只在字段间移动高亮，**不会改动任何值**
- **编辑态**：在高亮字段上按 `enter` 才进入，此时才接受键盘输入
- 编辑态按 `enter`/`esc` 结束编辑回到导航态；`tab` 结束并跳到下一字段
- `ctrl+s` 在任何时候保存

字段顺序按实际填写逻辑排列，**远端在前、本地在后**：

```
╭ 新建转发 ──────────────────────────────────────────────────────────╮
│ SSH 主机  testhost                                                ▾ │
│ 名称      demo                                                      │
│ 类型      < 本地转发 -L >                                           │
│ 远端地址  127.0.0.1                                                 │
│ 远端端口  8080                                                      │
│ 本地地址  127.0.0.1                                                 │
│ 本地端口  8080                                                      │
│ 自动重连  [x] 网络类失败时自动退避重试                              │
│ · 已自动设为 8080（与远端端口相同）（可编辑）                       │
│ · 127.0.0.1:8080 可用                                               │
│ ↑↓ 选择字段  ·  enter 编辑  ·  ctrl+s 保存  ·  esc 取消             │
╰─────────────────────────────────────────────────────────────────────╯
```

**本地端口自动推荐**：填完远端端口后自动给出建议值，用户仍可编辑覆盖。

| 场景 | 推荐行为 |
|---|---|
| 远端端口空闲 | 直接用相同数值（`8080 → 8080`） |
| 远端端口被占用 | 向上顺延到下一个空闲端口（`8080 → 8081`） |
| 远端端口是特权端口（<1024） | 用 `+2200` 的惯用别名（`22 → 2222`），因为绑定低位端口需要 root |
| 与其它转发条目冲突 | 跳过该端口（即使那条转发当前没在跑） |
| 用户手改过本地端口 | **不再自动覆盖**；只在提示里说明 |

推荐结果只是建议，字段始终可编辑；修改远端端口会重新推荐，一旦你手动改过本地端口就停止自动填充。

## 配置

`~/.config/sshpf/config.toml`（`$XDG_CONFIG_HOME` 生效；`SSHPF_CONFIG` 可覆盖），权限 `0600`。
运行时状态单独放 `~/.local/state/sshpf/state.toml`，不污染用户编辑的配置。

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

**不存任何密码或密钥口令。** 认证只走 `~/.ssh/config` 的 `IdentityFile` 或 ssh-agent（与旧版本的 `BatchMode=yes` 行为一致）。需要密码/2FA 的主机会立即失败并在表格里显示「认证失败」，提示你先配好免密登录——TUI 不会卡在密码提示上。密码与键盘交互登录暂不支持。

## 失败原因分类

表格「最后错误」列只放归类短句，完整 ssh 原始输出在 `enter` 详情页里（可滚动、不截断）：

| 短句 | 触发 |
|---|---|
| 本地端口被占用 | `bind …: Address already in use`；也包含启动前的端口预检 |
| 端口权限不足(<1024) | `bind …: Permission denied` |
| 认证失败(公钥/agent) | `Permission denied (publickey…)` |
| 域名解析失败 | `Could not resolve hostname` |
| 连接被拒绝 / 连接超时 | `Connection refused` / `Connection timed out` |
| 主机密钥校验失败 | `Host key verification failed`、`REMOTE HOST IDENTIFICATION HAS CHANGED` |
| 远端转发失败 | `remote port forwarding failed` |
| 目标不可达 | 隧道通但目标端口拒绝（属正常日志，不算隧道失败） |

无 ssh-agent 时的 `ssh_get_authentication_socket: Connection refused` 是**无害**提示，不会被误判为失败。

## 故障排查

```bash
sshpf --list-hosts          # 确认主机能被读到
sshpf --check               # 确认配置没写坏
```

- **主机密钥校验失败**：本机 `known_hosts` 与服务端密钥不一致（服务器重装过）。在终端里手动 `ssh <主机>` 确认指纹后处理。
- **认证失败**：先手动 `ssh <主机>` 确认能免密登录，或 `ssh-add` 加载密钥。
- **端口被占用**：详情页会显示占用进程的 `pid/进程名`。
- **ProxyCommand 主机**：这类主机的配置无法烘焙成一次性配置，会自动回退使用你自己的 `~/.ssh/config`（日志里有 warn）。

## 开发

```bash
cargo test                    # 213 单元 + 11 集成
cargo test --test e2e -- --ignored --test-threads=1   # 真实 sshd 端到端
cargo clippy --all-targets    # 零警告
cargo fmt --check             # 格式检查
```

测试分布：

| 模块 | 测试数 | 覆盖重点 |
|---|---|---|
| `keys` | 33 | 两阶段编辑语义、主机下拉、确认键位、未知键忽略 |
| `app` | 54 | 就绪信号、重试退避、确认门控、过滤与选中 |
| `config` | 27 | 缺省值、原子写、0600/0700 权限、字段校验、容错 |
| `ui` | 30 | 1×1 到 40×12 极端尺寸、超长文本、NO_COLOR |
| `model` | 24 | 三种转发 spec、失败码分类、输入缓冲边界 |
| `ssh` | 57 | config 解析（含 Match 污染防护）、known_hosts 匹配、SOCKS5、错误分类 |
| `tunnel` | 10 | 启停、事件顺序、重复启动、停止幂等 |

模块分层（每层只依赖上层）：

```
model   纯数据类型，无 I/O、无 ratatui
ssh     ~/.ssh/config 解析、known_hosts 校验、SOCKS5、russh 客户端
tunnel  隧道线程监管：启动 / 停止 / 事件
config  TOML 持久化（原子写、0600）
app     状态机：持有转发、消费事件
keys    按键分发（从 main 抽出以便单元测试）
ui      ratatui 渲染（纯函数，无副作用）
main    CLI、终端生命周期、信号处理
```

调研依据：`docs/research/pure-rust-ssh-plan.md`（纯 Rust 化的决策与实测）、
`docs/research/pure-rust-ssh-spike.md`（三种转发的端到端验证）、
`docs/research/tech-options.md`（早期技术选型）、`docs/research/prior-art.md`（竞品与交互设计）。

## 已知限制（v1）

- **端口占用反查进程**仅 Linux 可用（依赖 `/proc`）。其它平台该信息显示为空，不影响转发本身。
- 隧道既是 tokio task，杀进程即关闭 socket，会话随之结束——不需要单独的孤儿回收机制；
  macOS 走 Unix 分支。
- 只支持公钥 / ssh-agent 认证，TUI 内不接管密码输入。
- 不统计流量（需要按 channel 累计字节，留待后续）。
- 同一主机多条转发各自建连，未做连接复用（ControlMaster 的 `-O forward` 绑定失败仍返回 exit 0，无法可靠判定成败，故不采用）。

## 许可证

[MIT](LICENSE)

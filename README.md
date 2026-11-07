# sshpf

SSH 端口转发管理器，命令行 TUI 表格界面。Linux 优先（v1），后续可移植 Windows。

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

**SSH 连接交给系统 `ssh`，不自己实现 SSH 协议。** 这样 ProxyJump、IdentityFile、ssh-agent、known_hosts、`Include`、`Host *` 通配等语义原生可用，无需重新实现一遍（这是 `ssh2`/`russh` 方案最难补齐的部分，见 `docs/research/tech-options.md`）。

**主机来自 `~/.ssh/config`，转发条目由本工具管理。** 职责分离：ssh config 负责「怎么连」，sshpf 负责「转发什么」。不会往 `~/.ssh/config` 写入任何内容。

**每条转发 = 一个独立的 `ssh -N` 进程。** 状态清晰、失败原因准确、可独立启停。进程以 `process_group(0)` 启动，停止时 `kill(-pgid)` 连 ProxyJump fork 出的 `ssh -W` 子进程一起收掉；同时设了 `PR_SET_PDEATHSIG`，所以即使 sshpf 被 `SIGKILL` 或 OOM 杀掉，内核也会通知子进程退出，不会留下占着端口的孤儿。

**就绪判定不靠进程存活，靠 ssh 的 debug 输出。** 这是实测踩出来的：

- `-L`/`-D` 认 `Local forwarding listening on …` + `Entering interactive session.`
- `-R` 必须认 `remote forward success for` —— `Entering interactive session.` 在 `-R` 场景下会**先于**远端监听结果打印，只看它会误判成功。

**转发被拒绝时主动杀掉进程。** `ssh -N` 在远端拒绝转发（`-R` 端口被占、`-L` 只绑上一半地址族）后仍然活着：会话是通的，但隧道没用。不处理就会留下「界面显示失败、实际有进程占着端口」的假象。

## 安装

```bash
cargo build --release
install -m755 target/release/sshpf ~/.local/bin/sshpf
```

依赖：Linux + OpenSSH 客户端（`ssh` 在 `PATH` 里）。没有其他运行时依赖。

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

**不存任何密码或密钥口令。** 认证完全交给 ssh-agent / IdentityFile。遇到需要密码的主机，`BatchMode` 会让它立即失败并在表格里显示「认证失败」，提示你先配好免密登录——TUI 不会卡在密码提示上。

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
cargo test                    # 单元 + 集成测试（91 项）
cargo test --test e2e -- --ignored --test-threads=1   # 真实 sshd 端到端
cargo clippy --all-targets    # 零警告
```

模块分层（每层只依赖上层）：

```
model   纯数据类型，无 I/O、无 ratatui
ssh     ~/.ssh/config 发现、ssh 命令构造、stderr 分类
tunnel  进程监管：spawn / stop / 进程组回收
config  TOML 持久化（原子写、0600）
app     状态机：持有转发、消费事件、处理按键
ui      ratatui 渲染（纯函数，无副作用）
main    CLI、终端生命周期、信号处理
```

调研依据：`docs/research/tech-options.md`（技术选型与实测证据）、`docs/research/prior-art.md`（竞品与交互设计）。

## 已知限制（v1）

- 仅 Linux（`/proc` 用于端口占用反查、`kill(-pgid)` 用于进程树回收）。
- 只支持公钥 / ssh-agent 认证，TUI 内不接管密码输入。
- 不统计流量（`ssh -v` 只在退出时打印一次，实时统计需 `/proc/<pid>/io` 汇总，留待后续）。
- 同一主机多条转发各自建连，未做连接复用（ControlMaster 的 `-O forward` 绑定失败仍返回 exit 0，无法可靠判定成败，故不采用）。

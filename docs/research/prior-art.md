# SSH 端口转发 TUI 管理器 · 竞品与设计调研

> 目标读者：v1 实现者。结论优先，证据附链接。调研时间 2026-11。

## 1. 同类工具盘点与功能对照

| 工具 | 形态/语言 | 多条目呈现 | 状态呈现 | 失败原因呈现 | 对本项目可借鉴点 |
|---|---|---|---|---|---|
| [twbot/mole](https://github.com/twbot/mole) | CLI（Rust，封装 autossh） | `mole list` 每行一隧道，`○/●` + uptime + 健康 `✓` | 行内符号 + uptime 倒计时 | 状态只分 up/inactive，错误靠 `mole logs` | 从 `~/.ssh/config` 直接**发现**隧道（零配置）；省略名字时给**模糊选择器**；启动前做端口冲突检测；日志落 `~/.mole/logs/` 可 `-f` 跟随 |
| [davrodpin/mole](https://github.com/davrodpin/mole) | CLI（Go） | 别名 + `show instances` 结构化输出 | 仅进程存活 / 日志行 | 无专门 UI，日志字段化（`level=info msg=…`） | **自动挑空闲本地端口**（`--source` 可省）；**多个 tunnel 复用一条 SSH 连接**；`--detach` + `show logs --follow` 分离"控制面/日志面" |
| [SSH Tunnel Manager](https://justcodepl.github.io/ssh-tunnel-manager/) | GUI（Go+WebView，跨平台） | 一个"隧道"含多条 local→remote 映射 | 托盘图标 + 详情页 | 详情页展示错误 | 数据直接写回 `~/.ssh/config`（元数据用注释），**不锁定私有格式**；导入/导出 `LocalForward`/`RemoteForward`/`DynamicForward` |
| [0fuz/ssh-tunnel-manager](https://github.com/0fuz/ssh-tunnel-manager) | GUI（macOS 菜单栏，Swift） | 分组 + 组级总开关 | 菜单栏红/灰点 | **枚举失败原因**：auth / refused / unreachable / DNS / host-key change / **port in use**；原因保持到重连 | 端口冲突**边输入边校验**并在连接时二次拦截；"失败原因分类"而不是抛原始 stderr |
| [autossh](https://manpages.debian.org/bookworm/autossh/autossh.1.en.html) | CLI（C，无 UI） | 无（一进程一隧道） | 仅退出码/syslog | syslog / `AUTOSSH_LOGFILE` | **启动闸门**（首 30s 失败即退出，避免无限重试掩盖配置错误）；建议配 `ExitOnForwardFailure=yes` 让"转发失败"能变成进程失败 |
| [gost](https://github.com/go-gost/gost) | CLI/配置（Go） | `-L` 多监听，配置文件数组 | 无（前台日志） | stderr | 支持 `-L/-R/-D` 全类型、UDP、链式代理；说明"remote forward 需服务端 `GatewayPorts`"这类**服务端前置条件**必须在错误里点名 |
| [sshuttle](https://github.com/sshuttle/sshuttle) | CLI（Python） | 无条目概念（整段网段透明代理） | 前台日志 | stderr | 反面参照：不是"条目式"管理，说明本项目应坚持**一条目一状态**，不要退化成全局隧道 |
| [Termius](https://docs.termius.com/organize-and-connect-to-hosts/port-forwarding-and-tunneling) / [PortX 类 GUI](https://portuber.github.io/portato/) | GUI | 单页列出所有 port-forwarding rule，双击启停 | 规则行内状态 | 向导式创建，向导内报错 | **所有规则同屏**；创建用**分步向导**（先选类型再填参）；Portato 另有 `space` 启停、`l` 每隧道实时日志、`/` 模糊过滤、`e/n/d` 编辑/新建/删除、`C` 复制并**自动避开已占用端口** |
| [VS Code Remote](https://code.visualstudio.com/docs/remote/ssh) / [Ports 视图](https://code.visualstudio.com/docs/debugtest/port-forwarding) | 编辑器内 | Ports 面板逐条，含 Forwarded Address | 面板行内状态 | 通知/输出通道 | 端口被占时**自动映射到其它端口并明确告知**（"3000 被占用，已映射为 4123"）；`remote.restoreForwardedPorts` 决定**是否恢复上次端口**，默认关闭 |

**共性结论**
1. 成熟工具都做到"**一行一条目 + 一个明确状态** + 可展开的原始日志"三层信息粒度；没有工具把完整 stderr 直接塞进列表行。
2. 被反复验证的 CLI 交互：`space` 启停、`r` 重启、`n/e/d` 增改删、`/` 模糊过滤、`l` 打开该条目日志、`?` 帮助、`q` 退出时**确认**（有活跃隧道时）。
3. 失败原因要**分类 + 短句**（`port in use` / `auth failed` / `host key mismatch` / `dns` / `timeout`），原始报错放详情；端口冲突是最高频失败，必须在**录入时**和**启动时**各拦一次。

## 2. 终端 TUI 的信息架构

**列（按终端 120 列定稿）**

| 列 | 宽度 | 必要性 | 说明 |
|---|---|---|---|
| 光标/标记 | 1 | 必 | `▶` 当前行；`*` 表示有未保存编辑 |
| 名称 | 12–14 | 必 | 唯一、可模糊搜索；缺省用 `host:localport` 生成 |
| 类型 | 1 | 必 | `L` / `R` / `D`，比 `local`/`remote` 省 5 列 |
| 本地地址 | 16 | 必 | `127.0.0.1:8080`；越界截断中间 |
| 远端地址 | 16 | 必 | `D` 类型显示 `(SOCKS5)` 占位 |
| SSH 主机 | 11–14 | 必 | 显示 ssh config 别名（而非解析后 IP），完整值进详情 |
| 状态 | 11–12 | 必 | `● connected` / `◐ connecting` / `◌ retrying` / `✗ failed` / `○ off` |
| 最后错误 | 26–34 | 必（可折叠） | 只放**归类短句**，绝不换行 |
| 时长 | 6 | 可选 | 仅 connected 显示，`02:14 / 1h07m / 3d02h` |
| PID / 连接数 | — | 不进主表 | 放详情面板；进程内多路复用时无独立 PID，对用户易误导，v1 可后置 |

**空间不足时的取舍顺序**：① 丢"时长" → ② 远端/本地合并为 `8080→3000`（同 host）→ ③ SSH 主机缩为 4 字符前缀 → ④ 错误列缩到 18 列只留分类词（PID/连接数本就不在主表）。
**长错误折叠规则**：`短句 ≤ 34 列`；超出用尾部省略 `…` 而**保留开头**（开头通常是 `bind:`/`auth`/`dial` 这类分类词）。完整文本只在两处出现：选中行的底部**一行错误条**（可横向滚动）和右侧/底部**详情面板**的滚动区。

**错误详情交互（推荐组合）**
- **底部日志区（常驻 5–8 行）**：给"当前选中条目"的最近日志，`L` 切级别、`f` 跟随；优点是不打断列表上下文，符合 `portato`/`mole logs` 习惯。
- **右侧详情面板（`enter` 打开，宽 ≥ 100 列）**：完整字段 + 完整错误 + 重试历史，`y` 复制等价 ssh 命令；窄终端降级为全屏弹窗。常规失败**不用阻塞弹窗**（失败是常态，会打断批量操作），弹窗只用于删除确认/退出确认/未知 host key。

## 3. "新增转发"录入流程

**结论：单表单 + 顶部"主机选择"引导，不用多步向导。**
理由：字段总量 5–7 个（名称/类型/本地/远端/主机/可选身份），分步会强制 3 次回车且难以回看；GUI 需要向导是因为字段藏在折叠面板，TUI 一屏放得下。借鉴 Portato 的 `n` 直接出表单 + `tab` 切字段。

```
步骤 0（仅当 ssh config 有 Host 时）：/ 模糊搜索 Host 别名 → 选中后自动填充 host/user/port/identity
步骤 1：表单（单页）  Name  Type(L/R/D)  Local  Remote  SSH Host  [Identity]  [ ] auto-start
        校验实时进行，错误显示在字段下方一行（红色），Enter 提交 / Esc 取消
步骤 2：提交即落盘并立即尝试连接；失败不撤销，行保留为 ✗ failed
```

**Host 选择**：启动时解析 `~/.ssh/config`（含 `Include`，跳过 `Host *` 与 `Match`），取 `Host` 别名列表；用**子序列模糊匹配**（`dbs` 命中 `db-staging`），按"最近使用"排序，最多显示 8 行；允许直接输入 `user@host:port` 跳过别名。

**默认值策略**
- `Type` 默认 `L`（占比最高）。
- 主机默认 = 上次使用的主机 / 唯一主机时直接预填。
- `Remote` 默认 `127.0.0.1:<Remote 端口>`；用户只填端口时自动补 `127.0.0.1:`（同 mole："只给 destination 端口即视为 127.0.0.1"）。
- `Local` 默认：取 `Remote 端口`，若已被占用（含本配置内其它条目 + 系统实际监听）则从 `remote_port` 起 **+1 递增**探测第一个空闲端口，并在字段旁提示"已自动改用 5434"。Portato 的 `C` 复制即用此策略。
- `Name` 默认 `<host>-<local_port>`，冲突加 `-2`。

**必填校验（提交前 + 提交时都跑）**
| 规则 | 处理 |
|---|---|
| 端口 1–65535 整数 | 字段级红字，禁止提交；<1024 额外提示需 `CAP_NET_BIND_SERVICE` 或 root |
| host 非空、不含空格 | 同上 |
| 本地端口在**本配置内**重复 | 阻止提交，提示冲突条目名 |
| 本地端口在**系统内**已监听 | 允许提交但**警示**（可能是别人刚释放）；提交后由真实 bind 决定成败 |
| `R` 类型的远端 bind ≠ 127.0.0.1 | 提示需服务端 `GatewayPorts yes`，默认改为 `127.0.0.1:<port>` |

## 4. "立即生效"的交互语义

**语义**：保存 → 立刻异步连接，**UI 永不阻塞**。新行立即出现在表中，状态 `◐ connecting`，成功后转 `● connected` 并开始计时。

- **状态列是主表达**：`◐ connecting`（≤10s）、`● connected`、`✗ failed`（附短因）、`◌ retrying`（显示下次重试秒数）。
- **底部消息条（单行，非模态）是次表达**：只报"刚刚发生的事"，3–5s 后淡出，如 `✓ db-prod 已连通 (412ms)` / `✗ pg-repo 本地端口被占用`。**不要**用 toast 弹窗（TUI 里 toast 会遮住表格且难以复制文本）。
- **失败条目必须保留在表中**（所有调研工具一致，也是唯一可用做法）：删除会让用户重填；保留则 `e` 改端口、`r` 重试、`C` 复制到空闲端口。行上加 `!` 前缀或在错误列常驻，直到成功或用户显式关闭自动重试。
- **重试策略**：首次失败不自动无限重试。区分两类——配置类错误（auth/host key/port in use/dns）**停止重试**并标 `✗ failed`；网络类（timeout/connection reset）进入 `◌ retrying`，退避 2s→4s→8s…上限 30s（Portato 同款）。`s` 停止、`r` 立即重试。
- **未知 host key**：不静默接受；弹确认框显示指纹，`y` 写入 `known_hosts` 并重连（Portato 的 TOFU 流程）。

## 5. 状态持久化与恢复

**位置**：`$XDG_CONFIG_HOME/sshpf/config.toml`（默认 `~/.config/sshpf/config.toml`），遵循 [XDG Base Directory](https://specifications.freedesktop.org/basedir-spec/latest/)；`--config` 可覆盖。**不要**放 `~/.ssh/`（那里是 OpenSSH 的领地，误写会破坏用户配置，且备份/同步语义不同），**不要**放项目目录（TUI 是全局工具）。
运行时状态另置 `$XDG_STATE_HOME/sshpf/state.toml`（默认 `~/.local/state/sshpf/`）：上次运行时间、每条的 `last_state`、窗口布局。**配置与状态分离**，避免每次启停都重写用户配置（Portato 把 update cache 放 state 目录同理）。

**格式：TOML**。理由：注释友好、层级清晰、Rust 生态 `serde` + `toml` 成熟；JSON 无注释、YAML 缩进易错。参考 boring 的 `~/.boring.toml`、twbot/mole 的 `~/.mole/config.toml`。

```toml
version = 1
[defaults]
bind = "127.0.0.1"          # 全局默认绑回环
restore_on_start = true    # 启动恢复 enabled 条目

[[tunnels]]
name = "db-prod"
type = "local"              # local | remote | dynamic
local  = "127.0.0.1:5432"
remote = "10.0.4.9:5432"
ssh    = "bastion-prod"     # ssh config 别名，不落密码
enabled = true              # "上次是否在运行" → 启动时恢复
```

**"上次运行状态"策略（推荐）**
- 记录 `enabled` 布尔（= 用户意图），**不记录**"当时恰好 connected"。
- 启动时只对 `enabled = true` 的条目发起连接；`enabled = false` 保持 `○ off`。
- 首次安装默认全部 `enabled = false`（Portato 明确"tunnels are disabled by default"），避免开机即弹一串失败；`--no-restore` 可跳过恢复。

**启动时端口已被占用**
1. 启动前对每条做 `TcpListener::bind` 预检（快速、无副作用），记录占用者 PID（Linux 读 `/proc/net/tcp` 或调 `ss -ltnp`）。
2. 被占用 → 该条标 `✗ failed / local bind: port in use (pid N, 进程名)`，**不自动改端口**（改端口会让用户脚本里的连接地址悄悄失效）。
3. 在错误条给出可执行动作：`[e] 改端口` / `[C] 复制到空闲端口` / `[enter] 详情`。
4. 同一端口被两条 `enabled` 条目占用时，启动前就报"配置冲突"，只启动先声明的那条，并高亮另一条。

## 6. 安全注意事项

- **默认绑定 `127.0.0.1`**，与 OpenSSH 一致（`LocalForward` 未写 bind 地址即 loopback）。用户显式填 `0.0.0.0` 时：表单给出黄色警示"将对本机所在网络暴露该端口"，并记录到配置文件注释。远端转发（`-R`）同理：不写 bind 时 sshd 默认 loopback，写 `*`/空才全网卡且需 `GatewayPorts`。
- **配置文件权限 `0600`，目录 `0700`**：创建时用 `OpenOptions::mode(0o600)`（先设 umask 再创建，避免"先 chmod 后写"窗口）；启动时检查，权限过宽则警告并 `chmod` 收紧。参考 OpenSSH 对 `~/.ssh/config` 的严格要求与 Portato 的 socket/token `0600`。
- **绝不存密码/私钥口令**。认证交给 `ssh-agent` + `IdentityFile`；口令仅在内存中短暂持有，或用系统 keyring（v1 可不做）。配置文件里只允许 `ssh = "alias"` 或 `user@host:port`。
- host key 校验默认开启（用 `~/.ssh/known_hosts`），提供显式 TOFU 弹窗，不提供"全局跳过校验"开关（最多给 `--insecure` 一次性参数）。
- 不执行 shell 拼接：优先用 SSH 库（`russh`/`ssh2`）；必须调系统 `ssh` 时参数用数组传。日志屏蔽口令/key/env，错误只留分类 + 目标地址。
- 提权端口（<1024）不自动 sudo，明确报错并给 `CAP_NET_BIND_SERVICE` 指引。

## 7. v1 主界面提案（120×35）

```text
┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│  sshpf 0.1.0  ·  profile: default  ·  ssh-config: 14 hosts  ·  cfg: ~/.config/sshpf/config.toml  (0600)              │
│  ● 4 connected   ◐ 2 connecting   ◌ 1 retrying   ✗ 3 failed   ○ 5 off       filter: none   last: 12:04:31            │
├──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│   NAME         T LOCAL            REMOTE           SSH HOST       STATUS       LAST ERROR (tail)                  UP │
├──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ ▶ api-dev      L 127.0.0.1:8080   127.0.0.1:3000   dev-box        ● connected  —                               02:14 │
│   db-prod      L 127.0.0.1:5432   10.0.4.9:5432    bastion-prod   ● connected  —                               1h07m │
│   redis        L 127.0.0.1:6379   127.0.0.1:6379   dev-box        ● connected  —                               1h07m │
│   metrics      L 0.0.0.0:9090     127.0.0.1:9090   bastion-prod   ◐ connecting dial tcp 10.0.4.9:22: timeout       — │
│   webhook      R 127.0.0.1:3000   0.0.0.0:8443     edge-vps       ◌ retrying   GatewayPorts disabled               — │
│   pg-repo      L 127.0.0.1:5433   127.0.0.1:5432   bastion-prod   ✗ failed     local bind: port in use             — │
│   k8s-api      L 127.0.0.1:6443   10.0.4.20:6443   prod-ops       ✗ failed     host key mismatch                   — │
│   old-jump     L 127.0.0.1:2222   127.0.0.1:22     legacy-host    ○ off        —                                   — │
│   socks-mesh   D 127.0.0.1:1080   (SOCKS5)         bastion-prod   ◐ connecting agent refused key                   — │
│   grafana      L 127.0.0.1:3001   127.0.0.1:3000   dev-box        ● connected  —                               03:41 │
│   mysql-stg    L 127.0.0.1:3306   10.0.9.7:3306    stg-bastion    ○ off        —                                   — │
│   jenkins      L 127.0.0.1:8888   127.0.0.1:8080   ci-runner      ✗ failed     dns: no such host                   — │
│   mq-admin     L 127.0.0.1:15672  127.0.0.1:15672  stg-bastion    ○ off        —                                   — │
│   artifactory  L 127.0.0.1:8081   10.0.9.12:8081   stg-bastion    ○ off        —                                   — │
│   db-read      L 127.0.0.1:5434   10.0.4.11:5432   bastion-prod   ● connected  —                               00:12 │
│   prom-push    R 127.0.0.1:9091   127.0.0.1:9091   edge-vps       ○ off        —                                   — │
├──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│  ✗ pg-repo   local bind 127.0.0.1:5433 failed: address already in use  →  pid 41207 docker-proxy                     │
│    fix: [e] change local port (suggest 5434)   [C] dup on free port   [enter] detail   [l] log                       │
│  ── log · pg-repo ────────────────────────────────────  [all] [warn+] [error]   L: level  f: follow                  │
│    12:04:31 INFO   dialing bastion-prod 10.0.4.9:22 as deploy (-J none)                                              │
│    12:04:31 INFO   host key ok SHA256:8mZ…  auth publickey id_ed25519                                                │
│    12:04:31 INFO   session up in 412 ms; requesting -L 127.0.0.1:5433 → 127.0.0.1:5432                               │
│    12:04:31 ERROR  listen tcp 127.0.0.1:5433: bind: address already in use                                           │
│    12:04:31 WARN   retry #1 in 2s (2s→4s→8s…30s)  ·  s: stop  ·  r: retry now                                        │
├──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│  j/k ↑↓ move   spc start/stop   r restart   n new   e edit   d delete   C duplicate   l logs                         │
│  / filter   a start-all   x stop-all   R reload config   ? help   q quit                                             │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**状态色**：`● connected` 绿、`◐ connecting`/`◌ retrying` 黄、`✗ failed` 红、`○ off` 暗灰（`NO_COLOR` 时退化为符号+文字，不依赖颜色）。**布局规则**：表头与底部快捷键固定；日志区高度 = 终端高 − 表格可见行 − 7，最小 4 行、最大 8 行；窄于 100 列时隐藏日志区、隐藏 SSH HOST 列，宽于 110 列时可在右侧展开详情面板。

## v1 建议（可执行清单）

1. 配置 `~/.config/sshpf/config.toml`（TOML，`0600`）+ 状态 `~/.local/state/sshpf/state.toml`；条目字段 `name/type/local/remote/ssh/enabled`。
2. 表格 8 列：名称、类型、本地、远端、SSH 主机、状态、错误短句、时长；PID/连接数只进详情。
3. `n` 打开**单页表单**，`/` 选 ssh config 别名，本地端口自动避让，提交后异步连接、行保留、底部单行报结果。
4. 错误分两类：配置类不重试标 `✗ failed`，网络类 `◌ retrying` 2s→30s 退避。
5. 默认 `127.0.0.1`；不存密码；启动预检端口占用并给"改端口/复制到空闲端口"动作。

**参考**：[twbot/mole](https://github.com/twbot/mole) · [davrodpin/mole](https://davrodpin.github.io/mole/) · [SSH Tunnel Manager](https://justcodepl.github.io/ssh-tunnel-manager/) · [0fuz/ssh-tunnel-manager](https://github.com/0fuz/ssh-tunnel-manager) · [boring](https://github.com/alebeck/boring) · [Portato](https://github.com/portuber/portato) · [autossh(1)](https://manpages.debian.org/bookworm/autossh/autossh.1.en.html) · [gost](https://github.com/go-gost/gost) · [sshuttle](https://github.com/sshuttle/sshuttle) · [ssh_config(5)](https://man7.org/linux/man-pages/man5/ssh_config.5.html) · [VS Code Remote SSH](https://code.visualstudio.com/docs/remote/ssh) · [VS Code Port Forwarding](https://code.visualstudio.com/docs/debugtest/port-forwarding) · [Termius](https://docs.termius.com/organize-and-connect-to-hosts/port-forwarding-and-tunneling) · [XDG Base Directory](https://specifications.freedesktop.org/basedir-spec/latest/)

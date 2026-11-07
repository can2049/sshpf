# sshpf 设计说明

本文记录 v1 的关键设计决策与其依据。所有结论都有实测支撑，证据在
`docs/research/tech-options.md`（技术选型）与 `docs/research/prior-art.md`（竞品调研）。

## 1. 为什么调用系统 ssh 而不是 SSH 库

| 维度 | 系统 `ssh` 子进程（选用） | `ssh2`（libssh2） | `russh`（纯 Rust） |
|---|---|---|---|
| 复用 `~/.ssh/config` | 原生 100% | 需自己实现语义 | 需自己实现语义 |
| ProxyJump | 原生 | libssh2 无此功能 | 需自写通道链 |
| ssh-agent / known_hosts | 原生 | 需自行接线 | 部分内建 |
| 失败原因 | stderr 原文，最精确 | 枚举 + errno | 枚举 |
| 转发能力 | `-L/-R/-D` 全支持 | 需手写 accept 循环 | 需手写 accept 循环 |
| 新增依赖 | 0 | libssh2-sys + openssl-sys | 203 个包（含 tokio） |

决定性因素是 ProxyJump：本机 `cloud_pc` 就依赖 `ProxyJump cloud_pc_jump`，
而 libssh2 协议层没有这个能力，russh 也需自行实现。加上 v1 只要求 Linux，
系统 ssh 必然存在，所以选它。`russh` 是将来做 Windows 版或需要细粒度流量
统计时的替换路径。

## 2. 合成配置：让 ssh 只做我们要求的转发

用户 `~/.ssh/config` 里可能已有 `LocalForward`。直接 `ssh -L` 会**追加**监听，
导致端口冲突或意料之外的监听。

`-o ClearAllForwardings=yes` 不行——实测它连命令行传的 `-L` 一起清空（0 条监听）。

正确做法：把 `ssh -G` 求值出的完整配置烘焙成一次性 `-F` 配置文件，排除
`localforward/remoteforward/dynamicforward/clearallforwardings/proxycommand`。
实测 `ssh -G` 有 73–74 个键，往返 `ssh -G -F synth` 字节级一致。

### 两个必须处理的坑

1. **ProxyJump 链必须整体烘焙。** ssh 为跳板 fork 的
   `ssh -F <我们的synth> -W … <jumphost>` 读的是同一个 `-F` 文件；只烘焙目标
   主机块会得到 `Could not resolve hostname cloud_pc_jump`。实测烘焙完整链路后
   `Authenticated to … (via proxy)` 且隧道可用。
2. **含空格的值必须加引号回写。** `identityfile ~/my keys/key with space`
   不加引号会被 ssh 静默丢弃；多值键（`userknownhostsfile`/`ciphers`/
   `sendenv` 等）则不能加。按白名单区分。

另外 `ssh -G` 输出里可能出现 `host`/`match`/`include` 这类结构性关键字，
当普通键烘焙会另起 Host 块并丢掉后续所有键，必须排除。

`ProxyCommand` 类主机无法安全烘焙 → 返回 `Err`，`app` 回退到用户真实配置并告警。

## 3. 就绪判定：三种假信号

**不能用进程存活判定。** 认证失败、端口占用都会让 ssh 立刻退出；而 `ssh -N`
在转发被拒绝后**仍然活着**（会话通、隧道无用）。

所以靠 `-v` 的 debug 行，且失败行优先：

| 类型 | 成功标记 | 必须先排除的失败标记 |
|---|---|---|
| `-L`/`-D` | `Local forwarding listening on …` → `Entering interactive session.` | `bind …: Address already in use` / `Could not request local forwarding.` |
| `-R` | `remote forward success for` | `remote forward failure for` / `Error: remote port forwarding failed` |

三个实测出来的假信号：

1. **`-L` 双栈半失败。** 不显式写 bind 地址时 ssh 会同时试 `::1` 和
   `127.0.0.1`；只占用了 IPv4 时 ssh 只报一行 bind 错误却继续运行。
   对策：始终显式写 `-L 127.0.0.1:…`，并把 bind 失败行当作终止信号。
2. **ProxyJump 子进程的假就绪。** 跳板子进程自己的
   `Entering interactive session.` 会合并进父进程 stderr，且在目标认证**之前**。
   对策：`-L`/`-D` 要求先见过 `Local forwarding listening on` 才接受 ready。
3. **`-R` 的顺序反了。** 实测 `-R` 场景下 `Entering interactive session.`
   （第 45 行）早于 `remote forward success for`（第 50 行）——若只看
   `Entering` 会把失败的 `-R` 标成成功。对策：`-R` 只认
   `remote forward success for`。

## 4. 进程生命周期

- `Command::process_group(0)` 让 ssh 独立成组；ProxyJump 的 `ssh -W` 子进程
  与父进程同 PGID。停止时 `kill(-pgid, SIGTERM)` 一次收全树，实测有效。
- 必须 `wait()` 回收，否则留僵尸。SIGTERM 后给 2s 宽限再 SIGKILL。
- 信号处理只置 flag，主循环正常 break 退出，让 `Drop` 跑清理。
  `std::process::exit()` **不会**执行 `Drop`，实测确认。
- **孤儿防护**：`SIGKILL`/OOM 时没有任何析构函数会运行，子进程会变成孤儿
  并永久占住监听端口（实测：主进程 SIGKILL 后 ssh 仍持有端口）。因此子进程
  以 `PR_SET_PDEATHSIG = SIGTERM` 启动，父进程无论怎么死，内核都会通知它们。
  修复后实测：SIGKILL 主进程，端口在 0.4s 内释放。
  `pre_exec` 里同时校验 `getppid()`，堵住 fork 与 prctl 之间父进程已死的竞态。
- 退出时删除生成的临时配置目录；启动时清扫 pid 已消失的陈旧目录（崩溃残留）。

## 5. 重试策略

失败分两类：

- **配置类**（端口占用、权限、认证、主机密钥、远端拒绝）：重试无用，标
  `✗ 失败` 并停下。
- **网络类**（连接拒绝、超时、DNS、进程异常退出）：`◌ 重试`，指数退避
  2s→4s→…→30s，尊重 `auto_restart` 开关。

`arm_retry` 在启动时插入的占位记录 `attempt = 0`，`tick_retries` 只处理
`attempt >= 1` 的记录——否则占位符会让程序在启动瞬间陷入无限重连。
这是集成测试发现并修复的真实缺陷。

## 6. 配置与状态的分离

- `~/.config/sshpf/config.toml`：用户意图（转发定义、`enabled`），可手改，
  原子写（temp + rename），`0600`，目录 `0700`。
- `~/.local/state/sshpf/state.toml`：上次运行时间与每条的末次状态，纯诊断用。
  启停转发不重写用户配置之外的东西。

放 `~/.ssh/` 是错的（那是 OpenSSH 的领地，误写会破坏用户配置）；放项目目录
也是错的（TUI 是全局工具）。

## 7. 并发模型

不用 tokio。主线程 `crossterm::event::poll(200ms)` 阻塞等键，每帧非阻塞地：

1. `try_recv` 排空 stderr 读线程发来的行；
2. `try_wait` 回收已退出的子进程；
3. 处理到期的重试；
4. `terminal.draw()`。

每条转发一个 stderr 读线程 + `mpsc`。UI 永远不会阻塞在某个慢主机的输出上。

## 8. 主机选择：内嵌下拉框而非独立弹窗

新增转发时最常做的第一件事是选一台主机，所以「从 `~/.ssh/config` 选主机」必须是
**看得见的入口**，不能藏在快捷键后面。

v1 初版是 `ctrl+h` 打开一个居中模态框，实际使用中很难被发现。改成：

- **SSH 主机字段本身就是下拉框**，右侧有 `▾`（展开时 `▴`），空值时显示
  `(N台可选)`，让用户一眼看到有列表可挑。
- 展开后在**表单内部**紧贴该字段下方渲染列表，不另开模态窗口，上下文不丢。
- `enter`/`↓` 展开（不是在主机字段上直接提交表单），`esc` 收起不改值，
  `tab` 切字段时自动收起。
- 输入即过滤；别名优先匹配，别名无命中时才回退到 `user@host:port` 匹配——
  否则输入 `de` 会因为某台机器的用户名是 `deploy` 而列出 `bastion-prod`，
  看起来像 bug。
- 选中后填别名，名称留空时同步填别名；**字段一旦有值就不再显示 `(N台可选)`**，
  否则选中的主机名会被提示挤出去，用户无法确认自己选了什么。

实现上是个 8 行的滚动窗口（`HOST_DROPDOWN_ROWS`），键处理与渲染共用同一个常量，
避免两边算出的可见行数不一致。

## 9. 表单两阶段编辑与本地端口推荐

### 两阶段编辑

方向键在表单里是最常按的键。如果按一下就改动值，用户只是想把光标挪到下一个
字段却会改变类型或勾选状态，很挫败。所以表单区分两种模式：

- `EditMode::Navigating`（默认）：`↑↓`/`tab` 只移动高亮
- `EditMode::Editing`：在文本字段上按 `enter` 才进入，此时才接收输入；
  `enter`/`esc` 退出编辑，`tab` 退出并前进

视觉上用三档区分，避免用户不知道自己处在哪种状态：编辑态是**实心选中条**，
仅高亮是**加粗强调色**，其余是普通色。光标只在编辑态出现。

枚举/开关类字段（类型、自动重连）不需要进入编辑态，`enter`/`←`/`→`/空格直接操作。

### 本地端口推荐

填远端端口后自动推导本地端口，但**只是建议**，用户始终可以改。规则：

1. 远端端口 ≥1024 且空闲 → 用同一数值（最符合直觉）
2. 远端端口 <1024 → 用 `remote + 2200` 的惯用别名（`22 → 2222`）。
   这里有个实测踩到的坑：低位端口绑定需要 root，`port_available` 会把
   **所有** <1024 端口都报告为不可用，若照搬「从远端端口开始向上扫」，
   扫描预算会在特权区间里全部耗尽，最后只能报「找不到可用端口」。
3. 端口被系统占用 → 向上顺延
4. 端口被其它转发条目占用 → 跳过。这一条不能只靠 OS 探测：
   已停用的转发并不在监听，但它一旦启动就会冲突。
5. 用户手改过本地端口 → `local_port_auto = false`，之后不再自动覆盖
6. 编辑既有转发 → 同样不自动推导，保留原值

### 字段顺序

顺序改为 `SSH 主机 → 名称 → 类型 → 远端地址 → 远端端口 → 本地地址 → 本地端口 → 自动重连`。
理由：远端是用户知道的目标（「我要访问 8080」），本地是派生出来的落地端口，
所以远端排在本地之前，填完远端就能立刻看到推荐值。

## 10. 风险操作确认

### 什么需要确认

判断标准是三条，命中任意一条就要拦：

1. **会丢数据** —— 删除条目、放弃未保存的表单输入
2. **会断连接** —— 停用、重启、批量停用、保存编辑（编辑会重建进程）
3. **事后不易察觉** —— 批量启用会一次开出很多监听端口

反过来，可逆且无副作用的操作**刻意不拦**：移动光标、过滤、看详情、启动一条
未启用的转发、新建条目。给这些加确认会让高频操作变烦，而用户一旦习惯无脑按
`y`，真正的危险确认就失效了 —— 这是确认框设计里最容易犯的错。

### 实现

`ConfirmAction` 枚举穷举所有受保护操作，`App::ask_confirm()` 统一生成提示文案。
把「哪些操作受保护」收敛到一个枚举里，好处是新增操作时编译器会强制你想清楚它
属于哪一类，而不是散落在十几个按键分支里各写一遍。

表单保存拆成两步，这是本次改动最关键的一点：

- `validated_form_config()` —— 只校验，不落盘
- `commit_form_create()` —— 新建，直接保存（增量操作）
- `commit_form_edit()` —— 编辑，**等确认后**才停进程、写配置

待保存的配置暂存在 `App.pending_form` 里，所以用户确认前运行中的进程不会被打断。
如果先停进程再弹窗，取消就会莫名其妙地把用户的连接断掉。

### 提示文案

提示由 `ask_confirm` 按 action 与当前状态生成，能带上具体上下文：

- 单条操作带条目名（`确认停用「db-prod」？`）
- 批量操作带数量（`确认停用全部 2 条运行中的转发？`）
- 退出带运行中数量，没有在跑时不提「停止」

对话框高度按文案宽度自适应（提示可能折行），避免出现大片空白。

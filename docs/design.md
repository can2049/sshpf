# sshpf 设计说明

本文记录关键设计决策与其依据。所有结论都有实测支撑，证据在
`docs/research/pure-rust-ssh-plan.md`（纯 Rust 化决策）、
`docs/research/pure-rust-ssh-spike.md`（三种转发端到端验证）、
`docs/research/pure-rust-ssh-russh.md`（russh API 核实）、
`docs/research/pure-rust-ssh-config.md`（config/密钥/agent 生态）、
以及更早的 `docs/research/tech-options.md`、`docs/research/prior-art.md`。

## 1. 为什么用 russh 而不是调用系统 ssh

v1 曾经用 `std::process::Command` 调系统 `ssh`，理由是「ProxyJump / IdentityFile /
ssh-agent / known_hosts 的语义太复杂，重写容易漏」。这个判断在当时成立，但代价是
**Linux 和 Windows 行为不一致**（Windows 上要用户自装 OpenSSH），而且失败原因只能
靠正则解析 stderr 文本。

换成 `russh` 后实测（`docs/research/pure-rust-ssh-spike.md`）：

| 维度 | 系统 `ssh` 子进程（旧） | `russh`（现在） |
|---|---|---|
| 跨平台一致性 | 依赖用户装 OpenSSH | **同一套代码路径** |
| `-L` | ✓ | ✓ 本地 `TcpListener` + `direct-tcpip` |
| `-R` | ✓ | ✓ `tcpip_forward` + `forwarded-tcpip` 回调 |
| `-D` | ✓ | ✓ 自写 SOCKS5（约 150 行） |
| ProxyJump | 原生 | ✓ `connect_stream` 套 channel（实测三跳） |
| ssh-agent | 原生 | ✓ Unix socket / Windows named pipe |
| 失败原因 | stderr 正则，脆弱 | **结构化 `russh::Error`** |
| 就绪判定 | 解析 debug 日志 | **真实 bind / 协议回复** |
| 运行时依赖 | `ssh` 二进制 | **零** |
| 二进制体积 | 1.63 MB | 4.34 MB |

决定性收益不是"少一个依赖"，而是**三类实测过的误判 bug 从根上消失**（见 §3）。

## 2. `~/.ssh/config` 由 `ssh2-config` 解析

不再调用 `ssh -G`。`ssh2-config` 0.9.0 是纯 Rust、零 C 依赖（crate 名里的 "ssh2"
只是历史原因，`cargo tree` 里没有 `libssh2-sys`），覆盖 `Include`、`Host` 多模式
与通配、否定、`Host *` 继承、`ProxyJump`、`IdentityFile`。

### 必须自己兜住的一个上游 bug

`ssh2-config` 的 `Field` 枚举里**没有 `Match`**。`Match` 行走
`UnknownField` 分支被忽略，**但它后面的选项会继续写进上一个 `Host` 块**：

```text
Host web1  { HostName 10.0.0.1; User alice }
Match host foo { User ignored; Port 9999 }

ssh2-config query(web1) -> user="ignored" port=9999   ← 静默错误
ssh -G -F ...        web1 -> user=alice    port=22     ← 正确
```

静态错误比崩溃更危险：用户看到的是错误的 user/port 且没有报错。
`ssh::config::strip_match_blocks` 在解析前剔除 `Match` 块，并有单测固定这个行为。

另外两处缺口也由薄封装补上：`HostName` 未设置时回退到 alias（OpenSSH 语义），
以及 `IdentityFile` 的 `~` / `%d` 展开。

**不再需要合成配置。** 旧方案要把 `ssh -G` 的 73 个键烘焙成一次性 `-F` 文件，
并处理 `ProxyJump` 链整体烘焙、含空格值加引号等坑。纯 Rust 里我们直接把
`HostName`/`Port`/`User` 交给 `connect()`，`~/.ssh/config` 里的
`LocalForward` 也不会被带进来——因为根本不会去读它。

## 3. 就绪判定：从"猜日志"变成"看结果"

**旧实现**靠解析 ssh 的 debug 输出，有三类实测过的假信号：

1. `-L` 双栈半失败：一个地址族绑上了，另一个报错，但 `Entering interactive
   session.` 照常打印。
2. **ProxyJump 子进程的假就绪**：跳板 fork 出的子 ssh 自己也会打印
   `Entering interactive session.`，混进我们的 stderr。
3. `-R` 顺序倒置：`Entering interactive session.` **早于**真正的
   `remote forward success for`，只看前者会误判成功。

**现在**就绪是二值的、来自真实结果：

- `-L` / `-D`：`TcpListener::bind` 返回 `Ok` 才算就绪；
- `-R`：`tcpip_forward()` await 到服务端对 `GLOBAL_REQUEST` 的回复才算就绪。

`TunnelEvent::Ready` 是通往 `TunnelStatus::Up` 的**唯一**路径，`app::on_ready`
不读任何日志文本。三类假信号在结构上不可能再发生。

## 4. 生命周期：tokio task 取代子进程

旧实现要处理进程组、`PR_SET_PDEATHSIG`、Windows Job Object，因为子进程会
在父进程被杀后变成孤儿并占住端口。

现在每条隧道是**一个后台线程 + 一个 `current_thread` tokio runtime**：

- 停止 = 置停止标志 → `select!` 立刻返回 → runtime 丢弃 → socket 关闭 → 会话结束；
- 端口释放实测 **172 µs**；
- `SIGKILL` sshpf 也不会留孤儿——没有子进程，socket 由内核关闭；
- 30 次启停循环 **无 fd 泄漏**。

因此 `sys` 层从两个 200 行的平台文件缩减到只剩文件权限与信号两类函数
（`src/sys/unix.rs` 55 行 / `src/sys/windows.rs` 40 行）。

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

**UI 线程保持同步。** 主线程 `crossterm::event::poll(200ms)` 阻塞等键，每帧非阻塞地：

1. `try_recv` 排空隧道线程发来的事件；
2. 处理到期的重试；
3. `terminal.draw()`。

**每条隧道一个后台线程 + 一个 `current_thread` tokio runtime**，事件通过
`std::sync::mpsc` 回灌。UI 永远不会阻塞在某个慢主机的握手或数据拷贝上。

为什么不用共享 runtime：实测 10 条隧道并发建立仅 80 ms（单条约 8 ms 摊薄），
本项目规模（几条到几十条）下"一线程一 runtime"完全够用，且隔离性更好——
一条隧道 panic 不影响其它隧道。

`russh::client::Handle` **不是 `Clone`**，所以单条隧道内的多个 task 共享它必须
用 `Arc<Handle>`。

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

## 11. 测试策略

### 为什么把按键分发搬出 main.rs

`main.rs` 原本承载了约 530 行按键处理（每个 overlay 一个 handler）。二进制
crate 里的私有函数无法被 `tests/` 下的集成测试调用，只能靠 pty 驱动真实 TUI
来验证 —— 慢、脆、且抓不到具体分支。

现在这些 handler 在 `src/keys.rs`（lib 内），签名统一是
`fn(app: &mut App, key: KeyEvent)`，本身无副作用，于是可以像纯函数一样测：
构造 `App`（`App::new` 不 spawn 任何进程）、喂一个 `KeyEvent`、断言 `App` 状态。
`main.rs` 只剩终端生命周期与事件循环。

### 覆盖重点

优先测「错了会静默出问题」的地方，而不是追求行覆盖率：

- **时序类**：退出事件与最后几行日志可能乱序，因此失败原因随
  `TunnelEvent::Exited` 一起传递。测试直接调 `on_exit` / `on_line` 验证迁移。
- **判定类**：`TunnelStatus::Up` 只能经由 `TunnelEvent::Ready` 到达
  （`on_ready`），任何日志文本都不得改变状态。这是最容易被"优化"掉的约束。
- **门控类**：确认框必须在动作之前弹出，且确认前不得产生任何副作用。
  表驱动穷举 6 类风险操作，逐项断言。
- **边界类**：1×1 终端、4000 字符的行、CJK 宽度、空配置、损坏的状态文件、
  越界的选中索引。这些在真实使用中一定会遇到，且往往直接 panic。

### 变异测试验证有效性

补齐测试后，故意改坏关键逻辑，确认测试会变红。以下变异全部被捕获：

| 变异 | 捕获它的测试 |
|---|---|
| 交换 `-R` 的 forward_spec 参数顺序 | `forward_spec_for_each_kind` |
| 配置权限从 0600 改成 0644（两处都改） | `save_is_0600` |
| 就绪改由日志文本触发而非 `Ready` 事件 | `happy_path_starting_to_up_is_driven_by_the_driver` |
| 停用不弹确认直接执行 | `all_risky_operations_ask_first` + `space_stops_only_after_confirmation` |
| 删除不弹确认直接执行 | 4 个测试同时失败 |
| 让 `clamp_selection` 失效 | `selection_survives_a_filter_that_shrinks_the_list` |

变异测试也纠正了一个误解：早期一次"变异存活"其实是我的 `sed` 没匹配上，
用 Python 精确替换后才真正生效。**验证测试本身有效，比增加测试数量更重要。**

### 补测过程中发现的真实缺陷

1. **表单导航态按 `esc` 静默丢失输入** —— 上一轮加确认时只改了编辑态分支，
   导航态仍直接 `close_overlay()`。这个分支恰好是用户按一次 `esc` 就会走到的。
2. **过滤后 `selected` 越界** —— 收窄过滤条件后索引可能指向不存在的行，
   而 `select_next` 只在"未越界"时前进，导致选中行消失、操作落空。
   修法是加 `clamp_selection()` 并在移动前先夹紧。

## 12. Windows 支持

### 纯 Rust SSH 让平台差异几乎消失

v1（系统 `ssh` 子进程）时 Windows 需要一整套等价机制：Job Object 替代进程组、
`KILL_ON_JOB_CLOSE` 替代 `PR_SET_PDEATHSIG`，两个平台文件各 200 行。

改用 `russh` 后**子进程不存在了**，进程树回收问题自然消失。`sys` 层剩下的差异只有：

| 关注点 | Unix | Windows |
|---|---|---|
| 文件私密性 | `chmod 0600/0700` | 用户配置文件 ACL 继承（无需操作）|
| 退出信号 | `SIGINT`/`SIGTERM`/`SIGHUP` | Ctrl-C 映射的 `SIGINT`/`SIGTERM` |

两个平台文件合计不到 100 行，且 **SSH 相关代码零平台分支**——这正是
"跨平台可靠一致性"的实质收益。

### 三个 Windows 专属的实测结论

1. **`ring` 加密后端是必须的。** `russh` 默认的 `aws-lc-rs` 在 Windows x86-64
   构建时需要 NASM 和 CMake。同一句 `cargo check --target x86_64-pc-windows-gnu`
   的对比实验里，`aws-lc` 请求了 `59×gcc / 2×NASM / 3×CMake`，
   `ring` 只请求 `9×gcc`。所以 `Cargo.toml` 显式写
   `default-features = false, features = ["ring", "flate2", "rsa"]`。
   CI 的 `windows-latest` 预装 MSVC，因此无需额外安装步骤。
   注意这是**构建期**依赖，产物仍自包含。

2. **不再需要假 ssh 夹具。** v1 曾用 `rustc` 编译一个假 ssh 可执行文件
   （因为 `/bin/sh` 脚本在 Windows 上无法执行，`os error 193`）。
   现在测试直接对着真实 SSH 协议跑，夹具问题整体消失。

3. **本地端口反查进程仅 Linux 可用**，Windows 上返回空——不影响转发本身。

### 分支保护

`main` 开启了保护，合并必须满足：

* 4 个 CI 检查全绿（`strict: true`，即分支落后也要先 rebase）
* 禁止强推、禁止删除分支
* `enforce_admins: true` —— 管理员也不能绕过
* 必需 review 批准为 0（单人仓库无法自我批准；实测 `mergeStateStatus`
  会因此卡在 `BLOCKED`）

实测验证过：CI 失败时 `gh pr merge` 被拒绝。

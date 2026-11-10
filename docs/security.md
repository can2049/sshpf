# 安全与健壮性 / Security & robustness

> **这份文档回答**：为什么生产代码里不许 panic、`unsafe` 只在哪里、崩溃怎么被记录下来、文件权限与日志脱敏的边界、主机密钥策略、哪些测试在守这些规则。
> **不负责**：日志格式与级别（见 [logging.md](logging.md)）、事件字段全表（见 [logging/events.md](logging/events.md)）。
> **相关代码路径**：`src/logging/crash.rs`（唯一 `unsafe`）、`src/logging/panic.rs`、`src/sys/`、`src/ssh/known_hosts.rs`、`src/ssh/hostkey_conf.rs`、`tests/safety_guards.rs`、`tests/panic_hardening.rs`。

## 政策 / Policy（全员硬约束）

| 规则 | 内容 |
|---|---|
| 不许新增 `.unwrap()` / `.expect()` | 生产代码（`#[cfg(test)]` 之外）一律不许；测试里是**好断言**，豁免 |
| 不许新增 `panic!` / `unreachable!` / `todo!` / `unimplemented!` | 同上 |
| `unsafe` 只在 `src/logging/crash.rs` | 每个 `unsafe` 块必须有 `// SAFETY:` 一句话理由 |
| 不许用 `#[allow(...)]` 压 clippy | 有警告就改代码 |
| 失败路径必须可观测 | 返回 `Result`/`Option` → 调用方记日志 → 界面出**可读**状态条（`StatusMsg`），**不许静默** |

这些不是评审口径，是**可执行断言**：`tests/safety_guards.rs` 逐文件扫描源码（注释与字符串先涂空，`#[cfg(test)]` 区域先屏蔽），命中即失败并打印文件名 + 行号 + 构造。豁免只能写进该测试的 allowlist 常量并写明理由——**不是** `#[allow]`。

## 核心不变式 / The one invariant

> **边界之外是任意的，边界之内不许 panic；出问题就降级，降级必须留痕。**

边界外的一切都不由我们控制：`~/.ssh/config` 是人写的文本（可能中文注释、非 UTF-8、恶意构造），`config.toml` 允许手改，环境变量是 shell 里的任意字节，`stdin`/`stdout` 可能根本不是终端。

## 三层防御 / Three layers

### 第一层：输入解析一律不 panic

解析器面对的是**字节**，不是"我们已经相信的字符串"。两个真实崩过的例子：

| 缺陷 | 崩法 | 修法 |
|---|---|---|
| `strip_match_blocks` 用 `trimmed[..5]` 比 `Match` | 首行中文注释时第 5 字节落在 3 字节字符中间 → `byte index 5 is not a char boundary` | 按**字节**比较（`as_bytes().get(..5)`），这也正是 OpenSSH 的 ASCII 关键字语义 |
| `std::env::args()` 每参数 `unwrap()` 成 UTF-8 | 非 UTF-8 路径在非 UTF-8 locale 下合法 → panic | 用 `args_os()`；路径按字节保留，作**参数名**时才拒绝 |

同类替换贯穿全部边界：`print!`/`println!`（写失败即 panic）→ 显式句柄的 `write_all`；`ratatui::init()`（内部 `expect`）→ `try_init()` + 正常 `app.fatal` 通路；所有 u16 算术先扩宽到 `u32`、`try_from` 收口；`max_id() + 1` → `saturating_add`。

### 第二层：只剩不变量可依赖的地方，降级而不是断言

有些下标"逻辑上一定安全"，但这个"一定"依赖 ratatui 的 layout 契约或某个 `len()` 与常量保持同步——两者都可能在下一次依赖升级或重构里悄悄失效，代价是整帧 panic（终端停在 raw mode，界面全无）。改成 `get()` + 安全默认值：

| 位置 | 降级行为 | 记录 |
|---|---|---|
| 渲染层拿不到 layout 分区 | 这一块这一帧不画 | `ui.render_degraded reason=layout_band_missing` |
| 行号 + 表头顶出 `u16` | 视为屏幕外，不进命中图 | `ui.render_degraded reason=row_overflow` |
| 文本宽度超 `u16` | 截断渲染 | `ui.render_degraded reason=width_overflow` |
| `ForwardKind::all()[i]` 越界 | 回落默认类型（`-L`） | `form.kind_index_invalid` |
| 确认框的目标隧道已被删 | 对话框照常开（不卡住用户），渲染时没有名字 | `action.target_missing` |
| `u64` id 空间耗尽 | 拒绝新建，不复用已占用的 id | `app.id_space_exhausted` |

**每次降级都写一条记录**，且**每帧都可能触发**的路径必须**限流**（每进程每处只记一次）——否则一次 resize 就能把日志刷满。

### 第三层：日志系统自己是最后防线

前两层保证"别的地方不 panic"，但崩溃捕获机制本身也可能挂，而它挂了就什么都没了。三个约束：① **panic hook 必须自安全**（见下节"重入"）；② **信号处理器只做 async-signal-safe 调用**（不分配、不加锁、不用格式化机制）；③ **线程 panic 不传染进程**（`catch_unwind` 把隧道线程 panic 转成 `Event::Exited{code:101}` + `tunnel.thread_panic`）。

这一层只保"记录得下来"，**不保"程序活着"**——进程该崩还是崩，退出码和 core dump 行为不变。把崩溃静默掉才是更坏的选择。

## 崩溃捕获 / Crash capture

两条独立通路，覆盖不同的崩法。每条的记录格式见 [logging.md](logging.md#崩溃记录--crash-records)。

### panic

`set_panic_hook` 链式调用上一个 hook（所以 `ratatui` 恢复终端照常生效），写 `crash.panic` + `crash.panic.trace`。`thread` 字段区分"UI 挂了"与"某条隧道挂了"；栈只保留前 25 帧（debug 栈顶几乎全是 `std::panicking` 的管道代码）。

### 致命信号（`SIGSEGV` / `SIGBUS` / `SIGABRT` / `SIGILL` / `SIGFPE`）

**仅 Unix。** Windows 没有 `sigaction`，`src/logging/crash.rs` 的 `imp` 在 `#[cfg(not(unix))]` 下是一组同签名的空实现（`install` / `adopt` / `release` / `set_tui_active` / `emergency`）。三个后果，都是设计上接受的：

| 缺失 | 后果 |
|---|---|
| 无 `crash.signal` | 硬故障（访问违例、栈溢出）**不产生任何 sshpf 日志行**，只留 Windows 自己的 WER 事件日志 |
| `emergency()` 空实现 | 日志重入（`crash.log_reentrant`）只剩 stderr 一行，文件里没有痕迹 |
| `set_tui_active` 空实现 | 崩溃后终端停在备用屏，用户要自己 `chcp`/重开窗口，不会看到 Unix 那句 "请执行 reset" |

Rust 层面的 panic **仍然被记录**（走 `crash.panic`，与平台无关）。

`src/logging/crash.rs` 用 `sigaction` 装 handler，只做四件事：① TUI 持有终端时先写转义序列退出备用屏；② `write(2)` 一条**手工拼好**的记录到预先 `dup` 出的 fd；③ 往 stderr 写一行**启动时就拼好**的提示；④ 恢复默认处理器 → **解除内核对该信号的屏蔽** → `raise`。

第 ④ 步最容易漏：内核在 handler 执行期间屏蔽该信号，不解除则 `raise` 只排队，`_exit(139)` 抢先执行，进程从"被 SIGSEGV 杀死"变成"以 139 退出"，**core dump 也不产生**（`a_fatal_signal_keeps_the_signal_exit_status` 钉住它）。代价：这条记录**没有格式化时间戳**，只有 `ts=<epoch 秒>`；递归崩溃由 `SA_RESETHAND` 兜住。

### 重入：`crash.log_reentrant`

panic hook 要写日志，写日志要拿锁。**如果 panic 恰好发生在持锁的写入过程中**，hook 再 `lock()` 就是自己等自己——进程永久僵住、日志 0 字节、终端停在备用屏，比崩溃更难查。所以不靠 `try_lock()`，而是**事先知道自己在不在临界区**：thread-local 哨兵（`CriticalSection::enter()`，RAII `Drop` 复位，panic 展开也会复位）。已在本线程临界区内就跳过这条常规记录，改用预 `dup` 的 fd 裸 `write(2)` 写一行 `crash.log_reentrant`，然后让原 panic 照常传播。

**为什么不能用 `try_lock()`**：它区分不了"拿不到锁（跨线程竞争）"和"自己在里面（重入）"，会把正常竞争误判成重入而丢记录。标记法只识别真正的重入。**宁可少一条记录，不能不死。**

### `unsafe` 的 SAFETY CONTRACT

`src/logging/crash.rs` 是全仓唯一 `unsafe` 文件，文件头有 SAFETY CONTRACT 段：每个 `unsafe` 块一句话理由、只允许 async-signal-safe 调用、不得分配/加锁。它的单元测试就是这些 `unsafe` 的**看护证据**：`build_signal_record` 的格式/截断/边界（`i32::MIN`）、`push_int` 边界、`signal_name` 全覆盖。

`src/sys/mod.rs` 提供 `pub fn signal_name(sig: i32) -> String`；`logging::crash` 里给 handler 用的 `fn signal_name(..) -> &'static str` 保留，注释说明**为什么不能复用**（handler 内禁止分配）。

## 权限 / File permissions

路径与权限**都按平台取**（`dirs` 解析）：

| 内容 | Linux 路径 | Windows 路径 | Linux 权限 | Windows |
|---|---|---|---|---|
| 配置 | `~/.config/sshpf/config.toml` | `%APPDATA%\sshpf\config.toml` | `0600`，目录 `0700` | 无 mode bit，靠 ACL |
| 运行状态 | `~/.local/state/sshpf/state.toml` | `%LOCALAPPDATA%\sshpf\state.toml` | `0600` | 无 mode bit，靠 ACL |
| 日志 | `~/.local/state/sshpf/logs/sshpf.log` | `%LOCALAPPDATA%\sshpf\logs\sshpf.log` | `0600`，目录 `0700` | 无 mode bit，靠 ACL |

- 载入时若配置对同组/所有人可读，收紧到 `0600` 并记 `config.perms_tightened`（`WARN`，`ok=false` 时 `ERROR`）。
- **Windows 没有 mode bit**，走**用户配置文件目录继承的 ACL**（实测为 `SYSTEM` / `Administrators` / 本人，全部继承自 profile），所以 `src/sys/windows.rs` 里 `restrict_file` / `restrict_dir` 是空实现、`perms_too_open` **恒返回 `false`**。后果有两个，都是设计使然：Windows 上不会出现 `config.perms_tightened` 事件，也不会因为"权限过宽"提示用户。
- 为什么 Windows 的 state 落在 `%LOCALAPPDATA%` 而不是 `%APPDATA%`：`dirs::state_dir()` 在 Windows 恒返回 `None`，`Paths::resolve` 用 `.or_else(dirs::data_local_dir)` 兜底（`src/config.rs`）。

## 日志脱敏 / Redaction

| 写 | 不写 |
|---|---|
| 主机名、端口、用户名 | 私钥内容 |
| 失败原因、路径 | passphrase |
| 版本、pid、`os`/`arch` | `known_hosts` 内容 |
| 栈（背压截断到 25 帧） | 任何从 agent 读出的密钥材料 |

- 认证只走 `~/.ssh/config` 的 `IdentityFile` 或 ssh-agent，**不存任何密码或口令**。
- `SSHPF_LOG_LEVEL=off` 只关常规记录，**崩溃记录无条件写**。
- 主机名和用户名仍属敏感信息：报 bug 前可先 `sed -E 's/(host|name)=[^ ]*/\1=REDACTED/g'`。

## 主机密钥 / Host keys

`src/ssh/known_hosts.rs` 自写匹配（`russh` 自带的有四个实测缺陷，其中最严重的是 `@revoked` 与正常条目并存时**会放行被吊销的密钥**）。

| 策略 | 未知主机 | 密钥变了 | 被吊销 |
|---|---|---|---|
| `accept-new`（默认） | 接受并写入 `known_hosts`，记 `knownhosts.learned`（WARN） | **拒绝** | **拒绝** |
| `strict` | 拒绝 | 拒绝 | 拒绝 |
| `ask` | 接受（当前实现） | 拒绝 | 拒绝 |
| `ignore`（`--ignore-host-key-changes`） | 接受，**不写入** `known_hosts`（故不记 `knownhosts.learned`） | **接受**，记 `knownhosts.ignored_changed`（WARN，含 SHA256 指纹） | **拒绝** |

密钥被拒时界面明确显示"已改变"还是"已吊销"，并指向 `known_hosts` 的行号；日志记 `knownhosts.rejected`（ERROR，含 `policy`）。

**条目按「主机 + 算法」匹配，与 OpenSSH 一致。** 一台主机通常同时提供 `ssh-ed25519`、`ecdsa-sha2-nistp256`、`ssh-rsa` 几种密钥，而 `known_hosts` 里可能只记了其中一种；协商到另一种**不算变更**，只是该算法尚未记录（按未知处理：`accept-new` 学到并追加一行，`strict` 拒绝）。只有**同一算法下出现不同密钥**才是 `Changed`。这条区分修的是一个极常见的误报：此前任何不匹配的条目都被记为「已改变」，于是"服务器提供多算法、`known_hosts` 只记了一种"会被误报成「可能存在中间人攻击」。

### 忽略密钥变更 / Ignoring key changes

两种入口，语义一致（都映射到 `HostKeyPolicy::Ignore`），区别只在作用域：

| 入口 | 作用域 | 何时用 |
|---|---|---|
| `--ignore-host-key-changes` | **全局**（等价 `ssh -o StrictHostKeyChecking=no`） | 临时排障；全局优先，**不会被更严格的 `Host` 块削弱** |
| `~/.ssh/config` 的 `StrictHostKeyChecking no` / `UserKnownHostsFile /dev/null` | **per-host**，更推荐 | 某台开发机 / 跳板机密钥轮换，其它主机保持严格 |

`~/.ssh/config` 的读取在 `src/ssh/hostkey_conf.rs`：`no`/`off` 或 `UserKnownHostsFile /dev/null` → `Ignore`；`yes`/`ask`/未写 → 保持 sshpf 默认的 `accept-new`（**不会**升格成 `strict`，否则给不写该指令的用户平添破坏）。`Host *` 继承与首个匹配块优先由 `ssh2-config` 处理。策略是**逐跳**的：`ProxyJump` 链上每一跳各用自己的策略，因此"跳板机忽略、目标主机严格"是正常组合。

**`@revoked` 永不被忽略。** 吊销是对这把密钥本身的判决，而忽略只针对"这台主机的密钥换了"——`Ignore` 的 `is_acceptable` 对 `Revoked` 恒为 `false`。忽略同时是**降级**：每次接受都留 `knownhosts.ignored_changed`（WARN），且**不改写** `known_hosts`，所以下次启动仍会看到同一处不一致，直到你用 `ssh-keygen -R` 清理或确认新指纹。

## 这些规则由谁守 / What enforces this

| 断言 | 在哪里 |
|---|---|
| 生产代码无 `unwrap`/`expect`/`panic!`/`unreachable!`/`todo!` | `tests/safety_guards.rs` |
| `unsafe` 只在 `*crash*.rs`，且每块有 `// SAFETY:` | `tests/safety_guards.rs` |
| 每个源文件有 `//!` 模块文档 | `tests/safety_guards.rs` |
| 单文件生产行 ≤ 400 | `tests/safety_guards.rs`（`MAX_PROD_LINES`） |
| 恶意 ssh config / config.toml / 环境变量：**不是被信号杀死**且日志有对应事件 | `tests/panic_hardening.rs` |
| 崩溃真写盘、轮转不切记录、重入不死锁、`0600`/`0700`（后两项仅 Unix） | `tests/logging.rs` |
| 致命信号保持信号退出状态 + 产生 core dump（**仅 Unix**，Windows 无 `sigaction`） | `tests/logging.rs` |

`tests/logging.rs` 与 `tests/panic_hardening.rs` **只在 Linux 跑**（`.github/workflows/ci.yml` 的 `hardening` job 是 `ubuntu-latest`），其中相当一部分用例又被 `#[cfg(unix)]` 门控。所以上表最后两行在 Windows 上是**没有断言**的——这正是"Windows 只覆盖 panic"这条限制的来源。

测试分层与怎么跑见 [testing.md](testing.md)。

# 自研逻辑盘点：哪些能用开源项目替代 / Self-built logic vs. OSS

> **这份文档回答**：当前代码里哪些逻辑是自研的、哪些能换成知名开源 crate、换的代价与收益各是什么、**建议做哪几项**。
> **不负责**：当前行为规则（见 [../index.md](../index.md)）；实施步骤（方案获批后另开 worktree，按 [../../AGENTS.md](../../AGENTS.md) 落地）。
> **相关代码路径**：`src/ssh/`、`src/logging/`、`src/ipc/`、`src/text.rs`、`src/cli.rs`。
>
> **调研时点 2026-11-10**。库缺陷与 API 形状的结论来自**读本机 `~/.cargo/registry` 的实际源码**
> 或上游仓库源码，不是引用旧文档；版本号取自 crates.io API。复跑方法见 §7。
>
> **本文有两次自我更正**（§1.1 的 `ssh-key` 行号、§1.3 的 `fast-socks5` 出站），
> 两处都是先写错、再读源码改对的。见 §9。

## 0. 结论先说 / TL;DR

**核心结论：这个仓库已经相当精简，没有一个"换掉立刻大赚"的目标。**
`russh 0.65.1`（2026-11-03）与 `ssh2-config 0.9.0`（2026-10-20）都已是当前最新版。
三处最大的自研模块（`logging` 1244 行、`ipc` 1129 行、`ssh/client` 1328 行）**逐条核对后，
替换都是"搬家"甚至"变差"**——因为它们实现的是 **OpenSSH 的用户语义与产品约束**，
不是通用基础设施。

按"是否真的值得动"排序：

| # | 项 | 判定 | 净变化 | 代价 |
|---|---|---|---|---|
| **1** | SOCKS5 → `fast-socks5` 1.0.0 | ✅ **唯一真正的替代品** | **-110 行** | **+2 crate**（`async-trait`、`tokio-stream`） |
| **2** | `text::char_width` → `unicode-width` | ⚠️ **修 bug，不是精简** | ≈ **0 行**（重构） | 零新增 crate（已在树内） |
| **3** | `known_hosts` → `ssh_key` | ❌ **被行号需求挡住** | 收益极小 | 见 §1.1 |
| **4** | `forwards` → `ssh2-config` 一半 | ⚠️ 收益被补丁吃掉 | -80~120 → 实际更少 | 见 §1.2 |

**明确不建议动**（§4 有逐条证据）：日志（`tracing`/`flexi_logger`）、崩溃捕获（`human-panic`/`crash-handler`）、
CLI（`clap`）、IPC 协议（通用 RPC）。

**一句话**：想减代码就做 SOCKS5（拿 +2 crate 换 -110 行自己维护的协议代码）；
想修正确性就做 `unicode-width`（不省行数，但修 ZWJ emoji 算宽）；
其余保持现状是**合理判断，不是欠债**。

---

## 1. SSH 相关：逐项盘点

判断依据是"**这段逻辑是在实现 SSH 协议，还是在实现 OpenSSH 的用户语义**"——
前者该交给库，后者没有库能替。

| 模块 | 生产行 | 性质 | 可否替代 |
|---|---|---|---|
| `ssh/client/driver.rs` | 222 | 三种转发的 serve 循环（`-L`/`-R`/`-D`） | ❌ `russh` 只给 channel 原语 |
| `ssh/client/connect.rs` | 242 | TCP 建连 + `ProxyJump` 链 | ❌ `russh` 无 ProxyJump 概念 |
| `ssh/client/classify.rs` | 270 | `russh::Error` → `FailureKind` | ❌ 领域映射 |
| `ssh/client/health.rs` | 219 | `keepalive@openssh.com` 探活 | ❌ 见 §1.4 |
| `ssh/client/auth.rs` | 176 | 私钥/agent + Windows 三路回落 | ⚠️ 探测链无 crate 替代，但 `russh` 已给三个原语 |
| `ssh/client/handler.rs` | 168 | 主机密钥裁决 + `-R` 入站 channel | ❌ 裁决语义是本项目的 |
| `ssh/known_hosts.rs` | **390** | 解析 + 匹配 + 四种裁决 | ❌ **A：被行号需求挡住** |
| `ssh/socks5.rs` | **173** | RFC 1928 CONNECT | ✅ **1：真正可换** |
| `ssh/forwards.rs`+`scan`+`parse` | **476** | `~/.ssh/config` 行扫描 | ⚠️ **D：部分可换，收益被吃掉** |
| `ssh/config.rs` | 380 | `ssh2-config` + `Match` 兜底 | ⚠️ `Match` 缺陷**仍在**，兜底不能删 |
| `ssh/hostkey_conf.rs` | 83 | `StrictHostKeyChecking` → 策略 | ❌ 映射是本项目的 |
| `ssh/probe.rs` | 133 | 远端 `ss`/`netstat` 端口发现 | ❌ 无对应 crate |

### 1.1 A：`known_hosts` — 被"行号"这一个需求挡住

`ssh-key 0.7.0-rc.11`（**已在本项目依赖树**，`russh::keys` 就是它）确实有结构化解析器：

```rust
// ssh-key-0.7.0-rc.11 的 known_hosts.rs
pub struct KnownHosts<'a>;                          // impl Iterator<Item = Result<Entry>>
impl KnownHosts<'_> {
    pub fn read_file(path: impl AsRef<Path>) -> Result<Vec<Entry>>;
}
impl Entry {
    pub fn marker(&self) -> Option<&Marker>;        // @revoked / @cert-authority
    pub fn host_patterns(&self) -> &HostPatterns;   // Patterns(Vec<String>) | HashedName{salt, hash}
    pub fn public_key(&self) -> &PublicKey;
}
```

看上去能替掉约 97 行（`Entry` + `parse_entries` + `|1|` HMAC + 自写 base64）。
**但有三个独立的阻塞点，任何一个都足以否掉它：**

**阻塞 1 —— 它不给行号。** `Entry` 只有 `marker`/`host_patterns`/`public_key` 三个访问器，
**没有行号字段**。而本项目的 `HostKeyVerdict::Changed { line }` 要行号，
用来在界面上说「主机的密钥已改变（known_hosts 第 N 行）」（`known_hosts.rs:85`），
`HostKeyPolicy::Ignore` 下的告警也要它（`handler.rs:90`）。用库就得自己在遍历时数，
那 `parse_entries` 的循环骨架又回来了——省不下多少。

**阻塞 2 —— 它空格敏感，会把本项目修掉的两个 bug 带回来。** `Entry::from_str` 是：

```rust
line = line.trim_end();                              // 只去尾部
let (marker_str, line) = line.split_once(' ')...;    // 只认单个空格
let (hosts_str, public_key_str) = line.split_once(' ')...;
```

它**不认 Tab 分隔，也不认行首空格**——而这两条**正是本项目当初自研 parser 修掉的 bug**
（[pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) §8 表格第 2、3 行）。
现实现用 `raw.trim()` + `split_whitespace()`，两者都容忍。要保住行为就得先归一化空白，
又多一层代码。

**阻塞 3 —— 它只解析不匹配。** 无 `matches`/`check`，`*`/`?` glob、`!` 否定、
`[host]:port` 比较仍要自己写。这正确（匹配是 OpenSSH 语义），但意味着 `host_field_matches`、
四种裁决、`HostKeyPolicy` **全部保留**。

**结论：不做。** 三个阻塞点叠加后，收益从 -97 缩到接近 0，而代价是把
`ssh-key 0.7.0-**rc**.11` 从传递依赖提升为直接依赖（RC 版本的 API 稳定性风险）。
纯按收益算它不成立。

> **`russh::keys::known_hosts` 仍然完全不能用**：我读了 `russh-0.65.1` 的 `keys/known_hosts.rs`
> 全部 232 行，四个缺陷**全部原样存在**——`match_hostname` 只做 `host == entry`（无 glob）、
> `buffer.split(' ')`（Tab 就断）、无 `@` marker 识别、`@revoked` 与其他条目同时命中时返回
> `Ok(true)`（放行被吊销的密钥）；返回类型仍是 `Result<bool>`，无法表达四种裁决。
> 旧文档结论今天仍成立。但**它底下那层 `ssh-key` 的解析器确实修好了**——两件事常被混为一谈。

### 1.2 D：`forwards` — 可换，但收益被补丁吃掉

`ssh2-config 0.9.0` 我在源码里确认了四个事实（**与旧文档不同，很重要**）：

| 指令 | 0.9.0 状态 | 证据 |
|---|---|---|
| `RemoteForward` | ✅ **已建模**，`HostParams::remote_forward: Vec<RemoteForward>` | `parser.rs:481` push；`params.rs:65` |
| `LocalForward` | ❌ 仍在 `UnsupportedField` 列表 | `parser.rs:545` |
| `DynamicForward` | ❌ 同上 | `parser.rs:520` |
| `Match` | ❌ 枚举里**仍然没有**这个变体 | `field.rs` 90 个变体，无 `Match` |

`RemoteForward` 不仅被解析，且按出现顺序 push 进 `Vec`——旧文档说"丢失顺序和数量"
**对 `RemoteForward` 已不成立**。`LocalForward` 多条丢失的真因是
`unsupported_fields` 是 `HashMap<String, Vec<String>>`（`parser.rs:213` 的 `insert`），
**同名字段后写覆盖前写**。

**但 D 不清爽**：`RemoteForward` 的三参数形式、unix socket 形式、
`SshForward::line`（界面显示"配置第几行"）在库的类型里都没有，仍要自己补。
`LocalForward`/`DynamicForward` 还得分两条路走（一个走库、一个走自研扫描），
**两套解析器并存比现在一套更难看**。

`Match` 的 `strip_match_blocks` 兜底（`config.rs:143`）**无论 A 还是 D 都不能删**。

### 1.3 SOCKS5 → `fast-socks5` —— 唯一真正的替代品

**这一节我先写错了，读源码后更正。** 初稿说我"必须自己写出站胶水、所以收益为负"——
事实**正好相反**：`fast-socks5` 的设计是**从不自己连接上游**，永远把目标地址交给你。
这正是本项目需要的形状。它的 `examples/router.rs`（我完整读过）就是证据：

```rust
// 1) 握手 + 认证
let (proto, cmd, target_addr) = Socks5ServerProtocol::accept_no_auth(socket)
    .await?.read_command().await?;
// 2) 拒绝非 CONNECT
if cmd != Socks5Command::TCPConnect {
    proto.reply_error(&ReplyError::CommandNotSupported).await?;
    return Ok(());
}
// 3) ← 这里由我们决定去哪：换成 handle.channel_open_direct_tcpip(...)
let (host, port) = target_addr.into_string_and_port();
// 4) 告诉客户端已连接
let inner = proto.reply_success(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
// 5) 双向拷贝
transfer(inner, ssh_channel).await;
```

它替掉的是 `socks5.rs` 里**最难自己维护**的那部分：RFC 1928 的 VER/NMETHODS/METHODS 协商、
ATYP IPv4/IPv6/域名的字节解析、REP 码选择、`00`/`07`/`08` 的回复编码。
这些现在由本项目手写（`socks5.rs` 约 110 行生产代码 + 9 个测试）。

**代价（这是唯一的反对理由）**：新增 2 个 crate。实测 `fast-socks5 1.0.0` 的运行期依赖是
`anyhow`/`async-trait`/`log`/`socket2`/`thiserror`/`tokio`/`tokio-stream`，其中
**`async-trait` 与 `tokio-stream` 不在当前 `Cargo.lock` 里**（我核对过）。
（顺带更正：初稿说它引入 `tokio-util`——**错**，它不依赖 `tokio-util`。）

**取舍**：-110 行自维护的协议代码，换 +2 个纯 Rust 无 C 依赖的 crate。
`transfer()` 泛型于 `AsyncRead + AsyncWrite`，`russh::Channel::into_stream()` 满足。

**要留的东西**：`eventlog::socks5_rejected` 的三类原因（握手失败/请求不可解析/上游失败）
要映射到库的错误类型上，这层翻译约 10 行。与 `ssh -D` 对齐的
「BIND/UDP 不支持 → `REP=0x07`」语义由库提供，但**要加断言钉住**。

**判定**：✅ **可做**。这是本次调研唯一"换掉真能减自研代码"的项目。
是否值得，取决于你更在意"少 110 行自己维护的协议实现"还是"少 2 个依赖"。

### 1.4 明确保留自研的 SSH 部分

- **`ProxyJump` 链**（`connect.rs`）：`russh` 协议层没有这个概念。
- **探活**（`health.rs`）：`russh` 自带 keepalive，但它的超时只结束 session task，
  serve 循环观察不到（D36 的由来）。换成库的 keepalive 会**重新引入**那个 bug。
- **失败分类**（`classify.rs`）：产品语义。
- **端口探测**（`probe.rs`）：跑 `ss`/`netstat` 并解析，无 crate；且刻意不插值（§6）。
- **agent 探测链**（`auth.rs`）：无 crate 做三路回落，但 `russh` 已给
  `connect_env()`（unix）、`connect_named_pipe()`、`connect_pageant()` 三个原语，
  现实现是对的。注意 `connect_env()` 在 Windows 上被 `#[cfg(unix)]` 门控，
  所以 Windows 分支必须自己读环境变量——这就是回落不能删的原因。

---

## 2. 非 SSH：逐项盘点

| 模块 | 生产行 | 自研内容 | 判定 |
|---|---|---|---|
| `logging/` | 1244 | 定列格式 + `seq` + 轮转 + 重入防护 + 崩溃 fd | ❌ §4.1 |
| `ipc/` | 1129 | 长度前缀帧 + TOML 消息 + 版本/令牌校验 | ❌ §4.3 |
| `eventlog/` | 1012 | `event=` 词汇表（一个个具名函数） | ❌ 文档契约 |
| `text.rs` | 227 | 宽度表、截断、双语 fit、折行、glob | ⚠️ **2** |
| `cli.rs` | 362 | 参数解析（~120）+ 双语 `USAGE`（~120） | ❌ §4.4 |
| `sys/` | 225 | 文件权限、退出信号 | ❌ 已很薄 |
| `server/`+`client/` | 2621 | 选举、广播、镜像 | ❌ 产品架构 |

### 2.1 B：`text::char_width` → `unicode-width` —— 修 bug，不省行

**这一节我也要收窄。** 初稿说"删 45 行手维护表，干净收益"——**过于乐观**。
`unicode-width 0.2.3` 确实正确实现了 UAX #11（含 ZWJ 序列），且**已在依赖树**
（`ratatui-core` → `unicode-width 0.2.3`），零新增 crate。但换了它不是等价替换：

**差异 1 —— 类型变了。** `UnicodeWidthChar::width(self) -> Option<usize>`（`lib.rs:211`），
控制字符返回 `None`。现 `char_width` 返回 `usize`（1 或 2，从不 0/None）。
所有调用点要处理 `Option`。

**差异 2 —— 字符串宽度不是逐字符相加。** 这是关键：`unicode-width` 的
`str_width` 是**带状态的右折叠**（`lookup.rs:339`，靠 `width_in_str` 携带上下文）——
因为 ZWJ 序列要整体算 2。而本项目 `display_width` 是 `s.chars().map(char_width).sum()`
（`text.rs:63`）。两者对 `👨‍👩‍👧` 会给出不同答案（ZWJ 序列：现在相加约 6，
`unicode-width` 说 2）。

**所以 B 的真实形态**：`display_width`/`truncate_to_width`/`pad_to_width`/`wrap`/`fit`
必须一起改成 **str 感知**，否则截断与填充会互相矛盾（`truncate_to_width` 用逐字符累加，
`display_width` 用整体测量 → 同一行算出两个宽度，表格错位）。
**这是一次重构，不是删掉 45 行。**

**那还值不值得做？** 值得，但理由要换成正确性而非精简：
现在的手写表把 `0x1F300..=0x1F64F` 整段当 2 列，对组合 emoji 必然算宽，
`char_width` 也从不返回 0（combining mark 应为 0）。这是**真实存在的显示 bug**，
`unicode-width` 是正解。既有断言（`✓`=1、`🚀`=2、`中文`=4）都与 `unicode-width` 一致，
说明只有 exotic 用例会变。

**不动**：`fit`（双语三级降级）、`bi`、`wrap` 的算法契约（"永不返回空 vec"）、
`glob_match`（`wildmatch` 语义等价但不支持 `ignore_case`；`globset`/`glob` 是超集，
会改变本项目**文档明写**的"不支持 `[...]`"行为，见 `text.rs:195`）。

---

## 3. 依赖现状（事实核对）

| 直接依赖 | 版本 | 状态 |
|---|---|---|
| `russh` | 0.65.1 | ✅ **最新**（2026-11-03），MSRV 1.89 与本项目一致 |
| `ssh2-config` | `=0.9.0` | ✅ **最新**（2026-10-20），但 `Match`/`LocalForward`/`DynamicForward` 仍缺 |
| `ratatui` / `crossterm` / `tokio` / `toml` | 0.30.2 / 0.29 / 1.54.1 / 1 | ✅ |

**已在依赖树、可复用的 crate**（"零新增"的候选）：

| crate | 版本 | 由谁引入 | 可用于 |
|---|---|---|---|
| `unicode-width` | 0.2.3 | `ratatui-core`、`unicode-truncate` | **B**（但见 §2.1 的类型/语义差异） |
| `ssh-key` | 0.7.0-rc.11 | `russh` | ❌ A 已否（§1.1） |
| `glob` | 0.3.4 | `ssh2-config` | `expand_include` 的 glob（`scan.rs:198`），-15 行 |
| `data-encoding` | 2.11.1 | `russh` | 仅 A 成立时有用 |
| `wildmatch` | 2.7.0 | `ssh2-config` | ❌ 不支持 `ignore_case` |
| `shlex` | 1.x | `cc`（**构建期**） | ❌ 不能当运行期依赖 |

`russh` 的 feature 现状：`default = ["flate2", "aws-lc-rs", "rsa"]`；本项目用
`default-features = false` + `["ring", "flate2", "rsa"]`，**配置正确**（`ring` 仍是可选 feature，
`flate2`/`rsa` 已显式补回）。

---

## 4. 为什么不做这几项 / Rejected

### 4.1 日志 → `tracing` / `flexi_logger`

`logging/` 1244 行里，"通用基础设施"占比很小，大部分是本程序特有的：

| 需求 | 通用 crate |
|---|---|
| 定列 `ts LEVEL seq=N event=k v=k2 v2` | ❌ 要自定义 `FormatEvent`——工作量与现在相同 |
| **动态键**（`&[(&str, &str)]` 而非字面量） | ❌ `tracing` 的字段名必须是字面量，本项目做不到 |
| 值转义（空格/引号/反斜杠/`\xNN`） | ❌ 要自己写 |
| 单调 `seq` | ❌ 无 crate 提供 |
| 三层重入防护（panic hook 内再进 logger） | ❌ `TracingSubscriber` 同样会死锁 |
| **崩溃 fd 重定向**（轮转时 `dup` 并重挂） | ❌ **无任何 crate 能表达** |
| 永不 stdout/stderr（TUI 占着终端） | ⚠️ `flexi_logger` 默认 `LogTarget::StdErr`，要三处 opt-out |
| 失败即降级 disabled | ⚠️ 需自己保证 |

两个具体障碍：

- **轮转与崩溃描述符耦合**：`sink.rs:19-23` 记录崩溃写入器持有日志 fd 的 `dup`，
  轮转必须先 `release()` 再重新 `adopt()`。任何"帮你轮转"的 crate 会在背后
  rename/reopen，使 fd 指向归档文件——崩溃记录会落进 `sshpf.log.1`。**这个不变量无法通过
  任何 crate 的轮转 API 表达**。
- **`tracing-appender` 根本没有按大小轮转**：`Rotation` 只有
  `MINUTELY/HOURLY/DAILY/WEEKLY/NEVER`，`max_log_files(n)` 是按时间命名的文件计数，
  表达不了"2 MiB 保留 3 份"。`flexi_logger` 的
  `Criterion::Size` + `Cleanup::KeepLogFiles` 可以，但被上一条否掉。

### 4.2 崩溃捕获 → `human-panic` / `crash-handler`

`crash-handler 0.8.1` 是唯一覆盖信号集的候选（`EXCEPTION_SIGNALS = [Abort,Bus,Fpe,Illegal,Segv,Trap]`，
`SA_ONSTACK|SA_SIGINFO`，退出路径 `install_default_handler` + `tgkill`，**确实保住 core dump**）。
但：

- **它比现实现更松**：crash path 上取 `parking_lot::Mutex` 锁，源码自己注明
  "technically mutexes are not async signal safe on linux"。而 `logging/crash.rs` 是刻意
  **无分配、无锁**的。
- **仍要自己写**：备用屏转义序列、记录格式，以及一个 `unsafe { make_crash_event(...) }` 调用。
- **`unsafe` 并不会消失**，只是从"可审计的自有代码"变成"别人的 `unsafe` 你审不到"——
  对本项目的威胁模型更差。
- `human-panic 2.0.8`（2026-04-02，仍维护）只挂 panic hook，**不处理信号**，且往 **stderr** 写横幅。
- `signal-hook` 明确拒绝（`FORBIDDEN` 信号会 panic）。

### 4.3 IPC 协议 → 通用 RPC（tarpc / tonic）

`ipc/` 的核心不是"传消息"，而是 [../client-server.md](../client-server.md) 那条约束：
**过期视图不得拥有破坏权**——`Command` 必须带 `expect_rev`，server 一律重新校验。
通用框架不提供这个语义；换 gRPC 还要引入 `prost`/`hyper`/`tonic`，只为本地回环两进程通信。
帧层（182 行）是 4 字节前缀 + 256 KiB 上限，序列化库替不掉"帧边界"本身。

### 4.4 CLI → `clap`

`clap 4.6.7` 技术上满足三个硬要求：`ValueParser::os_string()` 收非 UTF-8；
`Command::override_help()` 可整段替换帮助；`Error::exit()` 内部 `// Swallow broken pipe errors`。
**但仍然净中立**：

- `override_help` 意味着**只用 clap 的解析**，而帮助生成恰好是它唯一真正擅长的东西
  ——等于把它最好的部分扔掉。
- **错误文案会退化成英文单语**：现在是 `"未知参数 / Unknown option: {other}"`，
  clap 给 `error: unexpected argument '--foo' found`。要对齐契约就得自己格式化 clap `Error`。
- 依赖从 0（120 行 `match`）变成 `clap_builder` + `clap_lex`（+ 默认 feature 下的 `anstream`/`anstyle`）。

若哪天只为"好看"而重构，**`lexopt`** 是对齐的选择（零依赖、原生 `OsString`、
明确不做帮助生成——正是这里要的契约）。但对现状是**等价替换，净收益 0**。

---

## 5. 建议的实施方案

### 若要动手：二选一，不要都做

**选项 1（减代码）：SOCKS5 → `fast-socks5`**

| 步 | 改动 | 验收 |
|---|---|---|
| 1 | 加 `fast-socks5 = "1.0"`，确认 `Cargo.lock` 只新增 `async-trait`+`tokio-stream` | `cargo tree` 无 C 依赖 |
| 2 | `driver.rs` 的 `serve_dynamic` 改走库的 `accept_no_auth`→`read_command`→`reply_success`→`transfer` | 现有 SOCKS5 测试语义不退化 |
| 3 | `socks5_rejected` 三类原因映射到库错误类型 | `event=` 名称逐字不变 |
| 4 | 钉住「BIND/UDP → `REP=0x07`」「未知 ATYP → `REP=0x08`」两条断言 | 新增断言 |

**选项 2（修正确性）：`char_width` → `unicode-width`**

| 步 | 改动 | 验收 |
|---|---|---|
| 1 | 加 `unicode-width = "0.2"`（零新增 crate） | `Cargo.lock` 不变 |
| 2 | `display_width` 改 `UnicodeWidthStr::width`；`char_width` 改 `Option`→`unwrap_or(0)` | 现有 ~15 条断言通过 |
| 3 | **`truncate_to_width`/`pad_to_width`/`wrap` 同批改成 str 感知** | 三者与 `display_width` 一致 |
| 4 | 新增 ZWJ emoji 用例（`👨‍👩‍👧` = 2 列） | 新增断言 |
| 5 | 更新 `text.rs:28` 与 [../design.md](../design.md) 里"不走 `unicode-width`"的说明 | 文档与代码一致 |

> ⚠️ 注意：`text.rs:28` 说理由在 `docs/design.md`，但**我在 `design.md` 里 grep 不到
> `unicode`/`width`/`emoji`**。这条引用已经悬空——无论做不做 B，都该补上或删掉。

### 不建议

A（被行号挡住）、D（收益被补丁吃掉）、§4 的日志/崩溃/CLI/IPC 四项。

---

## 6. 安全与不变量（改这些时不许碰）

| 不变量 | 出处 | 与本文改动的关系 |
|---|---|---|
| 生产代码无 `unwrap`/`expect`/`panic!` | [../../AGENTS.md](../../AGENTS.md) 第 2 条 | `unwrap_or(0)` 不是 `unwrap`，但 `Option` 处理要显式 |
| `unsafe` 仅在 `logging/crash.rs` | 同上第 3 条 | 全部不碰 |
| `@revoked` **任何策略下都拒绝** | `known_hosts.rs:67`、[../security.md](../security.md) | A 不做，此项天然保住 |
| 探针命令是固定字面量，无插值 | `probe.rs:53` | 不动 |
| `event=` 名称逐字不变 | [../logging/events.md](../logging/events.md) | **选项 1 必须守住** `socks5_rejected` |
| 用户配置与运行状态分离 | [../design.md](../design.md) D7 | 不动 |

---

## 7. 复跑方法 / How to re-verify

```bash
# 下面都是**上游 crate 内部**的相对路径。先 cd 进 registry 目录，并记住 crate 的源码子目录名：
cd ~/.cargo/registry/src/*/
S=src

# 1) 版本是否还最新
#    https://crates.io/api/v1/crates/russh          → newest_version
#    https://crates.io/api/v1/crates/ssh2-config    → newest_version

# 2) russh 的 known_hosts 缺陷是否仍在（应看到 `host == entry`，且无 glob 分支）
grep -n 'fn match_hostname' -A 20 russh-*/$S/keys/known_hosts.rs

# 3) ssh-key 的两个阻塞点：无行号字段 + 空格敏感
grep -n 'pub fn \(marker\|host_patterns\|public_key\)' ssh-key-*/$S/known_hosts.rs  # 无 line()
grep -n 'trim_end\|split_once' ssh-key-*/$S/known_hosts.rs                          # 空格敏感

# 4) unicode-width 的两个差异：width 返回 Option + str 宽度带状态
grep -n 'fn width(self) -> Option<usize>' unicode-width-*/$S/lib.rs
grep -n 'pub fn str_width' -A 6 unicode-width-*/$S/lookup.rs                        # rfold 不是 sum

# 5) ssh2-config 仍缺 Match / LocalForward / DynamicForward
grep -n 'Field::LocalForward\|Field::DynamicForward' ssh2-config-*/$S/parser.rs
grep -c 'Match' ssh2-config-*/$S/parser/field.rs   # 0 个变体

# 6) fast-socks5 的出站形状（应看到 read_command 之后才 reply_success）
curl -s https://raw.githubusercontent.com/dizda/fast-socks5/master/examples/router.rs

# 7) 哪些候选 crate 已在依赖树（决定"零新增"）——回到仓库根
grep -E '^name = "(unicode-width|ssh-key|glob|wildmatch|data-encoding|async-trait|tokio-stream)"' Cargo.lock
```

## 8. 本次未核实的部分 / Not verified

| 项 | 状态 |
|---|---|
| 任何 crate 的**编译/运行**验证 | ⚠️ 全部结论来自读源码，未 `cargo add`/`cargo test` |
| `transfer()` 在 `russh::Channel::into_stream()` 上是否真的跑通 | ⚠️ 类型上满足 `AsyncRead+AsyncWrite`，未端到端实测 |
| `unicode-width` 对现有 ~15 条断言的逐条结果 | ⚠️ 未跑；理论上前提 3 条（`✓`/`🚀`/`中文`）应一致 |
| `fast-socks5` 的具体版本号与 MSRV | ⚠️ 1.0.0（2026-01-20）确认；**未声明 `rust-version`** |
| `ssh2-config` 0.9.0 与 0.8.x 的逐版差异 | ⚠️ 未比对；已锁 `=0.9.0` |
| `glob` 替换 `expand_include` 是否完全等价 | ⚠️ `~` 展开与 `out.sort()`（`scan.rs:224`）未验证 |
| 行数变化 | ⚠️ 按函数边界数出，以实现为准 |
| 开放式 crate 发现 | ⚠️ 搜索端点返 401，只能核对**指定候选** + crates.io 关键词搜索 |

## 9. 自我更正记录 / Corrections

两处初稿错误，都由读源码发现，保留记录以免后人重走：

| 初稿说法 | 事实 | 影响 |
|---|---|---|
| "`fast-socks5` 出站仍需自己写胶水，收益为负" | **反了**：它的设计就是从不自己连接上游，永远交给你——正是本项目需要的形状（`examples/router.rs`） | SOCKS5 从"拒绝"改为"**唯一真正的替代品**" |
| "`ssh-key` 能替掉约 97 行，净 -70 行，建议做" | 它**不给行号**（`Changed { line }` 需要）、**空格敏感**（带回 Tab/行首空格 bug）、**只解析不匹配** | A 从"建议做"改为"**不做**"，且从 TL;DR 的必做项移到否决项 |
| "`fast-socks5` 强制引入 `tokio-util`" | **错**：依赖是 `anyhow`/`async-trait`/`log`/`socket2`/`thiserror`/`tokio`/`tokio-stream`，无 `tokio-util` | 新增 crate 数是 2（`async-trait`+`tokio-stream`），不是 1 个更重的 |
| "`unicode-width` 是干净 -45 行收益" | `width()` 返回 `Option`，且 **str 宽度是带状态折叠、不是逐字符相加** | B 从"精简"改为"**重构/修 bug**"，净行数≈0 |

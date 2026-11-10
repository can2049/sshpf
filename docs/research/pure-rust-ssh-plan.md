# 决策：用纯 Rust `russh` 替换系统 `ssh` 子进程

> **这份文档回答**：为什么全量换成 `russh`、验收判据是什么、目标架构长什么样（**§4 是 `src/sys/mod.rs` 引用的架构段**）、分几步落地、哪些替代方案被否决。
> **不负责**：实测数字与库缺陷复现——见 [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md)；config/密钥/agent 选型——见 [pure-rust-ssh-config.md](pure-rust-ssh-config.md)；当前实现——见 [../design.md](../design.md)、[../runtime.md](../runtime.md)。
> **相关代码路径**：`src/ssh/`、`src/tunnel.rs`、`src/sys/`、`src/app.rs`。

> 历史决策文档（2026-11）。其中"改造前"的代码量数字来自 v1，**已全部过时**；判断与判据仍然有效。

## 1. 目标与判据

**用户目标（原话）**：把调用本地 `ssh` 工具的做法替换为直接使用 Rust 实现的 SSH，
以实现**跨平台的可靠一致性**。

| # | 判据 |
|---|---|
| C1 | 支持 `-L` / `-R` / `-D` 全部三种转发（现有功能不能退化） |
| C2 | 从 `~/.ssh/config` 读取主机（原需求 #2 不能退化） |
| C3 | 失败原因能显示给用户（原需求 #6 不能退化） |
| C4 | 不再依赖系统 `ssh` 可执行文件 |
| C5 | Linux 与 Windows 走**同一条代码路径**（这才是「一致性」的实质） |
| C6 | 用户侧零额外安装（`.exe` 拿到就能跑） |

## 2. 结论

**采用 `russh` 全量替换，分 3 阶段落地**（判据见 §6）。理由：

1. **三种转发协议层零缺口** —— `-L`/`-R`/`-D` 全部在真实 OpenSSH 8.2p1 上跑通端到端流量，
   ProxyJump 三跳也通（[spike §1](pure-rust-ssh-spike.md#1-端到端结果)）。
2. **`-R` 不是缺口**（最容易误判的一项）——`Handle::tcpip_forward()` +
   `Handler::server_channel_open_forwarded_tcpip()` 是完整的客户端 `-R` 实现；
   GitHub issue #183 的方向不同（[spike §2.1](pure-rust-ssh-spike.md#21--r-不是能力缺口易误判点)）。
3. **失败分类反而更准** —— 从"正则匹配 stderr"升级为"结构化 `russh::Error`"；主机密钥校验从
   "解析 `Host key verification failed`"变成"我们主动决定"，`strict`/`accept-new`/`ask` 三种语义可完整实现。
4. **现有架构几乎不用改** —— tokio 跑在每隧道一个后台线程，用 `std::sync::mpsc` 把事件回灌给
   UI 线程；现有 `TunnelEvent` + mpsc 模型**原样保留**（§4）。
5. **`~/.ssh/config` 比预期简单得多** —— `ssh2-config` 0.9.0 纯 Rust、零 C 依赖，覆盖
   Include/Match 语法/Host 通配/否定/ProxyJump/`Host *` 继承（但 `Match` 语义有 bug，须自己兜，§5）。

## 3. 技术选型

```toml
[dependencies]
# SSH 协议实现。ring 后端：Windows 构建不需要 NASM/cmake
russh = { version = "0.65.1", default-features = false, features = ["ring", "flate2", "rsa"] }
# ~/.ssh/config 解析（纯 Rust，零 C 依赖）
ssh2-config = "=0.9.0"
# 异步运行时（仅用于后台隧道线程）
tokio = { version = "1", features = ["rt", "net", "io-util", "time", "sync"] }
```

复用 russh 内置的 `russh::keys`（含 `ssh_key`、`agent`、`known_hosts`），不额外引入 `ssh-key` /
`ssh-agent-client-rs`，避免版本分裂。

### 3.1 为什么选 `ring` 而不是默认的 `aws-lc-rs`

同一目标 `x86_64-pc-windows-gnu`，统计构建过程中请求的外部工具：

```text
ring   后端：  9 × mingw32-gcc                      （无 nasm，无 cmake）
aws-lc 后端： 59 × mingw32-gcc,  2 × NASM,  3 × CMake
```

ring 发布包带 104 个预生成文件（其中 12 个预汇编 `.o`），包内无 `.git`，`build.rs` 走
`PREGENERATED` 分支**直接使用 `.o` 而不调用 nasm**——这是选它的决定性理由。

> ⚠️ 这是**构建期**依赖，不是运行期（满足 C6）。GitHub 的 `windows-latest` 预装 MSVC。
> 详见 [spike §10](pure-rust-ssh-spike.md#10-构建依赖与体积)。

## 4. 目标架构

```text
┌─────────────────────────────────────────────────────────┐
│ UI 线程（同步，无 tokio）                                │
│   crossterm::event::poll(200ms) → keys::handle_key       │
│   app.rs 消费 TunnelEvent ← std::sync::mpsc              │
└────────────────────┬────────────────────────────────────┘
                     │ mpsc (TunnelEvent)
┌────────────────────┴────────────────────────────────────┐
│ 隧道线程 × N（每条隧道一个）                              │
│   tokio current_thread runtime                           │
│   ┌───────────────────────────────────────────┐          │
│   │ driver: Local | Remote | Dynamic          │          │
│   │   Local   → TcpListener + direct-tcpip    │          │
│   │   Remote  → tcpip_forward + forwarded 回调│          │
│   │   Dynamic → SOCKS5 + direct-tcpip         │          │
│   │ 前置: ProxyJump 链（connect_stream 嵌套）  │          │
│   │ 校验: known_hosts（自研匹配）              │          │
│   └───────────────────────────────────────────┘          │
└─────────────────────────────────────────────────────────┘
```

关键点：

- **一线程一 runtime**（`current_thread`）。实测 10 条隧道并发建立仅 80 ms，本项目规模
  （几条到几十条）完全够用，不需要共享 runtime；隔离性也更好——一条隧道 panic 不影响其它。
- `client::Handle` **不是 `Clone`**，多 task 共享用 `Arc<Handle>`。
- 取消用 `tokio::task::abort()`。端口释放实测 **172 µs**，30 次循环**无 fd 泄漏**。
- **`sys` 层只剩下文件权限与退出信号**：进程组 / `PR_SET_PDEATHSIG` / Windows Job Object
  这一整层因为不再有子进程而删除。

## 5. 必须自己写的胶水（含实测坑）

| # | 项目 | 规模 | 为什么必须自写 |
|---|---|---|---|
| 1 | known_hosts 匹配与校验 | ~120 行 | russh 只返回 `bool`，但需要区分**未知/变更/已吊销/CA 签发**四种状态；且实测有四个误判 |
| 2 | SOCKS5 服务端 | ~150 行 | russh 零 SOCKS 代码。实测 60 行可跑通 |
| 3 | ProxyJump 链 | ~100 行 | `connect_stream` 套 channel，每层约 20 行 |
| 4 | `ssh2-config` 的 `Match` 兜底 | ~25 行 | **已复现的静默污染 bug** |
| 5 | `russh::Error` → `FailureKind` 映射 | ~150 行 | 需实现 `channel_open_failure` 拿服务端描述 |
| 6 | `tcpip_forward` 返回值封装 | ~10 行 | 非 0 端口成功时返回 **`Ok(0)`**，直接用就是 bug |

### 5.1 三个已复现的坑（必须写进测试）

**坑 1：`@revoked` 会放行被吊销的密钥（安全问题）**

```text
@revoked 条目单独存在        -> Ok(false)   碰巧安全（理由是"没匹配到"）
@revoked + 同 key 正常条目   -> Ok(true)    ❌ 接受被吊销的密钥
```

复现细节与另外三个 known_hosts 误判（通配 / Tab / 行首空格）见
[spike §8](pure-rust-ssh-spike.md#8-known_hosts--本节是库里最危险的缺陷)，
这是 `src/ssh/known_hosts.rs` 自研匹配的依据。

**坑 2：`ssh2-config` 的 `Match` 块静默污染前一个 `Host`**

```text
输入: Host web1{HostName 10.0.0.1; User alice} + Match host foo{User ignored; Port 9999}
ssh2-config query(web1) -> user="ignored" port=9999   ❌
真实 ssh -G -F ... web1 -> user=alice     port=22     ✅
```

对策：解析前剔除 `Match` 块（行首 `Match` 起，跳过后续所有缩进行）。**必须加单测**。
这是 `src/ssh/config.rs` 的 `strip_match_blocks` 与 [../design.md](../design.md) D2 的由来。

**坑 3：`tcpip_forward` 返回 `Ok(0)`**

请求非 0 端口且成功时返回 `Ok(0)`，不是所请求的端口。
封装原则：请求端口非 0 用请求值；请求 0（让服务端分配）用返回值。

## 6. 分阶段落地

```text
阶段 1  deps + 骨架（tokio 线程 + mpsc 桥接），只实现 -L
        → 立刻提 PR，用 CI 验证 Windows 构建（这是唯一无法本地验证的风险）
        → 此时保留系统 ssh 实现，用 feature/env 切换，可随时退回

阶段 2  -R / -D / ProxyJump / known_hosts / SOCKS5 / 错误映射
        → 补齐测试，双平台 CI 全绿

阶段 3  默认切到纯 Rust，删除子进程实现 + testutil + sys 里的进程组/JobObject
```

**为什么必须先做阶段 1**：唯一无法在本地验证的是「Windows + MSVC + ring 编译」。本地只能
`cargo check --target x86_64-pc-windows-gnu`，且因无 mingw 链接器必然失败（交叉编译固有限制，
不是代码问题）。**先让 CI 说话，再投入大量改造**。

## 7. 其他影响（当时的估算）

| 项 | 变化 |
|---|---|
| 代码量 | 净增约 900–1100 行，删除约 1200 行（子进程管理、stderr 分类、`config_parse`） |
| 二进制 | 1.63 MB → **4.34 MB**（+2.7 MB，release strip+lto 实测） |
| 依赖 | 92 → 约 135–142 个 crate |
| 编译时间 | 冷编译 +11~22 s |
| MSRV | Rust 1.89+（russh 要求） |
| 测试 | `testutil.rs`（假 ssh 夹具）整体删除；改起真实 sshd 做集成测试 |

## 8. 风险与未验证项（当时的诚实清单）

| 风险 | 状态 | 缓解 |
|---|---|---|
| **Windows + MSVC + ring 构建** | ❌ 本地未验证（无 mingw 链接器） | 阶段 1 立刻用 CI 验证 |
| known_hosts `@revoked` 放行 | ✅ 已复现 | 自研校验（必做） |
| `ssh2-config` Match 污染 | ✅ 已复现 | 预处理剔除（必做） |
| `tcpip_forward` 返回 0 | ✅ 已复现 | 封装（必做） |
| 真实环境 ProxyJump 多跳 | ⚠️ 只在本机双跳验证 | 阶段 2 用真实跳板机验证 |
| keyboard-interactive（2FA） | — `BatchMode=yes` 本就不支持，非退化 | 需要时作独立需求 |
| `sk-*` 硬件密钥 | ⚠️ 仅格式可解析 | 明确不支持并提示 |
| russh 0.x 无 semver 保证 | ⚠️ 2–4 周一版 | 锁 minor 版本 |

## 9. 考虑过但否决的方案

| 方案 | 否决理由 |
|---|---|
| **混合：Linux 用系统 ssh，Windows 用 russh** | 两条代码路径 = 两套语义、两套 bug —— **恰恰破坏用户要的「一致性」**。且省不下 config 层工作量（russh 也不读 config） |
| 继续用系统 ssh，只做 Windows 适配 | 不满足 C4/C5，且 Windows 上用户需自装 OpenSSH |
| `ssh2` crate（libssh2 绑定） | 引入 C 库运行时依赖，与 C6 冲突；且需自己处理 config；**协议层无 ProxyJump** |
| 保留双后端 + `trait SshBackend` | 曾被列为"v1 预留"，最终判定为**双路径即双语义**，与 C5 直接冲突 |

## 10. 当时留待确认的问题（后已裁定）

| 问题 | 结论 |
|---|---|
| 接受二进制涨到约 4.34 MB？ | 接受：`cargo install` 分发不受影响 |
| 接受「构建需要 C 工具链」？ | 接受：仅 CI/开发机需要，用户侧零影响 |
| 分阶段、且阶段 1 先提 PR 让 CI 验证 Windows？ | 是，已按此执行 |
| password / keyboard-interactive 是否需要？ | 作为独立后续需求，不混在本次重构里 |
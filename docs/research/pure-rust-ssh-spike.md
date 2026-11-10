# 实测：russh 纯 Rust 实现 -L / -R / -D / ProxyJump

> **这份文档回答**：把系统 `ssh` 子进程换成 `russh` 是否可行、实测数字是多少、哪些库缺陷被复现（尤其 §8 的 known_hosts 四种误判）。
> **不负责**：当前实现——见 `src/ssh/` 与 [../design.md](../design.md)；config 解析选型见 [pure-rust-ssh-config.md](pure-rust-ssh-config.md)；选型动机见 [pure-rust-ssh-plan.md](pure-rust-ssh-plan.md)。
> **相关代码路径**：`src/ssh/client.rs`、`src/ssh/known_hosts.rs`、`src/ssh/socks5.rs`、`src/ssh/forwards.rs`、`src/tunnel.rs`。

> **这些都是历史实测**（2026-11，russh 0.65.1 / OpenSSH_8.2p1 / rustc 1.99.1 / Ubuntu）。
> 数字与库行为此后可能已变；**要用之前先复跑**。原可复现工程（`spike/`，含临时 sshd 与六个探针）已删除，
> 复跑方法见 §13。

## 0. 结论

| 问题 | 实测答案 |
|---|---|
| `-L` / `-R` / `-D` 三种转发 | ✅ 全部在真实 sshd 上跑通端到端流量；`-D` 需自写 SOCKS5（约 60 行） |
| ProxyJump | ✅ 三跳跑通（SSH 跑在 SSH 的 channel 里） |
| 摆脱系统 `ssh` 二进制 | ✅ 运行期可以；构建期仍需 C 工具链（见 §10） |
| 收益 | 失败原因从"正则猜 stderr"变成结构化错误；`-R` 假就绪问题从根上消失 |
| 主要成本 | 重建 OpenSSH 周边语义：known_hosts（必须自写）、config 求值、错误映射 |

## 1. 端到端结果

| 能力 | 结论 | 证据 |
|---|---|---|
| `-L` | ✅ | 经隧道读到目标 banner `SSH-2.0-OpenSSH_8.2p1` |
| `-R` / `-R` 取消 | ✅ / ✅ | 经远端端口回环收到 `hello-through-remote-forward`；`cancel_tcpip_forward()` 返回 `Ok` |
| `-D` SOCKS5 | ✅（自写） | IPv4 + 域名两种 ATYP 均验证 |
| ProxyJump | ✅ | 3 跳嵌套认证成功，嵌套 `-R` 拿到端口，嵌套 exec 输出 `NESTED_SSH_OK` |
| publickey / agent / 加密私钥 | ✅ | 认证耗时 73–86 ms |
| known_hosts | ⚠️ 部分 | 精确匹配可用，通配/`@cert-authority`/`@revoked` 有缺陷（§8） |
| 失败原因分类 | ✅ 优于旧实现 | 结构化 `russh::Error` |
| tokio 与同步 TUI 共存 | ✅ | 后台线程 + `std::sync::mpsc`，UI 线程零 tokio（§13） |

## 2. russh 库本身（0.65.1 时点的元数据）

| 项 | 值 |
|---|---|
| 版本 / License | 0.65.1（2026-11-03）/ Apache-2.0，仓库 `Eugeny/russh` |
| 下载量 | 累计约 1,000 万，近 90 天约 443 万 |
| 维护活跃度 | 近 6 个月约 154 次提交，含持续的安全与兼容修复；README 列出 Warpgate / Devolutions Gateway / Yazi / Sandhole 等生产采用者（Sandhole 明说用 `russh` 做反向转发） |
| 节奏 | 约 2–4 周一个次要版本，**0.x 无 semver 保证** → 锁 minor |
| README 声明的转发特性 | `direct-tcpip`（本地）/ `forward-tcpip`（远程）/ `direct-streamlocal` / `forward-streamlocal` |
| 相关 API | `client::Handle::channel_open_direct_tcpip`、`tcpip_forward`、`client::connect_stream`、`keys::known_hosts`、`keys::agent::AgentClient` |
| `Handle` 生命周期 | **不是 `Clone`**，多 task 共享必须 `Arc<Handle>`（20 并发 channel 实测 OK） |
| `Channel::into_stream()` | 返回同时实现 `AsyncRead + AsyncWrite` 的流，省掉手写 `data()/wait()` 循环——这一条极大降低了胶水成本 |

**`connect_stream` 要求流是 `'static`**：`into_stream()` 的结果必须整个 move 进去，不能传 `&mut`。

### 2.1 当时 open 的三个 issue（都是误判源或真实摩擦）

| issue | 状态 | 读法 |
|---|---|---|
| **#183**「Client Callback For TCPIP-Forward Missing」 | 2023-09 开，至今 open | **不是 `-R` 缺失**。诉求是「服务端主动向客户端发起反向转发」这一 RFC 4254 不支持的方向；作者自述的 workaround 就是标准 `-R` |
| **#541**「0.53.0 build on Windows」 | 2025-07 开，至今 open | **真实摩擦**：默认 `aws-lc-rs` 需要 NASM + CMake。用 `ring` 后端规避（§10） |
| **#177**「`AgentClient::connect_uds` on Windows」 | 2023-09 开，至今 open | 指的是在 Windows 上提供 **UDS 语义**；named pipe 本身没缺，`connect_named_pipe` 不受影响 |

### 2.2 与备选方案的三维对比（当时的实测口径）

| 维度 | **russh 0.65.1** | **ssh2 0.9.7**（libssh2） | **系统 `ssh` 子进程** |
|---|---|---|---|
| 运行期外部依赖 | ✅ **零**（静态链接进 .exe） | ✅ 零（需 vendored OpenSSL 否则要 DLL） | ❌ 需要 `ssh` 在 PATH |
| 构建期依赖 | ⚠️ C/C++ 编译器（`ring` 后端可省 NASM/cmake） | ⚠️ C 编译器 + **OpenSSL 头文件/库** | ✅ 无 |
| `-L` / `-R` | ✅ / ✅（实测跑通） | ✅ / ✅（`Listener::accept` 比 russh 更"成品"） | ✅ 原生 |
| `-D` | ⚠️ SOCKS5 自写（~60 行） | ⚠️ 同 | ✅ 原生 |
| ssh-agent | ✅ Unix UDS + Windows Pageant + named pipe | ✅ `userauth_agent` | ✅ 原生 |
| known_hosts | ⚠️ 有模块但四处误判（§8） | ⚠️ `KnownHosts` 同样需自己补语义 | ✅ 完整语义 |
| `~/.ssh/config` | ❌ 不在 russh 内（用 `ssh2-config`） | ❌ 完全没有 | ✅ 完整语义 |
| ProxyJump | ⚠️ 自搭链（~20 行/跳，实测可行） | ❌ **libssh2 协议层无此能力** | ✅ 原生 |
| 失败原因精度 | ⚠️ 枚举偏粗（`Disconnect` 尤其） | ⚠️ `Error` + errno | ✅ stderr 原文最精确 |
| 流量/字节统计 | ✅ 可精确到 channel | ✅ 可 | ❌ 需解析输出或读 `/proc` |
| 必需运行时 | **tokio** | 同步 | 无 |
| unique crates | 142（aws-lc）/ 136（ring） | 未精确测 | 92 |
| release 体积增量 | 实测 **+3.59 MB** | 未实测（通常 +1.5–3 MB） | 0 |
| Windows 交付 | ⚠️ 需 MSVC 构建，产物干净 | ⚠️ vcpkg/OpenSSL 更麻烦 | ✅ 依赖用户已装 OpenSSH |

**决策**：全量替换为 russh（理由与判据见 [pure-rust-ssh-plan.md](pure-rust-ssh-plan.md) §2）。
"Linux 用系统 ssh、Windows 用 russh"的混合方案被否决——两条代码路径 = 两套语义、两套 bug。

### 2.1 `-R` 不是能力缺口（易误判点）

`russh::server::Handler::tcpip_forward`（服务端决定是否监听）与
`russh::client::Handler::server_channel_open_forwarded_tcpip`（服务端开 `forwarded-tcpip`
channel 回来，**这正是客户端 `-R` 的入口**）名字容易混淆。

GitHub issue **#183** 的标题看着像"客户端 `-R` 缺失"，读原文可知其诉求是**服务端主动向客户端
发起反向转发**（RFC 4254 不支持的方向）；issue 里作者自述的 workaround 恰好就是标准 `-R`。
当前 russh 对 `ssh -R` 语义是完整的。

`server_channel_open_forwarded_tcpip` 的 `reply` 必须 `reply.accept().await`——**drop 掉
`reply` 等于自动拒绝**（源码注释明写）。

## 3. `-L` 实测数字

| 指标 | 值 |
|---|---|
| 认证耗时 | 73 ms |
| channel 打开 | 202 ms（首次，含 TCP 建连） |
| RTT（4 字节回环 ×20） | min 317 µs / median 367 µs / max 657 µs |
| 批量吞吐 | **48.8 MiB/s**，32 MiB 零丢失 |

## 4. `-R` 边界情况

| 场景 | 实测 |
|---|---|
| `tcpip_forward(addr, 0)`（请服务端分配） | `Ok(port)`，返回真实端口 ✅ |
| `tcpip_forward(addr, 非 0 端口)` 成功 | **`Ok(0)`** —— 不是请求的那个端口 |
| 端口已被占用 / 重复请求同一端口 | `Err(RequestDenied)` |
| 特权端口 80（非 root） | `Err(RequestDenied)` |
| `cancel_tcpip_forward` 已释放的端口 | `Err(RequestDenied)` |
| `tcpip_forward("0.0.0.0", 0)`，sshd `GatewayPorts no` | `Ok(port)`，但**静默降级**为只监听 `127.0.0.1` + `[::1]`（`ss -ltn` 确认） |

> ⚠️ **`Ok(0)` 是真实坑**：符合 RFC 4254（成功回复仅在 `port == 0` 时带端口字段）。
> 直接 `p as u16` 当端口用会得到 0，而远端其实已在请求的端口上监听。
> 封装原则：请求端口非 0 用请求值；请求 0 用返回值。

**顺带收益**：旧实现靠解析 `ssh -v` 的 `remote forward success for` 判 `-R` 就绪，而
`Entering interactive session.` 会**早于**监听结果打印，造成假成功。`tcpip_forward()` 是 await
真实的 `GLOBAL_REQUEST` 回复，返回 `Ok` 即代表远端已 listen——**这类 bug 从根上消失**。

## 5. `-D` 动态转发（SOCKS5 必须自写）

russh 源码中**零** SOCKS 相关代码（`grep -rni socks` 无输出）。但 `-D` 在协议层就是
"本地监听 + 每个 SOCKS CONNECT → `direct-tcpip`"，缺的只是握手那一层。

实测手写最小可用实现约 60 行：`05 NMETHODS METHODS` → `05 00`（NOAUTH）→
`05 01 00 ATYP ADDR PORT` → `05 00 00 01 0.0.0.0 0` → `copy_bidirectional`。

| 边界 | 处理 |
|---|---|
| ATYP `0x01` IPv4 / `0x03` 域名 | 已实测通过 |
| ATYP `0x04` IPv6 | 需补 16 字节解析 |
| `BIND` / `UDP ASSOCIATE` | `ssh -D` 不支持，应回 `0x07 Command not supported` |
| 建 channel 失败 | 应回 SOCKS 失败码而不是直接断开 |

备选 `fast-socks5`（SOCKS5 client+server，可自定义出站）会带进额外抽象；自写 60 行零新增依赖
且能精确映射到 `FailureKind`，实测首选。`tokio-socks` 方向相反（把流量转出 SOCKS 代理），不适用。

## 6. ProxyJump

「在一段 channel 上跑一整个 SSH 会话」：`jump.channel_open_direct_tcpip(target, 22)` →
`ch.into_stream()` → `client::connect_stream(cfg, stream, Handler)` → 内层认证。多跳就是递归。

| 项 | 工作量 |
|---|---|
| 单跳嵌套 | ~20 行 |
| 多跳链（A→B→C→D） | ~50 行，循环 + 保存最内层 Handle |
| 跳板机自己的 `ProxyJump`/`IdentityFile`/`User` 递归 resolve | **要自己实现** |
| 跳板机断开 | 外层断则内层必断，重连要重建整条链 |
| `ProxyCommand` | 仍要 fork 外部进程，纯 Rust 目标下无法摆脱（多数用户用 ProxyJump） |

## 7. 认证与密钥格式

| 方式 | 支持 | 备注 |
|---|---|---|
| publickey（含加密 ed25519/RSA + passphrase） | ✅ | `load_secret_key(path, Some(pass))`；密码错误报 `cryptographic error` |
| RSA 自动协商 hash | ✅ | 必须调 `best_supported_rsa_hash()`，否则可能用 SHA-1 被拒 |
| ssh-agent（Unix） | ✅ | `AgentClient::connect_env()` 读 `SSH_AUTH_SOCK` |
| ssh-agent（Windows） | ✅（源码确认，未实机） | `connect_pageant()` 或 `connect_named_pipe("\\.\pipe\openssh-ssh-agent")`；**"该用哪个"的探测要自己写** |
| password / keyboard-interactive | ✅ API 都在 | 多轮 `InfoRequest` 需自写状态机（2FA） |
| OpenSSH 证书 | ✅ API 在 | 未实测（未生成 CA 证书） |

密钥格式实测全通过：ed25519、rsa 2048/4096、ecdsa 256/384/521、PKCS8(PEM)、加密 key + 正确
passphrase；加密 key 无/错 passphrase 给出明确错误（「The key is encrypted」/「cryptographic error」）。

> `AgentClient::connect_env()` 在 `SSH_AUTH_SOCK` 指向失效 socket 时 `Connection refused`。
> 生产需要「有 env → 试 UDS；失败 → 回落 Pageant/named pipe」探测链。

## 8. known_hosts —— 本节是库里最危险的缺陷

`russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path}` 精确匹配正常，
但有四个实测会咬人的边界（**这一节是 `src/ssh/known_hosts.rs` 自研匹配的依据**）：

| 条目 | OpenSSH 期望 | russh 实测 |
|---|---|---|
| `*.0.0.1` / `*.example.com`（通配） | 匹配 | ❌ `Ok(false)` |
| Tab 分隔 `host\talgo\tkey` | 匹配 | ❌ `Ok(false)` 漏判 |
| 行首有空格的条目 | 匹配 | ❌ `Ok(false)` 漏判 |
| `@revoked` + 同 key 的正常条目 | **必须拒绝** | ❌ **`Ok(true)` —— 接受已吊销的密钥** |
| `@cert-authority` | 按 CA 校验 | ❌ 不识别，判为未知主机 |
| `[h]:22200`、逗号列表、`|1|` hashed、同 host 换 key → `KeyChanged{line}` | — | ✅ 正确 |

复现证据（原样）：

```text
@revoked 条目单独存在        -> Ok(false)   碰巧安全（理由是"没匹配到"，不是"已吊销"）
@revoked + 同 key 正常条目   -> Ok(true)    ❌ 严重：接受被吊销的密钥
```

根因（`keys/known_hosts.rs` 的 `match_hostname()`）：只做「精确字符串相等 + `|1|` 哈希校验」，
**没有 glob**；`known_host_keys_path()` 用 `split(' ')` 切分（所以 Tab 就断了）；**从未检查
`@revoked` / `@cert-authority`**（只跳 `#` 注释，`@` 行被当普通行解析、marker 字段被丢弃）。

**为什么不能"打个补丁继续用"**：`check_known_hosts_path` 只返回 `bool`，而本项目需要区分
**未知主机（可提示 learn）/ key 变更（拒绝）/ revoked（拒绝）/ CA 签发（走证书校验）** 四种状态——
`Ok(false)` 除了"没有这条"什么都表达不了。所以自研匹配是**必需**，不是可选优化。

### 8.1 隐蔽陷阱：`ssh_key::PublicKey` 的 `PartialEq` 包含 comment

```text
learn 后立刻 check（key 带 comment）  -> Err(KeyChanged { line: 2 })
同一 key 不带 comment                 -> Ok(true)
```

线上收到的 key（`check_server_key` 的参数）**没有 comment**，所以真实路径安全；但测试与边界
场景会踩坑。对策：校验前统一 `key.set_comment("")`。

### 8.2 写回的三个局限

`learn_known_hosts_path` 实测：① 文件开头多一个空行（`SeekFrom::End(-1)` 在空文件上的行为）；
② 不遵循 `HashKnownHosts yes`；③ **不去重**（重复 learn 追加多条）。
`[host]:port` 格式、目录自动创建、末尾补换行都是对的。

## 9. 失败原因分类

| 场景 | 实测错误 | 映射 |
|---|---|---|
| 端口无人监听 | `IO(ConnectionRefused)` | `ConnectionRefused` |
| DNS 失败 | `IO(Custom{"failed to lookup address information"})` | `DnsFailure` |
| 本地端口被占 | `io::ErrorKind::AddrInUse` | `PortInUse` |
| 认证失败 | `AuthResult::Failure{remaining_methods}`（**不是 `Error`**） | `AuthFailed` |
| 远程转发被拒 | `Error::RequestDenied` | `RemoteForwardFailed` |
| 目标不可达 | `ChannelOpenFailure(ConnectFailed)` | `TargetUnreachable` |
| 主机密钥不匹配 | 由我们的 `check_server_key` 决定 | `HostKeyMismatch` |

**两个精度缺口**：

1. `ChannelOpenFailure::ConnectFailed` 同时覆盖"端口拒绝"与"DNS 失败"，
   `Error::from_u32()` 还**丢弃服务端给的 description 字符串**。服务端文本其实被解析出来了，
   只是传给 `Handler::channel_open_failure(... description ...)` 回调——**要精确原因必须实现这个回调**。
2. `Error::Disconnect` 过于笼统：非 SSH 对端、协议错误、服务端主动断开都归到它。嵌套 ProxyJump
   时"第二跳握手失败"只能拿到它，无法区分"跳板机关了 channel"还是"目标不是 SSH"。

另外 `Error::UnknownKey`（Display 为 "Unknown server key"）与 `keys::Error::KeyChanged` 是
**两个不同类型**；做校验时要把 `check_server_key` 里的原始 `keys::Error` 自己暂存，否则行号会丢。

## 10. 构建依赖与体积

| 后端 | 需要什么 | 同一句 `cargo check --target x86_64-pc-windows-gnu` 请求的外部工具 |
|---|---|---|
| `aws-lc-rs`（默认） | C/C++ 编译器 + cmake；Windows x86-64 还需 NASM | `59 × mingw32-gcc, 2 × NASM, 3 × CMake` |
| `ring`（**选用**） | C 工具链（Windows 上是 MSVC，CI 已预装） | `9 × mingw32-gcc`（无 nasm、无 cmake） |

ring 的发布包带 104 个预生成文件（其中 12 个预汇编 `.o`），`build.rs` 走 `PREGENERATED` 分支
**直接用 `.o` 而不调 nasm**——这是选它的决定性理由。推荐配置：

```toml
russh = { version = "0.65.1", default-features = false, features = ["ring", "flate2", "rsa"] }
```

> 这是**构建期**依赖，不是运行期：用户拿到的二进制仍自包含。只有 CI / 开发机需要编译器。
> GitHub 的 `windows-latest` 预装 MSVC，满足构建需求。

| 工程 | release 体积（strip+lto） | unique crates |
|---|---|---|
| 当时 sshpf（系统 ssh 版） | 1.63 MB | 92 |
| + russh(ring) + tokio + ssh2-config | **4.34 MB** | 142 |

冷编译 +11~22 s（16 核）。**本地无法验证「Windows + MSVC + ring」**（无 mingw 链接器）——
这是唯一必须靠 CI 说话的风险。

## 11. `~/.ssh/config` 求值

`russh-config` 只覆盖 10 个指令、无 `Include`/`Match`，不能直接用。实测 **`ssh2-config` 0.9.0
覆盖度足够**且零 C 依赖（名字有误导性，纯 Rust）。详见 [pure-rust-ssh-config.md](pure-rust-ssh-config.md)。

⚠️ 但它有一个**静默污染** bug：`Match` 行被当未知字段忽略后，**其后的选项继续写进上一个
`Host` 块**，用户看到错误的 `user`/`port` 且不报错。对策：解析前按行剔除 `Match` 块（约 25 行）+ 单测。
这也是 [../design.md](../design.md) D2 的由来。

## 12. 生命周期与并发

| 场景 | 实测 |
|---|---|
| abort accept 任务后端口释放 | **172 µs** |
| 握手中途 abort | 202 ms 内干净退出，无 hang |
| 20 次快速 connect/disconnect | 1.61 s，80 ms/次 |
| 30 次循环后 fd 数 | before=10 → after=9，**无泄漏** |
| 10 条隧道同时建立并各传数据 | **80.8 ms** 全部 up，10/10 数据正确 |

**80 ms 建 10 条**（并行，单条约 8 ms 摊薄）说明「一线程一 runtime」在本项目规模（几条到几十条）
完全够用，不需要共享 runtime 或连接池。取消语义也比子进程简单：`tokio::task::abort()` 即可。

> 顺带消除的风险：`PR_SET_PDEATHSIG` 在 `process::exit()` 时不触发（旧实现靠"flag + break"绕开），
> 新架构里这个坑不存在。

## 13. tokio 与同步 TUI 共存（迁移要先回答的问题）

旧 `app.rs` 明确写「deliberately no `tokio` here」。实测方案：tokio 独占**一个后台线程** +
`current_thread` runtime，用 `std::sync::mpsc` 把事件回灌；UI 线程保持 `recv_timeout(200ms)`。

**结论：既有 `TunnelEvent` + `mpsc` 事件模型可以原样保留**，`TunnelEvent::{Line, Exited,
StderrClosed, SpawnFailed}` 语义一一对应，UI 层几乎不用改。线程数从「1 个 stderr reader」
变成「1 个 runtime 线程」，量级相同。

### 复跑方法（原 `spike/` 工程已删除）

需要三样东西，任一临时目录即可：① 一次性 sshd（临时 ed25519 host key + 用户 key，
`AllowTcpForwarding yes`、`GatewayPorts no`、`PasswordAuthentication no`、监听 `127.0.0.1:22200`）；
② 一个 `russh 0.65.1` + `tokio` + `anyhow` 的探针工程；③ 对真实 sshd 跑 `-L`/`-R`/`-D`/ProxyJump，
再对本机 `~/.ssh/config` 跑 `ssh2-config` 差分对照 `ssh -G -F`。

## 14. 风险（当时的诚实清单）

| 风险 | 状态 | 缓解 |
|---|---|---|
| Windows + MSVC + ring 构建 | ❌ 本地未验证 | 先提只加依赖的 PR 让 CI 说话 |
| known_hosts `@revoked` 放行 | ✅ 已复现 | 自研校验（必做，§8） |
| `ssh2-config` Match 污染 | ✅ 已复现 | 预处理剔除（必做） |
| `tcpip_forward` 返回 `Ok(0)` | ✅ 已复现 | 封装（必做） |
| 真实环境 ProxyJump 多跳 | ⚠️ 只在本机双跳验证 | 阶段 2 用真实跳板机验证 |
| `sk-*` 硬件密钥 | ⚠️ 仅格式可解析 | 明确不支持并提示 |
| russh 0.x 无 semver | ⚠️ 2–4 周一版 | 锁 minor |
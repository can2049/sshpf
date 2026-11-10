# russh API 速查（0.65.1 核实）

> **这份文档回答**：russh 的哪个 API 对应哪项能力——三种转发、ProxyJump、认证、agent、known_hosts 的函数名与签名要点。
> **不负责**：实测数字与结论——见 [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md)（本文的每条结论在那里有证据）；当前实现——见 `src/ssh/`。
> **相关代码路径**：`src/ssh/client.rs`、`src/ssh/forwards.rs`、`src/ssh/socks5.rs`、`src/ssh/known_hosts.rs`。

> 历史核实（2026-11，russh 0.65.1 / OpenSSH_8.2p1）。**版本号已过期**，以 `Cargo.toml` 为准；
> 0.x 无 semver 保证，升级时逐条复核签名。这是"查 API 用哪一行"的对照表，不是教程。

## 0. 一页对照

| 能力 | API | 结论 |
|---|---|---|
| `-L` 本地转发 | `client::Handle::channel_open_direct_tcpip(host, port, orig_host, orig_port)` → `Channel` → `into_stream()` | ✅ 完整，无需胶水 |
| `-R` 远程转发 | `client::Handle::tcpip_forward(addr, port)` + `client::Handler::server_channel_open_forwarded_tcpip(...)` | ✅ 完整（**不是缺口**，issue #183 是另一个方向） |
| `-R` 取消 | `cancel_tcpip_forward(addr, port)` | ✅ |
| `-D` 动态转发 | 无内置；`channel_open_direct_tcpip` + 自写 SOCKS5（约 60 行） | ⚠️ 协议层够用，SOCKS5 自己写 |
| ProxyJump | `client::connect_stream(cfg, stream, handler)`，stream 来自 `channel.into_stream()` | ✅ 每跳约 20 行 |
| 本地 bind 探测 | `tokio::net::TcpListener::bind` | ✅ 比解析 stderr 更准 |
| 认证 | `authenticate_publickey` / `authenticate_password` / `authenticate_keyboard_interactive_start/respond` / `authenticate_openssh_cert` | ✅ 全在 `client::Handle` |
| ssh-agent | `keys::agent::client::AgentClient::{connect_env, connect_uds, connect_pageant, connect_named_pipe}` | ✅ 四平台入口都在 |
| known_hosts | `keys::known_hosts::{check_known_hosts_path, known_host_keys_path, learn_known_hosts_path}` | ⚠️ 有 API 但四处误判，且只返回 `bool` —— 本项目自研匹配 |
| 流式读写 | `Channel::into_stream()`（`AsyncRead + AsyncWrite`，要求 `'static + Send + Sync`） | ✅ 省掉手写 `data()/wait()` 循环 |
| 多 task 共享连接 | `Arc<client::Handle<H>>` | `Handle` **不是 `Clone`** |

## 1. 关键签名与用法陷阱

```rust
// -L：每个本地连接开一条 direct-tcpip channel
let ch = handle
    .channel_open_direct_tcpip(remote_host, remote_port as u32, peer.ip().to_string(), peer.port() as u32)
    .await?;
let mut stream = ch.into_stream();                     // AsyncRead + AsyncWrite
tokio::io::copy_bidirectional(&mut sock, &mut stream).await?;

// -R：请求远端监听；port=0 请服务端分配
let granted: u32 = handle.tcpip_forward("127.0.0.1", 0).await?;
//   回调里必须 reply.accept().await —— drop 掉 reply 等于自动拒绝

// ProxyJump：在已有 channel 上跑整个 SSH 握手
let ch = jump.channel_open_direct_tcpip(target, 22, "127.0.0.1", 0).await?;
let inner = client::connect_stream(Arc::new(cfg), ch.into_stream(), Handler2::new()).await?;

// RSA 必须先协商 hash，否则可能用 SHA-1 被拒
let hash = handle.best_supported_rsa_hash().await?.flatten();
```

| 陷阱 | 说明 |
|---|---|
| `tcpip_forward(addr, 非 0 端口)` 成功时返回 **`Ok(0)`** | 不是请求的端口。封装：请求端口非 0 用请求值，请求 0 用返回值 |
| `ChannelOpenFailure::from_u32()` **丢弃服务端 description** | 精确原因要靠 `Handler::channel_open_failure(... description ...)` 回调 |
| `Error::Disconnect` 过于笼统 | 非 SSH 对端、协议错误、主动断开都归它；嵌套 ProxyJump 时无法定位哪一跳 |
| `Error::UnknownKey` 与 `keys::Error::KeyChanged` 是**两个类型** | 校验时要自己暂存原始 `keys::Error`，否则 `KeyChanged` 的行号会丢 |
| `connect_stream` 要求 stream `'static` | `into_stream()` 后必须整个 move，不能传 `&mut` |
| `check_server_key` 返回 `false` 后 `connect()` 直接失败 | "ask 后用户接受"要在回调里 `await` 用户答复，不能先 false 再重连 |
| `keys::Error::KeyChanged{line}` | 专为"密钥不匹配 = 可能 MITM"设计，带行号，可直接填 `FailureKind::HostKeyChanged` |

## 2. 认证是否齐全

| 方式 | API 存在 | 实测 |
|---|---|---|
| publickey（含加密 ed25519/RSA + passphrase） | ✅ | ✅ 用真实 sshd 跑通 |
| RSA 自动 hash 协商 | ✅ `best_supported_rsa_hash()` | ✅ |
| ssh-agent | ✅ `AgentClient` | ✅ Unix 实测通过；Windows 仅源码确认 |
| password | ✅ | API 正常，当时 sshd 配置禁用 |
| keyboard-interactive（2FA） | ✅ 多轮 `InfoRequest` | 需自写状态机 |
| OpenSSH 证书 / GSSAPI | ✅ | 未实测 |

## 3. 与 `ssh2` / 系统 `ssh` 的一句话区别

| | russh | ssh2（libssh2） | 系统 `ssh` |
|---|---|---|---|
| 纯 Rust / 运行期依赖 | Rust，加密后端是 C（`ring` 可免 NASM） | C 库 + OpenSSL | 外部二进制 |
| ProxyJump | 自搭链（可行） | ❌ 协议层无此能力 | ✅ 原生 |
| `~/.ssh/config` | ❌ 不在 russh 内 | ❌ | ✅ |
| 失败原因 | 结构化枚举（偏粗） | `Error` + errno | stderr 原文最精确 |

完整三维对比与实测数字见 [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) §2.2。
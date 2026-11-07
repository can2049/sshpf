# 调研：用 russh 纯 Rust 实现 SSH 转发（替代系统 `ssh` 子进程）

> 调研对象：`sshpf`（本仓库）从 `std::process::Command` 调用系统 `ssh -N -L/-R/-D` +
> 解析 `ssh -v` stderr，迁移到纯 Rust SSH 库的可行性。
>
> **本文所有结论都经过实测**，实测环境：Ubuntu / OpenSSH_8.2p1 / rustc 1.99.1 /
> russh 0.65.1。可复现脚本与源码在 `/tmp/russh-probe`（临时工程，未污染仓库）。
> 凡是未能实际验证的条目，文中明确标注「未确认」。

---

## 0. 结论速览（先看这里）

| 问题 | 答案 |
|---|---|
| russh 能做 `-L` 本地转发吗？ | ✅ 可以，`channel_open_direct_tcpip`。已在**真实 OpenSSH sshd** 上跑通端到端流量 |
| russh 能做 `-R` 远程转发吗？ | ✅ **可以**。客户端 `Handle::tcpip_forward()` + `Handler::server_channel_open_forwarded_tcpip()`。已实测跑通真实流量，`port=0` 服务端分配端口也支持 |
| russh 能做 `-D` 动态转发吗？ | ⚠️ **协议层支持，SOCKS5 需自己写**（或引入 `fast-socks5`）。russh 源码中**零** SOCKS 相关代码 |
| 能摆脱系统 `ssh` 二进制吗？ | ✅ 运行时可以完全摆脱。**但构建期仍需要 C 工具链**（见 §1.4），这是「无外部依赖」的唯一破口 |
| 最大风险 | ① 加密后端强制 C 编译器/Windows 需 NASM；② `~/.ssh/config` 语义要重写；③ known_hosts 不支持通配符 |
| 结论 | **russh 足以支撑三种转发**，转发协议层没有缺口。真正的工程量在转发之外：config 语义、known_hosts、SOCKS5、进程模型改造 |

---

## 1. 版本、维护活跃度、平台与构建依赖

### 1.1 版本与下载量

数据来源：crates.io API（`https://crates.io/api/v1/crates/russh`），抓取时间 2026-11-07。

| 指标 | 值 |
|---|---|
| 最新稳定版 | **0.65.1** |
| 发布时间 | **2026-11-03**（crates.io `created_at`） |
| 累计下载量 | 9,993,956 |
| 近 90 天下载量（`recent_downloads`） | 4,429,092 |
| 按日聚合的月下载量（`version_downloads` 的 `extra_downloads`） | 2026-08: 825,239 / 2026-09: 1,058,049 / 2026-10: 878,748 |
| 峰值日均下载 | 约 28k–52k/天（2026-10 日均 28,346） |
| 历史版本数 | 134 |
| License | **Apache-2.0** |
| 仓库 / 文档 | https://github.com/Eugeny/russh （`warp-tech/russh` 已 301 重定向到 `Eugeny/russh`） / https://docs.rs/russh |

版本节奏（近 10 个版本，实测 `created_at`）：0.63.0 (08-21) → 0.63.1 (08-23) →
0.63.2 (09-03) → 0.63.3 (09-09) → 0.64.0 (10-04) → 0.64.1 (10-05) →
0.65.0 (10-25) → 0.65.1 (11-03)。**约 2–4 周一个次要版本**，无长期停滞迹象。

### 1.2 维护活跃度

GitHub API（`/repos/Eugeny/russh`）实测：

| 指标 | 值 |
|---|---|
| stars / forks | 1,930 / 324 |
| open issues | 77 |
| `pushed_at` | 2026-11-03T20:44:49Z（与最新版发布同日） |
| archived | false |
| 最近 30 次提交的时间跨度 | 2026-09-09 → 2026-11-03（约 8 周，30 次提交） |
| **近 6 个月提交数** | **154**（`since=2026-05-07&until=2026-11-07`，GitHub API pagination `last=154`） |

最近 6 个月提交（抽样，来自 `/commits?per_page=30`）显示持续的安全与兼容性修复，
而非仅依赖升级：

- `fix(cipher): count the MAC tag in the minimum packet length check (#802)`
- `fix(server): let Dropbear through strict kex by accounting for the ignored first_kex_packet (#…)`
- `fix(AgentClient): propagate method errors (#808)`
- `feat(client): support OpenSSH agent session binding (#794)`
- `fix(agent): honor SSH_AGENT_RSA_SHA2_* flags when signing with RSA keys (#796)`
- `ci: harden against supply chain attacks (#817)`

**判读：维护活跃、有安全响应能力、有多个生产级 adopters**（README 列出 Warpgate、
Devolutions Gateway、Yazi、Sandhole、kty 等）。Sandhole 明确写着"Uses `russh::server`
for reverse forwarding connections, local forwarding tunnels"——这是 `-R` 能力已被
第三方生产项目使用的旁证。

README 的 Features 一节原文列出：

```
- Local port forwarding (`direct-tcpip`)
- Remote port forwarding (`forward-tcpip`)
- Local UNIX socket forwarding (`direct-streamlocal`, client only)
- Remote UNIX socket forwarding (`forward-streamlocal`)
```

### 1.3 Windows 支持

**支持，但不是零工具链。**

- `russh/Cargo.toml.orig` 有 `[target.'cfg(windows)'.dependencies] pageant = ...`，
  以及 `#[cfg(windows)] allow_named_pipe` 路径（见 §5）。
- 官方 CI（`.github/workflows/rust.yml`）有 `Build-Windows` job，`runs-on: windows-2025`，
  **并且显式 `choco install nasm`**。这是关键信号：Windows 构建需要 NASM。
- 相关 issue：**#541「Surprising behavior of 0.53.0 build on Windows」**（2025-07-06 开，
  **至今 open**），原文：「With default features, the build fails on a custom build script
  for aws-lc-rs. The error suggests that aws-lc-rs expects NASM and cmake, neither of which
  are present by default on Windows.」

### 1.4 构建依赖：**这是本节最重要的发现**

russh 的两个加密后端**都**需要一个 C（C++）编译器：

| 后端 | feature | 需要什么 | 来源 |
|---|---|---|---|
| `aws-lc-rs`（**默认**） | `aws-lc-rs` | C/C++ 编译器；Windows x86-64 还需 **NASM**（或启用 `prebuilt-nasm` feature / `AWS_LC_SYS_PREBUILT_NASM=1`） | aws-lc-rs README：「Consuming projects will need a C/C++ compiler to build」「For Windows x86 and x86-64, NASM is required for assembly code compilation」 |
| `ring`（备选） | `ring` | C 工具链 + Windows 需 Visual Studio C++ build tools | ring BUILDING.md：「*ring* currently requires a C (but not C++) toolchain」 |

本地实测 `cargo tree`：

```
# 默认 features（aws-lc-rs）
aws-lc-rs v1.19.0
└── aws-lc-sys v0.47.0
    ├── cc v1.11.1
    └── cmake v0.1.58        <- 注意：cmake 在依赖树里（非 FIPS 构建「不需要」它，但依赖存在）

# default-features = false, features = ["ring"] 时
ring v0.17.14
└── cc v1.11.1
```

**对 Windows 交付的实际影响**：用户「装了 .exe 就能用」是可以的（C 依赖是**构建期**的，
不是运行期）；但 CI/开发机必须装 MSVC Build Tools + NASM。这与当前仓库
`sshpf` 的构建模型（纯 Rust、`ubuntu-latest` + `windows-latest` 直编）有冲突——
现有 CI 的 windows job 没有 NASM 步骤，迁移时必须补。

> 缓解方案：`russh = { default-features = false, features = ["ring","flate2","rsa"] }`
> 可以去掉 cmake/NASM 依赖。ring 在 Windows x86-64 用 crate 内置的预编译汇编对象
> （ring BUILDING.md：「the packaged crate contains precompiled object files for the
> assembly language modules so no macro assembler」）。这是**推荐配置**。

**根依赖规模实测**（`cargo tree --prefix none | awk '{print $1}' | sort -u | wc -l`）：

| 工程 | unique crates |
|---|---|
| 仅 tokio + anyhow（基线） | 13 |
| russh + tokio（默认 features） | **142** |
| russh + tokio（ring 后端） | **136** |
| 当前 sshpf（无 tokio） | 92 |

注意：`docs/research/tech-options.md` 里写的「203 个包」是包含了完整 tokio `full`
feature 的数字；本处 142/136 是精简 tokio feature 后的实测值。

---

## 2. `-L` 本地转发

### 2.1 结论

✅ **完整支持，无需协议层胶水**。API 是 `russh::client::Handle::channel_open_direct_tcpip`。

### 2.2 调用链（已在真实 OpenSSH sshd 上跑通）

四步：

1. `russh::client::connect(config, addr, handler)` → `Handle<H>`
2. `handle.authenticate_publickey(...)` / `authenticate_password(...)` → `AuthResult`
3. 本地 `tokio::net::TcpListener` 接受连接
4. 每个连接 `handle.channel_open_direct_tcpip(target_host, target_port, orig_host, orig_port)`
   → `Channel<Msg>`，然后 `Channel::into_stream()` 得到 `AsyncRead + AsyncWrite`，
   直接 `copy_bidirectional`

关键 API 签名（源码 `src/client/mod.rs:887`）：

```rust
pub async fn channel_open_direct_tcpip<A: Into<String>, B: Into<String>>(
    &self,
    host_to_connect: A,
    port_to_connect: u32,
    originator_address: B,
    originator_port: u32,
) -> Result<Channel<Msg>, crate::Error>
```

完整可编译片段（`/tmp/russh-probe/src/bin/interop.rs` 的实际代码，已跑通）：

```rust
use std::sync::Arc;
use russh::client::{self, Msg};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::*;
use tokio::net::{TcpListener, TcpStream};

struct SshpfHandler;
impl client::Handler for SshpfHandler {
    type Error = russh::Error;
    // 必须实现：返回 true 表示接受该主机密钥（对接 known_hosts 见 §6）
    async fn check_server_key(&mut self, _k: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

async fn local_forward(ssh_port: u16, target: &'static str, target_port: u16) -> anyhow::Result<()> {
    let cfg = Arc::new(client::Config { nodelay: true, ..Default::default() });
    let mut handle = client::connect(cfg, ("127.0.0.1", ssh_port), SshpfHandler).await?;

    let key = Arc::new(load_secret_key("/path/to/id_ed25519", None)?);
    let hash = handle.best_supported_rsa_hash().await?.flatten(); // RSA 才需要，Ed25519 传 None 也可
    let res = handle
        .authenticate_publickey("user", PrivateKeyWithHashAlg::new(key, hash))
        .await?;
    anyhow::ensure!(res.success(), "auth failed");

    let handle = Arc::new(handle); // Handle 不是 Clone，共享用 Arc
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let bound = listener.local_addr()?;

    loop {
        let (mut sock, peer) = listener.accept().await?;
        let handle = handle.clone();
        tokio::spawn(async move {
            let ch = handle
                .channel_open_direct_tcpip(target, target_port as u32,
                                           peer.ip().to_string(), peer.port() as u32)
                .await
                .expect("direct-tcpip open");
            // Channel::into_stream() 实现了 AsyncRead + AsyncWrite,
            // 不用手写 data()/wait() 的双向循环
            let mut stream = ch.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
        });
    }
}
```

### 2.3 实测结果

```
=== russh 0.65.1 client  vs  OpenSSH 8.2p1 sshd (interop) ===
1. publickey (ed25519, no passphrase)     auth.success()=true
   captured host key: alg=ssh-ed25519 sha256=SHA256:sGyBuVGs4+IUUT4OUJCm6o38knOtTFlgyIn4BeA3Q6Q
   -L  through real sshd  => ECHO:ping
```

`Channel::into_stream()`（`src/channels/mod.rs:663` + `channel_stream.rs`）返回
`ChannelStream<S>`，同时实现了 `AsyncRead` 和 `AsyncWrite`。**这一条极大降低了胶水成本**：
不用手写 `select!` 的 `channel.data()` / `channel.wait()` 循环。

另外实测 **20 个并发 `direct-tcpip` channel 共享一个 `Arc<Handle>`，20/20 成功**——
`-D` 的动态扇出没有锁竞争问题。

### 2.4 失败面（用于替代现有的 `classify_failure`）

实测对照（`/tmp/russh-probe/src/bin/redge.rs`）：

| 场景 | russh 返回值 | 能否区分原因 |
|---|---|---|
| 远端目标端口无人监听 | `ChannelOpenFailure(ConnectFailed)` | ✅ 明确 |
| 远端 DNS 解析失败 | `ChannelOpenFailure(ConnectFailed)` | ⚠️ **与端口拒绝同码**，需自己看 description |
| 服务端禁止转发 | `ChannelOpenFailure(AdministrativelyProhibited)` | ✅ 明确 |
| 本地 bind 失败 | 由你自己的 `TcpListener::bind` 返回 `io::Error` | ✅ 明确（比 ssh 的 stderr 更准） |

**关键局限**：`ChannelOpenFailure::from_u32()` **丢弃了服务端给的 description 字符串**
（`src/lib_inner.rs:420`，只有 `Other { code, reason }` 会带上，且 reason 是本地拼的
`format!("Unknown code {code}")`，不是服务端文本）。而 `ChannelOpenFailure` 的 `Display`
就是 `Failed to open channel ({0:?})`。

但服务端文本其实**被解析出来了**——`src/client/encrypted.rs:563` 里的 `descr` 变量会
传给 `Handler::channel_open_failure(channel, reason, description, language, session)`
回调。所以**精确原因要靠实现这个回调拿到 `&str description`**，而不是靠 `Error` 枚举。
这一点必须写进设计，否则会丢掉「远端拒绝的具体理由」这类当前 README 引以为豪的能力。

---

## 3. `-R` 远程转发 —— 重点核实项

### 3.1 结论

✅ **russh 客户端支持 `-R`，不是能力缺口。** 这是本次调研最需要澄清的一点，
因为 `docs/design.md` 和 `docs/research/tech-options.md` 里对 `-R` 的表述含糊，
而 GitHub 上有一个 open issue 容易让人误判为「不支持」。

### 3.2 为什么容易误判

`russh::server::Handler::tcpip_forward` 和 `russh::client::Handler::
server_channel_open_forwarded_tcpip` 分属两侧，名字容易混淆：

| 方向 | 谁调用 | API | 语义 |
|---|---|---|---|
| 客户端**发起** `-R` | `client::Handle` | `tcpip_forward(address, port)` | 发 `global request "tcpip-forward"`，让**服务端**监听 |
| 服务端**处理**请求 | `server::Handler` | `tcpip_forward(address, &mut port, session)` → `bool` | 服务端决定是否监听 |
| 服务端**通知**新连接 | `client::Handler` | `server_channel_open_forwarded_tcpip(...)` | 服务端开 `forwarded-tcpip` channel 回来 |
| 服务端**主动开** | `server::Handle` | `channel_open_forwarded_tcpip(...)` | 与上一行是同一件事的两端 |

任务描述里担心的「`server_channel_open_forwarded_tcpip` 回调是给 server 端的」是**误解**：
它在 `src/client/mod.rs:2921`，是 `client::Handler` 的方法，**正是客户端 `-R` 的入口**。
`src/client/encrypted.rs:927` 明确把收到的 `ChannelType::ForwardedTcpIp` 分派给它。

### 3.3 关于 GitHub issue #183

「**Client Callback For TCPIP-Forward (Remote Port Forwarding) Missing**」
（`Eugeny/russh#183`，2023-09-15 开，**至今 open**）。

读原文可以确认它**描述的是 2023 年的状态，且诉求与 `-R` 无关**：作者想要的是
「服务端主动向客户端发起反向转发」（RFC 4254 里不支持的方向）。issue 里他自己写的
workaround 恰好就是标准 `-R` 的做法：

> 「Without this functionality, the server can't call reverse port forwards to the client,
> unless done through a custom task that the client needs to interpret, then open the
> listener, then call the client handle's `channel_open_direct_tcpip` to callback to the
> server once the tunnel has been established.」

而 Russh 现在提供的 `server_channel_open_forwarded_tcpip` 比这个 workaround 更直接。
评论（HealsCodes, 2024-03-11）也说：「With standard RFC conform client / server setups
it's always the client knowing which ports should be forwarded ... or server to client
([`-R` for openssh])」。**原 issue 作者诉求 = 非标准方向，不是 `ssh -R`。**

### 3.4 实测：`-R` 全链路跑通

`/tmp/russh-probe/src/bin/interop.rs` 对真实 OpenSSH sshd：

```
2. tcpip_forward (server-chosen port)    granted port 46579
   -R  through real sshd  => ECHO:ping
```

客户端代码核心：

```rust
// 1) 请求远端监听；port=0 表示请服务端分配，返回值是真实施加的端口
let remote_port: u32 = handle.tcpip_forward("127.0.0.1", 0).await?;

// 2) 服务端每来一个连接，就通过 handler 回调把 channel 交给我们
impl client::Handler for SshpfHandler {
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        // 把 channel 丢给一个 task，去连接本地目标
        let _ = self.tx.send((channel, connected_address.to_string(), connected_port));
        Ok(())
    }
}
```

`reply.accept()` **必须调用**：源码注释写明「Dropping `reply` automatically rejects」
（`src/client/mod.rs:2905` 附近）。

### 3.5 `-R` 边界情况实测

| 场景 | 实测结果 |
|---|---|
| `tcpip_forward(addr, 0)` 服务端分配端口 | `Ok(port)`，返回真实端口 ✅ |
| `tcpip_forward(addr, 固定空闲端口)` | **`Ok(0)`**（不是返回该端口！） |
| 端口已被占用 | `Err(RequestDenied)` |
| 特权端口 80（非 root） | `Err(RequestDenied)` |
| `cancel_tcpip_forward` 已释放的端口 | `Err(RequestDenied)` |
| `tcpip_forward("0.0.0.0", 0)` | `Ok(port)`，但 sshd `GatewayPorts no` 时**静默降级**为只监听 `127.0.0.1` + `[::1]`（`ss -ltn` 实测确认） |

⚠️ **`port != 0` 时返回 `0` 是一个真实的坑**。源码 `src/client/encrypted.rs:983-1006`：

```rust
Some(GlobalRequestResponse::TcpIpForward(return_channel)) => {
    let result = if r.is_empty() {
        // If a specific port was requested, the reply has no data
        Some(0)
    } else { /* decode u32 */ };
    let _ = return_channel.send(result);
}
```

这符合 RFC 4254（成功回复仅在 `port == 0` 时带端口字段），但**调用方必须知道**：
拿到 `Ok(0)` 要理解成「按我请求的端口成功了」，而不是「分配到了 0 号端口」。
如果直接 `let p = handle.tcpip_forward(a, b).await?` 然后 `p as u16` 当端口用，
会得到 0 并且远端其实已经在 `b` 上监听——这是很容易写出的 bug。

### 3.6 `-R` 与现有实现的语义对照

当前 `sshpf` 对 `-R` 的判定依赖 `remote forward success for`，并且 README 里明确记录
「`Entering interactive session.` 会**先于**远端监听结果打印，只看它会误判成功」。

改用 russh 之后这个问题**从根上消失**：`tcpip_forward()` 是 `await` 一个
`GLOBAL_REQUEST` 的 reply（服务端在 `want_reply` 下必须回
`REQUEST_SUCCESS`/`REQUEST_FAILURE`），返回 `Ok` 就代表远端**已经 listen 成功**。
不再有「进程活着但隧道没用」的假信号。这是迁移的一个实打实的收益。

---

## 4. `-D` 动态转发（SOCKS5）

### 4.1 结论

⚠️ **russh 完全不内置 SOCKS5。** 源码 grep 实测：

```bash
$ grep -rni "socks" /tmp/russh-src/russh-0.65.1/src/
# （无输出）
```

但 `-D` 在协议层就是「本地监听 + 每个 SOCKS 请求 → `direct-tcpip`」，
russh 提供的 `channel_open_direct_tcpip` 已经够用，**缺的只是 SOCKS5 握手那一层**。

### 4.2 工作量评估

实测手写一个最小可用 SOCKS5（NOAUTH + CONNECT + IPv4/域名两种 ATYP）**约 60 行**：

```rust
async fn socks5(mut sock: TcpStream, handle: Arc<client::Handle<SshpfHandler>>) -> anyhow::Result<()> {
    // 1) 版本/方法协商
    let mut hdr = [0u8; 2];
    sock.read_exact(&mut hdr).await?;
    anyhow::ensure!(hdr[0] == 5, "not socks5");
    let mut methods = vec![0u8; hdr[1] as usize];
    sock.read_exact(&mut methods).await?;
    sock.write_all(&[5, 0]).await?; // 选 NOAUTH

    // 2) 请求：VER CMD RSV ATYP
    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await?;
    anyhow::ensure!(req[0] == 5 && req[1] == 1, "unsupported");
    let (host, port) = match req[3] {
        1 => { /* IPv4 */ }
        3 => { /* 域名：1 字节长度 + 名字 */ }
        4 => anyhow::bail!("IPv6 ATYP 4"), // 需自行补
        t => anyhow::bail!("ATYP {t}"),
    };

    // 3) 建立 direct-tcpip，再回 SOCKS 成功响应
    let ch = handle.channel_open_direct_tcpip(host, port as u32, "127.0.0.1", 0).await?;
    sock.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;

    // 4) 裸字节双向搬运
    let mut stream = ch.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
    Ok(())
}
```

实测（对真实 sshd）：

```
-D  SOCKS5 domain     => ECHO:ping
```

即 **域名 ATYP (0x03) 也通了**，说明「客户端解析域名 + 让服务端连 IP」和
「把域名原样交给服务端连」两种策略都可行（上面代码是后者）。

### 4.3 需要补的边界

最小实现之外，生产可用还需：

| 项 | 说明 |
|---|---|
| ATYP=4（IPv6） | 上面代码未实现，需补 16 字节地址解析 |
| BIND / UDP ASSOCIATE | `ssh -D` 只支持 CONNECT，两者都应回 `0x07 Command not supported` |
| 认证 | `ssh -D` 默认 NOAUTH；若要做用户/密码（0x02）需额外协商 |
| 失败响应码 | 建 channel 失败时应回 SOCKS 回复码（0x05 Connection refused 等），而不是直接断开 |
| 半关闭 | `copy_bidirectional` 已处理 EOF；但 SOCKS 客户端常在收到 reply 前就发数据，需注意缓冲 |

### 4.4 现成 crate 备选

若不想手写，可用现成 SOCKS5 库，只把"连出去"这一步接到 russh：

| crate | 最新版 | 下载量（累计 / 近 90 天） | 用途 |
|---|---|---|---|
| `fast-socks5` | 1.0.0 | 2,550,432 / 506,947 | SOCKS5 client+server，可自定义出站连接 |
| `socks5-impl` | 0.10.3 | 461,564 / 78,415 | 协议抽象层，需要自己写 IO 胶水 |
| `tokio-socks` | 0.5.3 | 25,015,020 / 4,230,273 | **这是 SOCKS 客户端**（把流量从 SOCKS 代理转出去），方向相反，不适用 |

**建议**：本项目 SOCKS5 只需 CONNECT，60 行手写比引入 `fast-socks5`（会带进额外的
tower/异步抽象）更可控，且能精确控制错误映射到 UI 的 `FailureKind`。

---

## 5. 认证方式支持矩阵

全部 API 位于 `russh::client::Handle`（`src/client/mod.rs`）。

| 认证方式 | russh API | 实测结果（真实 sshd） | 备注 |
|---|---|---|---|
| publickey（未加密 ed25519） | `authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, None))` | ✅ `success()=true` | |
| publickey（**加密 RSA + passphrase**） | 同上，key 由 `load_secret_key(path, Some("pass"))` 加载 | ✅ `success()=true` | passphrase 错误 → `SshKey: cryptographic error`（实测） |
| publickey（**加密 ed25519 + passphrase**） | 同上 | ✅ `success()=true` | |
| publickey（RSA，自动协商 hash） | `best_supported_rsa_hash().await?` → `.flatten()` | ✅ | 必须调，否则 RSA 用 `ssh-rsa`/SHA-1 可能被拒 |
| publickey（ssh-agent） | `AgentClient::connect_env()` / `connect_uds(path)` + `authenticate_publickey_with(user, pubkey, hash, &mut agent)` | ✅ `success()=true`（实测 agent 里放 ed25519 后认证成功） | `Signer` trait 唯一实现就是 `AgentClient` |
| password | `authenticate_password(user, pass)` | ⚠️ 服务器禁用了 password，返回 `Failure { remaining_methods: MethodSet([PublicKey, KeyboardInteractive]), partial_success: false }` | API 存在且工作，只是本次 sshd 配置不允许 |
| keyboard-interactive（2FA/MFA） | `authenticate_keyboard_interactive_start(user, submethods)` → 循环 `authenticate_keyboard_interactive_respond(vec![...])` | ⚠️ 同上，实测走通协议拿到 `Failure`（服务器禁用） | 支持多轮 `InfoRequest`，可做 OTP/2FA |
| OpenSSH certificate | `authenticate_openssh_cert(user, Arc<PrivateKey>, Certificate)` + `load_openssh_certificate(path)` | 未实测（未生成 CA 签发证书） | API 已确认存在，且示例 `client_open_direct_tcpip.rs` 里演示了用法 |
| certificate + agent 签名 | `authenticate_certificate_with(...)`（`src/client/mod.rs:680`） | 未实测 | 对应 `FutureCertificate` 方法 |
| none | `authenticate_none(user)` | 未实测 | |
| GSSAPI / Kerberos | `authenticate_gssapi_with_mic(...)` | 未实测 | 需要自己实现 `GssapiAuthenticator`，与企业 SSO 集成时可考虑 |
| 公钥指纹/host key 弹窗 | `Handler::auth_banner` / 认证失败后的 `remaining_methods()` | ✅ 能拿到服务器支持的方法集 | 可用来提示"服务器只接受 publickey" |

### 5.1 Windows 上的 agent

源码 `src/keys/agent/client.rs` 实测确认三条路径：

```rust
#[cfg(unix)]
pub async fn connect_uds<P: AsRef<Path>>(path: P) -> Result<Self, Error>   // Unix socket
#[cfg(unix)]
pub async fn connect_env() -> Result<Self, Error>                          // 读 SSH_AUTH_SOCK

#[cfg(windows)]
pub async fn connect_pageant() -> Result<Self, Error>                      // Pageant (PuTTY)
#[cfg(windows)]
pub async fn connect_named_pipe<P: AsRef<OsStr>>(path: P) -> Result<Self, Error>  // Windows named pipe
```

`Cargo.toml.orig` 里 `[target.'cfg(windows)'.dependencies] pageant = { version = "0.2" }`。
**Windows 上 OpenSSH 的 agent 是 named pipe `\\.\pipe\openssh-ssh-agent`**，
`connect_named_pipe` 正是为它准备的。所以 Windows agent 支持的拼图是齐的，
但**「该用哪个」的探测逻辑要自己写**（Pageant vs OpenSSH agent vs 无 agent）。

⚠️ 有一个 open issue **#177「Implement AgentClient::connect_uds on Windows」**
（2023-09-08 开，仍 open）。它指的是**在 Windows 上提供 UDS 语义的 `connect_uds`**，
不是 named pipe 缺失。实际用 `connect_named_pipe` 不受影响。

---

## 6. 主机密钥校验（known_hosts）

### 6.1 结论

russh **内置** known_hosts 模块：`russh::keys::known_hosts`，从 russh-keys 合并而来
（现在是 russh 内的 `src/keys/known_hosts.rs`，不再是独立 crate）。

API（`src/keys/mod.rs:87` 也 re-export 了前两个）：

```rust
pub fn check_known_hosts(host: &str, port: u16, pubkey: &ssh_key::PublicKey) -> Result<bool, Error>
pub fn check_known_hosts_path(host, port, pubkey, path) -> Result<bool, Error>
pub fn known_host_keys(host, port) -> Result<Vec<(usize, ssh_key::PublicKey)>, Error>
pub fn known_host_keys_path(host, port, path) -> Result<Vec<(usize, ssh_key::PublicKey)>, Error>
pub fn learn_known_hosts(host, port, pubkey) -> Result<(), Error>          // 写回 ~/.ssh/known_hosts
pub fn learn_known_hosts_path(host, port, pubkey, path) -> Result<(), Error>
```

用法：在 `check_server_key` 回调里调用 `check_known_hosts_path`，返回 `Ok(true/false)`：

```rust
async fn check_server_key(&mut self, k: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
    let PublicKeyOrCertificate::PublicKey { key, .. } = k else { return Ok(false) };
    match russh::keys::known_hosts::check_known_hosts_path(&self.host, self.port, key, &self.kh_path) {
        Ok(true)  => Ok(true),                       // 匹配
        Ok(false) => { /* 未知主机 -> 按 strict/accept-new/ask 策略决定 */ }
        Err(russh::keys::Error::KeyChanged { line }) => {
            // 主机密钥变了！必须硬失败并告知行号
            Ok(false)
        }
        Err(e) => Ok(false),
    }
}
```

`Error::KeyChanged { line }` 是专门为「密钥不匹配 = 可能的 MITM」设计的变体
（`src/keys/mod.rs:118`），带行号，正好能填进 UI 的 `FailureKind::HostKeyChanged`。

### 6.2 ⚠️ 与 OpenSSH 的行为差异（实测，会影响 strict 语义）

我构造了一个同时含「明文 / 多主机 / 哈希 / 通配符 / @cert-authority / @revoked /
`[host]:port`」的 known_hosts，用 russh 和 `ssh-keygen -F` 分别判定同一文件
（`/tmp/russh-probe/src/bin/knownhosts.rs`、`knownhosts2.rs`）：

| 条目类型 | russh 判定 | OpenSSH 判定 | 一致？ |
|---|---|---|---|
| `plain.example.com` | MATCH | MATCH | ✅ |
| `multi1,multi2` 的第二个名字 | MATCH | MATCH | ✅ |
| `[nonstd.example.com]:2222`（端口 2222） | MATCH | MATCH | ✅ |
| 同主机但端口 22（应不匹配） | no-match | no-match | ✅ |
| 哈希条目 `\|1\|salt\|hash` | MATCH | MATCH | ✅ |
| **通配符 `*.wild.example.com`** | **no-match** | **MATCH** | ❌ **分歧** |
| **`@cert-authority`** | **no-match** | **MATCH** | ❌ **分歧** |
| `@revoked` | no-match | MATCH (但按 revoked 语义处理) | ⚠️ 巧合正确 |
| 不存在的 host | no-match | no-match | ✅ |
| 同主机不同密钥 | `Err(KeyChanged { line })` | 报冲突 | ✅ |

原因在 `src/keys/known_hosts.rs:110-127` 的 `match_hostname()`：

```rust
fn match_hostname(host: &str, pattern: &str) -> bool {
    for entry in pattern.split(',') {
        if entry.starts_with("|1|") {
            /* HMAC-SHA1 校验 */
        } else if host == entry {          // <-- 只做精确字符串比较
            return true;
        }
    }
    false
}
```

**没有 `*` / `?` 通配符匹配，也不处理 `@cert-authority` / `@revoked` 标记。**

对 `sshpf` 的影响：如果用户的 `~/.ssh/config` 里有 `Host *.corp.example.com`
配 `UserKnownHostsFile`，而他靠 `*.corp.example.com` 这类通配 host key 条目工作，
russh 会判定为「未知主机」。→ **必须自己补通配符匹配，或至少明确降级并提示用户。**

### 6.3 写回 known_hosts 的局限

`learn_known_hosts_path` 实测行为：

| 行为 | 实测结果 | OpenSSH 默认 |
|---|---|---|
| 写入格式 | **明文**（`learned.example.com ssh-ed25519 AAAA...`） | `HashKnownHosts yes` 时写**哈希** `\|1\|salt\|hash` |
| 去重 | **不去重**（同一 host 写两次 → 2 行，实测） | 会更新/去重 |
| 非 22 端口格式 | `[host]:port` ✅ 正确 | 同 |
| 目录不存在 | 自动 `create_dir_all` ✅ | |
| 文件末尾无换行 | 自动补 `\n` ✅ | |

所以 **accept-new 语义可以做到约 80%，但要与 OpenSSH 的写回行为完全一致，需要自己重写
写入逻辑**（至少 HashKnownHosts 支持 + 去重）。

### 6.4 strict / accept-new / ask 三种语义的可实现性

| 语义 | 能否实现 | 实现方式 |
|---|---|---|
| `strict`（yes） | ✅ 容易 | 未知主机 → `check_server_key` 返回 `Ok(false)`；实测返回 `Err(Unknown server key)` |
| `accept-new` | ⚠️ 可以，需自己写 | 未知 → 用 `learn_known_hosts_path` 写回（但注意 §6.3 的 hash/去重差异） |
| `ask`（交互询问） | ✅ 结构上最自然 | `check_server_key` 是 `async fn`，可以 `await` 一个 UI 确认 channel；`sshpf` 本来就是 TUI，这是**比系统 ssh 更好的形态** |

注意：`check_server_key` 返回 `false` 后 `connect()` 直接失败，**无法在同一个
`connect()` 内重试**——「ask 后用户接受」的正确做法是在回调里 `await` 用户答复，
拿到 `true` 才 `return Ok(true)`，而不是先返回 false 再重连。

### 6.5 需要自己写的部分（known_hosts 汇总）

1. 通配符 / 负向匹配（`*`、`?`、`!`），补齐与 OpenSSH 一致的 `match_hostname`
2. `@cert-authority`（CA 签名的 host key）与 `@revoked` 语义
3. `HashKnownHosts yes` 的写回
4. 多 `UserKnownHostsFile` 路径（含 `/etc/ssh/ssh_known_hosts` 全局文件）
5. `UpdateHostKeys` / `HostKeyAlgorithms` 偏好选择（`sshpf` 现在完全靠 ssh 默认）

---

## 7. ProxyJump / 跳板机

### 7.1 结论

✅ **可行，而且实现路径比想象中干净**——因为 `russh::client::connect_stream` 接受任意
`AsyncRead + AsyncWrite + Unpin + Send + 'static`，而 `Channel::into_stream()` 正好提供它。
**在已有 SSH 连接的 channel 上跑第二个完整 SSH 握手**，就是 ProxyJump。

### 7.2 实测：两跳真实 OpenSSH

`/tmp/russh-probe/src/bin/interop.rs`：

```rust
// 1) 连跳板机，并在它上面开一条到下一跳 22 端口的 direct-tcpip channel
let ch = jump_handle
    .channel_open_direct_tcpip("127.0.0.1", sshd_port as u32, "127.0.0.1", 0)
    .await?;

// 2) 把这条 channel 当成字节流，跑第二个 SSH 握手
let inner = client::connect_stream(
    Arc::new(client::Config::default()),
    ch.into_stream(),          // <- ChannelStream: AsyncRead + AsyncWrite
    Handler2::new(),
).await?;

// 3) 在内层 session 上正常认证
let inner = Arc::new(inner);
inner.authenticate_publickey(...).await?;
```

实测输出：

```
3. ProxyJump nested SSH auth (real sshd)  auth.success()=true
   -L via ProxyJump       => ECHO:ping
```

**即：三层嵌套（本地 listener → 内层 SSH channel → 外层 SSH channel → 真实 sshd）
的 `-L` 流量跑通了。** 这是比「能建连」更强的证据。

### 7.3 胶水代码量评估

| 项 | 工作量 | 说明 |
|---|---|---|
| 单跳嵌套 | **~20 行** | 上面那段代码 |
| 多跳链式（A→B→C→D） | **~50 行** | 循环 `for hop in chain`，每层 `connect_stream` + 认证，保存最内层 Handle |
| `ProxyJump` 配置解析 | 需注意 | 格式 `user@host:port`，逗号分隔多跳；`ssh -J a,b` 语义是**顺序**连接 |
| `~/.ssh/config` 的 `ProxyJump` 继承 | **要自己实现** | 跳板机自己的 `ProxyJump`/`IdentityFile`/`User` 要递归 resolve |
| 跳板机的 known_hosts 校验 | ✅ 天然支持 | 每层 `client::connect_stream` 都有自己的 Handler，可复用同一套校验 |
| 跳板机断开后的重连 | 需设计 | 外层断了内层必然断，重连要重建整条链 |
| `ProxyCommand` | ⚠️ 仍要 fork 外部进程 | `russh-config` 的 `Stream::proxy_command` 就是 `tokio::process::Command`。纯 Rust 目标下 `ProxyCommand` 无法摆脱外部二进制（但绝大多数用户用 ProxyJump 而非 ProxyCommand） |

### 7.4 ⚠️ 重要坑：`connect_stream` 的错误分类

实测（`/tmp/russh-probe/src/bin/failures.rs`）：

| 场景 | 返回 |
|---|---|
| 主机密钥被 Handler 拒绝 | `Err(Unknown server key)` → `russh::Error::UnknownKey` |
| TCP 连接被拒 | `Err(IO(Os { code: 111, kind: ConnectionRefused, ... }))` |
| 对端不是 SSH（收到 `HTTP/1.1 400`） | `Err(Disconnect)` |
| 密码错误 | `AuthResult::Failure { remaining_methods, partial_success }`（**不是 Error**） |
| 公钥未授权 | 同上 `Failure { remaining_methods: MethodSet([PublicKey, KeyboardInteractive]), partial_success: false }` |

⚠️ **`Disconnect` 过于笼统**：非 SSH 对端、协议错误、服务器主动断开都归到它。
嵌套 ProxyJump 时，「第二跳握手失败」只能拿到 `Disconnect`，无法区分
「跳板机把 channel 关了」还是「目标机不是 SSH」。要精确分类必须依赖
`Handler::auth_banner` / `channel_open_failure` 等回调自己记录上下文。

另外注意 `Error::UnknownKey` 的 `Display` 是「Unknown server key」——与
`keys::Error::KeyChanged` 是**两个不同的类型**。做 host key 校验时要把
`check_server_key` 里的原始 `keys::Error` 自己暂存，否则 HostKeyChanged 的行号会丢。

---

## 8. 与备选方案对比

### 8.1 三维对比表

| 维度 | **russh 0.65.1** | **ssh2 0.9.7**（libssh2） | **系统 `ssh` 子进程**（现状） |
|---|---|---|---|
| 纯 Rust | ⚠️ Rust 代码，但加密后端是 C（aws-lc-sys / ring） | ❌ libssh2-sys + openssl-sys | ❌ 外部二进制 |
| **运行期**外部依赖 | ✅ **零**（静态链接进 .exe） | ✅ 零（但需目标机有 OpenSSL DLL，除非 vendored 静态） | ❌ 需要 `ssh` 在 PATH |
| **构建期**依赖 | ⚠️ C/C++ 编译器；Windows x86-64 需 NASM（ring 后端的 crate 内置汇编可省） | ⚠️ C 编译器 + **OpenSSL 头文件/库**（`openssl-sys`） | ✅ 无（纯 Rust） |
| `-L` | ✅ `channel_open_direct_tcpip` | ✅ `channel_direct_tcpip` | ✅ 原生 |
| `-R` | ✅ `tcpip_forward` + `server_channel_open_forwarded_tcpip`（实测跑通） | ✅ `channel_forward_listen` + `Listener::accept` | ✅ 原生 |
| `-D` | ⚠️ 协议够用，SOCKS5 自己写（~60 行） | ⚠️ 同，SOCKS5 自己写 | ✅ 原生 `-D` |
| ssh-agent | ✅ `AgentClient`（Unix UDS + **Windows Pageant + named pipe**） | ✅ `Session::userauth_agent` | ✅ 原生 |
| known_hosts | ⚠️ 有模块，但**无通配符/@cert-authority**，写回无 hash/去重 | ⚠️ `KnownHosts`（`check_port`/`add`/`write_file`），同样需自己补语义 | ✅ 完整语义 |
| `~/.ssh/config` | ❌ 不在 russh 内；`russh-config` 0.58.0 只解析 **10 个指令**（`Host`/`User`/`HostName`/`Port`/`IdentityFile`/`ProxyCommand`/`ProxyJump`/`AddKeysToAgent`/`UserKnownHostsFile`/`StrictHostKeyChecking`），**无 Include/Match**，`HostConfig` 只有 9 个字段 | ❌ 完全没有 | ✅ 完整语义（含 Include/Match/Host *） |
| ProxyJump | ⚠️ 需自己搭链（~20 行/跳，实测可行） | ❌ libssh2 协议层无此能力 | ✅ 原生 |
| keyboard-interactive / 2FA | ✅ 完整 | ✅ `KeyboardInteractivePrompt` | ✅ |
| 失败原因精度 | ⚠️ 枚举（`RequestDenied`/`ConnectFailed`/`Disconnect` 较粗） | ⚠️ `Error` + errno | ✅ **stderr 原文最精确** |
| 流量/字节统计 | ✅ 可精确到 channel | ✅ 可 | ❌ 需解析 ssh 输出或读 /proc |
| 必需运行时 | **tokio**（异步） | 同步（`Mutex`+阻塞），也可包成异步 | 无 |
| unique crates（+tokio） | 142（aws-lc）/ 136（ring） | 未精确测；libssh2-sys+openssl-sys 链较短 | 92（现状） |
| release 二进制增量 | 实测 **+3.59 MB**（见 §8.2） | 未实测；libssh2+静态 OpenSSL 通常 +1.5–3 MB | 0 |
| 冷编译耗时 | 实测 **22.4s**（release，with LTO） | 未实测；openssl-sys 编译通常更慢 | 现状 |
| Windows 交付 | ⚠️ 需 NASM/MSVC 构建，产物是干净 .exe | ⚠️ vcpkg/OpenSSL 更麻烦 | ✅ 依赖用户已装 OpenSSH |
| License | Apache-2.0 | MIT/Apache-2.0（libssh2 是 BSD-3） | — |
| 维护活跃度 | ✅ 高频（2-4 周/版） | ✅ 高频（2026-11-05 更新） | ✅ 随 OS |

### 8.2 二进制体积实测

用同一 `[profile.release] strip = true, lto = "thin"` 对比：

| 工程 | release 体积 |
|---|---|
| 仅 tokio + anyhow（基线） | 0.61 MB |
| tokio + russh（默认 aws-lc-rs） | **4.20 MB** |
| **russh 增量** | **+3.59 MB** |
| 当前 `sshpf`（实测 `target/release/sshpf`） | 1.70 MB |
| **迁移后预估** | **≈ 5.3 MB** |

> 注：这是"最小可行程序"的对比。真实 `sshpf` 的 ratatui/crossterm 等已包含在 1.70 MB
> 里，russh 部分是增量，所以 5.3 MB 是合理估算，不是精确值。
> 若改用 `ring` 后端，体积通常略小（未实测）。

### 8.3 ssh2 的关键否决理由（复核）

任务描述里的假设「ssh2 依赖 libssh2+C」成立，实测确认：

- `ssh2 0.9.7` → `libssh2-sys ^0.3.4`
- `libssh2-sys 0.3.4` 的 `Cargo.toml`：`[target."cfg(unix)".dependencies] openssl-sys = "0.9.35"`，
  `build-dependencies: pkg-config, cc, semver`，`[target.'cfg(target_env = "msvc")'.build-dependencies] vcpkg`
- 特性 `vendored-openssl = ["openssl-sys/vendored"]` 可以把 OpenSSL 也静态编进来，
  但代价是构建更慢、更容易在 Windows 上失败

**`ssh2` 的 `-R` 是支持且比 russh 更"成品"**：`channel_forward_listen()` 返回
`(Listener, u16)`，`Listener::accept()` 直接给 `Channel`——比 russh 的
「回调用 mpsc 转发」更符合同步阻塞式设计。但 `ssh2` **完全没有 ProxyJump**，
且是同步 API，与本项目 TUI 的事件循环结合需要再包一层。

### 8.4 决策建议

| 方案 | 适合场景 |
|---|---|
| **继续用系统 ssh**（不迁移） | 只做 Linux、需要 100% OpenSSH 语义（config/ProxyJump/known_hosts）、失败原因要 stderr 原文 |
| **russh** | 需要 Windows 开箱即用（不依赖用户装 OpenSSH）、需要精确字节统计、需要 TUI 内交互式 host key 确认 |
| **ssh2** | 希望同步简单模型 + `-R` 现成 `Listener`，能接受 OpenSSL 构建复杂度 |

**混合路线（推荐）**：保留 `trait SshBackend`，实现 `SystemSshBackend`（现有）+ `RusshBackend`。
按平台或用户配置切换——Linux 上默认系统 ssh（语义最全），Windows 上默认 russh
（无需 OpenSSH）。仓库 `docs/design.md` 已经预留了这个方向（「v1 预留 `trait SshBackend` 即可」）。

---

## 9. 现有实现与 russh 的能力映射（迁移工作量）

| 现有能力（`src/ssh.rs` / `src/tunnel.rs`） | russh 对应 | 工作量 |
|---|---|---|
| `list_hosts()` 解析 `~/.ssh/config` 的 Host 行 | ❌ 无 | **大**：`config_parse.rs` 845 行基本可复用（本来就是自己写的解析器） |
| `resolve_host(alias)` 用 `ssh -G -T` 求值 | ❌ 无 | **大**：`ssh -G` 的 73 键求值语义要自己实现，或引入 `ssh-config`-类 crate |
| `write_synth_config()` 烘焙一次性 `-F` 配置 | ✅ 不需要了 | **负工作量**：整个 §2「合成配置」的问题消失 |
| `build_ssh_args()` 拼 `-L/-R/-D` 参数 | ✅ 变成 API 调用 | 小 |
| `classify_failure(line)` 解析 stderr | ⚠️ 变成枚举 + `channel_open_failure` 回调 | 中：`failure.rs` 248 行的规则要重写成 `russh::Error` → `FailureKind` 映射 |
| `is_ready_line()` / `is_local_listening_line()` | ✅ 不需要了 | **负工作量**：`-R` 的假就绪问题从根上消失 |
| `tunnel.rs` 进程组 / `PR_SET_PDEATHSIG` / Job Object | ✅ 换成 Drop + abort task | **中**：反而更简单（无子进程），但要保证 tokio task 在 `Drop` 时取消 |
| `port_available()` / `port_owner()` | ✅ 不变（仍用 `TcpListener::bind` 探测） | 0 |
| `ssh_available()` 检查 ssh 在 PATH | ✅ 删除 | 负 |
| ProxyJump 支持 | ⚠️ 自己搭链（实测 20 行/跳） | 中 |
| ssh-agent 支持 | ✅ `AgentClient` | 小（但要写 Pageant/named-pipe 探测） |
| known_hosts | ⚠️ 内置但有缺口 | 中（见 §6.5） |

**净判断：迁移不是"换个库"，而是"把 ssh 的语义重新实现一部分"。**
好消息是 `sshpf` 已经把 config 解析写成了自己的 `config_parse.rs`（845 行），
这部分本来就与 ssh 二进制无关，可以直接复用。

---

## 10. 风险点清单

### 10.1 高

1. **构建期 C 工具链**（§1.4）
   Windows CI 必须装 MSVC Build Tools + NASM，否则 `windows-latest` 直接编译失败。
   issue #541 至今 open 说明这不是文档疏忽而是真实摩擦。
   → 缓解：`default-features = false, features = ["ring", "flate2", "rsa"]`。

2. **`~/.ssh/config` 语义重写**（§9）
   `ssh -G` 的求值语义（73 键、Include、Match、Host * 优先级、ProxyJump 递归）
   是 OpenSSH 的核心复杂度。`russh-config` 0.58.0 只覆盖 10 个指令且**无 Include/Match**，
   不能直接用。这是最大的单项工作量。
   → 缓解：混合路线，Linux 保留系统 ssh。

3. **known_hosts 通配符/CA 缺口**（§6.2）
   `*.example.com` 条目 russh 判定为"未知主机"，会导致"用户明明连过却被要求确认"，
   或反向——如果实现成 accept-new 会**静默写入大量重复条目**。
   → 缓解：自己实现 `match_hostname`（~40 行），或在 accept-new 前做归属判断。

### 10.2 中

4. **`tcpip_forward` 返回 `Ok(0)` 的歧义**（§3.5）
   非 0 端口请求成功时返回 0。写错会得到"远端 0 端口"的诡异行为。
   → 缓解：封装函数，`port != 0` 时直接返回请求的 port，不依赖返回值。

5. **`ChannelOpenFailure` 丢失服务端描述**（§2.4）
   `ConnectFailed` 同时覆盖"端口拒绝"和"DNS 失败"，无法区分。
   → 缓解：必须实现 `Handler::channel_open_failure` 拿 `description`。

6. **`russh::Error::Disconnect` 过于笼统**（§7.4）
   非 SSH 对端、嵌套 ProxyJump 内部失败都归到这个变体。
   → 缓解：在 Handler 里维护上下文状态机，记录"失败发生在哪一跳/哪个阶段"。

7. **tokio 引入 + 进程模型改造**（§9）
   当前 `tunnel.rs` 是 `std::thread` + `mpsc` + 子进程；改 russh 后是 tokio runtime
   + task。`Drop` 时取消 task、`PR_SET_PDEATHSIG` 的孤儿保护逻辑全部作废
   （因为没有子进程了——这其实是**变简单**，但要重新设计）。
   → 注意：`sshpf` 的 `cargo` edition 是 2024，russh 要求 **rust-version 1.89**，
   当前 `rustc 1.99.1` 满足，但 README 写的「需要 Rust 1.88+」要改成 1.89+。

8. **二进制 +3.59 MB**（§8.2）
   对一个 TUI 工具从 1.70 MB 涨到 ~5.3 MB，是否可接受取决于分发方式
   （`cargo install` 无所谓；若做单文件分发需权衡）。

### 10.3 低

9. 版本节奏快（2–4 周/版），**0.x 无 semver 保证**，锁定 minor 版本需谨慎。
   `russh-config` 还停在 0.58.0（落后主 crate 7 个版本），说明它维护优先级较低。
10. `Channel::into_stream()` 的 `AsyncWrite` impl 要求 `S: 'static + Send + Sync`
    （`channel_stream.rs:47`），`Channel<Msg>` 满足，但自定义泛型时要留意。
11. `Handle` **不是 `Clone`**，多 task 共享必须 `Arc<Handle>`（实测 20 并发 OK）。
12. keyboard-interactive 的多轮 `InfoRequest` 需要自己写状态机（含 2FA 提示文案）。
13. `@revoked` 的处理：实测 russh 返回 no-match（碰巧"安全"），但如果用户有
    accept-new 逻辑，可能会把 revoked 的 key 重新加进去。

---

## 11. 结论

### 11.1 russh 能否支撑本项目的 `-L` / `-R` / `-D`？

**能。三种转发的协议层都没有缺口。**

| 转发 | 支持度 | 依据 |
|---|---|---|
| `-L` | ✅ **完整支持，开箱即用** | `channel_open_direct_tcpip` + `Channel::into_stream()`；已在真实 OpenSSH 8.2p1 上跑通端到端流量 |
| `-R` | ✅ **完整支持**（**不是能力缺口**） | 客户端 `Handle::tcpip_forward()` + `Handler::server_channel_open_forwarded_tcpip()`；已在真实 sshd 上跑通端到端流量，含 `port=0` 服务端分配、`cancel_tcpip_forward`、拒绝场景（端口占用/特权端口） |
| `-D` | ⚠️ **协议层够用，SOCKS5 必须自己写** | russh 源码零 SOCKS 代码；但 `direct-tcpip` 可承载任意 SOCKS CONNECT。实测手写 ~60 行跑通 IPv4 + 域名 ATYP |

> 关于 `-R` 特别说明：GitHub 上 open 的 issue #183 容易让人误判。读原文可知它描述的是
> 2023 年的状态，且诉求是「服务端主动向客户端发起反向转发」这一**非标准方向**，
> 与 `ssh -R`（客户端请服务端监听）无关。当前 russh 0.65.1 对 `ssh -R` 语义是完整的。

### 11.2 哪些必须自己写胶水代码

按工作量排序：

| # | 胶水项 | 规模 | 备注 |
|---|---|---|---|
| 1 | **`~/.ssh/config` 求值语义** | 大（最大单项） | `russh-config` 只覆盖 10 个指令、无 `Include`/`Match`，不能直接用。现有 `config_parse.rs` 可复用但要补 `ssh -G` 的 73 键求值 |
| 2 | **known_hosts 语义补齐** | 中 | 通配符 `*`/`?`/`!`、`@cert-authority`、`@revoked`、HashKnownHosts 写回、多文件（含 `/etc/ssh/ssh_known_hosts`） |
| 3 | **`russh::Error` → `FailureKind` 映射** | 中 | 必须实现 `Handler::channel_open_failure` 拿服务端 description，否则丢失原因精度；`Disconnect` 需自己补上下文 |
| 4 | **ProxyJump 搭链** | 中（~20 行/跳） | `connect_stream` + `channel_open_direct_tcpip` 组合，实测可行；难点在 config 递归继承与多层重连 |
| 5 | **SOCKS5 服务端** | 中（~60 行） | NOAUTH + CONNECT + ATYP 1/3（IPv6 需补）、错误码映射 |
| 6 | **进程/任务模型改造** | 中 | `tunnel.rs` 的 thread+子进程+pgid+PDEATHSIG 全部换成 tokio task；实际上变简单（无子进程），但要重新设计取消语义 |
| 7 | **ssh-agent 探测** | 小 | 无 env → 试 Pageant（Windows）/ named pipe；有 `SSH_AUTH_SOCK` → UDS |
| 8 | **RSA hash 协商** | 小 | 每个连接调 `best_supported_rsa_hash()` |
| 9 | **`tcpip_forward` 返回 0 的封装** | 小但必须做 | 见 §3.5，否则是真实 bug |
| 10 | **构建侧** | 小 | `default-features=false` + ring；Windows CI 加 MSVC/NASM 说明 |

### 11.3 一句话结论

> **russh 0.65.1 足以支撑 `-L`/`-R`/`-D` 三种转发，协议层零缺口**；
> `-R` 明确支持（已实测验证，GitHub issue #183 是误解）。
> 真正的成本不在转发，而在**重建 OpenSSH 的周边语义**：
> `~/.ssh/config` 求值（最大）、known_hosts 通配符/写回、失败原因精度。
> 另外构建期 C 工具链（Windows 需 NASM）是"纯 Rust"承诺上的唯一破口，
> 用 `ring` 后端可减轻但不能完全消除。
>
> **建议路线**：保留 `trait SshBackend`，Linux 默认系统 ssh（语义最全），
> Windows 默认 russh（无需用户装 OpenSSH）。这是投入产出比最高的方案。

---

## 附录 A：复现方式

所有实测在 `/tmp` 下的临时工程完成，未修改仓库除本文外的任何文件。

```bash
# 1. 拉 russh 源码
cd /tmp && mkdir -p russh-src && cd russh-src
curl -sL https://crates.io/api/v1/crates/russh/0.65.1/download -o russh.tar.gz
tar xzf russh.tar.gz && ls russh-0.65.1/src

# 2. 建 probe 工程
mkdir -p /tmp/russh-probe/src/bin && cd /tmp/russh-probe
cat > Cargo.toml <<'EOF'
[package]
name = "russh-probe"
version = "0.1.0"
edition = "2024"
[dependencies]
russh = "0.65.1"
tokio = { version = "1", features = ["rt-multi-thread","macros","net","io-util","time","sync"] }
anyhow = "1"
rand = "0.10"
EOF
# bin: server.rs（russh 服务端）, client.rs（-L/-R/-D + ProxyJump 全跑通）
# bin: interop.rs（对真实 OpenSSH sshd）, knownhosts.rs/knownhosts2.rs,
#      failures.rs（失败面）, redge.rs（-R 边界）, agent.rs, gw.rs

# 3. 起一个一次性 sshd 用于 interop
mkdir -p /tmp/russh-interop/sshd && cd /tmp/russh-interop/sshd
ssh-keygen -q -t ed25519 -N "" -f host_ed25519
ssh-keygen -q -t ed25519 -N "" -f user_key
ssh-keygen -q -t rsa -b 2048 -N "secret-pass" -f user_key_rsa_enc
ssh-keygen -q -t ed25519 -N "secret-pass" -f user_key_ed_enc
cat user_key.pub user_key_rsa_enc.pub user_key_ed_enc.pub > authorized_keys
cat > sshd_config <<'EOF'
Port 22990
ListenAddress 127.0.0.1
HostKey /tmp/russh-interop/sshd/host_ed25519
PidFile /tmp/russh-interop/sshd/sshd.pid
AuthorizedKeysFile /tmp/russh-interop/sshd/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
AllowTcpForwarding yes
LogLevel VERBOSE
EOF
/usr/sbin/sshd -f sshd_config -E sshd.log

# 4. 跑
cd /tmp/russh-probe && cargo build --bins
./target/debug/interop    # 全部 PASSED
```

## 附录 B：引用来源

| 内容 | 链接 |
|---|---|
| russh crate 页面 / 版本 / 下载量 | https://crates.io/crates/russh |
| russh API 文档 | https://docs.rs/russh/0.65.1/russh/ |
| `client::Handle` 文档（`channel_open_direct_tcpip`/`tcpip_forward`） | https://docs.rs/russh/0.65.1/russh/client/struct.Handle.html |
| `keys::known_hosts` 文档 | https://docs.rs/russh/0.65.1/russh/keys/known_hosts/index.html |
| russh 源码仓库 | https://github.com/Eugeny/russh |
| russh 官方 `client_open_direct_tcpip` 示例 | https://github.com/Eugeny/russh/blob/main/russh/examples/client_open_direct_tcpip.rs |
| russh `tcpip_forward` 集成测试（client 侧 -R） | https://github.com/Eugeny/russh/blob/main/russh/tests/test_contention.rs |
| issue #183（易误解为 -R 不支持，实为非标准方向） | https://github.com/Eugeny/russh/issues/183 |
| issue #541（Windows 构建需 NASM/CMake，仍 open） | https://github.com/Eugeny/russh/issues/541 |
| issue #177（Windows `connect_uds`，named pipe 不受影响） | https://github.com/Eugeny/russh/issues/177 |
| issue #658（`tcpip_forward` 的 `&mut self`，已修） | https://github.com/Eugeny/russh/issues/658 |
| russh-config crate（config 解析辅助，覆盖有限） | https://crates.io/crates/russh-config |
| aws-lc-rs 构建要求（需 C/C++ 编译器，Windows 需 NASM） | https://github.com/aws/aws-lc-rs |
| ring 构建要求（需 C 工具链） | https://github.com/briansmith/ring/blob/main/BUILDING.md |
| ssh2 crate | https://crates.io/crates/ssh2 |
| libssh2-sys（依赖 openssl-sys / vcpkg） | https://crates.io/crates/libssh2-sys |
| fast-socks5（SOCKS5 备选） | https://crates.io/crates/fast-socks5 |
| RFC 4254 §7（TCP/IP Forwarding） | https://www.rfc-editor.org/rfc/rfc4254#section-7 |

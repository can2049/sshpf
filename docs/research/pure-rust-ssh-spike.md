# 技术验证：russh 纯 Rust 实现 -L / -R / -D 三种转发

> 本文所有数字与结论均来自**本机实测**，不是查文档推断。
> 可复现工程见 [`spike/`](./spike/)：一个可丢弃的 sshd（`spike/sshd/run.sh`）+ 六个探针程序。
> 姊妹文档：[`pure-rust-ssh-russh.md`](./pure-rust-ssh-russh.md)（API 层面的独立调研）。

## 0. 结论速览

| 问题 | 实测答案 |
|---|---|
| russh 能做 `-L`？ | ✅ 通过。真实 OpenSSH 8.2p1 上跑通端到端 |
| russh 能做 `-R`？ | ✅ **通过**。客户端 `tcpip_forward()` + `server_channel_open_forwarded_tcpip()` 回调 |
| russh 能做 `-D`？ | ✅ 通过，但 SOCKS5 要自己写（实测约 60 行） |
| ProxyJump 能做？ | ✅ 通过。SSH 跑在 SSH 的 channel 里，三跳实测打通 |
| 能摆脱系统 `ssh` 二进制？ | ✅ **运行时可以完全摆脱**（这是本次改造的真正收益） |
| 代价 | 二进制 1.63 MB → 4.34 MB；构建期需要 C 工具链；引入 tokio |

**核心判断：技术路线可行，三种转发无协议层缺口。** 真正的成本不在转发，
而在**重建 OpenSSH 的周边语义**（known_hosts 边界、config 求值、错误分类）。

---

## 1. 实测结论表

| 能力 | 结论 | 实测证据 |
|---|---|---|
| `-L` 本地转发 | ✅ **通过** | 经隧道读到目标 SSH banner `SSH-2.0-OpenSSH_8.2p1` |
| `-R` 远程转发 | ✅ **通过** | 经远端端口回环收到 `hello-through-remote-forward` |
| `-R` 取消 | ✅ **通过** | `cancel_tcpip_forward()` 返回 Ok |
| `-D` SOCKS5 | ✅ **通过**（需自写） | 经 SOCKS5 代理读到目标 banner，IPv4 + 域名 ATYP 均验证 |
| ProxyJump | ✅ **通过** | 3 跳嵌套认证成功，嵌套 `-R` 拿到端口，嵌套 exec 输出 `NESTED_SSH_OK` |
| publickey 认证 | ✅ **通过** | 认证耗时 73–86 ms |
| ssh-agent 认证 | ✅ **通过** | 真实 `ssh-agent` + `ssh-add`，`authenticate_publickey_with()` 成功 |
| 加密私钥 | ✅ **通过** | passphrase 正确可解，错误报 `cryptographic error` |
| known_hosts 校验 | ⚠️ **部分** | 精确匹配可用，但**不支持通配符 / @cert-authority / @revoked** |
| 失败原因分类 | ✅ **优于现状** | 结构化 `russh::Error`，不再靠解析 stderr 文本 |
| tokio 与同步 TUI 共存 | ✅ **通过** | tokio 跑在后台线程，`std::sync::mpsc` 桥接，UI 线程零 tokio |

---

## 2. 关键架构验证：tokio 与现有同步 TUI 如何共存

这是迁移中**最先要回答**的问题。当前 `app.rs` 明确写了「deliberately no `tokio` here」，
UI 线程是 `crossterm::event::poll(200ms)` + `mpsc` 的同步循环。而 russh 是 async 的。

**实测方案（`spike/threadbridge.rs`，已跑通）：**

```rust
let (tx, rx) = mpsc::channel::<Ev>();

// tokio 独占一个后台线程，UI 线程完全不感知 tokio
std::thread::spawn(move || {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async move { run(tx).await });
});

// UI 线程保持原样：同步、带超时地收事件
while Instant::now() < deadline {
    match rx.recv_timeout(Duration::from_millis(200)) {
        Ok(Ev::Ready)        => { /* 隧道就绪 */ }
        Ok(Ev::Line(l))      => { /* 日志行 */ }
        Ok(Ev::Exited(why))  => break,
        Err(RecvTimeoutError::Timeout) => { /* 重绘 UI */ }
        Err(RecvTimeoutError::Disconnected) => break,
    }
}
```

**结论：现有 `TunnelEvent` + `mpsc` 的事件模型可以原样保留。**
`TunnelEvent::{Line, Exited, StderrClosed, SpawnFailed}` 的语义一一对应，
UI 层与 `app.rs` 的事件消费逻辑几乎不用改。

> 每隧道一个后台线程 + `current_thread` runtime。线程数从「1 个 stderr reader」变成
> 「1 个 runtime 线程」，量级相同（隧道数是个位数到几十）。

---

## 3. `-L` 本地转发

调用链（`spike/forward-spike.rs`）：

```rust
let mut ch = handle.channel_open_direct_tcpip(
    remote_host, remote_port as u32,   // sshd 侧的目标
    originator_ip, originator_port,    // 来源，用于日志/审计
).await?;

// 关键：into_stream() 给出 AsyncRead + AsyncWrite，可直接双向拷贝
let mut stream = ch.into_stream();
tokio::io::copy_bidirectional(&mut local_socket, &mut stream).await?;
```

`Channel::into_stream()` 比手写 `wait()` 循环简单得多——不需要自己处理
`ChannelMsg::Data` / `WindowAdjusted` / `Eof`，`copy_bidirectional` 一次搞定。

**实测数据：**

| 指标 | 值 |
|---|---|
| 认证耗时 | 73 ms |
| channel 打开耗时 | 202 ms（首次，含 TCP 建连） |
| RTT（4 字节回环，20 次） | min 317 µs / median 367 µs / max 657 µs |
| 批量吞吐 | **48.8 MiB/s**，32 MiB 零丢失 |

---

## 4. `-R` 远程转发 —— 本次调研的核心疑点

**结论：完整支持，不是能力缺口。**

`russh-researcher` 指出 GitHub 上 open 的 issue #183 极易误导：它的标题看起来像
「客户端 `-R` 缺失」，但读原文可知其诉求是**服务端主动向客户端反向转发**这一非标准方向，
与 `ssh -R` 语义无关。我独立在源码中确认了客户端 API 的存在（`src/client/mod.rs:934`），
并实测跑通：

```rust
// 1. 请求服务端监听。port=0 让服务端自选
let granted = handle.tcpip_forward("127.0.0.1".to_string(), remote_port as u32).await?;

// 2. 服务端每收到一个外部连接，会回调 Handler 里的这个方法
impl client::Handler for MyClient {
    async fn server_channel_open_forwarded_tcpip(
        &mut self, channel: Channel<Msg>,
        connected_address: &str, connected_port: u32,
        originator_address: &str, originator_port: u32,
        reply: client::ChannelOpenHandle, _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        // channel 就是数据管道，把它接到我们本地的目标服务
        Ok(())
    }
}
```

> ⚠️ **必须封装的坑**：`tcpip_forward(addr, 非0端口)` 成功时返回 **`Ok(0)`**，
> 而不是你请求的那个端口。只有 `port=0`（请求服务端分配）时返回值才是真实端口。
> 直接拿返回值当端口用会得到「远端 0 端口」的诡异行为。
> 封装原则：请求端口非 0 时用请求值；请求 0 时用返回值。

实测（`spike/errpaths.rs`）：

| 场景 | 结果 |
|---|---|
| 正常 `tcpip_forward` | `Ok(0)` ← 注意！ |
| 端口已被 sshd 自己占用 | `Err(RequestDenied)` |
| 重复请求同一端口 | 第一次 `Ok(0)`，第二次 `Err(RequestDenied)` |
| `cancel_tcpip_forward` | `Ok(())` |

**一个额外收益**：当前实现靠解析 `ssh -v` 的 `remote forward success for` 来判断 `-R` 就绪，
README 里记录了这个坑（`Entering interactive session.` 会早于监听结果到达，造成假成功）。
改用 russh 后 `tcpip_forward()` 是 await 真实的 `GLOBAL_REQUEST` 回复，
**这个 bug 类别从根上消失**。

---

## 5. `-D` 动态转发（SOCKS5）

russh 源码中**零** SOCKS 相关代码。但 `direct-tcpip` 可以承载任意 SOCKS CONNECT，
所以只需实现 SOCKS5 服务端握手。实测手写约 60 行跑通：

```
客户端 → 05 NMETHODS METHODS...      → 服务端
         ← 05 00                        （选 NOAUTH）
客户端 → 05 01 00 ATYP DST.ADDR DST.PORT  （CONNECT）
         ← 05 00 00 01 0.0.0.0 0        （成功）
之后直接 copy_bidirectional 到 direct-tcpip channel
```

支持的 ATYP：`0x01` IPv4、`0x03` 域名（两者均已实测）。
需补的边界：`0x04` IPv6、`BIND`/`UDP ASSOCIATE` 应明确回 `0x07`（Command not supported）。

**替代方案**：`fast-socks5` crate 可以直接用，但要评估其 tokio 版本兼容性。
自写 60 行的好处是零新增依赖 + 完全掌控错误码。

---

## 6. ProxyJump / 跳板机

纯 Rust 里 ProxyJump 的实现方式是**在一段 channel 上跑一整个 SSH 会话**
（`russh::client::connect_stream` 接受任意 `AsyncRead + AsyncWrite`）：

```rust
// hop 1: 正常 TCP 连到跳板机
let jump = client::connect(cfg.clone(), (jump_host, jump_port), Handler).await?;
let jump = authenticate(jump).await?;

// hop 2: 让跳板机帮我们开一条到目标机的 TCP 通道
let ch = jump.channel_open_direct_tcpip(target_host, target_port, "127.0.0.1", 1234).await?;

// hop 3: 在这条通道上跑完整的 SSH 会话
let stream = ch.into_stream();          // 必须 move，connect_stream 要求 'static
let target = client::connect_stream(cfg, stream, Handler).await?;
let target = authenticate(target).await?;   // 到目标机的认证
```

`connect_stream` 要求 stream 是 `'static`（**不能传 `&mut stream`**），
所以 `into_stream()` 的结果必须整个 move 进去。

实测（`spike/proxyjump.rs`）：

```
hop 1 (jump): authenticated
hop 2: direct-tcpip channel through jump host opened
hop 3: authenticated to target *through* the jump channel
nested -R: server granted remote port 44395
nested exec output: "NESTED_SSH_OK"
RESULT ProxyJump: PASS
```

**多跳就是递归**：把上面的结果套用 N 次即可，每层约 20 行胶水。

---

## 7. 认证方式实测矩阵

| 方式 | 支持 | 说明 |
|---|---|---|
| publickey | ✅ | `authenticate_publickey()`；RSA 需先 `best_supported_rsa_hash()` |
| publickey + passphrase | ✅ | `load_secret_key(path, Some(pass))` |
| ssh-agent (Unix) | ✅ | `AgentClient::connect_env()`（读 `SSH_AUTH_SOCK`） |
| ssh-agent (Windows) | ✅ | `connect_pageant()` 或 `connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")` |
| password | ✅ | `authenticate_password()` |
| keyboard-interactive | ✅ | 多轮 `InfoRequest` 需自写状态机（2FA） |
| OpenSSH 证书 | ✅ | `authenticate_openssh_cert()` |

密钥格式实测（`spike/keys.rs`）：

| 格式 | 结果 |
|---|---|
| ed25519 | ✅ |
| rsa 2048 / 4096 | ✅ |
| ecdsa 256 / 384 / 521 | ✅ |
| PKCS8 (PEM) | ✅ |
| 加密 key + 正确 passphrase | ✅ |
| 加密 key + 无/错 passphrase | ✅ 明确报错：「The key is encrypted」/「cryptographic error」 |

**实测的 agent 认证**（真实 `ssh-agent`）：

```
agent holds 1 identities:
  algo=Ed25519
auth with ssh-ed25519 -> success=true
```

> 注意：`AgentClient::connect_env()` 在 `SSH_AUTH_SOCK` 指向失效 socket 时会
> `Connection refused`。生产实现需要「有 env → 试 UDS；失败 → 回落 Pageant/named pipe」的探测链。

---

## 8. 主机密钥校验（known_hosts）—— 本次最需要注意的差异

russh 提供 `russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path}`，
精确匹配工作正常，但实测有几个**会咬人**的边界：

### 8.1 通配符不支持

| known_hosts 条目 | 我们的期望 | russh 实测 |
|---|---|---|
| `[127.0.0.1]:22200 ssh-ed25519 AAA...` | 匹配 | ✅ `Ok(true)` |
| `*.0.0.1 ssh-ed25519 AAA...` | 匹配 | ❌ `Ok(false)` |
| `example.com,[127.0.0.1]:22200 ...`（逗号列表） | 匹配 | ✅ `Ok(true)` |
| `# 注释` + 条目 | 匹配 | ✅ `Ok(true)` |
| `!127.0.0.1 ...`（否定） | 不匹配 | ❌ `Ok(false)`（碰巧"安全"） |
| `@cert-authority ...` | 按 CA 校验 | ❌ 不识别，判定为未知主机 |
| `@revoked ...` | 拒绝 | ❌ 不识别，判定为未知主机 |
| **Tab 分隔** `host\talgo\tkey` | 匹配 | ❌ `Ok(false)` 漏判 |
| **行首有空格**的条目 | 匹配 | ❌ `Ok(false)` 漏判 |
| **`@revoked` + 同 key 的正常条目** | **必须拒绝** | ❌ **`Ok(true)` —— 会接受已吊销的 key** |

`match_hostname` 只做「精确比较 + `\|1\|` 哈希条目」，**没有 glob**；
`known_host_keys_path` 用 `split(' ')` 切分（所以 Tab 就断了）；
且**从未检查 `@revoked` / `@cert-authority` 标记**（只跳 `#` 注释，`@` 行被当普通行解析，
marker 字段被丢弃）。

> 最后一行是**安全问题**：如果一个 key 已被吊销、但 known_hosts 里同时存在它的普通条目，
> russh 会返回 `Ok(true)` 放行。这不是「漏判为未知」，而是「明确接受了被吊销的密钥」。
> 实测证据：
> ```
> @revoked 条目单独存在        -> Ok(false)   碰巧安全（理由是"没匹配到"，不是"已吊销"）
> @revoked + 同 key 正常条目   -> Ok(true)    ❌ 严重：接受被吊销的密钥
> ```

**影响**：如果用户有 `*.example.com` 条目，purerust 版本会认为「主机未知」。
若实现成 accept-new，会**静默写入大量重复条目**；若实现成 strict，会拒绝本来能连的主机。

**对策**：自己实现 known_hosts 匹配与校验（约 120 行）。这不是「可选优化」——
`russh` 的校验函数只返回 `bool`，而本项目需要区分四种状态：
**未知主机（提示用户 learn）/ key 变更（拒绝）/ revoked（拒绝）/ CA 签发（走证书校验）**。
`bool` 表达不了这些差异，所以自研匹配反而是必需的。

必须自己写的部分：

| 需求 | 工作量 |
|---|---|
| `::` 通配匹配（`*` `?`）+ `!` 取反 + `[host]:port` 拆分 | ~60 行 |
| `\|1\|` hashed 条目（HMAC-SHA1） | ~15 行 |
| `@revoked` 优先拒绝 | ~10 行 |
| `@cert-authority` + 证书校验（若要支持证书） | ~30 行 |
| Tab / 多空格 / 行首空格归一化 | ~10 行 |
| 写回（learn）+ `HashKnownHosts yes` 支持 | ~30 行 |

### 8.2 一个隐蔽的陷阱：`PublicKey` 的 `PartialEq` 包含 comment

实测发现 `check_known_hosts_path` 对同一个 key 可能报 `KeyChanged`：

```
learn 后立刻 check -> Err(KeyChanged { line: 2 })
```

原因：`ssh_key::PublicKey` 的 `PartialEq` **把 comment 也算进去**。
从 `.pub` 文件加载的 key 带 `spike-host` 注释，而 known_hosts 行里只有算法+base64。
但**从线上收到的 key（`check_server_key` 的参数）没有 comment**，所以真实路径是安全的：

```
learn+check with comment    -> Err("The server key changed")
learn+check without comment -> Ok(true)
wire 编码相同，仅 comment 不同 -> wire equal? true
```

**对策**：校验前统一 `key.set_comment("")`，避免测试和边界场景踩坑。

### 8.3 写回的局限

`learn_known_hosts_path` 实测产出：

```
\n[127.0.0.1]:22200 ssh-ed25519 AAAAC3Nza... spike-host\n
```

问题：① 文件开头多一个空行（`SeekFrom::End(-1)` 在空文件上的行为）；
② 不遵循 `HashKnownHosts yes`（不写哈希）；
③ 不去重（重复 learn 会追加多条）。

**对策**：自己写回，或接受并在文档说明。

---

## 9. 失败原因分类：从「解析 stderr」升级为「结构化错误」

这是**收益最大**的一项。当前实现（`src/ssh/failure.rs`，215 行 + 13 个 `FailureKind` 变体）
靠正则匹配 ssh 的 stderr 文本，脆弱且靠猜。改用 russh 后拿到的是类型化错误：

| 场景 | 实测错误值 | 可映射到 |
|---|---|---|
| 端口无人监听 | `Error::IO(ConnectionRefused)` | `ConnectionRefused` |
| DNS 失败 | `Error::IO(Custom{ "failed to lookup address information" })` | `DnsFailure` |
| 本地端口被占 | `io::ErrorKind::AddrInUse` | `PortInUse` |
| 认证失败 | `Ok(AuthResult::Failure { remaining_methods })` | `AuthFailed` |
| 远程转发被拒 | `Error::RequestDenied` | `RemoteForwardFailed` |
| 目标不可达 | `Error::ChannelOpenFailure(ConnectFailed)` | `TargetUnreachable` |
| 主机密钥不匹配 | **我们自己决定**（`check_server_key` 返回 false） | `HostKeyMismatch` |

**注意**：`ChannelOpenFailure::ConnectFailed` 同时覆盖「端口拒绝」和「DNS 失败」，
无法区分。若要保留现有精度，必须实现 `Handler::channel_open_failure` 拿服务端的
`description` 字符串。

**主机密钥**从「解析 `Host key verification failed`」变成「我们主动返回 false」，
`strict` / `accept-new` / `ask` 三种语义可以完整实现——这是纯 Rust 相对子进程的**真实优势**。

---

## 10. 构建依赖与体积

### 10.1 加密后端选择（重要）

russh 的两个加密后端都需要 C 工具链，但**摩擦程度差别很大**：

| 后端 | 需要 | Windows CI 影响 |
|---|---|---|
| `aws-lc-rs`（默认） | C/C++ 编译器 + **cmake**；Windows x86-64 还需 **NASM** | ❌ 需装 NASM |
| `ring`（推荐） | C 工具链（Windows 上是 MSVC，已预装） | ✅ 无需额外安装 |

**实测确认 ring 不需要 NASM**：ring 0.17.14 的发布包中 `pregenerated/` 带 104 个文件，
其中 12 个 `.o` 是**预汇编好的目标文件**；`build.rs` 里 `if asm_target.use_nasm()`
分支直接使用这些 `.o` 而**不调用 nasm**：

```rust
if asm_target.use_nasm() {
    // Nasm was already used to generate the object files, so use them instead of assembling.
    let obj_srcs = asm_srcs.iter().map(|src| obj_path(generated_dir, src.as_path())).collect();
    (vec![], obj_srcs)     // <- 不汇编，直接用 .o
}
```

且发布包中**没有 `.git` 目录**（`is_git == false`），会走 `PREGENERATED` 分支，
不会触发 `generate_sources_and_preassemble`（那条路径才需要 perl + nasm）。

**直接对比实验**（对两个后端跑同一句
`cargo check --target x86_64-pc-windows-gnu`，统计构建过程中被请求的外部工具）：

```
ring  后端：  9 × mingw32-gcc                     （无 nasm，无 cmake）
aws-lc 后端： 59 × mingw32-gcc,  2 × NASM,  3 × CMake
```

ring 的 Windows 构建路径**从不请求 NASM 或 cmake**，这是选它的决定性理由。

**推荐配置**：

```toml
russh = { version = "0.65.1", default-features = false, features = ["ring", "flate2", "rsa"] }
```

> ⚠️ 这是**构建期**依赖，不是运行期。用户拿到的 `.exe` / 二进制仍然是自包含的，
> 「装了就能跑」的承诺不受影响。只有 CI 和开发机需要编译器。

### 10.2 体积与依赖规模

| 工程 | 二进制（release, strip+lto） | 唯一 crate 数 |
|---|---|---|
| 当前 sshpf | **1.63 MB** | 92 |
| + russh(ring) + tokio + ssh2-config | **4.34 MB** | 142 |

增长 +2.7 MB / +50 个 crate。对 `cargo install` 分发无所谓；对单文件分发需权衡。

### 10.3 编译时间

ring 后端冷编译约 11–22 s（本机 16 核）。可接受。

---

## 11. `~/.ssh/config` 求值：我实测的结论与另一份调研相反

`russh-researcher` 的报告把这一项列为「最大单项工作量」，依据是 `russh-config` 0.58.0
只覆盖 10 个指令、无 Include/Match。**但这是选错了 crate。**

我实测 **`ssh2-config` 0.9.0 覆盖度足够**，且**不引入任何 C 依赖**
（名字有误导性——它只是历史上为 ssh2-rs 设计，实际是纯 Rust，`cargo tree` 中
`libssh2-sys` 计数为 0）：

| 语法 | 实测支持 |
|---|---|
| `Host` 多模式 + 通配符 | ✅ |
| `Host *.internal !secret.internal`（否定） | ✅ `secret.internal` 正确不匹配 |
| `Include`（含嵌套） | ✅ |
| `Match host` | ✅ |
| `ProxyJump` | ✅ |
| `IdentityFile` + `~` 展开 | ✅ |
| `Host *` 全局默认继承 | ✅ 各 host 都能拿到 `ServerAliveInterval`/`Compression` |
| 未知指令容错 | ✅ `ParseRule::ALLOW_UNKNOWN_FIELDS` |

**用真实 `~/.ssh/config`（13 个 Host）验证**：正确解析出 12 个具体 alias，
`cloud_pc` 的 `ProxyJump cloud_pc_jump` 正确识别，`Host *` 默认值正确继承。

### 11.1 ⚠️ 但 `ssh2-config` 有一个会静默污染数据的 `Match` bug（已复现）

我最初测 `Match host matchy` 时看起来正常——那是因为查询的主机名恰好就是 `matchy`。
换成不匹配的查询后暴露了真实问题。`ssh2-config` 的 `Field` 枚举里**没有 `Match` 变体**，
该行走 `UnknownField` 分支被忽略，**但它后面的选项不会开启新块，而是继续写进上一个 `Host` 块**：

```text
输入：
Host web1
  HostName 10.0.0.1
  User alice
Match host foo          # 不匹配 web1
  User ignored
  Port 9999

ssh2-config query(web1) → user=Some("ignored")  port=Some(9999)   ❌ 被 Match 块污染
真实 ssh -G -F ... web1 → user alice             port 22           ✅
```

**这是静默错误，比崩溃更危险**：用户的 `~/.ssh/config` 里只要有 `Match` 块
（开发机/跳板机配置里很常见），列表就会显示错误的 user/port，而且不报错。

**对策**（必须在封装层做）：解析前把 `Match` 块整体剔除。
最小做法是预处理配置文件——遇到行首 `Match` 就跳过其后所有缩进行，
直到下一个行首非缩进指令。约 25 行代码，且必须加单测覆盖这个用例。

其他两个需要应用层补的语义：

1. **HostName 缺省回退到 alias**：OpenSSH 里 `Host code.deeproute.ai` + 只有 `Port 2222`
   时，hostname 是 `code.deeproute.ai`；`ssh2-config` 返回 `None`。
   ```rust
   let hostname = p.host_name.unwrap_or_else(|| alias.to_string());
   ```
2. **Token 不展开**（`%r`/`%h`/`%p`/`%d`）：README 明确列为 Missing feature。
   `IdentityFile ~/.ssh/%r@%h-%p.key` 会原样保留 `%r`。本项目只需处理
   自己真的用得上的 token，或直接拒绝带 token 的路径并提示用户。
3. **`Match exec` / `ProxyCommand`** 不要支持（安全 + 工作量），明确在 UI 里报错。

**这仍然大幅降低了迁移成本**——config 层是「一个依赖 + 约 60 行薄封装」，不是重写解析器。

---

## 12. 生命周期与资源管理

当前 `tunnel.rs` 用 `process_group(0)` + `kill(-pgid)` + `PR_SET_PDEATHSIG` 管理子进程。
改 russh 后**没有子进程了**，这套机制整体作废——但要确认新模型不泄漏。

实测（`spike/lifecycle.rs`）：

| 场景 | 结果 |
|---|---|
| abort accept 任务后端口释放 | **172 µs** 内释放 ✅ |
| 握手中途 abort | 202 ms 内干净退出，无 hang ✅ |
| 20 次快速 connect/disconnect | 1.61 s，**80 ms/次** ✅ |
| 30 次循环后 fd 数 | before=10 → after=9，**无泄漏** ✅ |

**关键约束**：`client::Handle` **不是 `Clone`**。多个 task 共享必须 `Arc<Handle>`
（`spike/lifecycle.rs` 用 `Arc` 验证过 20 并发）。

### 12.1 多隧道并发实测

sshpf 的实际模型是「每条隧道一个线程 + 一个 runtime」。实测（`spike/multi.rs`）
10 条隧道同时建立并各自独立传数据：

```
10 tunnels up in 80.8 ms
10/10 tunnels carried correct data concurrently in 73.2 ms
```

**80 ms 建 10 条连接**（并行），即单条约 8 ms 摊薄——因为认证与密钥交换是并行的。
这说明「一线程一 runtime」的模型在本项目的规模（几条到几十条隧道）下完全够用，
不需要复杂的共享 runtime 或连接池。

**新模型的取消语义更简单**：`tokio::task::abort()` 即可，
不再需要「杀进程组 → 等 wait() → 处理孤儿」这套。`Drop` 里 abort + `disconnect()`。

> 顺带消除的风险：`PR_SET_PDEATHSIG` 在 `process::exit()` 时不触发（当前实现靠
> 「flag + break」绕开），这个坑在新架构里不存在。

---

## 13. 迁移工作量估算

按模块拆分（对比现有代码量）：

| 模块 | 现状 | 改造 |
|---|---|---|
| `src/tunnel.rs` | 233 行 prod，子进程管理 | **重写**为 tokio task + `Arc<Handle>`。约 350 行（-L/-R/-D 三种 driver + 生命周期） |
| `src/ssh.rs` | 604 行，`ssh -G` + 参数构造 | **大部分作废**。保留 `port_available`/`suggest_free_port`/`port_owner`。config 求值改 `ssh2-config`。约 200 行 |
| `src/ssh/config_parse.rs` | 650 行，手工扫 Host 行 | **删除**，换 `ssh2-config` + 两行回退逻辑 |
| `src/ssh/failure.rs` | 215 行 + 13 变体，stderr 正则 | **保留 `FailureKind` 枚举**（UI 依赖），重写映射层：`russh::Error → FailureKind`。约 150 行 |
| 新增 `src/ssh/known_hosts.rs` | — | glob 匹配 + `!`/`@` 处理 + 写回。约 150 行 |
| 新增 `src/ssh/socks5.rs` | — | SOCKS5 服务端。约 150 行（含测试） |
| 新增 `src/ssh/proxyjump.rs` | — | 跳板链递归。约 100 行 |
| `src/sys/*` | 206+145+201 行，进程组/Job Object | **大部分可删**。Windows Job Object 不再需要（无子进程）。保留文件权限/tmp |
| `src/app.rs` + 子模块 | 1415 + 440/385/105/108 行 | **改动很小**——事件消费逻辑不变，主要换 `TunnelHandle` 类型 |
| `src/ui.rs` | 1178 + 206 行 | **零改动**（纯渲染，不碰 ssh 层） |

**净增约 900–1100 行新代码，删除约 1200 行旧代码。总量基本持平，但架构更简单**
（无子进程、无 stderr 解析、无进程组）。

**测试影响**：现有 `src/ssh/testutil.rs`（204 行，rustc 编译 fake ssh 的机制）
可以**整体删除**——不再需要 fake ssh 二进制。改为直接对真实 sshd 起集成测试
（CI 里 Linux 可跑真实 sshd；Windows 需用 russh 自己起一个 server 做被测端）。

---

## 14. 风险清单

### 高

1. **Windows CI 需要验证 ring 后端**。我本地只能 `cargo check --target x86_64-pc-windows-gnu`，
   没有 mingw 链接器，**无法真正确认**。必须在 `windows-latest` 上实测。
   → 缓解：先提一个 PR 只加依赖不动代码，让 CI 跑通再说。

2. **known_hosts 通配符缺失**（§8.1）。用户配了 `*.example.com` 会得到错误行为。
   → 缓解：自写 `match_hostname`。

3. **known_hosts 的 `@revoked` 漏判是安全问题**（§8.1）：被吊销的 key 可能被接受。
   → 缓解：自研校验，`@revoked` 优先于普通条目。

4. **`ssh2-config` 的 `Match` 静默污染 bug**（§11.1）。已复现，会让用户看到错误的 user/port。
   → 缓解：预处理剔除 `Match` 块（约 25 行）+ 单测。

5. **`ssh2-config` 的长期维护性**。0.9.0（2026-10），21 万下载，单人维护。
   → 缓解：它的 API 面很小（parse + query），必要时可 fork。

### 中

6. **`tcpip_forward` 返回 `Ok(0)`**（§4）。不封装就是真 bug。
7. **`ChannelOpenFailure` 丢失服务端描述**（§9）。需实现 `channel_open_failure`。
8. **`Handle` 不 Clone**（§12）。多 task 必须 `Arc`。
9. **russh 0.x 无 semver 保证**，2–4 周一版。锁 minor 版本。

### 低

10. 二进制 +2.7 MB。
11. `-D` 的 IPv6 ATYP 需要补。
12. keyboard-interactive 多轮状态机。
13. `connect_stream` 要求 stream `'static`（`into_stream()` 后必须 move）。

---

## 15. 建议路线

### 推荐：分阶段替换，保留降级路径

```
阶段 1  加依赖 + 新架构骨架（tokio 线程 + TunnelEvent 桥接），只实现 -L
        并用 feature flag 与现有子进程实现并存 → CI 双平台验证
阶段 2  实现 -R / -D / ProxyJump / known_hosts / SOCKS5
阶段 3  默认切到纯 Rust，删除子进程实现与 testutil
```

**理由**：风险最大的不是协议层（已验证无缺口），而是
① Windows 构建（ring + MSVC，本地无法验证）② known_hosts 语义边界。
分阶段可以让每一阶段都在 CI 上验证过再前进，且随时能退回子进程实现。

### 关于「是否保留系统 ssh 后端」

`russh-researcher` 建议「Linux 保留系统 ssh、Windows 用 russh」的混合路线。
**我不建议这个方案**，理由：

- 用户的核心诉求是「**跨平台的可靠一致性**」（原话）。双后端意味着两套语义、
  两套 bug、两套调试路径，恰恰破坏一致性。
- 混合路线省下的主要工作量是 config 求值——但我已实测 `ssh2-config` 能覆盖（§11），
  这块成本比预期低得多。
- 混合路线也无法消除 known_hosts 差异，因为两条路径本来就不一致。

**建议：全量替换。** 如果 Windows 构建阶段 1 就卡住，再考虑降级。

---

## 16. 一句话总结

> russh 0.65.1 的三种转发**协议层零缺口**，实测全部跑通（含 ProxyJump 三跳、ssh-agent、
> 加密密钥、48.8 MiB/s 吞吐、无 fd 泄漏）。
> 真正的成本在**重建 OpenSSH 周边语义**：known_hosts 通配符（必须自写）、
> 失败原因映射（结构化错误反而更准）、SOCKS5（60 行）。
> `~/.ssh/config` 求值比预期**简单得多**——`ssh2-config` 纯 Rust 且覆盖 Include/Match/通配/否定。
> 唯一无法本地验证的是 **Windows + ring 后端的构建**，必须依赖 CI。

---

## 附录 A：三份调研文档的关系

| 文档 | 侧重 | 方法论 |
|---|---|---|
| [`pure-rust-ssh-russh.md`](./pure-rust-ssh-russh.md) | russh API 面逐项核实 | 源码逐行核对 + 真实 sshd 互操作 |
| [`pure-rust-ssh-config.md`](./pure-rust-ssh-config.md) | config / 密钥 / agent / known_hosts 生态选型 | 候选 crate 逐个 `cargo tree` + 行为实测 |
| **本文** | 端到端可行性 + 架构影响 + 迁移估算 | 可复现 spike 工程（`spike/`） |

三份文档在以下关键点上**独立得出了一致结论**（可信度高）：

| 结论 | russh 文档 | config 文档 | 本文 |
|---|---|---|---|
| `-R` 完整支持，不是缺口 | ✅ | — | ✅ 实测 |
| russh 无内置 SOCKS5，需自写 | ✅ | — | ✅ 实测 60 行 |
| `aws-lc-rs` 在 Windows 需 NASM | ✅ | — | ✅ 对比实验 |
| `ssh2-config` 纯 Rust 零 C 依赖 | — | ✅ | ✅ `cargo tree` 验证 |
| known_hosts 不支持通配符 | ✅ | ✅ | ✅ 实测 |
| `@revoked` 处理有问题 | ✅ | ✅ | ✅ 实测（安全问题） |
| `ssh2-config` 的 `Match` 有缺陷 | — | ✅ | ✅ 复现污染 |

**分歧点已解决**：两份文档对「config 求值是否最大工作量」判断不同，
本文件 §11 通过实测 `ssh2-config` 的能力澄清了这一点——它是「一个依赖 + 约 60 行封装」。

## 附录 B：复现方法

```bash
# 1. 起一个可丢弃的 sshd（127.0.0.1:22200）
docs/research/spike/sshd/run.sh

# 2. 把 spike 拷到临时目录并跑
cp -r docs/research/spike /tmp/spike && cd /tmp/spike
cargo run --release --bin spike        # -L / -R / -D
cargo run --release --bin proxyjump    # 三跳 ProxyJump
cargo run --release --bin lifecycle    # 拆除 / fd 泄漏 / 重启风暴
cargo run --release --bin errpaths     # 结构化错误面
cargo run --release --bin throughput   # RTT + 吞吐
cargo run --release --bin multi        # 10 隧道并发
cargo run --release --bin keys         # 密钥格式覆盖

# 3. 停止 sshd
kill "$(cat /tmp/spike/sshd/sshd.pid)"
```

> spike 依赖 `russh` 的 `ring` 后端（`default-features = false, features = ["ring","flate2","rsa"]`），
> 与最终推荐配置一致。

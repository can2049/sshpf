# 纯 Rust 化：`~/.ssh/config` 解析、密钥加载、agent、known_hosts 生态调研

> 调研时间：2026-11-07（UTC 15:14 抓取元数据）。环境：rustc 1.99.1 / cargo 1.99.1 / OpenSSH_8.2p1 / rsproxy。
> 方法：**能实测的一律实测**。每个"能力"结论都在 `/tmp` 临时工程里 `cargo add` + `cargo tree` + 跑真实代码验证，
> 或用本机 `ssh -G -F` 做差分对照；不能实测的（如 Windows named pipe、`sk-*` 私钥）明确标注"未实测"。
> 仅写入本文件，未改动仓库其他任何文件。

---

## 0. 结论先行

| 问题 | 结论 |
|---|---|
| **config 解析选型** | **采用 `ssh2-config` 0.9.0**（MIT，纯 Rust，**零 C 依赖**）+ **在它之上补 3 处缺陷**（Match 语义、枚举去重、Token 展开）。不推荐自研全量解析器，也不推荐 `russh-config`（无 Include、无 Token、依赖 tokio 全家桶）。 |
| **是否引入 C 依赖** | **不引入**。`ssh2-config` 依赖树实测 `libssh2-sys` / `openssl-sys` / `cc` **全部为 0**；它不是 libssh2 绑定，crate 名里的 "ssh2" 只是历史原因（原本给 `ssh2-rs` 用户做配套）。 |
| **密钥加载** | **`ssh-key`**（RustCrypto），**建议 pin `0.6.7`（`max_stable_version`）** 而非 `0.7.0-rc.11`，理由见 §6.2。纯 Rust，可读 aes256-ctr+bcrypt 加密私钥、ed25519/rsa/ecdsa、`sk-*`。 |
| **ssh-agent** | **`ssh-agent-client-rs` 1.1.4**（同步 API，Unix socket + Windows named pipe 都内置），或改用 `russh` 的 `keys::agent`（但那会拉整个 russh + tokio + ring 的 C 代码）。**推荐前者**。 |
| **known_hosts** | **解析用 `ssh-key::known_hosts`；校验/写回自研 ~120 行**。`russh::keys::known_hosts` 看似现成，实测有 3 个会误判的缺陷（`@revoked` 放行、通配漏判、Tab 漏判，见 §8.3），不建议直接用。 |
| **总依赖增量** | 基线 86 个 normal 包 → 推荐组合 **160 包（+74）**，其中 `ssh2-config` 只占 **+3**（`ssh2-config`/`glob`/`wildmatch`），其余 +71 全在 `ssh-key` 的密码学栈 + agent。**全部纯 Rust、零 C 构建步骤**。对比 `russh` 路线：+43~129 包**且需要 C 工具链**（Windows 交叉编译实测直接失败）。 |

**一句话**：`ssh2-config` 解决了"读文件 + Host 通配 + Include + 首值优先"这些真正繁琐的部分，剩下的 Match 语义和 Token 展开本项目**本来就必须自己定策略**（因为 `ssh -G` 退役后 `%h %r %p` 的展开时机由我们自己决定），所以"`ssh2-config` + 一层薄封装"是性价比最高的落点。

### 0.1 task-4 六个必答问题的定位索引

| task-4 问题 | 对应章节 | 一句话答案 |
|---|---|---|
| **Q1** `ssh2-config`（0.9.0）能否覆盖 Include 递归 / Host 通配多模式 / Match（host,user,exec,originalhost）/ ProxyJump / ProxyCommand / IdentityFile Token / User·Port·HostName / HostName 缺省回退？是否真依赖 libssh2？ | **§2.1**（不是 libssh2 绑定）、**§3.1**（覆盖且正确的 12 项）、**§3.2**（6 个坑） | Include/通配/ProxyJump/User·Port·HostName/缺省回退 **全部支持且与 `ssh -G` 差分一致**；**Match 完全不支持且会污染前一个 Host**；**Token 不展开**；ProxyCommand 仅存字符串。**不依赖 libssh2/openssl，零 C 依赖。** |
| **Q2** 其他候选 `ssh-config` / `sshconf` / `openssh-config` / `russh-config` 等逐一评估 | **§2**（全景对比表）、**§4**（逐个展开） | `ssh-conf`/`sshconf`/`openssh-config`/`openssh_config`/`ssh-config-parser`/`sshconfig2`/`parse-ssh-config` **在 crates.io 上均不存在（404）**。实际候选 5 个：`ssh2-config`（唯一可用）、`ssh2-config-rs`（构建期 197 包）、`russh-config`（无 Include/无 Token/强拉 tokio）、`ssh_config`（停更 6 年）、`sshconfig`（玩具级）、`ssh_cfg`（停更+强制 async）。**全部否决，只留 `ssh2-config`。** |
| **Q3** 结论：直接采用 / vendored 改造 / 自研；若自研给出最小语法子集清单 | **§5**（选型推理 + 落地设计）、**§5.4**（最小子集清单，18 条分三档） | **采用 `ssh2-config` + 薄封装补 3 处**（Match 兜底、Token 展开、枚举去重）。不建议自研全量解析器，但 §5.4 给出了完整的最小语法子集清单（必须 10 条 / 建议 4 条 / 不需要 4 条）可直接当验收标准。 |
| **Q4** `ssh-key` 与 `russh::keys` 关系；能否加载 OpenSSH 私钥（aes256-ctr+passphrase）、ed25519/rsa/ecdsa/rsa-sha2/sk-*；API 示例 | **§6** | `russh::keys` 只是 `ssh-key` 的 re-export + 包装。**全部可以**（逐项实测：ed25519 明文/加密、rsa 加密、ecdsa 均通过；`sk-*` 能解析但签名需硬件、未实测）。**版本建议 0.6.7 而非 0.7.0-rc.11**（与 agent crate 版本统一，否则双份 `ssh-key`）。API 示例见 §6.3。 |
| **Q5** `russh::keys::agent` 对 Unix（`SSH_AUTH_SOCK`）与 Windows（Pageant / `\\.\pipe\openssh-ssh-agent`）的支持 | **§7** | `russh` **两者都支持**（`connect_env` / `connect_pageant` / `connect_named_pipe`），但全 async + 强制 C crypto backend。**推荐 `ssh-agent-client-rs` 1.1.4**：同步 API、Unix socket + Windows named pipe 同一入口、零 C 依赖（Pageant 不支持，可接受）。Unix 侧已实测通过。 |
| **Q6** known_hosts 解析/校验/写回；hashed 条目、`@cert-authority`、`@revoked` | **§8** | **解析**：`ssh-key::known_hosts` ✅（含 hashed / `@cert-authority` / `@revoked` 全部能识别为 `Marker`），但 **Tab/多空格分隔会解析失败**（需前置归一化）。**校验**：`russh` 有 API 但**实测 3 个误判**（`@revoked` 与正常条目共存时会放行 ⚠️、通配漏判、Tab 漏判），且只返回 bool 无法区分失败原因 → **建议自研匹配（~60 行）**。**写回**：`russh::learn_known_hosts_path` 实测正确（自动补 `[host]:port`）。 |

---


## 1. 需求基线：本项目到底需要解析什么

先明确范围，避免过度设计。`src/ssh/config_parse.rs`（845 行）当前做的事，以及纯 Rust 后必须替代的部分：

| 当前实现 | 依赖 `ssh -G`? | 纯 Rust 后的归属 |
|---|---|---|
| 扫 `Host` 行得到**具体别名列表**（跳过 `*`/`?`/`!`） | 否，自己解析 | crate 的 `get_hosts()` 可直接给 |
| `Include` 递归 + glob（`~` / 相对 `~/.ssh` / 绝对） | 否，自己解析（§`expand_include`） | crate 已支持（实测通过） |
| `Match` 块**跳过**（不当作 Host） | 否 | **crate 不支持**，需自己处理（§4.2） |
| `HostName`/`User`/`Port`/`IdentityFile`/`ProxyJump` 首值优先 | 否，但只作 fallback | crate 已支持（实测与 `ssh -G` 一致） |
| ProxyJump 展开、`Host *` 默认值叠加、`Include` 顺序 | **是** | crate 已支持 |
| `IdentityFile` 的 `%d %h %r %p %u %C` 令牌展开 | **是** | **crate 不支持**，需自己实现（§4.3） |
| `ProxyCommand` | **是** | 现状是"遇到就拒绝合成配置"（`src/ssh.rs:352`），可保持 |

**关键前提**：本项目**不做完整 ssh 语义仿真**。目标只是「新建连接时从 config 选主机，拿到 alias → hostname/user/port/proxyjump/identityfile」。`Match exec`、`LocalCommand`、`CanonicalizeHostname` 这些都不是必须的。下面所有评估都以此为标尺。

---

## 2. 候选 crate 全景对比

元数据抓取时间 2026-11-07，来源 crates.io API（`https://crates.io/api/v1/crates/<name>`）+ docs.rs。

| crate | 最新版 | License | 最近发布 | 总下载 / 近 90 天 | Include | Host 通配/取反 | Match | ProxyJump | ProxyCommand | Token 展开 | C 依赖 | 依赖包增量* |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **`ssh2-config`** | **0.9.0** | MIT | 2026-10-20 | 215,442 / 70,002 | ✅ 递归+glob | ✅ | ❌ **完全不认** | ✅ | ⚠️ 仅存字符串 | ❌ | **无** | **+3** |
| `ssh2-config-rs` | 0.7.2 | MIT | 2026-01-26 | 14,859 / 6,812 | ✅ | ✅ | ❌ | ✅ | ⚠️ 同 | ❌ | 构建期 `gix`+`cmake`（197 包） | +? （构建 197） |
| `russh-config` | 0.58.0 | Apache-2.0 | 2026-03-18 | 312,008 / 177,194 | ❌ **不支持** | ✅ | ❌ | ✅ | ✅ 能执行 | ⚠️ 仅 `%u %h %H %p %%` | 无，但拉 **tokio** | **+23** |
| `ssh_config` | 0.1.0 | MPL-2.0 | **2020-01-10** | 29,380 / 15,872 | ❌ | ⚠️ 有 pattern 模块但 API 未接 | ❌ | ❌ | ❌ | ❌ | 无 | +1 |
| `sshconfig` | 0.1.0 | MIT/Apache | 2025-05-20 | 1,385 / 128 | ❌ | ❌ 无通配概念 | ❌ | ❌ | ❌ | ❌ | 无 | +3（`sshconfig`+`shellexpand`+`dirs 6`，注意会多一份 `dirs`） |
| `ssh_cfg` | 0.3.0 | MIT/Apache | **2021-12-02** | 11,665 / 725 | ❓ | ❓ | ❓ | ❓ | ❓ | ❓ | 无 | — |

\* "依赖包增量"= 在本仓库真实 `Cargo.toml` 上 `cargo add` 后，`cargo tree -e normal` 的唯一包数减去基线 86。

**不存在**的 crate（crates.io API 返回 404，已逐一确认）：`ssh-conf`、`sshconf`、`openssh-config`、`openssh_config`、`ssh-config-parser`、`sshconfig2`、`parse-ssh-config`。

### 2.1 为什么 `ssh2-config` 不是 libssh2 绑定（重要澄清）

这是最容易误判的一点。crate 名字里带 `ssh2`，描述是 "an ssh configuration parser for **ssh2-rs**"，但：

```toml
# ~/.cargo/registry/src/.../ssh2-config-0.9.0/Cargo.toml（实测读取）
[dependencies.bitflags] version = "2"
[dependencies.dirs]     version = "7"
[dependencies.glob]     version = "0.3"
[dependencies.log]      version = "0.4"
[dependencies.thiserror] version = "2"
[dependencies.wildmatch] version = "2"

[dev-dependencies.ssh2]   version = "0.9"     # ← 只是 dev-dependency，用于 examples
```

`ssh2` 只在 **dev-dependencies**（`examples/client.rs` 用），不进入任何使用者的依赖树。实测：

```text
$ cargo tree    # 干净工程 cargo add ssh2-config
probe v0.1.0
└── ssh2-config v0.9.0
    ├── bitflags, dirs(→dirs-sys→libc,option-ext), glob, log, thiserror, wildmatch
# 全树 15 个包，grep -icE "libssh2|openssl" → 0

$ cargo check --target x86_64-pc-windows-gnu   # 在含本仓库全量依赖的副本上
    Finished `dev` profile ... in 5.37s          # 无需任何 C 工具链
```

同时在一台**没有 mingw** 的机器上对照验证了反例（`russh` + `ring`）：

```text
$ cd russh-only && cargo check --target x86_64-pc-windows-gnu
  error occurred in cc-rs: failed to find tool "x86_64-w64-mingw32-gcc": No such file or directory
```

**结论：`ssh2-config` 是纯 Rust，与本项目"Windows 免外部依赖"目标不冲突。** 这是本次调研最需要纠正的预设。

### 2.2 `ssh2-config` 的 HostParams 覆盖面（对照 6 个必答问题）

从 `src/params.rs` 的 `HostParams` 结构体（实测读取源码，字段名逐字核对）看，与 SSH 连接真正相关的字段：

```rust
pub struct HostParams {
    pub add_keys_to_agent: Option<bool>,
    pub bind_address: Option<String>,
    pub bind_interface: Option<String>,
    pub ca_signature_algorithms: Algorithms,
    pub certificate_file: Option<PathBuf>,
    pub ciphers: Algorithms,
    pub compression: Option<bool>,
    pub connection_attempts: Option<usize>,
    pub connect_timeout: Option<Duration>,
    pub forward_agent: Option<bool>,
    pub host_key_algorithms: Algorithms,
    pub host_name:      Option<String>,
    pub identity_file:  Option<Vec<PathBuf>>,   // 多行累加，保留出现顺序
    pub ignore_unknown: Option<Vec<String>>,
    pub kex_algorithms: Algorithms,
    pub mac:            Algorithms,
    pub port:           Option<u16>,
    pub proxy_jump:     Option<Vec<String>>,
    pub pubkey_accepted_algorithms: Algorithms,
    pub pubkey_authentication: Option<bool>,
    pub remote_forward: Vec<RemoteForward>,
    pub server_alive_interval: Option<Duration>,   // ⚠️ 没有 server_alive_count_max
    pub tcp_keep_alive: Option<bool>,
    #[cfg(target_os = "macos")] pub use_keychain: Option<bool>,
    pub user:           Option<String>,
    pub ignored_fields:     HashMap<String, Vec<String>>,   // 未知选项落这里
    pub unsupported_fields: HashMap<String, Vec<String>>,   // 认识但未实现的选项落这里
}
```

> ⚠️ **实测确认的覆盖面缺口**（`src/parser.rs` 的 unsupported 分支，逐行核对）：
> - **`StrictHostKeyChecking` / `UserKnownHostsFile` / `GlobalKnownHostsFile` / `HashKnownHosts` / `IdentityAgent` / `IdentitiesOnly`** 全部在 unsupported 列表里，`HostParams` **没有**对应字段（`grep "pub identity_agent\|pub identities_only\|pub strict_host_key" src/params.rs` → 0 命中）。
> - 影响：**无法从 config 里读到每个主机自己指定的 `IdentityAgent` / `UserKnownHostsFile`**。如果本项目要尊重这些按主机配置，需要自己从 `unsupported_fields` map 里再捞（它在 map 里保留了原始 tokens），或者退化为"全局默认路径"。
> - 同样缺 `ServerAliveCountMax`（`HostParams` 只有 `server_alive_interval`）。本项目启动 ssh 子进程时用命令行 `-o ServerAliveCountMax=3` 即可，此缺口无影响。
> - 实测 `ParseRule` 行为（标量选项对比）：

```text
# ParseRule 实测（列 = STRICT / ALLOW_UNKNOWN_FIELDS / ALLOW_UNSUPPORTED_FIELDS / 两者）
LocalForward            STRICT=OK          ← 认识，进 unsupported_fields
StrictHostKeyChecking   STRICT=OK          ← 认识，同上
IdentitiesOnly          STRICT=OK          ← 认识，同上
ControlMaster           STRICT=OK          ← 认识，同上
ServerAliveInterval     STRICT=OK          ← 这个真解析进 HostParams
Match                   STRICT=ERR(unknown field: Match)   只有 ALLOW_UNKNOWN 能过
FooBarBaz（真未知）      STRICT=ERR                           只有 ALLOW_UNKNOWN 能过
UseKeychain（Linux 未编入）STRICT=ERR                          只有 ALLOW_UNKNOWN 能过
```

> 注意 `ALLOW_UNSUPPORTED_FIELDS` 与 `ALLOW_UNKNOWN_FIELDS` 是**两个独立开关**：前者只救"认识但未实现"的字段，后者只救"完全不认识"的字段。**两个都要开。**
> （补充实测：`Field` 枚举里那些 unsupported 选项**不会**让 `STRICT` 失败，因为 `update_host` 返回 `UnsupportedField` 后被 `or_else` 分支"吞掉"并记入 map —— 见 `src/parser.rs:208-222`。真正会让 `STRICT` 失败的只有**不在 `Field` 枚举里**的键，例如 `Match`、`UseKeychain`（Linux）、拼写错误。）

**实践建议**：用 `ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS`。理由：真实 `~/.ssh/config` 必然含该项目用不到的合法选项，用 STRICT 会导致**整个文件解析失败**（这与 `ssh -G` 时代的"宽容"体验差异巨大）。代价是未知选项被丢进 `ignored_fields`/`unsupported_fields`，而不是报错。

---

## 3. `ssh2-config` 实测：覆盖了什么，哪些有坑

全部在本机实测（`/tmp/sshcfg-probe`），并与真实 `ssh -G -F` 做差分。

### 3.1 支持且正确的部分

| 特性 | 实测结果 |
|---|---|
| **Include 递归 + glob** | ✅ `Include conf.d/*.conf` 正确展开；`Include` 在 `Host` 块内时，被包含文件的选项归属该 Host（与 ssh 一致） |
| **Include 多路径** | ✅ 0.8.3 起修复（CHANGELOG 明确记录），逐路径 glob、按序读取、未匹配跳过 |
| **Include 循环检测** | ✅ 返回 `SshParserError::IncludeCycle`（0.9.0 新增）。对照：真实 ssh 是 `Too many recursive configuration includes` + exit 255，两者都是"拒绝"，行为可接受 |
| **Include 文件不存在** | ✅ 静默跳过（与 ssh 一致） |
| **Host 多模式 + 通配 + 取反** | ✅ `Host *.example.com !bad.example.com` 正确；`Host a b c` 三个别名都生效；`Host web1:2222` 这种带端口的模式也支持 |
| **首值优先（first obtained value）** | ✅ 与 ssh 完全一致，实测 `Host *` 在前的场景：ssh2-config 与 `ssh -G` 都返回 `globalu`；`Host *` 在后则都返回 `specific` |
| **`IdentityFile` 多行累加** | ✅ 保留出现顺序（`Vec<PathBuf>`），跨块累加也正确 |
| **`~` 展开** | ✅ `~/.ssh/id_rsa` → `/home/<user>/.ssh/id_rsa`（0.9.0 起可配置 home，见 `home_dir()` builder） |
| **行内注释 / CRLF / 制表符缩进 / `Key=Value` / 大小写不敏感关键字** | ✅ 全部实测通过 |
| **真实 13 主机 config 差分** | ✅ 12 个具体别名，`hostname`/`user`/`port`/`proxyjump`/`identityfile 条数` 与 `ssh -G` **全部一致** |
| **嵌套 Include 的作用域继承**（含取反） | ✅ 见下方专项差分，三个查询点与 `ssh -G` **逐字段一致** |
| **通配块 + 具体块叠加** | ✅ `Host *.example.com {Port 2200}` + `Host web1.example.com {User specific}` → `user=specific, port=2200`，与 `ssh -G` 一致 |

**专项差分：嵌套 Include + 取反 + 兜底 `Host *`**（这是最容易被实现错的组合）：

```ssh
# cfg3
Host *.example.com !app-blocked.example.com
  Include inc3.conf        # inc3.conf 内含：Host app* / HostName included.internal
  Port 2222
Host *
  User fallback
```

| 查询 | ssh2-config | `ssh -G`（权威） | 一致? |
|---|---|---|---|
| `app1.example.com` | hostname=`included.internal` port=2222 user=`fallback` | hostname=`included.internal` port=2222 user=`fallback` | ✅ |
| `app-blocked.example.com` | hostname=`None` port=`None` user=`fallback` | hostname=`app-blocked.example.com`（= 输入串，即"无显式 HostName"）port=22 user=`fallback` | ✅ 语义等价 |
| `app1.example.net` | hostname=`None` port=`None` user=`fallback` | hostname=`app1.example.net` port=22 user=`fallback` | ✅ 语义等价 |

> `None` vs "回退到输入串" 见下方说明，**不是 bug**。这个 case 同时覆盖了「Include 作用域继承外层 `Host` 的 `!negated` 约束」「`Host *` 兜底与首值优先」两个难点，`ssh2-config` 都对了。

差分实测输出（本机真实 `~/.ssh/config`，节选）：

```text
alias                | ssh2-config                                              | ssh -G
cloud_pc             | h=10.0.0.11 u=alice p=None id=1 pj=cloud_pc_jump    | h=10.0.0.11 u=alice p=22 id=1 pj=cloud_pc_jump
cloud_pc_jump        | h=jump.example.com u=bob p=80 id=None           | h=jump.example.com u=bob p=80 id=7
jeprof               | h=10.0.0.12 u=carol p=22 id=1                           | h=10.0.0.12 u=carol p=22 id=1
```

（`p=None` 与 `p=22` 的差异**不是 bug**：ssh 把 `Port` 补成默认 22，crate 保留 `None`。调用方 `.unwrap_or(22)` 即可。`id=1` vs `id=7` 同理：ssh 还会补 `~/.ssh/id_rsa`、`id_ed25519` 等**默认候选**，而 crate 只报告配置里显式写的。本项目只需要"配置里写的那个"，反而更准确。）

### 3.2 有坑的地方（必须写清）

#### 坑 1：`Match` 完全不支持，且会**污染前一个 Host 块** ⚠️ 最严重

`Field` 枚举（`src/parser/field.rs`）里**根本没有 `Match` 变体**。`Match` 走 `UnknownField` 分支：

- `ParseRule::STRICT` → 直接 `unknown field: Match` **整个文件解析失败**
- `ParseRule::ALLOW_UNKNOWN_FIELDS` → 该行被忽略，**但其后的选项不会开启新块，而是继续写进上一个 `Host` 块**

实测证据（三条独立验证）：

```rust
// 输入
Host web1
  HostName 10.0.0.1
  User alice
Match host foo
  User ignored
  Port 9999

// ssh2-config 输出（ALLOW_UNKNOWN_FIELDS）
query("web1") → hostname=10.0.0.1  user=Some("ignored")  port=Some(9999)   // ❌ 被 Match 块污染
get_hosts()   → [["*"], ["web1"], ["web2"]]                               // ❌ Match 块消失，无独立块
```

真实 ssh 的答案（`ssh -G -F` 实测）：

```text
alpha: user=alice  hostname=10.1.1.1     # Match host nomatch 不命中 → 不影响 alpha
beta : user=matched  hostname=10.0.0.2   # Match host beta 命中 → 属下文
```

**后果（本项目视角）**：用户的 `~/.ssh/config` 里只要有 `Match` 块（在开发机/跳板机配置里相当常见），列出的主机就会**显示错误的 user/port**，且不会报错——静默错误，比崩溃更糟。

**这是 `ssh2-config` 唯一必须由我们自己兜住的语义缺口。**

#### 坑 2：`Token` 不展开（README 明确列为 Missing feature）

`README.md` §"Missing features" 原文：

```markdown
### Missing features
- [Match patterns](...#Match) (Host patterns are supported!!!)
- [Tokens](...#TOKENS)
```

实测：

```text
in : IdentityFile ~/.ssh/%r@%h-%p.key
out: /home/<user>/.ssh/%r@%h-%p.key        # %r %h %p 原样保留
in : IdentityFile %d/.ssh/id_ed25519
out: %d/.ssh/id_ed25519                     # 连 ~ 都没展开（首字符不是 ~）
```

**注意**：`ssh -G` 的输出**同样不展开** `IdentityFile` 里的 Token（实测 `ssh -G -F` 原文就是 `identityfile ~/.ssh/%r@%h-%p.key`）——OpenSSH 是在**实际使用时**才展开。所以这条不算 `ssh2-config` 的相对退步：**我们现在也没有展开**（`src/ssh.rs` 直接把 `identityfile` 值透传给合成配置）。纯 Rust 化后如果要真正用 `IdentityFile` 而不只是展示，就**必须自己实现展开**，这是新增工作量。

#### 坑 3：`ProxyCommand` 只存字符串，不解析

想支持 `ProxyCommand` 需要自己 tokenize + `sh -c` 执行，且**无法安全地再陈述进合成配置**。当前 `src/ssh.rs:352` 的策略是"检测到 ProxyCommand 就拒绝"，纯 Rust 后保持即可，**不算新增缺口**。

#### 坑 4：枚举别名时要自己去重、自己过滤通配

`get_hosts()` 返回的是**块序列**（含 `*` 块、含重复块、含 `Host web1:2222`），不是干净别名集。实测：

```text
Host dup {User first}
Host dup {User second}
→ get_hosts() = [["*"], ["dup"], ["dup"]]     # 两个 dup 块都在
```

需要调用方按 `!negated && !contains('*','?') && pattern != "*"` 过滤 + 排序去重——这正好是现有 `host_aliases()` 的逻辑，可复用。

#### 坑 5：`IdentityFile` 带空格路径的解析（0.9.0 已对，但注意版本）

实测 0.9.0 正确：

```text
in : IdentityFile "~/my keys/id_x"
out: /home/<user>/my keys/id_x              ✅
```

（补充实测：真实 ssh 对**未加引号**的同内容会直接报错中止 —— `ssh -G -T -F … `/ stderr: `garbage at end of line; "keys/id_x".`，exit 255，**不是静默丢弃**。`ssh2-config` 则是"吃掉后面两个词"，两者都属错误处理路径，本项目用引号写法即可，无实际影响。）

#### 坑 6：版本年轻 + 破坏性变更频繁

GitHub `veeso/ssh2-config`：**25 stars / 18 forks**，最后一次 push 2026-10-20（就是 0.9.0 发布）。
CHANGELOG 显示 0.8.2 → 0.8.3 → 0.9.0 全部在 **2026-10-12 ~ 2026-10-20 这 8 天内**发布，且 0.9.0 是 breaking change（`SshParserError` 新增 `HomeNotFound`/`IncludeCycle`，`~user` 不再改写）。说明**维护活跃但在快速演进**，建议 `= 0.9` 精确 pin，或 vendored 以锁定行为。

---

## 4. 其他候选逐一评估

### 4.1 `russh-config` 0.58.0 —— 看着最"正统"，实际最不合适

优势：它是 `russh` 官方 mono-repo 的一部分，下载量最高（31.2 万 / 近 90 天 17.7 万），Apache-2.0，能直接配合 `russh` 做 `ProxyCommand` 执行（`src/proxy.rs` 提供 `Stream::proxy_command`）。

但作为**纯解析器**实测下来硬伤很多：

| 缺陷 | 实测证据 |
|---|---|
| **完全不支持 `Include`** | `src/lib.rs` 全文无 `include` 关键字。实测配置含 `Include conf.d/*.conf` → 结果 `ERR Host not found` |
| **不支持 Token** | 只有 `expand_tokens()` 私有方法支持 `%u %h %H %p %%`，**缺 `%d` `%r` `%n` `%C` `%i` `%j` `%k` `%L` `%l`**（对照 [ssh_config.5 TOKENS](https://man.openbsd.org/ssh_config.5) 完整清单） |
| **强制拉 tokio** | `tokio = { features = ["io-util","net","macros","process"] }` 是**非 optional 依赖**。实测 `cargo add russh-config` 单独一个 → **+23 包**（tokio、futures 全家、globset、regex-automata、whoami、socket2…） |
| **全局选项在首个 `Host` 前 → 直接报错** | `User globalu` + `Host x` → `ERR Host not found`。真实 ssh 完全接受，这是**极常见**的 config 写法 |
| **`Match` 块污染前一 Host** | 与 `ssh2-config` 同病：`Match host b {User leaked}` 之后 `query("a")` 返回 `user=leaked` |
| **行内注释不剥离** | `User bob # hi` → `user = "bob # hi"`（真实 ssh 得 `bob`） |
| **引号不剥离** | `HostName "1.2.3.4"` → `host = "\"1.2.3.4\""`（带引号） |
| **`HostName=value` 形式不识别** | `HostName=1.2.3.4` → 无 hostname |
| **只认单空格分隔** | `HostName\t1.2.3.4`（Tab）→ 不识别 |
| **`HostPattern` 用 `globset`** | 语义与 ssh 的 `fnmatch` 有细微差异（`globset` 默认 `*` 不跨 `/`，且支持 `**`），对 hostname 场景影响小，但不是"等价"实现 |

**结论**：`russh-config` 的设计目标是"给 `russh` 用户提供一个够用的 config 读取 + ProxyCommand 执行器"，**不是 OpenSSH 语义的忠实复刻**。它的解析器只有 567 行，覆盖面明显窄于 `ssh2-config` 的 6330 行。**否决**。

### 4.2 `ssh2-config-rs` 0.7.2 —— `ssh2-config` 的纯 Rust fork

README 自述："a **pure Rust** SSH configuration parser library, designed to work with pure Rust SSH implementations like **russh**"。
依赖与 `ssh2-config` 几乎相同（`bitflags`/`dirs`/`glob`/`log`/`thiserror`/`wildmatch` 全一致），**同样不支持 Match 和 Token**（README Missing features 一字不差）。

问题在**构建期**：

```toml
[build-dependencies]
anyhow = "1"
gix = { version = "0.78", ..., features = ["blocking-http-transport-reqwest-rust-tls", "worktree-mutation"] }
```

`build/main.rs` 只在 `RELOAD_SSH_ALGO` 环境变量存在时才干活（默认直接 `return Ok(())`），但 **cargo 仍会编译整个 `gix` 依赖树**。实测：

```text
$ cargo tree -e normal        → 16 包
$ cargo tree -e normal,build  → 213 包     # 构建期多出 197 个包（gix、cmake、openssl-probe…）
```

对一个"只是想读个 config 文件"的 TUI，构建期拉 197 个包（含 `cmake`）**完全不可接受**。而且这个 fork 是单人项目（GitHub `pRizz/ssh2-config-rs`：**0 star**，最后 push 2026-01-26），更新已经落后上游 `ssh2-config` 9 个月。

**结论：否决**。上游 `ssh2-config` 本身就已是纯 Rust，没有 fork 的必要。

### 4.3 `ssh_config` 0.1.0 —— 停更 6 年

MPL-2.0，2020-01-10 发布后**再未更新**。实测 API 极不完整：

```rust
pub struct SSHConfig<'a> { entries: HashMap<HostPattern, HostConfig<'a>> }
pub fn parse_str(source: &'a str) -> Result<Self, Error>
pub fn query<Q: AsRef<str>>(&self, query: Q) -> HostConfig<'a>   // = HashMap<&str, &str>
```

- 无文件读取（`read_file` / `user_config` 都在源码里**被注释掉**，README 说"Not available in windows, we don't know where to get those"）
- 无 `Include`
- 返回 `HashMap<&str,&str>`，`Host` 头无法表达"通配/取反/多模式"的块归属（虽然有个独立 `Pattern` 模块，但没接进查询路径）
- 无 `Match`、无 Token

**结论：否决**（除非只想要一个 tokenizer）。

### 4.4 `sshconfig` 0.1.0 —— 玩具级

GitHub `poneding/sshconfig`，**1 star**，唯一依赖 `shellexpand`，`src/lib.rs` 全文 **158 行**（其中解析循环约 60 行）。实测源码：

```rust
let parts: Vec<&str> = line.splitn(2, ' ').collect();   // 只认单空格
if keyword == "host" { current_entry = Some(HostEntry::new(parts[1].trim().to_string())); }
// 只处理 hostname / port / user / identityfile 四个键
```

- 无 `Include`、无通配、无 `Match`、无 `ProxyJump`、无 Token
- `HostEntry::new` 把 `user` 默认成 `"root"`、`identity_file` 默认成 `~/.ssh/id_rsa`（凭空捏造默认值）
- 下载量 1,385 / 近 90 天 128

**结论：否决**。

### 4.5 `ssh_cfg` 0.3.0 —— 停更 + 异步

2021-12-02 之后再无发布，自述"Parses `~/.ssh/config` **asynchronously**"。本项目明确**不引入 tokio**（`docs/research/tech-options.md` §0："TUI / 并发：单线程 UI 轮询…不要 tokio"）。**否决**，未做深入实测（价值不足）。

---

## 5. 选型结论（问题 3）：`ssh2-config` + 薄封装

### 5.1 三个方案横向对比

| 方案 | 工作量 | 风险 | 依赖增量 | 结论 |
|---|---|---|---|---|
| **A. 直接 `ssh2-config` 裸用** | ~0 | ⚠️ Match 污染 → 静默错误 | +3 包 | **不可接受**（会给出错误 user/port） |
| **B. 自研最小子集解析器** | 中高（~500–700 行 + 大量测试） | 中（Include 顺序、首值优先、通配取反、引号/注释、glob 边界） | +0~2 包 | 可行但**重复造轮子** |
| **C. `ssh2-config` + 薄封装补 3 个缺口** ✅ | 低（~150–200 行 + 测试） | 低 | +3 包 | **推荐** |

### 5.2 为什么选 C 而不是 B

自研听起来"更可控"，但要读一遍现有 `ConfigParser`（`src/ssh/config_parse.rs:312-632`，约 280 行非空非注释代码）就会发现，它**已经**在手工实现：注释剥离、引号 tokenize、`=` 分隔、Include glob 递归、Include 循环检测、跳转通配/取反、首值优先。这些正是 `ssh2-config` 已经实现且**在真实 13 主机 config 上与 `ssh -G` 差分一致**的部分。重写它们要重新踩一遍 §3.1 表格里的所有边界（行内注释、CRLF、Tab 缩进、`Key=Value`、大小写、带空格引号路径、Include 在 Host 块内…）。

反过来，`ssh2-config` 真正缺的**只有 3 件事**，且其中 2 件无论如何都要自己写：

1. **Match 语义**（`ssh2-config` 没有 → 必须自己判）——因为退役 `ssh -G` 后，"`Match exec` 该不该执行"这个决策本来就要本项目自己做（我们**不应该**执行任意 shell 命令，这是安全边界，不是技术缺失）。
2. **Token 展开**（`ssh2-config` 没有，`ssh -G` 也没有 → 必须自己写）——这是纯新增能力。
3. **别名枚举去重**（现有 `host_aliases()` 逻辑可直接搬）。

### 5.3 落地设计

```rust
// src/ssh/config_parse.rs（改造后骨架）
use ssh2_config::{ParseRule, SshConfig};
use std::io::BufReader;

const RULES: ParseRule = ParseRule::ALLOW_UNKNOWN_FIELDS.union(ParseRule::ALLOW_UNSUPPORTED_FIELDS);

/// 1) 解析：一次读入，Match 安全性由 pre-scan 保证
fn load(paths: &[PathBuf]) -> Result<SshConfig> { /* ... */ }

/// 2) 枚举具体别名（复用现有 host_aliases 的过滤规则）
pub fn list_hosts() -> Result<Vec<SshHost>> {
    let cfg = load(&default_config_paths())?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for host in cfg.get_hosts() {
        for clause in &host.pattern {
            // 跳过通配/取反/空，去重（复用现有逻辑）
            if clause.negated || clause.pattern.contains(['*', '?']) { continue; }
            if !seen.insert(clause.pattern.clone()) { continue; }
            out.push(to_ssh_host(&clause.pattern, &cfg.query(&clause.pattern)));
        }
    }
    out.sort_by(|a, b| a.alias.cmp(&b.alias));
    Ok(out)
}
```

**必须补的三层**：

#### 补层 1：Match 安全的兜底（2 种做法，择一）

- **做法 a（推荐，简单）**：解析前 **pre-scan 原始文本**，检测是否存在行首（去缩进后）为 `Match` 的行。若存在，**放弃 `ssh2-config` 的 query，退回自研的块解析器**（即现有的 `ConfigParser`，它已经正确实现"Match 一下就把 skipping 置位、后续选项不归属任何 Host"）。这样：
  - 99% 的 config（无 Match）走 `ssh2-config`，享受完整 Include/通配/首值优先
  - 有 Match 的少数 config 走保守路径，**不会给出错误值**（宁可少解析，不可解析错）
  - 工作量最小，且是**渐进增强**——现有 `ConfigParser` 一行都不用删

- **做法 b（更彻底）**：自己实现 `Match` 求值（`Match all` / `Match host` / `Match user` / `Match originalhost` 求值；`Match exec` **一律视为不匹配**，不执行 shell）。工作量约 +150 行，收益是"Match 里的 `User`/`ProxyJump` 也能正确生效"。

  > 实测真实 ssh 行为：`Match all { User matcheduser }` 对**所有**主机生效；`Match host foo` 只对 `hostname==foo` 生效。如果采用做法 b，注意 `Match` 块的选项**不覆盖**已有值而是按"首值优先"参与合并——这与 `Host` 块完全同构。

**建议先做 a，把 b 列为 v2。**

#### 补层 2：Token 展开（新增 ~60 行）

```rust
/// OpenSSH TOKENS 子集。返回 None 表示含本项目不支持的 token，
/// 调用方应保留原文而不是给出错误展开值。
fn expand_tokens(raw: &str, ctx: &TokenCtx) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' { out.push(c); continue; }
        match chars.next()? {
            '%' => out.push('%'),
            'd' => out.push_str(&ctx.home),        // 用户家目录
            'h' => out.push_str(&ctx.hostname),    // 目标主机名（已解析的 HostName）
            'n' => out.push_str(&ctx.original),    // 命令行给的原始名（= alias）
            'p' => out.push_str(&ctx.port.to_string()),
            'r' => out.push_str(&ctx.user),        // 远程用户名（已解析）
            'u' => out.push_str(&ctx.local_user),  // 本地用户名
            // %C %i %j %k %L %l %H %T %t %f：本项目不用，遇到即判不支持
            _ => return None,
        }
    }
    Some(out)
}
```

**展开时机**（关键设计决策）：只在**真正要用这个文件路径时**（如给 `ssh -i` 传参、或读取私钥）展开，**不要**在解析阶段就地展开——因为 `%h` 依赖 `HostName` 的最终值，而 `HostName` 本身可能也含 Token（`HostName %h.example.com`，真实 ssh 支持，实测 `hostname tok.example.com`）。用一次性就地展开会产生顺序依赖 bug。

#### 补层 3：`HostName` 缺省回退 + 默认值归一

```rust
let hostname = params.host_name.clone().unwrap_or_else(|| alias.to_string());  // 与 ssh 一致
let port = params.port.unwrap_or(22);
let user = params.user.clone();                                              // None 表示"用当前登录用户"
```

> 实测确认真实 ssh 的缺省行为：`ssh -G somerandomhost` → `hostname somerandomhost`、`user wangcan`、`port 22`。即 **hostname 回退到输入串，user 回退到本地用户名，port 回退 22**。`user` 的回退我们**不建议**硬编码（`SSH` 交给 ssh 自己决定更安全），保持 `Option<String>` 即可。

### 5.4 如果最终仍决定自研（方案 B）：最小语法子集清单

按"必须 / 建议 / 不需要"三档给出，可直接作为验收标准：

**必须（缺了功能就坏）**
1. 行解析：`keyword value`，支持 **1+ 空格或 Tab** 分隔、`Key=Value` 形式、**大小写不敏感**关键字
2. 注释：行首 `#`；**行内 `#` 需在引号外且位于词边界**（`Host a#b` 不算注释，`Port 22 # x` 算）
3. 引号：双引号/单引号包裹含空格的值（`IdentityFile "~/my keys/id_x"`），引号内 `\"` 转义
4. 行尾：兼容 **CRLF**（Windows 编辑过的 config）
5. `Host` 块：**多模式**（`Host a b`）、**通配**（`*` `?`，`fnmatch` 语义）、**取反**（`!pattern`，命中即整块失效）
6. `Include`：**递归**、glob（`*` `?`）、`~` 展开、相对路径**相对 `~/.ssh`**、包含顺序 = 文本顺序、**循环检测**（深度上限或 visited 集）
7. 首值优先（除 `IdentityFile` 累加外，**所有**选项都是首个命中值生效）
8. 需要的选项：`HostName` `User` `Port` `IdentityFile`(累加) `ProxyJump` `ProxyCommand`(仅记录)
9. `HostName` 缺省回退到输入串；`Port` 缺省 22
10. **`Match` 行必须"开新上下文"**：其后选项不得归属前一 `Host`（即现有 `skipping` 标志的语义）

**建议（有更好，无则降级）**
11. `Match` 求值：`all` / `host` / `user` / `originalhost`；`exec` **一律不匹配**（安全）
12. Token 展开：`%d %h %n %p %r %u %%`（`%C` 等可判不支持）
13. 未知选项：**忽略**而不是报错（宽容模式）
14. 多文件叠加：`/etc/ssh/ssh_config` 先、`~/.ssh/config` 后

**不需要（明确不做，避免范围蔓延）**
15. `CanonicalizeHostname` / `CanonicalDomains`（DNS 相关，`ssh -G` 也不会替我们解析）
16. `ProxyCommand` 执行、`LocalCommand`、`Match exec` 执行
17. `LocalForward` / `RemoteForward` / `DynamicForward` 解析（本项目自己管转发）
18. 写回 config 文件

> 对照：现有 `src/ssh/config_parse.rs` 的 `ConfigParser` **已实现 1–10 中的绝大多数**（缺 12 Token、11 Match 求值；10 已用 `skipping` 实现）。所以"自研"的真实成本主要在**补齐测试**和**Include glob 边界**，不在从零开始。

---

## 6. 密钥加载（问题 4）：`ssh-key` vs `russh::keys`

### 6.1 两者关系：`russh::keys` 是 `ssh-key` 的包装层

```rust
// russh-0.65.1/src/keys/mod.rs
pub use ssh_key::{self, Algorithm, Certificate, EcdsaCurve, HashAlg, PrivateKey, PublicKey};
pub use known_hosts::{check_known_hosts, check_known_hosts_path};
pub use crate::cert::PublicKeyOrCertificate;
```

`russh` 把 `ssh-key` **原样 re-export**，并在外面包了：`load_secret_key` / `decode_secret_key` 便捷函数、agent 客户端/服务端、known_hosts 检查。

**`russh-keys` 已是废弃过渡态**：最后一版 `0.50.0-beta.7` 停在 **2025-01-08**，稳定版 `0.49.2` 停在 2024-12-20，功能已并入 `russh` 主 crate。**新代码不要依赖 `russh-keys`**，直接依赖 `ssh-key`（非 agent/known_hosts 部分）+ `russh`（如需要 agent）。

### 6.2 `ssh-key` 版本选择：**建议 0.6.7（stable），而非 0.7.0-rc.11**

| | 0.6.7 | 0.7.0-rc.11 |
|---|---|---|
| 发布 | 2024-10-15 | 2026-06-29 |
| 状态 | **max_stable_version** | 预发布（rc） |
| 下载 | 9,957,405 | 3,375,863 |
| License | Apache-2.0 OR MIT | Apache-2.0 OR MIT |
| `encryption` feature | ✅（aes-ctr/aes-cbc/aes-gcm/chacha20poly1305 + bcrypt-pbkdf） | ✅（cipher/aes + chacha20poly1305 + bcrypt-pbkdf） |
| `sk-*` 私钥 | ✅（`SkEd25519`、`SkEcdsaSha2NistP256` 都在 `KeypairData`） | ✅ |
| `known_hosts` | ✅ 同 API | ✅ 同 API |
| 与 `ssh-agent-client-rs` 兼容 | ✅ **统一到同一份 0.6.7** | ❌ **版本冲突，树里同时存在 0.6.7 和 0.7.0-rc.11** |

**版本冲突实测**（这是推荐 0.6.7 的硬理由）：

```text
$ cargo add ssh-key@0.7.0-rc.11 && cargo add ssh-agent-client-rs
$ cargo tree --prefix none | grep ^ssh-key
ssh-key v0.6.7
ssh-key v0.7.0-rc.11          # ← 两份同时编译，类型不互通
```

`ssh-agent-client-rs` 1.1.4 依赖 `ssh-key ^0.6`。若本项目用 0.7.0-rc，从 agent 拿到的 `PublicKey` 与自己的 `PrivateKey` 是**不同类型**，无法比较/签名，必须手工做字节转换。而用 0.6.7 + `ssh-agent-client-rs` 实测统一：

```text
$ cargo tree --prefix none | grep ^ssh-key
ssh-key v0.6.7
ssh-key v0.6.7 (*)            # 一份
```

**代价**：0.6.7 较旧（2024-10），但密钥文件格式（RFC4253 + OpenSSH）**没有变化**，`ssh-key` 0.7 主要是 API 迁移（`ssh-encoding` 0.2→0.3、`der`/`signature` 大版本升级）和新算法。对本项目"读私钥、解 passphrase、读 known_hosts"三个需求，0.6.7 完全够用（下面全部在 0.6.7 上实测通过）。

> 若团队更看重"用最新"，也可选 0.7.0-rc.11，但必须接受：① rc 版本 API 可能再变；② 与 agent crate 版本不一致，需要自己桥接类型。**不推荐**。

### 6.3 实测：能不能加载各种 OpenSSH 私钥

测试样本由本机 `ssh-keygen` 真实生成（OpenSSH_8.2p1）。测试代码（`/tmp/sk6`，`ssh-key 0.6.7` + `features = ["std","encryption","ed25519","rsa","ecdsa","p256"]`）：

```rust
use ssh_key::PrivateKey;

let bytes = std::fs::read(path)?;
let key = PrivateKey::from_openssh(&bytes)?;          // 解析（含加密头）
if key.is_encrypted() {
    let key = key.decrypt(passphrase)?;                // 解密
}
let pubkey = key.public_key();                         // → PublicKey
```

实测结果：

| 密钥 | 生成命令 | `from_openssh` | 加密算法识别 | `decrypt` | `algorithm()` |
|---|---|---|---|---|---|
| ed25519 明文 | `ssh-keygen -t ed25519 -N ""` | ✅ | — | — | `Ed25519` |
| **ed25519 + aes256-ctr + bcrypt** | `ssh-keygen -t ed25519 -N secret123` | ✅ | `Cipher::Aes256Ctr`, `Kdf::Bcrypt{salt,rounds:16}` | ✅ | `Ed25519` |
| 同上，**密码错误** | — | ✅ | — | ❌ `cryptographic error`（正确拒绝） | — |
| rsa 2048 + aes256-ctr | `ssh-keygen -t rsa -b 2048 -N secret123` | ✅ | 同上 | ✅ | `Rsa { hash: None }` |
| ecdsa nistp256 明文 | `ssh-keygen -t ecdsa -b 256 -N ""` | ✅ | — | — | `Ecdsa { curve: NistP256 }` |

`decrypt` 后还能完整往返写回（`to_openssh(LineEnding)` 长度 406 / 1858 字节），公钥指纹正确（`SHA256:tlfm6LgObYtz...`）。

**支持矩阵**（源码 + 实测交叉确认）：

- ✅ **ed25519**（`KeypairData::Ed25519`）— 实测
- ✅ **RSA**（`KeypairData::Rsa`）— 实测；`rsa-sha2-256` / `rsa-sha2-512` 是**签名算法**（`Algorithm::RsaSha2_256/512` 在 `algorithm.rs`），私钥本身统一是 `ssh-rsa`，`algorithm()` 返回 `Rsa{hash}` 由调用方指定 hash
- ✅ **ECDSA**（nistp256/384/521，`KeypairData::Ecdsa`）— p256 实测，p384/p521 为同构代码路径
- ✅ **DSA**（`KeypairData::Dsa`，需 `dsa` feature，默认关）— 未实测（已过时，不建议开）
- ✅ **`sk-ssh-ed25519@openssh.com`**（`KeypairData::SkEd25519`）/ **`sk-ecdsa-sha2-nistp256@openssh.com`**（`KeypairData::SkEcdsaSha2NistP256`）— **能解析，但签名需要硬件令牌**（`ssh-key` 只做格式解析，签名要设备配合）。**未实测**（无 FIDO 设备）。对本项目够用：列出/展示可以，真正用 `sk-*` 认证建议走 agent。
- ✅ **加密私钥**：`aes256-ctr` + `bcrypt`（实测最主流组合）；`ssh-cipher 0.3` 还支持 `aes128/192/256-cbc`、`aes256-gcm`、`chacha20-poly1305`、`tdes-cbc`（`Cipher` 枚举）
- ✅ **OpenSSH 证书**（`ssh_key::Certificate`）：`from_openssh` / `to_openssh` / `verify_signature` / `validate(ca_fingerprints)` / `valid_principals` / `valid_before` 等齐全（源码 `src/certificate.rs`）
- ⚠️ **PKCS#8 / PKCS#5 / PPK**：`russh::keys` 提供 `format::{openssh,pkcs5,pkcs8}` 模块；`ssh-key` 侧需 `ppk` feature。本项目只需 OpenSSH 格式，不必开。

**API 示例（完整可运行逻辑）**：

```rust
use ssh_key::PrivateKey;

pub fn load_key(path: &Path, passphrase: Option<&str>) -> Result<PrivateKey> {
    let bytes = std::fs::read(path)?;
    let key = PrivateKey::from_openssh(&bytes)?;
    if !key.is_encrypted() {
        return Ok(key);
    }
    let Some(pw) = passphrase else {
        return Err(anyhow!("私钥已加密，需要 passphrase"));
    };
    Ok(key.decrypt(pw)?)          // Err(Error::Crypto) 表示密码错误
}
```

### 6.4 与 `russh::keys` 便捷 API 的对比

`russh` 提供一行版本（实测可用）：

```rust
let key = russh::keys::load_secret_key(path, Some("secret123"))?;
// → 已自动解密，is_encrypted() == false
```

**但为了这一个便利函数引入整个 `russh` 不划算**：`russh` 0.65.1 需要显式选择 crypto backend（`ring` 或 `aws-lc-rs`），两者都是 **C 代码**：

```text
# russh 源码 lib.rs:90
#[cfg(not(any(feature = "ring", feature = "aws-lc-rs")))]
compile_error!("`russh` requires enabling either the `ring` or `aws-lc-rs` feature as a crypto backend.");
```

实测 `cargo add russh --no-default-features --features ring`：依赖 **129** 个包（vs 基线 86），且 `ring` 编译时通过 `cc` 编译 30 个 `.o`（`target/debug/build/ring-*/out/*.o`），Windows 目标直接失败：

```text
error occurred in cc-rs: failed to find tool "x86_64-w64-mingw32-gcc"
```

`ring` 的 [BUILDING.md](https://github.com/briansmith/ring/blob/main/BUILDING.md) 明确："*ring* currently requires a C (but not C++) toolchain. For Windows targets, 'Build Tools for Visual Studio 2022' … must be installed."

**结论：密钥加载只依赖 `ssh-key`，不引入 `russh`。** 那 3 行地自己写 `load_key()` 完全不值得换一个 C 工具链依赖。

---

## 7. ssh-agent（问题 5）

### 7.1 `russh::keys::agent` 的能力（已完成，但绑在 russh 上）

源码实测（`russh-0.65.1/src/keys/agent/client.rs`）：

| 平台 | API | 说明 |
|---|---|---|
| Unix | `AgentClient::connect_uds(path)` | 显式 UDS 路径 |
| Unix | `AgentClient::connect_env()` | 读 `SSH_AUTH_SOCK`；`ENOENT` 映射为 `Error::BadAuthSock`（更友好的错误） |
| **Windows** | `AgentClient::connect_pageant()` | Pageant 专用（依赖 `pageant` crate，Pageant 是 PuTTY 的 agent） |
| **Windows** | `AgentClient::connect_named_pipe(path)` | 通用 named pipe，带 `ERROR_PIPE_BUSY(231)` 重试循环（50ms 间隔） |

Windows OpenSSH 的标准 agent 端点 `\\.\pipe\openssh-ssh-agent` **需要调用方自己传**——`russh` **没有**"自动发现 Windows 默认 agent"的封装（源码中无该字符串）。所以 Windows 上要么写死 `\\.\pipe\openssh-ssh-agent`，要么先试 named pipe 再 fallback Pageant。

实测 Unix 侧（真实 `ssh-agent` + `ssh-add`）：

```rust
let mut c = russh::keys::agent::client::AgentClient::connect_env().await?;
let ids = c.request_identities().await?;
// → agent identities: 1 / "test-plain" alg=Ed25519     ✅ 实测通过
```

**代价**：`AgentClient` 全是 **async**（`async fn connect_env`、`request_identities().await`），必须配 tokio runtime。而本项目当前架构是"单线程 UI 轮询 + 每转发一 stderr 读线程 + mpsc，**不要 tokio**"（`docs/research/tech-options.md` §0）。为了拿 agent 身份列表引入 tokio 运行时 + `russh` + `ring`（C 代码），性价比极低。

### 7.2 推荐：`ssh-agent-client-rs` 1.1.4（同步 API，零 C 依赖）

| 项 | 值 |
|---|---|
| 版本 | 1.1.4（2026-10-14） |
| License | MIT OR Apache-2.0 |
| 下载 | 206,442 / 近 90 天 132,303 |
| GitHub | `nresare/ssh-agent-client-rs`，10 stars，最后 push 2026-10-14 |
| 依赖 | `bytes`, `interprocess`, `signature`, `ssh-encoding`, `ssh-key ^0.6`, `thiserror` |
| C 依赖 | **无**（`cargo tree` grep `cc\|cmake\|openssl\|libssh2\|ring\|aws-lc` → 0 命中） |
| API | **同步**（`Client::connect(&Path)`） |

Windows 支持（README 原文 + 源码确认）：

```rust
// src/lib.rs:99-107
// If you want to communicate with the ssh-agent shipped with windows you probably want to pass
// Path::new(r"\\.\pipe\openssh-ssh-agent")
#[cfg(target_family = "windows")]
pub fn connect(path: &Path) -> Result<Client> {
    let pipe = DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path(path)?;   // interprocess
    ...
}
```

即：**同一个 `Client::connect(&Path)`，Unix 传 socket 路径，Windows 传 `\\.\pipe\openssh-ssh-agent`**，`interprocess` 负责平台差异。这正是我们要的抽象。

实测（`/tmp/agent-probe`，真实 `ssh-agent`）：

```rust
let mut c = ssh_agent_client_rs::Client::connect(Path::new(&std::env::var("SSH_AUTH_SOCK")?))?;
let ids = c.list_all_identities()?;
// → identities: 1
//   PublicKey(PublicKey { key_data: Ed25519(...), comment: "test-plain" })    ✅ 实测通过
```

能力清单（README）：列身份、添加身份、删身份、清空、**签名**（`sign`）。
未实现：带约束的添加、`SSH_AGENTC_ADD_SMARTCARD_KEY` 专用消息（但 FIDO2 resident key 走普通 add 消息也能用）。

**注意 API 细节**（实测踩到）：
- `Client::connect` 收 `&Path` 而非 `&str`
- `list_identities()` 已 **deprecated**，用 `list_all_identities()`（后者保留证书身份）
- `ssh-key ^0.6` 依赖 → 与 §6.2 建议的 0.6.7 天然统一

### 7.3 agent 侧结论

| 方案 | 同步 | Unix | Windows OpenSSH pipe | Windows Pageant | 依赖增量 | C 依赖 |
|---|---|---|---|---|---|---|
| **`ssh-agent-client-rs` 1.1.4** ✅ | ✅ | ✅ 实测 | ✅ 内置路径约定 | ❌（需自己实现协议） | +~20 包（含 ssh-key 0.6.7 复用） | **无** |
| `russh::keys::agent` | ❌ async | ✅ 实测 | ✅ `connect_named_pipe` | ✅ `connect_pageant` | +129 包（russh+ring+tokio） | **ring C 代码** |

**推荐 `ssh-agent-client-rs`**。Pageant 支持缺失可接受：Windows 11 自带 OpenSSH agent，Pageant 属 PuTTY 生态小众场景，需要时可后续补。

---

## 8. known_hosts（问题 6）

### 8.1 三个层次，能力差别很大

| 层次 | 提供者 | 能力 |
|---|---|---|
| **解析** | `ssh-key::known_hosts` | ✅ 完整（`KnownHosts` 迭代器、`read_file`、`Entry{marker, host_patterns, public_key}`、`Marker::{CertAuthority, Revoked}`、`HostPatterns::{Patterns, HashedName}`） |
| **校验 + 写回** | `russh::keys::known_hosts` | ✅ 有函数（`check_known_hosts_path`、`known_host_keys_path`、`learn_known_hosts_path`），但**实测有 3 个缺陷** |
| **证书链校验** | `ssh-key::Certificate::validate(ca_fingerprints)` | ✅ 选举 CA、校验签名/有效期/principals |

### 8.2 `ssh-key::known_hosts` 解析：✅ 够用，但有 Tab 缺陷

实测（`ssh-key 0.7.0-rc.11` 与 `0.6.7` 行为一致）：

| 输入形式 | 结果 |
|---|---|
| `host ssh-ed25519 AAAA...`（单空格） | ✅ 1 条 |
| `host\tssh-ed25519\tAAAA...`（Tab 分隔） | ❌ `Error::FormatEncoding`（"length invalid"） |
| `  host ssh-ed25519 ...`（行首空格） | ❌ `FormatEncoding` |
| `host  ssh-ed25519  ...`（**多空格**） | ❌ `FormatEncoding`（"length invalid"） |
| 空行 + `#` 注释 | ✅ 正确跳过 |
| 注释在**行尾**（`#`） | ⚠️ `next_line_trimmed()` 用 `split_once('#')` **无条件截断**，含 `#` 的 hostname 会被切（罕见） |
| `\|1\|salt\|hash ssh-ed25519 ...`（hashed） | ✅ 解析成 `HostPatterns::HashedName{salt, hash}` |
| `@cert-authority *.example.com ...` | ✅ `Marker::CertAuthority` |
| `@revoked bad.example.com ...` | ✅ `Marker::Revoked` |
| `[host]:2222 ssh-ed25519 ...` | ✅ 保留在 `Patterns` 里（原样字符串） |
| `*.example.com ssh-ed25519 ...` | ✅（通配原样保留，**不做匹配**） |
| `!bad.example.com,good.example.com ...` | ✅（同上） |

**根因**（源码 `Entry::from_str`）：用 `split_once(' ')` 硬切，**只认恰好一个空格**：

```rust
let (hosts_str, public_key_str) = line.split_once(' ').ok_or(Error::FormatEncoding)?;
```

而真实 `ssh-keygen` 产出的 known_hosts 完全可能是 Tab 分隔（实测 `ssh-keygen -F` 能正常识别 Tab 分隔条目）。所以这是**真实存在的兼容性缺陷**，不只是理论问题。

**缓解**：解析前把行内的 `\t` 归一化为空格、`split_whitespace` 重排，再喂给 `KnownHosts`。约 5 行代码。**这是采用 `ssh-key` 解析 known_hosts 的必要前置处理。**

**关键限制**：`ssh-key::known_hosts` **只解析，不匹配**——没有 `matches()` / `verify()` / `check()` 方法，也没有 hostname 哈希函数（`HashKnownHosts` 写回不支持）。匹配逻辑必须自己写。

### 8.3 `russh::keys::known_hosts` 校验：有 API 但**实测有 3 个误判**

API（`pub mod known_hosts`，只有 `check_known_hosts` / `check_known_hosts_path` 被 re-export 到 `russh::keys` 顶层）：

```rust
pub fn check_known_hosts_path(host, port, pubkey, path) -> Result<bool, Error>
pub fn known_host_keys_path(host, port, path)       -> Result<Vec<(line, PublicKey)>, Error>
pub fn learn_known_hosts_path(host, port, pubkey, path) -> Result<(), Error>   // 追加写入
```

实测行为矩阵（`/tmp/rc-probe`）：

| 场景 | 期望 | `check_known_hosts_path` 实测 | 判定 |
|---|---|---|---|
| 普通空格分隔条目，key 匹配 | true | `Ok(true)` | ✅ |
| **`\|1\|salt\|hash` hashed 条目** | true | `Ok(true)` | ✅ 支持 hashed |
| 非标准端口 `[h]:2222` | true | `Ok(true)` | ✅ |
| key **不匹配**（同 host 已知但换 key） | Err（host key changed） | `Err(KeyChanged { line: 1 })` | ✅ 正确报警 |
| **Tab 分隔条目** | true | **`Ok(false)`** | ❌ **漏判**（误判为未知主机） |
| **行首空格**条目 | true | **`Ok(false)`** | ❌ **漏判** |
| **`@revoked` 条目（只有 revoked 行）** | false（应拒绝） | **`Ok(false)`** | ✅ 碰巧对，但**理由是"没匹配到"而非"已吊销"**，语义错误 |
| **`@revoked` 与正常条目同时存在**（同 host + 同 key） | false（revoked 优先） | **`Ok(true)`** | ❌ **严重漏判：会接受被吊销的 key** |
| **`@cert-authority` 条目** | 不匹配（CA 行不参与 host key 比对） | `Ok(false)` | ✅ 碰巧对 |
| **通配 `*.example.com`** | true | **`Ok(false)`** | ❌ **漏判**（不支持通配匹配） |
| 逗号列表 `a,b` | true | `Ok(true)` | ✅ |
| 取反 `!bad,good` 查 `good` | true | `Ok(true)` | ✅ |

**根因**（源码 `known_hosts.rs`）：`match_hostname()` 只处理 `|1|` hashed 和**精确字符串相等**，**没有 `::` 通配匹配**；`known_host_keys_path()` 用 `split(' ')` 切分（Tab 就断了）；**`@revoked` / `@cert-authority` 从未被检查**（`buffer.as_bytes().first() == Some(&b'#')` 只跳注释，`@` 开头的行被当普通行解析，`marker` 字段被丢弃）。

`learn_known_hosts_path` 写回实测正确：

```text
写入 [new.example]:2200 与 new.example（port 22）→
[new.example]:2200 ssh-ed25519 AAAAC3...
new.example ssh-ed25519 AAAAC3...
```
正确补 `[host]:port` 格式、正确补末尾换行。**写回能力可用**。

### 8.4 known_hosts 侧建议

| 需求 | 方案 | 工作量 |
|---|---|---|
| **解析** | `ssh-key::known_hosts::KnownHosts`（**先做 Tab/多空格归一化**） | ~10 行 |
| **匹配** | **自研** ~60 行：`::` 通配（`*` `?`）、`!` 取反、`[host]:port` 拆分、`\|1\|` hashed（HMAC-SHA1） | ~60 行 |
| **`@revoked` 优先拒绝** | 自研（`ssh-key` 给 `Marker`，语义自己定） | ~10 行 |
| **`@cert-authority`** | 自研：命中 CA 行 → 用 `Certificate::validate(&[ca_fingerprint])` 校验对端证书 | ~30 行（如果做证书认证） |
| **hashed 匹配** | 自研：`Hmac<Sha1>::new_from_slice(&salt)` + `chain_update(host)` + `verify_slice(&hash)` | ~15 行（`hmac` 0.12 + `sha1` 0.10，均纯 Rust，与 `ssh-key 0.6.7` 共用 `digest 0.10`，实测 +0 个 C 依赖） |
| **写回（learn）** | `russh::keys::known_hosts::learn_known_hosts_path` **或**自研 ~30 行 | 低 |
| **hashed 写回**（`HashKnownHosts yes`） | **`ssh-key` 不提供**；需自研（salt 随机 + HMAC-SHA1） | ~20 行 |

> 为什么不用 `russh` 的校验函数：它**只会返回 bool**，而本项目需要区分"未知主机（可提示用户 learn）/ key 变更（必须拒绝）/ revoked（必须拒绝）/ CA 签发（走证书校验）"四种状态——这四种状态正是 `src/ssh/failure.rs` 要归类的失败原因（`docs/research/prior-art.md` §1："失败原因要分类 + 短句（`host key mismatch`…）"）。用 bool 会丢掉全部信息。所以**自研匹配反而是必需的**，`russh` 的 `Ok(false)` 除了"没有这条"什么都表达不了。

---

## 9. 最终选型建议与工作量

### 9.1 推荐组合

```toml
# Cargo.toml 新增（全部纯 Rust，零 C 依赖，实测 Windows-gnu 目标 cargo check 通过）
[dependencies]
ssh2-config  = "=0.9.0"                                  # config 解析（MIT）
ssh-key      = { version = "0.6.7", features = [         # 私钥加载 + known_hosts 解析
                   "std", "encryption", "ed25519", "rsa", "ecdsa", "p256",
               ] }
ssh-agent-client-rs = "1.1"                              # ssh-agent（同步，Unix+Windows pipe）
hmac = "0.12"                                            # 仅当要支持 hashed known_hosts 匹配
sha1 = "0.10"                                            # 与 ssh-key 0.6.7 共用 digest 0.10，避免重复编译
```

> **为什么是 `hmac 0.12` + `sha1 0.10` 而不是最新的 0.13/0.11**：`ssh-key 0.6.7` 内部用 `digest 0.10`。若选 `hmac 0.13` + `sha1 0.11`，会**同时编译两套 digest/crypto-common**（实测 d8：引入 `digest 0.10.7` + `digest 0.11.3` + `crypto-common 0.1.7` + `0.2.2` + `hmac 0.12.1` + `0.13.0`）。
> 选 0.12/0.10 实测可完全复用（d9：`hmac 0.12.1 (*)`、`digest 0.10.7 (*)`），**少 9 个包**。若将来升级到 `ssh-key 0.7`（用 digest 0.11），再同步升到 `hmac 0.13` + `sha1 0.11` 即可。

**依赖增量实测**（在真实仓库副本上 `cargo add` 后 `cargo tree -e normal` 唯一包数，均排除 `sshpf` 自身）：

| 配置 | normal 包数 | 相对基线 86 |
|---|---|---|
| 基线（当前 `Cargo.toml`） | **86** | — |
| + `ssh2-config 0.9.0` | 89 | **+3**（`ssh2-config`、`glob`、`wildmatch`） |
| + `ssh-key 0.6.7`（含 encryption） | 152 | +66 |
| + `ssh-key 0.6.7` + `ssh-agent-client-rs 1.1.4` | 156 | +70 |
| **+ 上述全部 + `hmac 0.12` + `sha1 0.10`**（推荐组合） | **160** | **+74** |
| 对照：+ `russh`（ring，无 default feature） | 129 | +43 **但需 C 工具链，Windows 交叉编译失败** |
| 对照：+ `russh-config` | 109 | +23（纯 tokio 附加，无 C） |

> `ssh-key` 的 +66 主要来自 RSA/ECC 密码学栈（`rsa`、`num-bigint-dig`、`curve25519-dalek`、`p256`、`aes`、`bcrypt-pbkdf`…）。如果**第一版不需要解密私钥 / 不需要证书**，可以只开 `features = ["std"]`（读公钥 + known_hosts），增量会大幅下降。建议按 P0→P4 阶段逐步开 feature。
> 注意：**这些是编译期包数，不是运行期依赖**。所有包都是静态链接进二进制的纯 Rust 代码，对「Windows 交付物零外部依赖」无影响。

**Windows 免依赖验证**（实测 `cargo check --target x86_64-pc-windows-gnu`，本机未装 mingw）：

```text
$ cd d7 && cargo check --target x86_64-pc-windows-gnu      # ssh2-config + ssh-key + agent + hmac/sha1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 6.45s     ✅
$ cd d1 && cargo check --target x86_64-pc-windows-gnu      # 仅 ssh2-config
    Finished `dev` profile ... in 5.37s                                    ✅
$ cd d4 && cargo check --target x86_64-pc-windows-gnu      # 含 ssh-agent-client-rs
    Finished `dev` profile ... in 7.01s                                    ✅
```

对照 `russh` 路线（同一台机器、同一目标）：

```text
$ cd russh-only && cargo check --target x86_64-pc-windows-gnu
  cargo:warning=ToolNotFound: failed to find tool "x86_64-w64-mingw32-gcc"
  error occurred in cc-rs: failed to find tool "x86_64-w64-mingw32-gcc"      ❌
```

> 说明：`windows-gnu` 目标的 `cargo check` 通过，只证明**这些 crate 的构建脚本不调用 C 编译器**（即没有 `cc`/`cmake` 构建步骤），这正是"Windows 上无需任何外部依赖"的判据。`windows-gnu` 链接阶段仍需 linker，但那是**交叉编译**的固有限制；在 Windows 原生用 MSVC 工具链时，纯 Rust crate 不需要任何额外组件。

### 9.2 明确不建议

| 方案 | 否决理由 |
|---|---|
| **`russh` 全家桶** | ① crypto backend 必须 `ring`/`aws-lc-rs`，**两者都编译 C 代码**（实测 ring 编 30 个 `.o`，Windows 交叉编译直接失败）；② 依赖 +43~129 包；③ 强 async + tokio，与现有"单线程轮询、不要 tokio"架构冲突；④ **不读 config**——即使上 russh，config 解析仍要 `ssh2-config`，省不掉 |
| **`russh-config`** | 无 Include、无 Token、强拉 tokio、全局选项前置直接报错、注释/引号处理缺陷（§4.1） |
| **`ssh2-config-rs`** | 构建期 197 个包（含 `gix`、`cmake`），单人 0-star 项目且落后上游 9 个月（§4.2） |
| **`ssh_config` / `sshconfig` / `ssh_cfg`** | 停更 6 年（2020-01）/ 玩具级 158 行且无 Include·通配·Match / 停更 + 强制 async（§4.3–4.5） |
| **`russh-keys`** | 已是废弃过渡态（最后发布 2025-01-08），功能并入 `russh` 主 crate |
| **直接用 `russh::keys::known_hosts` 校验** | `@revoked` 会放行、通配会漏判、Tab 分隔会漏判（§8.3），且只返回 bool，丢掉本项目需要的失败原因分类 |
| **裸用 `ssh2-config` 不管 Match** | Match 块的 `User`/`Port` 会**静默污染前一个 Host**（§3.2 坑 1） |

### 9.3 分阶段工作量

| 阶段 | 内容 | 新增/改动代码 | 预估 |
|---|---|---|---|
| **P0：config 解析替换** | 接入 `ssh2-config`；复用现有 `host_aliases` 过滤逻辑；**Match pre-scan 兜底**（检测到 Match 则退回现有 `ConfigParser`）；`HostName` 缺省回退、`Port` 缺省 22 | 改 `src/ssh/config_parse.rs`（~150 行改动），**保留** `ConfigParser` 作为 Match 路径 | 1–2 天 |
| **P1：Token 展开** | `expand_tokens()`（`%d %h %n %p %r %u %%`），只在"真正使用路径"时展开；含不支持 token 时保留原文 | 新增 ~60 行 + 单测 | 0.5 天 |
| **P2：私钥加载** | `load_key()`（`ssh-key`，含 passphrase 交互）；先只做 ed25519/rsa/ecdsa | 新增 ~80 行 + 单测（可用 `ssh-keygen` 生成夹具） | 1 天 |
| **P3：agent** | `ssh-agent-client-rs`：列身份、（可选）用 agent 签名；Unix `SSH_AUTH_SOCK` + Windows `\\.\pipe\openssh-ssh-agent` | 新增 ~120 行 + 平台分支 | 1–2 天 |
| **P4：known_hosts** | Tab/多空格归一化 → `ssh-key` 解析 → **自研匹配**（通配/取反/port/hashed/revoked/CA）→ 写回（learn） | 新增 ~150 行 + 单测（夹具覆盖 §8.3 全部 12 种情形） | 2–3 天 |
| **P5（可选）：Match 真求值** | 实现 `Match all/host/user/originalhost`，`exec` 一律不匹配；替换 P0 的 pre-scan 兜底 | ~150 行 | 2 天 |
| **合计** | P0–P4 完成"纯 Rust config + 认证链" | **~560 行新增/改动** | **6–9 人天** |

> 对比"从零自研全套"（方案 B）的 §5.4 清单：约 700 行 + 需重新验证 Include/glob/首值优先/引号注释的**全部边界**（`ssh2-config` 已经用 6330 行 + 测试矩阵覆盖过的部分），估算 **10–14 人天**。P0 用 `ssh2-config` 相当于直接省掉 2–3 天且风险更低。

### 9.4 关键风险与缓解

| 风险 | 缓解 |
|---|---|
| `ssh2-config` 0.9.0 是 8 天内连发三个版本的新版本，`SshParserError` 已因 0.9.0 破坏性变更 | **精确 pin `=0.9.0`**；或 vendored 到 `vendor/ssh2-config`（MIT 允许），彻底锁行为 |
| Match 污染（坑 1） | P0 的 pre-scan 兜底：**有 Match 就走现有 `ConfigParser`**，该实现已正确处理（`skipping` 标志） |
| `ssh-key` 0.6.7 较旧（2024-10） | 密钥文件格式 RFC 稳定；若必须 0.7 则需自己桥接 agent crate 的 `ssh-key 0.6` 类型 |
| `ssh-key` known_hosts 的 Tab/多空格缺陷 | 解析前归一化（~5 行），并加单测覆盖 §8.2 表格 |
| Windows 上无 `ssh-agent` / Pageant | `ssh-agent-client-rs` 支持 OpenSSH named pipe；Pageant 缺失时降级到「读 `IdentityFile` 直连」，功能不瘫 |
| `ProxyCommand` 主机（现状即拒绝） | 保持现状：检测到即提示"该主机使用 ProxyCommand，暂不支持纯 Rust 连接"。这是**已有行为**，非本次回归 |

---

## 10. 可核实来源清单

**crates.io 元数据**（查询时间 2026-11-07，API `https://crates.io/api/v1/crates/<name>`；web 页面因 403 反爬未直接抓取，数据均来自公开 JSON API）：
- `ssh2-config` 0.9.0 — https://crates.io/crates/ssh2-config （215,442 dl / 70,002 recent，MIT，2026-10-20，`default_version: 0.9.0`，`rust_version: 1.88.0`，`linecounts.Rust: 6330`）
- `ssh2-config-rs` 0.7.2 — https://crates.io/crates/ssh2-config-rs （14,859 dl / 6,812，MIT，2026-01-26）
- `russh-config` 0.58.0 — https://crates.io/crates/russh-config （312,008 dl / 177,194，Apache-2.0，2026-03-18，`linecounts: 579`）
- `ssh_config` 0.1.0 — https://crates.io/crates/ssh_config （29,380 dl，MPL-2.0，2020-01-10）
- `sshconfig` 0.1.0 — https://crates.io/crates/sshconfig （1,385 dl，MIT/Apache，2025-05-20）
- `ssh_cfg` 0.3.0 — https://crates.io/crates/ssh_cfg （11,665 dl，2021-12-02）
- `ssh-key` 0.7.0-rc.11 / 0.6.7 — https://crates.io/crates/ssh-key （16.9M dl；`max_stable_version: 0.6.7`）
- `russh` 0.65.1 — https://crates.io/crates/russh （9.99M dl / 4.43M，Apache-2.0，2026-11-03）
- `ssh-agent-client-rs` 1.1.4 — https://crates.io/crates/ssh-agent-client-rs （206,442 dl / 132,303，MIT/Apache，2026-10-14）
- `russh-keys` 0.50.0-beta.7 — https://crates.io/crates/russh-keys （最后发布 2025-01-08，已废弃）
- `pageant` 0.2.5 / `interprocess` 2.4.7 / `hmac` 0.13.0 / `sha1` 0.11.0 / `ring` 0.17.14 — 各自 crates.io 页面

**docs.rs API 文档**（均已验证 HTTP 200）：
- https://docs.rs/ssh2-config/0.9.0/ssh2_config/
- https://docs.rs/ssh2-config/0.9.0/ssh2_config/struct.SshConfig.html
- https://docs.rs/ssh2-config/0.9.0/ssh2_config/struct.HostParams.html
- https://docs.rs/russh-config/0.58.0/russh_config/
- https://docs.rs/ssh2-config-rs/0.7.2/ssh2_config_rs/
- https://docs.rs/ssh_config/0.1.0/ssh_config/
- https://docs.rs/ssh-key/0.7.0-rc.11/ssh_key/
- https://docs.rs/ssh-key/0.7.0-rc.11/ssh_key/private/struct.PrivateKey.html （`from_openssh` / `decrypt` / `cipher` / `kdf`）
- https://docs.rs/ssh-key/0.7.0-rc.11/ssh_key/known_hosts/struct.KnownHosts.html
- https://docs.rs/ssh-key/0.7.0-rc.11/ssh_key/known_hosts/enum.Marker.html
- https://docs.rs/ssh-key/0.7.0-rc.11/ssh_key/known_hosts/enum.HostPatterns.html
- https://docs.rs/russh/0.65.1/russh/keys/
- https://docs.rs/russh/0.65.1/russh/keys/agent/client/struct.AgentClient.html （`connect_env` / `connect_pageant` / `connect_named_pipe`）
- https://docs.rs/russh/0.65.1/russh/keys/known_hosts/index.html
- https://docs.rs/russh/0.65.1/russh/keys/known_hosts/fn.check_known_hosts_path.html
- https://docs.rs/russh/0.65.1/russh/keys/known_hosts/fn.learn_known_hosts_path.html
- https://docs.rs/ssh-agent-client-rs/1.1.4/ssh_agent_client_rs/struct.Client.html

**GitHub 源码**：
- `veeso/ssh2-config` — https://github.com/veeso/ssh2-config （25★/18 fork，最后 push 2026-10-20，MIT）
  - `src/parser/field.rs`（`Field` 枚举**无 Match**）、`src/lib.rs`（`parse`/`query`/`get_hosts`/`parse_default_file`）、`src/params.rs`（`HostParams`）、`src/host.rs`（通配/取反匹配）、`src/parser.rs`（`ParseRule`、`Include`、`tokenize_line`、`host_scopes`）、`README.md` §Missing features、`CHANGELOG.md`
- `warp-tech/russh` → 实际 `Eugeny/russh` — https://github.com/Eugeny/russh （1930★/324 fork，最后 push 2026-11-03，Apache-2.0）
  - `russh-0.65.1/src/lib_inner.rs`（`compile_error!` 要求 crypto backend）、`src/keys/mod.rs`、`src/keys/known_hosts.rs`、`src/keys/agent/client.rs`、`russh-config-0.58.0/src/lib.rs`
- `RustCrypto/SSH` — https://github.com/RustCrypto/SSH （249★，最后 push 2026-11-03）
  - `ssh-key-0.7.0-rc.11/src/known_hosts.rs`（`Entry::from_str` 的 `split_once(' ')`）、`src/private/keypair.rs`（`KeypairData` 全变体）、`src/certificate.rs`、`src/dot_ssh.rs`
- `nresare/ssh-agent-client-rs` — https://github.com/nresare/ssh-agent-client-rs （10★，最后 push 2026-10-14，Apache-2.0）
- `pRizz/ssh2-config-rs` — https://github.com/pRizz/ssh2-config-rs （0★，最后 push 2026-01-26）
- `poneding/sshconfig` — https://github.com/poneding/sshconfig （1★，2025-05-20）
- `briansmith/ring` BUILDING.md — https://github.com/briansmith/ring/blob/main/BUILDING.md （"*ring* currently requires a C toolchain…"）

**OpenSSH 官方文档**：
- `ssh_config(5)` — https://man.openbsd.org/ssh_config.5 （Match / TOKENS / Include / IdentityFile 语义）
- `sshd(8)` SSH_KNOWN_HOSTS FILE FORMAT — https://man.openbsd.org/sshd.8

**本仓库内已有事实**：
- `src/ssh/config_parse.rs`（现有 845 行实现，`ConfigParser` 的 `skipping` 语义）
- `src/ssh.rs` §设计说明（`ssh -G` 实测结论、`SYNTH_EXCLUDED`、`ProxyCommand` 拒绝策略）
- `docs/research/tech-options.md` §0–§2（历史选型与实测数据）

**未确认项（明确标注）**：
- crate 级 Windows named pipe / Pageant 的**实机运行**验证：本机为 Linux，仅做源码审阅 + `--target x86_64-pc-windows-gnu` 的**编译**检查（`cargo check` 通过），未在 Windows 上运行。
- `sk-*`（FIDO）私钥的**签名**行为：无硬件令牌，仅确认 `ssh-key` 能解析该格式（`KeypairData::SkEd25519` / `SkEcdsaSha2NistP256`）。
- `ssh2-config` 对 `Include` 的 `~user` 展开细节：0.9.0 CHANGELOG 称"`~user` 前缀不再被改写"，未做逐用例实测。
- crates.io **web 页面**（`https://crates.io/crates/<name>`）返回 403（反爬），下载量/版本数据均取自其公开 JSON API，与页面应一致但未肉眼比对。

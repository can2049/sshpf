# `~/.ssh/config` / 密钥 / agent / known_hosts 生态选型

> **这份文档回答**：config 解析为什么选 `ssh2-config`、它有哪些必须自己兜的缺陷（§3）、`ssh-key` 怎么加载各类私钥、agent 与 known_hosts 该用哪套 API（**§3.1 与 §8 是 `src/ssh/config.rs`、`src/ssh/known_hosts.rs` 的依据**）。
> **不负责**：当前实现——见 [../design.md](../design.md) 与 `src/ssh/`；russh 的转发能力实测——见 [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md)。
> **相关代码路径**：`src/ssh/config.rs`、`src/ssh/hostkey_conf.rs`、`src/ssh/known_hosts.rs`。

> 历史调研（2026-11-07）。环境 rustc 1.99.1 / OpenSSH_8.2p1 / rsproxy。方法：能实测的一律实测
> （`cargo add` + `cargo tree` + 跑真实代码，或用本机 `ssh -G -F` 做差分对照）；
> 不能实测的（Windows named pipe、`sk-*` 私钥**签名**）明确标注"未实测"。
> **版本号与下载量已过时**，以 `Cargo.toml` 为准。

## 0. 结论

| 问题 | 结论 |
|---|---|
| config 解析 | **`ssh2-config` 0.9.0**（MIT，纯 Rust，**零 C 依赖**）+ 自己补 3 处缺陷（`Match` 语义、枚举去重、Token 展开）。不自研全量解析器，也不用 `russh-config` |
| 是否引入 C 依赖 | **不引入**。`libssh2-sys` / `openssl-sys` / `cc` 全部为 0；crate 名里的 "ssh2" 只是历史原因 |
| 密钥加载 | `ssh-key`（RustCrypto），pin **0.6.7**（`max_stable_version`）而非 0.7.0-rc，理由见 §5 |
| ssh-agent | `ssh-agent-client-rs` 1.1.4（同步 API，Unix socket + Windows named pipe 同一入口，零 C 依赖）；`russh::keys::agent` 会把整个 russh + tokio + ring(C) 拉进来 |
| known_hosts | 解析用 `ssh-key::known_hosts`；**校验/写回自研约 120 行**（`russh` 的校验有四个实测误判，见 §8） |
| 依赖增量 | 基线 86 → 推荐组合 **160 包（+74）**，其中 `ssh2-config` 只占 **+3**；全部纯 Rust、零 C 构建步骤 |

**一句话**：`ssh2-config` 解决了"读文件 + Host 通配 + Include + 首值优先"这些真正繁琐的部分，
剩下的 `Match` 语义与 Token 展开本项目**本来就必须自己定策略**——所以"`ssh2-config` + 一层薄封装"
是性价比最高的落点。

## 1. 本项目到底需要解析什么

v1 的 `src/ssh/config_parse.rs`（845 行，**已删除**）做的事，以及纯 Rust 后必须替代的部分：

| 需求 | 归属 |
|---|---|
| 扫 `Host` 行得到**具体别名列表**（跳过 `*`/`?`/`!`） | crate 的 `get_hosts()` 可直接给 |
| `Include` 递归 + glob（`~` / 相对 `~/.ssh` / 绝对） | crate 已支持（实测） |
| `Match` 块**跳过**（不当作 Host） | **crate 不支持**，需自己处理（§3.1） |
| `HostName`/`User`/`Port`/`IdentityFile`/`ProxyJump` 首值优先 | crate 已支持（与 `ssh -G` 一致） |
| ProxyJump 展开、`Host *` 默认值叠加、`Include` 顺序 | crate 已支持 |
| `IdentityFile` 的 `%d %h %r %p %u %C` 令牌展开 | **crate 不支持**（`ssh -G` 也不展开，是在使用时才展开） |
| `ProxyCommand` | 现状是"遇到就拒绝"；可保持 |

**关键前提**：本项目**不做完整 ssh 语义仿真**。目标只是「新建连接时从 config 选主机，拿到
alias → hostname/user/port/proxyjump/identityfile」。`Match exec`、`LocalCommand`、
`CanonicalizeHostname` 都不是必须的。

## 2. 候选 crate 全景对比

| crate | 最新版 | 最近发布 | Include | Host 通配/取反 | Match | ProxyJump | Token | C 依赖 | 包增量 |
|---|---|---|---|---|---|---|---|---|---|
| **`ssh2-config`** ✅ | 0.9.0 | 2026-10-20 | ✅ 递归+glob | ✅ | ❌ **完全不认** | ✅ | ❌ | **无** | **+3** |
| `ssh2-config-rs` | 0.7.2 | 2026-01-26 | ✅ | ✅ | ❌ | ✅ | ❌ | 构建期 `gix`+`cmake` | 构建 197 |
| `russh-config` | 0.58.0 | 2026-03-18 | ❌ | ✅ | ❌ | ✅ | ⚠️ 部分 | 无，但拉 tokio | +23 |
| `ssh_config` | 0.1.0 | **2020-01-10** | ❌ | ⚠️ | ❌ | ❌ | ❌ | 无 | +1 |
| `sshconfig` | 0.1.0 | 2025-05-20 | ❌ | ❌ | ❌ | ❌ | ❌ | 无 | +3 |
| `ssh_cfg` | 0.3.0 | **2021-12-02** | ❓ | ❓ | ❓ | ❓ | ❓ | 无 | — |

**不存在**的 crate（crates.io API 返回 404，已逐一确认）：
`ssh-conf`、`sshconf`、`openssh-config`、`openssh_config`、`ssh-config-parser`、`sshconfig2`、`parse-ssh-config`。
实际候选只有上表 6 个。

### 2.1 `ssh2-config` 不是 libssh2 绑定（重要澄清）

crate 名带 `ssh2`、描述写着 "for **ssh2-rs**"，但：

```toml
[dependencies]
bitflags = "2"; dirs = "7"; glob = "0.3"; log = "0.4"; thiserror = "2"; wildmatch = "2"
[dev-dependencies]
ssh2 = "0.9"     # ← 只是 dev-dependency，用于 examples
```

实测：干净工程 `cargo add ssh2-config` → 全树 15 包，`grep -icE "libssh2|openssl"` → **0**；
`cargo check --target x86_64-pc-windows-gnu` 通过（无需任何 C 工具链）。
对照反例：同机器上 `russh + ring` 的同一句 `cargo check` 直接失败
（`failed to find tool "x86_64-w64-mingw32-gcc"`）。

**结论：`ssh2-config` 纯 Rust，与「Windows 免外部依赖」目标不冲突。这是本次调研最需要纠正的预设。**

### 2.2 `HostParams` 的覆盖面缺口

`HostParams` 覆盖了 `host_name`/`user`/`port`/`identity_file`（多行累加、保序）/`proxy_jump`/
`server_alive_interval`/`compression`/`ciphers` 等，但以下选项在 **unsupported 列表里、没有对应字段**：

`StrictHostKeyChecking`、`UserKnownHostsFile`、`GlobalKnownHostsFile`、`HashKnownHosts`、
`IdentityAgent`、`IdentitiesOnly`。

影响：**无法从 config 读到每个主机自己指定的 `IdentityAgent` / `UserKnownHostsFile`**。
要尊重它们得自己从 `unsupported_fields` map 里再捞（那里保留了原始 tokens），或退化为"全局默认路径"。
（缺 `ServerAliveCountMax` 无影响——v1 时代用命令行 `-o`，现在由 `russh::client::Config` 管。）

**`ParseRule` 实测**：`ALLOW_UNSUPPORTED_FIELDS` 与 `ALLOW_UNKNOWN_FIELDS` 是**两个独立开关**——
前者只救"认识但未实现"的字段，后者只救"完全不认识"的字段。**两个都要开**：

```rust
const RULES: ParseRule = ParseRule::ALLOW_UNKNOWN_FIELDS.union(ParseRule::ALLOW_UNSUPPORTED_FIELDS);
```

理由：真实 `~/.ssh/config` 必然含用不到的合法选项，`STRICT` 会让**整个文件解析失败**。
真正让 `STRICT` 失败的只有**不在 `Field` 枚举里**的键（`Match`、Linux 上的 `UseKeychain`、拼写错误）。

## 3. `ssh2-config` 实测：覆盖什么、哪些有坑

### 3.1 支持且正确的部分

| 特性 | 实测 |
|---|---|
| `Include` 递归 + glob / 多路径 / 循环检测（`IncludeCycle`）/ 文件不存在静默跳过 | ✅ |
| `Host` 多模式 + 通配 + 取反（`Host *.example.com !bad.example.com`） | ✅ |
| **首值优先** | ✅ 与 ssh 完全一致（`Host *` 在前/在后两种顺序都验过） |
| `IdentityFile` 多行累加、跨块累加 | ✅ 保序 `Vec<PathBuf>` |
| `~` 展开 | ✅ |
| 行内注释 / CRLF / Tab 缩进 / `Key=Value` / 关键字大小写不敏感 | ✅ |
| 真实 13 主机 config 差分 | ✅ 12 个具体别名，`hostname`/`user`/`port`/`proxyjump`/`identityfile` 条数与 `ssh -G` 全部一致 |
| 嵌套 Include 的作用域继承（含取反）+ `Host *` 兜底 | ✅ 逐字段与 `ssh -G` 一致 |
| 通配块 + 具体块叠加 | ✅ `user=specific, port=2200` |

差分差异**不是 bug**：`port=None` vs `port=22`（ssh 补默认值，crate 保留 `None`，调用方
`.unwrap_or(22)`）；`id=1` vs `id=7`（ssh 会补 `~/.ssh/id_rsa` 等默认候选，crate 只报配置里显式写的
——本项目只需要"配置里写的那个"，反而更准确）。

### 3.2 六处缺陷（必须写清）

**坑 1：`Match` 完全不支持，且会污染前一个 `Host` 块** ⚠️ 最严重

`Field` 枚举里**没有 `Match` 变体** → 走 `UnknownField` 分支：

- `STRICT` → 整个文件解析失败（`unknown field: Match`）
- `ALLOW_UNKNOWN_FIELDS` → 该行被忽略，**但其后的选项继续写进上一个 `Host` 块**

```text
输入：Host web1 { HostName 10.0.0.1; User alice } + Match host foo { User ignored; Port 9999 }
ssh2-config query("web1") → user=Some("ignored") port=Some(9999)   ❌
ssh2-config get_hosts()   → [["*"], ["web1"], ["web2"]]            ❌ Match 块消失
真实 ssh -G -F ... web1   → user=alice port=22                     ✅
```

**后果**：用户的 config 里只要有 `Match` 块（开发机/跳板机配置里很常见），列出的主机就会**显示错误的
user/port 且不报错**——静默错误，比崩溃更糟。**这是 `ssh2-config` 唯一必须由我们兜住的语义缺口。**

**坑 2：Token 不展开**（README 明确列为 Missing feature）

```text
IdentityFile ~/.ssh/%r@%h-%p.key  → 原样保留 %r %h %p
IdentityFile %d/.ssh/id_ed25519   → 连 ~ 都没展开（首字符不是 ~）
```

注意 `ssh -G` 的输出**同样不展开**——OpenSSH 是在**实际使用时**才展开。所以这不算相对退步；
但纯 Rust 后若要真正使用 `IdentityFile` 而不只是展示，就**必须自己实现展开**（新增工作量）。

**坑 3：`ProxyCommand` 只存字符串**，要支持得自己 tokenize + `sh -c`，且无法安全再陈述进配置。
保持"检测到就拒绝"即可，**不算新增缺口**。

**坑 4：枚举别名要自己去重、自己过滤通配**。`get_hosts()` 返回**块序列**（含 `*` 块、重复块、
`Host web1:2222`），不是干净别名集。要按 `!negated && !contains('*','?') && pattern != "*"` 过滤 + 去重。

**坑 5：版本年轻 + 破坏性变更频繁**。0.8.2 → 0.8.3 → 0.9.0 全在 2026-10-12~10-20 这 8 天内发布，
且 0.9.0 是 breaking（`SshParserError` 新增变体）。建议 `=0.9` 精确 pin，或 vendored 锁行为。

**坑 6：`IdentityFile` 带空格路径**——0.9.0 引号写法正确（`IdentityFile "~/my keys/id_x"`）；
真实 ssh 对**未加引号**的同内容直接报错中止（`garbage at end of line`，exit 255），
`ssh2-config` 则是"吃掉后面两个词"。两者都属错误处理路径，用引号写法即可。

## 4. 其他候选为什么被否决

| crate | 否决理由（实测） |
|---|---|
| `russh-config` 0.58.0 | 完全不支持 `Include`（`ERR Host not found`）；Token 只有 `%u %h %H %p %%`；**强制拉 tokio**（+23 包）；**全局选项在首个 `Host` 前直接报错**（极常见写法）；`Match` 同样污染；行内注释/引号不剥离（`user = "bob # hi"`）；`HostName=value` 与 Tab 分隔不识别。解析器共 567 行 vs `ssh2-config` 的 6330 行 |
| `ssh2-config-rs` 0.7.2 | 依赖与上游几乎相同、同样不支持 Match/Token；但**构建期**因 `gix` 依赖树多出 **197 个包**（含 `cmake`）；单人项目 0 star，落后上游 9 个月。上游本身就是纯 Rust，没有 fork 的必要 |
| `ssh_config` 0.1.0 | 2020-01 后停更；无文件读取（`read_file` 源码里被注释掉）、无 Include、无 Match/Token；返回 `HashMap<&str,&str>`，Host 头无法表达通配/取反 |
| `sshconfig` 0.1.0 | 玩具级（`src/lib.rs` 158 行）；只认单空格；只处理 4 个键；凭空捏造默认值（`user = "root"`） |
| `ssh_cfg` 0.3.0 | 2021-12 后停更 + 强制 async；未深入实测（价值不足） |

## 5. 密钥加载：`ssh-key` 而非 `russh`

`russh::keys` 只是 `ssh-key` 的 re-export + 包装（`load_secret_key`/`decode_secret_key`、
agent 客户端、known_hosts 检查）。`russh-keys` 已是**废弃过渡态**（最后发布 2025-01-08），
新代码直接依赖 `ssh-key`。

### 5.1 版本选 0.6.7（stable）而不是 0.7.0-rc

| | 0.6.7 | 0.7.0-rc.11 |
|---|---|---|
| 状态 | **max_stable_version** | 预发布（rc） |
| 加密私钥 / `sk-*` / known_hosts | ✅ 全有 | ✅ 同 |
| 与 `ssh-agent-client-rs`（依赖 `ssh-key ^0.6`）共存 | ✅ 统一到同一份 | ❌ **树里同时存在两份，类型不互通** |

```text
$ cargo add ssh-key@0.7.0-rc.11 && cargo add ssh-agent-client-rs
$ cargo tree --prefix none | grep ^ssh-key
ssh-key v0.6.7
ssh-key v0.7.0-rc.11          # ← 两份同时编译，PublicKey 与 PrivateKey 无法比较/签名
```

代价：0.6.7 较旧（2024-10），但密钥文件格式（RFC4253 + OpenSSH）**没有变化**；0.7 主要是 API 迁移
与新算法。对"读私钥、解 passphrase、读 known_hosts"三个需求，0.6.7 完全够用。

### 5.2 实测：各类 OpenSSH 私钥都能加载

| 密钥 | `from_openssh` | 加密算法识别 | `decrypt` |
|---|---|---|---|
| ed25519 明文 | ✅ | — | — |
| ed25519 + aes256-ctr + bcrypt | ✅ | `Cipher::Aes256Ctr`、`Kdf::Bcrypt{salt,rounds:16}` | ✅ |
| 同上，**密码错误** | ✅ | — | ❌ `cryptographic error`（正确拒绝） |
| rsa 2048 + aes256-ctr | ✅ | 同上 | ✅ |
| ecdsa nistp256 明文 | ✅ | — | — |
| PKCS8 (PEM) | ✅ | — | — |

```rust
let bytes = std::fs::read(path)?;
let key = PrivateKey::from_openssh(&bytes)?;   // 解析（含加密头）
if key.is_encrypted() { let key = key.decrypt(passphrase)?; }
let pubkey = key.public_key();
```

支持矩阵：ed25519 / RSA（`rsa-sha2-256/512` 是**签名算法**，私钥统一 `ssh-rsa`，hash 由调用方指定）/
ECDSA(nistp256/384/521) / 加密私钥（aes256-ctr+bcrypt 实测；`ssh-cipher` 还支持 aes-cbc、aes-gcm、
chacha20-poly1305）/ OpenSSH 证书（`validate(ca_fingerprints)` / `valid_principals` / `valid_before` 齐全）。
`sk-ssh-ed25519@openssh.com` / `sk-ecdsa-...`：**能解析，签名需硬件令牌**（未实测，无 FIDO 设备）；
本项目"列出/展示"够用，真正认证建议走 agent。

**为什么不为省事直接依赖 `russh`**：`russh` 必须显式选 crypto backend（`ring` 或 `aws-lc-rs`），
两者都是 C 代码；实测 `cargo add russh --no-default-features --features ring` 拉 **129** 个包，
且 Windows 目标直接失败。为 3 行 `load_key()` 换一个 C 工具链依赖不值得。

## 6. ssh-agent

| 方案 | 同步 | Unix | Windows OpenSSH pipe | Windows Pageant | 包增量 | C 依赖 |
|---|---|---|---|---|---|---|
| **`ssh-agent-client-rs` 1.1.4** ✅ | ✅ | ✅ 实测 | ✅ 内置路径约定 | ❌（需自己实现协议） | +~20（复用 ssh-key 0.6.7） | **无** |
| `russh::keys::agent` | ❌ async | ✅ 实测 | ✅ `connect_named_pipe` | ✅ `connect_pageant` | +129（russh+ring+tokio） | **ring C 代码** |

`ssh-agent-client-rs` 的关键优势：**同一个 `Client::connect(&Path)`**，Unix 传 socket 路径、
Windows 传 `\\.\pipe\openssh-ssh-agent`，`interprocess` 负责平台差异——这正是要的抽象。
能力：列身份、添加、删除、清空、**签名**；未实现带约束的添加与 `SSH_AGENTC_ADD_SMARTCARD_KEY`。

API 细节（实测踩到）：`connect` 收 `&Path` 而非 `&str`；`list_identities()` 已 deprecated，
用 `list_all_identities()`（保留证书身份）。

Pageant 缺失可接受：Windows 11 自带 OpenSSH agent，Pageant 属 PuTTY 生态小众场景。

## 7. known_hosts：解析、校验、写回

三个层次，能力差别很大：

| 层次 | 提供者 | 能力 |
|---|---|---|
| **解析** | `ssh-key::known_hosts` | ✅ 完整（`KnownHosts` 迭代器、`Entry{marker, host_patterns, public_key}`、`Marker::{CertAuthority, Revoked}`、`HostPatterns::{Patterns, HashedName}`） |
| **校验 + 写回** | `russh::keys::known_hosts` | ✅ 有函数，但**实测有四个缺陷**（§8） |
| **证书链校验** | `ssh-key::Certificate::validate(ca_fingerprints)` | ✅ 选 CA、校验签名/有效期/principals |

### 7.1 `ssh-key::known_hosts` 解析：够用，但有 Tab 缺陷

| 输入形式 | 结果 |
|---|---|
| 单空格分隔 | ✅ 1 条 |
| **Tab 分隔** / **行首空格** / **多空格** | ❌ `Error::FormatEncoding`（"length invalid"） |
| 空行 + `#` 注释 | ✅ 跳过 |
| 行尾 `#` | ⚠️ `next_line_trimmed()` 用 `split_once('#')` **无条件截断**（含 `#` 的 hostname 会被切，罕见） |
| `|1|salt|hash`（hashed） | ✅ `HostPatterns::HashedName{salt,hash}` |
| `@cert-authority` / `@revoked` | ✅ 识别为 `Marker::CertAuthority` / `Marker::Revoked` |
| `[host]:2222` / 通配 / 取反 | ✅ 原样保留在 `Patterns` 里（**不做匹配**） |

根因（`Entry::from_str`）：`line.split_once(' ')` 硬切，**只认恰好一个空格**。
而真实 `ssh-keygen` 产出的 known_hosts 完全可能是 Tab 分隔（`ssh-keygen -F` 能识别）。
**缓解**：解析前把行内 `\t` 归一化为空格、`split_whitespace` 重排，约 5 行。**这是必要前置处理。**

**关键限制**：`ssh-key::known_hosts` **只解析、不匹配**——没有 `matches()`/`verify()`，
也没有 hostname 哈希函数（`HashKnownHosts` 写回不支持）。匹配逻辑必须自己写。

## 8. `russh::keys::known_hosts` 校验的四个误判（自研匹配的依据）

| 场景 | 期望 | 实测 | 判定 |
|---|---|---|---|
| 普通空格条目 / `|1|` hashed / `[h]:2222` / 逗号列表 / 取反 | 匹配 | `Ok(true)` | ✅ |
| 同 host 换 key | `KeyChanged` | `Err(KeyChanged{line:1})` | ✅ |
| **Tab 分隔条目** | 匹配 | **`Ok(false)`** | ❌ 漏判（误判为未知主机） |
| **行首空格条目** | 匹配 | **`Ok(false)`** | ❌ 漏判 |
| **通配 `*.example.com`** | 匹配 | **`Ok(false)`** | ❌ 漏判 |
| **`@revoked` + 正常条目同时存在**（同 host 同 key） | 拒绝 | **`Ok(true)`** | ❌ **严重：接受被吊销的 key** |
| `@revoked` 单独存在 | 拒绝 | `Ok(false)` | ⚠️ 碰巧对，但理由是"没匹配到"而非"已吊销" |
| `@cert-authority` | 不参与 host key 比对 | `Ok(false)` | ✅ 碰巧对 |

根因（`known_hosts.rs` 的 `match_hostname()`）：只处理 `|1|` hashed 和**精确字符串相等**，
**没有通配匹配**；`known_host_keys_path()` 用 `split(' ')` 切分（Tab 就断）；
**`@revoked` / `@cert-authority` 从未被检查**（只跳 `#` 注释，`@` 开头的行被当普通行解析，`marker` 被丢弃）。

**为什么自研是必需的、不是可选优化**：它**只会返回 bool**，而本项目需要区分
"未知主机（可提示 learn）/ key 变更（必须拒绝）/ revoked（必须拒绝）/ CA 签发（走证书校验）"
四种状态——`Ok(false)` 除了"没有这条"什么都表达不了。这正是 `FailureKind` 要归类的失败原因。

### 8.1 自研清单与工作量

| 需求 | 工作量 |
|---|---|
| 通配（`*` `?`）+ `!` 取反 + `[host]:port` 拆分 | ~60 行 |
| `|1|` hashed 条目（HMAC-SHA1） | ~15 行（`hmac` 0.12 + `sha1` 0.10，与 `ssh-key` 共用 `digest 0.10`，零 C 依赖） |
| `@revoked` 优先拒绝 | ~10 行 |
| `@cert-authority` + 证书校验 | ~30 行（若做证书认证） |
| Tab / 多空格 / 行首空格归一化 | ~10 行 |
| 写回（learn）+ `HashKnownHosts yes` 支持 | ~30 行 |

> `hmac 0.13` + `sha1 0.11` 会同时编译两套 `digest`/`crypto-common`（实测多 9 个包）；
> 选 0.12/0.10 可完全复用。若将来升到 `ssh-key 0.7`（用 digest 0.11），再同步升即可。

### 8.2 写回的局限

`learn_known_hosts_path` 实测：`[host]:port` 格式正确 ✅、目录自动创建 ✅、末尾补换行 ✅；
但**不去重**（同一 host 写两次 → 2 行）、不遵循 `HashKnownHosts yes`。

## 9. 推荐组合与依赖增量

```toml
[dependencies]
ssh2-config  = "=0.9.0"                                  # config 解析（MIT）
ssh-key      = { version = "0.6.7", features = [         # 私钥加载 + known_hosts 解析
                   "std", "encryption", "ed25519", "rsa", "ecdsa", "p256",
               ] }
ssh-agent-client-rs = "1.1"                              # ssh-agent（同步，Unix+Windows pipe）
hmac = "0.12"                                            # 仅当要支持 hashed known_hosts 匹配
sha1 = "0.10"                                            # 与 ssh-key 0.6.7 共用 digest 0.10
```

| 配置 | normal 包数 | 相对基线 86 |
|---|---|---|
| 基线 | **86** | — |
| + `ssh2-config` | 89 | **+3** |
| + `ssh-key`（含 encryption） | 152 | +66（RSA/ECC 密码学栈） |
| + `ssh-key` + `ssh-agent-client-rs` | 156 | +70 |
| **推荐组合全部** | **160** | **+74** |
| 对照：+ `russh`（ring） | 129 | +43 **但需 C 工具链，Windows 交叉编译失败** |

> 如果第一版不需要解密私钥/证书，可只开 `features = ["std"]`，增量大幅下降。
> 这些是**编译期**包数，不是运行期依赖——所有包静态链接，对「Windows 交付物零外部依赖」无影响。

### 9.1 明确不建议

| 方案 | 否决理由 |
|---|---|
| `russh` 全家桶 | crypto backend 必须编译 C 代码；+43~129 包；强 async + tokio；**不读 config**——即使上 russh，config 解析仍要 `ssh2-config` |
| `russh-config` / `ssh2-config-rs` / `ssh_config` / `sshconfig` / `ssh_cfg` | §4 |
| `russh-keys` | 已废弃（最后发布 2025-01-08） |
| 直接用 `russh::keys::known_hosts` 校验 | `@revoked` 放行、通配漏判、Tab 漏判（§8），且只返回 bool |
| 裸用 `ssh2-config` 不管 `Match` | 静默污染前一个 `Host`（§3.2 坑 1） |

### 9.2 分阶段工作量（当时的估算）

| 阶段 | 内容 | 规模 |
|---|---|---|
| P0 config 解析替换 | 接入 `ssh2-config`；`Match` pre-scan 兜底；`HostName` 缺省回退、`Port` 缺省 22 | ~150 行改动，1–2 天 |
| P1 Token 展开 | `expand_tokens()`（`%d %h %n %p %r %u %%`），只在"真正使用路径"时展开 | ~60 行，0.5 天 |
| P2 私钥加载 | `load_key()`（`ssh-key`，含 passphrase 交互） | ~80 行，1 天 |
| P3 agent | `ssh-agent-client-rs`：列身份、签名 | ~120 行，1–2 天 |
| P4 known_hosts | 归一化 → `ssh-key` 解析 → 自研匹配 → 写回 | ~150 行，2–3 天 |
| 合计 | P0–P4 | **~560 行，6–9 人天** |

对照"从零自研全套解析器"：约 700 行 + 需重新验证 Include/glob/首值优先/引号注释的**全部边界**，
估算 10–14 人天。

## 10. 未确认项（明确标注）

- crate 级 Windows named pipe / Pageant 的**实机运行**：本机 Linux，只做源码审阅 +
  `--target x86_64-pc-windows-gnu` 的**编译**检查。
- `sk-*`（FIDO）私钥的**签名**行为：无硬件令牌，只确认能解析格式。
- `ssh2-config` 对 `Include` 的 `~user` 展开细节：CHANGELOG 称 0.9.0 起不再改写，未逐用例实测。
- crates.io **web 页面**返回 403（反爬），下载量/版本数据取自公开 JSON API，未与页面肉眼比对。

## 11. 来源

- crates.io API：`ssh2-config`、`ssh2-config-rs`、`russh-config`、`ssh_config`、`sshconfig`、`ssh_cfg`、
  `ssh-key`、`russh`、`ssh-agent-client-rs`、`russh-keys`（均 https://crates.io/crates/<name>）
- 源码（**均为上游 crate 内部路径，不在本仓库**）：
  `veeso/ssh2-config` 的 `parser/field.rs`（`Field` 枚举无 Match）、`params.rs`、`host.rs`、`parser.rs`；
  `Eugeny/russh` 的 `lib_inner.rs`、`keys/*`；
  `RustCrypto/SSH`（`ssh-key`）的 `known_hosts.rs`、`private/keypair.rs`、`certificate.rs`；
  `nresare/ssh-agent-client-rs`
- 文档：`ssh_config(5)` https://man.openbsd.org/ssh_config.5 、`sshd(8)` SSH_KNOWN_HOSTS FILE FORMAT
- 构建要求：`aws-lc-rs` README（需 C/C++ + Windows NASM）、
  [ring BUILDING.md](https://github.com/briansmith/ring/blob/main/BUILDING.md)
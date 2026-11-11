# docs/research/ — 历史调研档案

> **这份文档回答**：`docs/research/` 里这些文件是什么、还能不能当参考、哪一篇值得读。
> **不负责**：任何当前行为、当前事实。
> **相关代码路径**：无（本目录不含可编译代码）。

## 定位

**这里全部是历史调研笔记，不是当前事实来源。**

它们在两个转折点上写成：① v1 选型（用系统 `ssh` 子进程）；② v2 纯 Rust 化（`russh`）与
client-server 改造。本目录只保留**为什么当时那样判断**、**测量数字**与**库缺陷的复现证据**。

> ⚠️ **client-server 那部分已回退**（v0.9，见 [CHANGELOG](../../CHANGELOG.md)）。当时的方案是
> 一个后台 server 进程拥有全部隧道、多个 client 窗口共享状态；现在已完全删除，回归**单进程**：
> 你启动的那个进程拥有全部转发。因此 `client-server.md` 与 `client-server/` 下的三篇**只作历史
> 记录**，其中描述的选举、令牌、快照、宽限窗口在当前代码里都不存在，原先那篇描述当前行为的
> 运行模型文档也已随之删除。现行进程模型见 [../architecture.md](../architecture.md#进程模型--process-model)。

| 现在要知道什么 | 去哪 |
|---|---|
| 当前行为、规则、政策 | [../index.md](../index.md) 指向的文档 |
| 某个设计为什么这样 | [../design.md](../design.md)（D1–D36 决策索引） |
| 当时的实测数字与库缺陷复现 | 本目录 |

> **本目录没有可编译代码。** 两个可丢弃的 Cargo 探针工程（`spike/`、`client-server-spike/`，
> 共 3000+ 行、不参与构建）已删除——它们的结论已并入下列正文，复跑方法在各篇末说明。
> 删除理由：它们从未与仓库一起构建，却让每次 `grep`/`rg` 都在死代码里返回命中。

## 只有这里有的东西（不可替代）

| 证据 | 位置 |
|---|---|
| **`russh` known_hosts 四处误判**（通配/Tab/行首空格漏判、**`@revoked` 放行被吊销的密钥**）+ 复现输出 | [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) §8 |
| `ssh2-config` 的 `Match` **静默污染前一个 `Host`**（复现输入/输出，本项目自研兜底的依据） | [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) §11、[pure-rust-ssh-config.md](pure-rust-ssh-config.md) §3.2 |
| **"已在依赖树里"的可复用 crate 清单**（`unicode-width`/`ssh-key`/`glob`/`data-encoding` 经谁引入，决定"零新增"）| [oss-replacement-audit.md](oss-replacement-audit.md) §3 |
| **`ssh-key` 解析器的两个阻塞点**（不给行号 + `trim_end`/`split_once(' ')` 空格敏感，直接换会带回 Tab/行首空格 bug）| [oss-replacement-audit.md](oss-replacement-audit.md) §1.1 |
| **`fast-socks5` 从不自己连上游**（`read_command` 后交还目标地址，正合本项目形状）| [oss-replacement-audit.md](oss-replacement-audit.md) §1.3 |
| **`text.rs:28` 指向的 `design.md` 依据不存在**（grep 无 `unicode`/`width`/`emoji`）| [oss-replacement-audit.md](oss-replacement-audit.md) §5 |
| `tcpip_forward` 非 0 端口成功时返回 `Ok(0)` 的封装规则 | [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) §4 |
| `ring` vs `aws-lc-rs` 跨平台构建对比（9×gcc vs 59×gcc+NASM+CMake） | [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) §10 |
| 系统 `ssh -v` 七类失败 stderr 原文（v1 分类规则的出处） | [tech-options.md](tech-options.md) §2.2 |
| 竞品功能对照、表格列宽与状态符号的来源 | [prior-art.md](prior-art.md) §1–2 |
| 52 条断言的实测数字（选举/广播/生命周期/codec/时钟/真实多进程 e2e） | [client-server.md](client-server.md) §4 |
| "过期视图不得拥有破坏权"的跨进程竞态复现 | [client-server.md](client-server.md) §4、[client-server/protocol.md](client-server/protocol.md) §9 |

## 什么时候**不要**读

- 查当前键位、配置字段、日志事件 → [../keymap.md](../keymap.md)、[../logging/events.md](../logging/events.md)。
- 查当前 `~/.ssh/config` 解析行为 → [../import.md](../import.md) 与 `src/ssh/`。
- 查 multi-process / client-server 当前行为 —— **不存在**：该方案已回退，现行是单进程，见
  [../architecture.md](../architecture.md#进程模型--process-model)。
- 引用其中的版本号、行数、依赖版本、下载量 —— 全部可能已过期。

## 目录

| 文件 | 内容 | 行数 |
|---|---|---|
| [prior-art.md](prior-art.md) | 同类工具盘点、表格列设计、录入流程、持久化与安全调研 | 127 |
| [tech-options.md](tech-options.md) | v1 选型实测：系统 `ssh` 子进程、`ssh -v` 失败面、进程树清理、TUI 栈（**§3.3 是 `src/app.rs` 事件循环的出处**） | 183 |
| [pure-rust-ssh-plan.md](pure-rust-ssh-plan.md) | 纯 Rust 化的**决策文档**：判据 C1–C6、目标架构、分阶段、否决的替代方案 | 198 |
| [pure-rust-ssh-spike.md](pure-rust-ssh-spike.md) | **实测主档**：三种转发 + ProxyJump 跑通、性能数字、known_hosts 缺陷（§8）、错误分类 | 312 |
| [pure-rust-ssh-russh.md](pure-rust-ssh-russh.md) | `russh` API 速查：哪项能力对哪个函数、签名陷阱 | 78 |
| [pure-rust-ssh-config.md](pure-rust-ssh-config.md) | config/密钥/agent/known_hosts 生态选型：候选 crate 对比、六处缺陷、依赖增量 | 377 |
| [oss-replacement-audit.md](oss-replacement-audit.md) | **自研逻辑盘点**：哪些 SSH/基础设施逻辑能换开源 crate、逐条否决理由、建议（SOCKS5 减代码 / `unicode-width` 修 bug）、**两处自我更正** | 330 |
| [client-server.md](client-server.md) | client-server 调研总览：为什么做、代价、52 条断言、**本文预测 vs 实际落地** | 196 |
| [client-server/protocol.md](client-server/protocol.md) | 帧格式、消息全集、命令、TOML 限制、体积预算、**§8.2 兼容变更契约** | 348 |
| [client-server/lifecycle.md](client-server/lifecycle.md) | 选举顺序、计数与宽限、关服时序、信号、崩溃重连 | 259 |
| [client-server/migration.md](client-server/migration.md) | 分阶段施工图、不变量、测试策略、**§11 退出提示的完整验收条款** | 239 |

## 仍被代码引用的路径（**不许改名**）

这些路径出现在 `src/` 的 `//!` 注释里，是"为什么这样写"的出处：

| 路径 | 引用点 | 指向 |
|---|---|---|
| `pure-rust-ssh-spike.md` §8 | `src/ssh/known_hosts.rs` | known_hosts 四处误判 |
| `pure-rust-ssh-config.md` | `src/ssh/config.rs` | `ssh2-config` 选型与 `Match` 缺陷 |
| `pure-rust-ssh-plan.md` §4 | `src/sys/mod.rs` | 目标架构（进程组/Job Object 为何消失） |
| `tech-options.md` §3.3 | `src/app.rs` | 单线程 UI 轮询 + mpsc 事件模型 |
| `prior-art.md` §7 | `src/ui.rs` | 主屏布局与状态符号 |

## 维护约定

- **不更新**这些文件去"跟上代码"——它们是历史记录，改写会破坏证据价值。
- 需要新调研时**新建**文件并在上表登记，不要覆写旧的。
- 若某个结论已被代码推翻，在对应篇的「本文预测 vs 实际落地」/「哪些结论被推翻」小节记一句，
  并把新结论写进 [../design.md](../design.md)；**不要**悄悄改历史笔记。
- 单篇 ≤ 400 行；表格 + 短句；每篇开头 3–8 行写清「回答什么 / 不负责什么 / 相关代码路径」。
- 发布包排除本目录（`Cargo.toml` 的 `exclude`），仓库里保留。

## 本目录的例外：链接校验

`docs/agents.md` 的全仓路径校验脚本**排除** `docs/research/`，因为历史笔记会引用 v1 源码路径
（如已删除的 `src/ssh/config_parse.rs`）与上游 crate 内部路径，这些目标本来就不该存在。
**但本目录内部的相对链接与锚点必须有效**——那一条在每次改本目录后单独校验：

```bash
# 从仓库根执行：校验本目录所有相对链接、#anchor 与 prose 里的 docs/src 路径
python - <<'EOF'
import re, pathlib, sys
root = pathlib.Path('.'); research = root/'docs'/'research'
allow_missing = {'src/ssh/config_parse.rs'}
link_re = re.compile(r'\[[^\]]*\]\(([^)\s]+)\)')
path_re = re.compile(r'(?:docs|src|tests)/[A-Za-z0-9_./-]+\.(?:md|rs)')
head_re = re.compile(r'^(#{1,6})\s+(.*?)\s*$', re.M)
def slug(t):
    t = t.strip().lower().replace('`','')
    return re.sub(r'[^\w\u4e00-\u9fff\s-]','',t).replace(' ','-')
cache = {}
def anchors(p):
    k = str(p.resolve())
    if k not in cache:
        cache[k] = {slug(m.group(2)) for m in head_re.finditer(p.read_text(encoding='utf-8', errors='ignore'))}
    return cache[k]
bad, n = [], 0
for page in sorted(research.rglob('*.md')):
    txt = page.read_text(encoding='utf-8', errors='ignore')
    for m in link_re.finditer(txt):
        t = m.group(1)
        if t.startswith(('http://','https://','mailto:')): continue
        n += 1
        f,_,frag = t.partition('#')
        dest = page if not f else (page.parent/f).resolve()
        if not dest.exists(): bad.append(('LINK', str(page), t))
        elif frag and dest.suffix=='.md' and frag not in anchors(dest): bad.append(('ANCHOR', str(page), t))
    for m in path_re.finditer(txt):
        n += 1
        r = m.group(0)
        if r not in allow_missing and not (root/r).exists(): bad.append(('PATH', str(page), r))
for b in bad: print(b)
print(f'checked {n} references; problems: {len(bad)}')
sys.exit(1 if bad else 0)
EOF
```

最后一次运行结果：`checked 155 references in docs/research/**; problems: 0`。

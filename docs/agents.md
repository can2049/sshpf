# 给 AI agent 的改码约定 / Working in this repo

> **这份文档回答**：加按键/加面板/加事件/加失败分类/改文案/加文档分别要动哪几处；怎么自检；汇报什么。
> **不负责**：六条硬约定（**在根 [`AGENTS.md`](../AGENTS.md)**）、项目是什么怎么用（见 [../README.md](../README.md)）、架构细节（见 [architecture.md](architecture.md)）。
> **相关代码路径**：全部。真正的"地图"是每篇文档的开头路标 + 每个源文件的 `//!` 头注释。

## 先做什么 / Start here

1. **开 worktree**（只要会产生提交，就不在共享 checkout 里改）——生命周期见根 [`AGENTS.md`](../AGENTS.md#一特性一-worktree--one-worktree-per-change)。
2. 读 [index.md](index.md) 找到你的任务对应的文档。
3. 读目标文件的 `//!` 模块文档——**每个源文件都必须有**（`tests/safety_guards.rs` 强制）。
4. `grep` 你要改的函数名，读上下文，再动手。
5. 改完跑自检（本文末），再向 Lead / 用户汇报。

**代码是唯一事实来源。** 文档与代码冲突时以代码为准，并回头修文档。

## 一特性一 worktree / One worktree per change

开发新特性、修复问题，**一律在独立 worktree 的独立分支里做**，不在仓库根直接改；纯文档改动同理，没有例外。理由：共享 checkout 的未提交改动、git index、`target/` 都是共享的，任一方 `git checkout`/`git stash`/`cargo fmt --all` 就会打断另一方，且没有任何提示。

```bash
git fetch origin
git worktree add .agents/worktrees/feat/my-feature -b feat/my-feature origin/main
cd .agents/worktrees/feat/my-feature
# 开发 → 自检（本文末）→ 提交 → push → PR
git push -u origin feat/my-feature && gh pr create --fill
# CI 全绿 + 评审通过 → 合并（merge commit，与既有 PR 一致）
# 合并后清理：worktree、本地分支、远端分支都不留（见下）
```

| 项 | 约定 |
|---|---|
| 分支名 | 前缀与提交前缀一致：`feat/…`、`fix/…`、`docs/…`、`refactor/…`、`release/…` |
| worktree 路径 | `.agents/worktrees/<分支名>`，与分支同名；`.agents/` 在 `.gitignore` 里 |
| 基线 | 从刚 `fetch` 的 `origin/main` 开；落后就 `rebase`，不要把 main merge 进分支 |
| git 写操作 | 只在自己 worktree 里 `add`/`commit`/`push`；不要在主 checkout 切换分支或 stash |
| 合并方式 | PR（不直接 push `main`）；`main` 上 CI 必须绿 |

### 合并后清理 / Clean up after merge

**PR 合并即这条线结束。** 残留的分支和 worktree 会让后来者（尤其是 agent）以为任务还在进行——`git worktree list` 和 `git branch` 是它们判断「谁在做什么」的唯一线索。

```bash
# 回到主 checkout 执行（在将被删除的 worktree 里执行会失败）
git worktree remove .agents/worktrees/<分支名>   # 有未提交改动会拒绝删除；别用 --force 掩盖
git branch -d <分支名>                           # -d 校验已合并；报「未合并」说明合并没到位
git push origin --delete <分支名>
git fetch --prune
```

| 检查 | 期望 |
|---|---|
| `git worktree list` | 只剩主 checkout（和别的**进行中**的线） |
| `git branch --merged origin/main` | 不含刚删的分支 |
| `gh pr view --json state` | `MERGED`，且远端分支已消失 |

例外只有一个：`release/vX.Y.Z` 由发版流程保留，不随合并删除；`main` 永不删除。

## 命名 / Naming

| 种类 | 约定 | 例子 |
|---|---|---|
| 文件 | 小写，职责名，不加 `utils`/`common`/`helpers` | `model/import.rs`、`ui/hit.rs` |
| handler | `handle_<上下文>_key` | `handle_import_key` |
| 事件函数 | 与 `event=` 值对应，`event=` 用点分 ASCII | `tunnel_ready` → `event=tunnel.ready` |
| 测试 | 说清**行为**的完整句子 | `a_drop_after_up_is_a_warning_while_a_failed_start_is_an_error` |
| 中文常量 | 不加拼音；界面文案走 `help::SECTIONS` 数据 | — |

## 加按键 / Add a key

三处，**第二处漏了 CI 会红**：

```text
① src/keys/<上下文>.rs      加 dispatch（KeyCode 字面量）
② src/help/<分组>.rs        对应 SectionId 加一条 HelpItem（中文在前、英文在后）
③ docs/keymap.md            对应表加一行（token 必须写成反引号单元格）

help 的分组：main.rs（主界面/多选）、form.rs（表单/下拉）、import.rs（导入/列筛选）、
ports.rs（对端端口）、dialogs.rs（确认/详情/过滤/帮助）、notes.rs（鼠标/状态图例/安全）。
SectionId 与文案在 src/help.rs 汇总。
```

书写规范（`tests/help_coverage.rs` 靠它切分 token，不是审美问题）：

| 规范 | 写法 |
|---|---|
| 并列键用 `" / "` 分隔 | `keys: "j / ↓"` |
| 修饰键用 `+` 粘住 | `ctrl+s`、`shift+tab` |
| 方向键用符号 | `↓ ↑ ← →`（不是 `down`） |
| "任意字符"不写 keys | 用 `HelpItem::note`（note 不参与覆盖检查） |

加完跑 `cargo test --test help_coverage`。若新 handler 是新函数，还要登记到测试里的 `HANDLERS` 表。跨 section 的刻意例外写进 `DOC_ONLY` 常量并注明理由——**不是**注释掉断言。

## 加界面面板 / Add an overlay

```text
① src/app.rs        Overlay 加一个变体
② src/ui/<name>.rs  渲染（纯函数，只读 &App，写 HitMap）
③ src/keys/<name>.rs 键处理（或并入现有 handler）
④ src/ui/hit.rs     HitTarget 加变体（语义，不是坐标）
⑤ src/app/mouse.rs  接住该 target，调与键盘相同的方法
⑥ src/help.rs       SectionId 加一节（新上下文）或并入 Mouse 节
⑦ docs/keymap.md    新上下文一节
⑧ docs/tui.md       浮层表加一行
```

细节与约束见 [tui.md](tui.md#加一个可点控件--add-a-clickable-control)。

## 加日志事件 / Add a log event

```text
① src/eventlog/<族>.rs   加一个函数（族：process / server / tunnel / ssh / user）
② docs/logging/events.md 加一行（事件名 | 级别 | 关键字段 | 含义）
③ 若有排查价值         docs/logging.md 的「排查配方」加一条
```

**硬约束**：函数名、`event=` 字符串、级别、字段名与顺序**逐字不变**——`docs/logging/events.md` 与既有测试依赖它们。级别按"读的人要做什么"分而不是按"严重程度"；四档判据与族分工见 [logging/events.md](logging/events.md#约定--conventions) 与 [logging.md](logging.md#级别策略)。

## 加失败分类 / Add a failure kind

`src/model/failure.rs` 里 `FailureKind` 有**四个**必须同步的地方：`kind()`（ASCII 稳定值，给 grep）、`short()`（中文短句，给表格）、`displays()` 的中英双语、以及 `retryable()` 归类（配置类 vs 网络类）。然后 `docs/logging/events.md` 的 `kind` 稳定值列表加一项。**`kind()` 值是日志契约，不许改。**

## 改文案 / Change user-facing text

| 场景 | 规则 |
|---|---|
| 双语 | 中文在前、英文在后（`text::bi(zh, en)`）——既有测试大量 `contains("中文")`，这是兼容性要求 |
| 窄列放不下 | `text::fit(zh, en, sep, width)` 三级退化：双语 → 仅中文 → 截断 |
| **不要**半个英文短语 | 截断的英文句子比没有更糟 |
| 宽度 | 一律 `text::display_width`（CJK = 2 列），**绝不用** `str::len()` |
| 帮助文案 | 改 `src/help.rs` 的数据；界面页脚复用同一批措辞，所以改一处两边都对 |

细节见 [tui.md](tui.md#文本与折行--text)。

## 加文档 / Add a doc

**先问：这是不是已经有一篇在讲同一件事？** 同一读者任务（例如"导入怎么工作"）追加到现有那篇的一节，而不是新开一篇——读者为回答一个问题要开五篇文件，是缺陷。

```text
① docs/<name>.md        开头 3–8 行：回答什么问题 / 不负责什么 / 相关代码路径
② docs/index.md         两张表都要登记（任务查找表 + 文档清单）
③ 相互链接              从 architecture.md / agents.md 等加相对链接
```

| 硬要求 | 值 |
|---|---|
| 单篇行数 | ≤ 400（超过就按职责再拆） |
| 结构 | 表格 + 短句；agent 靠标题和表格定位，**不写散文** |
| 不重复 | 同一事实只详述一处，其余用相对链接 |
| 不重复模块注释 | 文件的 `//!` 已经写了"什么属于这里"，文档不要逐文件转录 |
| 路径真实 | 文档链接、代码路径引用、**锚点**都必须存在 |
| 文件名 | 被 `src/` 引用的不能改：`docs/design.md`、`docs/logging.md`、`docs/keymap.md`、`docs/architecture.md`、`docs/dedup.md` |

## 提交前自检 / Pre-flight checklist

```bash
# 1) 格式（只对自己的文件，不要 cargo fmt --all）
rustfmt --edition 2024 --check src/<你改的文件>.rs

# 2) 编译与 lint（只看自己 scope 的路径）
cargo check --all-targets 2>&1 | grep '<你 scope 的路径>'
cargo clippy --all-targets -- -D warnings

# 3) 测试
cargo test --lib <模块前缀>            # 例如 cargo test --lib model::
cargo test --test help_coverage        # 动了 help/keys/keymap 必跑
cargo test --test safety_guards        # 动了 src 结构必跑

# 4) 测试数量守恒（迁移/拆分时）
cargo test --lib -- --list | wc -l
```

文档改动额外跑（每个 md ≤ 400 行 + 全仓路径校验）：

```bash
wc -l README.md CHANGELOG.md docs/*.md docs/**/*.md

# 全仓路径校验：src/tests/docs/README 里的 md/rs 路径引用必须存在。
# docs/research/ 被排除：那是历史调研，引用的是 v1 源码与上游 crate 内部路径，
# 这些目标本来就不该存在（见 docs/research/README.md）。排除是记录在案的决定，不是放宽。
python3 - <<'EOF'
import re, pathlib
root = pathlib.Path('.')
bad = []
for p in (list(pathlib.Path('src').rglob('*.rs'))
          + list(pathlib.Path('tests').rglob('*.rs'))
          + [q for q in pathlib.Path('docs').rglob('*.md') if 'research' not in q.parts]
          + [pathlib.Path('README.md')]):
    txt = p.read_text(errors='ignore')
    for m in re.findall(r'(?:docs|src|tests)/[A-Za-z0-9_./-]+\.(?:md|rs)', txt):
        if not (root / m).exists():
            bad.append((str(p), m))
for b in bad:
    print('MISSING', b)
print('checked; missing:', len(bad))
EOF
```

**唯一的已知良性命中**：`src/logging/panic.rs` 里有个测试夹具故意写 `Some("src/x.rs:1:1")` 当作"panic 位置"的假字符串。它不是文档引用，不该为了自检去改。出现且**只有**这一条时视为通过。

**上面的脚本不查锚点**。改了标题就得同步所有 `](foo.md#anchor)` 引用：`grep -rn '](.*\.md#' docs README.md` 列出全部锚点引用，逐个核对目标文件里是否有对应 `##` 标题。

## 汇报格式 / Reporting back

```text
WS: <名字>
状态: 完成 / 部分完成（卡在哪）
改动文件: <列表，含新增>
生产行数: 拆分前 N → 拆分后最大文件 M（列出每个新文件行数）
测试: cargo test --lib <前缀> → X passed; 新增/迁移测试 N 个，列表
自检: rustfmt --check ✓ / clippy 自己的文件 ✓ / cargo check 自己 scope ✓
API 变化: 无 / 有（列出，必须说明为什么必要）
给 Lead 的注意事项: <需要别人改的地方，比如别处仍引用旧路径>
```

尽量避免"完成但没验证"。没有把握的取舍按优先级 **① 便于扩展 ② 便于 agent ③ 人** 自行决定，**不要提问、不要停下来等人**。
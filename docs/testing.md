# 测试 / Testing

> **这份文档回答**：测试分几层、每层守什么、怎么写一条新测试、放哪个文件、本地和 CI 怎么跑。
> **不负责**：具体断言内容（读测试本身）、安全政策的理由（见 [security.md](security.md)）。
> **相关代码路径**：`src/**/#[cfg(test)]`（单元）、`tests/`（集成）、`.github/workflows/ci.yml`（CI）。

## 五层 / Five suites

| 层 | 位置 | 数量级 | 依赖 | 守什么 |
|---|---|---|---|---|
| **单元** | 各模块尾部 `#[cfg(test)] mod tests`，或大模块的 `src/<module>/tests.rs` | 570+ | 无（不 spawn、不 I/O） | 状态机、判定、边界、渲染纯函数 |
| **覆盖**（源码即断言） | `tests/help_coverage.rs`、`tests/safety_guards.rs` | 10 + 8 | 只读源码 | 帮助不得遗漏/不得撒谎；panic/`unsafe` 政策、模块文档、单文件 ≤400 行、单函数 ≤60 行、测试模块不得被平台 cfg 整段吞掉 |
| **加固**（真二进制） | `tests/logging.rs`、`tests/panic_hardening.rs` | 21 + 13 | 真进程，必须 `--test-threads=1` | 日志全链路、崩溃、恶意输入不 panic |
| **端到端** | `tests/e2e.rs` | 19 | 真 `sshd`，默认 `#[ignore]` | 三种转发真的通、主机密钥真的拒/真的可忽略、端口真的释放、探活真的发现失效会话 |
| **策略矩阵** | `tests/host_key_policy.rs` | 10 | 无（真实 `known_hosts` / `~/.ssh/config` 夹具） | 主机密钥判定完整矩阵：默认拒变更、`ignore` 接受但**不写盘**、`@revoked` 四策略恒拒；逐跳策略、全局开关优先 |

单元测试**跟着被拆模块走**：`src/model/import/table.rs` 的测试就放该文件末尾，不放回 `src/model.rs`。跨模块共用的夹具放 `#[cfg(test)]` 的夹具文件（如 `src/model/fixtures.rs`、`src/ui/test_support.rs`）——不要把一个夹具复制三份。

**测试放在哪，取决于文件大小**（两条都合法，不要混着猜）：

| 情况 | 放哪 | 为什么 |
|---|---|---|
| 模块小，生产代码 + 测试 ≤ 400 行 | 文件尾部 `#[cfg(test)] mod tests` | 改行为时测试就在眼前，一次打开 |
| 生产代码已接近预算 | `src/<module>/tests.rs`，开头 `#![cfg(test)]` | 生产文件回到"只讲它做什么"，测试在隔壁、文件名即承诺 |

后者对整个文件生效：`#![cfg(test)]` 把它排除在所有非测试构建之外，所以 panic/`unsafe`/体积规则都不适用于它。这个豁免是**被校验的**，不是被信任的——`tests/safety_guards.rs` 的 `every_whole_file_test_module_is_gated` 要求属性存在且文件名必须是 `tests.rs`，无门控的文件无法混进来。当前这样放的有 24 个文件，其余仍是文件尾部。

脚本化地判别当前状态：

```bash
# 哪些文件用整文件测试模块（都应以 #![cfg(test)] 开头）
grep -rl '^#!\[cfg(test' src
```

## 怎么跑 / Commands

```bash
cargo test                                     # 单元 + 集成（e2e 默认跳过）
cargo test --lib                               # 只跑单元（最快，CI 双平台跑这个）
cargo test --lib model::                       # 只跑某个模块
cargo test --test help_coverage                # 帮助覆盖（秒级）
cargo test --test safety_guards                # 安全/体积政策扫描
cargo test --test host_key_policy              # 主机密钥策略矩阵（进程内，秒级）
cargo test --test logging -- --test-threads=1  # 崩溃与日志（会 fork 自杀子进程）
cargo test --test panic_hardening -- --test-threads=1
cargo test --test e2e -- --ignored --test-threads=1   # 真 sshd 端到端
```

`--test-threads=1` 不是保守：`tests/logging.rs` 的崩溃用例会 fork 故意 abort 的子进程并断言逐进程的 `SSHPF_LOG_DIR`，并发跑会互相污染。

## CI 分工 / CI jobs

| Job | 平台 | 跑什么 |
|---|---|---|
| `lint` | ubuntu | `cargo fmt --all --check` + `cargo clippy --all-targets -- -D warnings` |
| `test` | ubuntu + windows | `cargo test --lib` + `cargo test --test help_coverage` + `cargo test --test host_key_policy` |
| `hardening` | ubuntu | `--test logging` 与 `--test panic_hardening`，均 `--test-threads=1` |
| `e2e` | ubuntu | `cargo test --test e2e -- --ignored --test-threads=1` |

**为什么 `help_coverage` 必须在 `test` job 里单独一行**：`cargo test --lib` 只编译库自身的 `#[cfg(test)]`，不执行 `tests/*.rs`。不写这一行，"每个按键都有帮助"的保证就永远躺在一个 CI 不会跑的文件里。`host_key_policy` 同理：它测的是"密钥变了默认拒绝、显式开启才接受、吊销永不接受"这条安全契约。

## 写新测试 / Write a new test

| 你在改什么 | 测试放哪 | 断言什么 |
|---|---|---|
| 纯数据类型 / 状态机 | 同文件 `#[cfg(test)] mod tests` | 输入 → 输出；边界值（0、`u16::MAX`、`u64::MAX`、空串） |
| 键分发 | `src/keys/<上下文>.rs` | 构造 `App`（`App::new` 不 spawn）→ 喂 `KeyEvent` → 断言 `App` 状态 |
| 渲染 | `src/ui/<面板>.rs` | 用 `TestBackend` 渲染到 `Buffer`，断言某行文本；尺寸矩阵（1×1 到 140×45） |
| 新增按键 | `src/help.rs` 数据 + `tests/help_coverage.rs` 自动覆盖 | 不用改测试，改数据即可（`HANDLERS` 表要登记新 handler） |
| 新的安全/降级路径 | 同文件 + `tests/safety_guards.rs` 自动扫 | 事件名出现在日志里 |
| 真 SSH 行为 | `tests/e2e.rs`（`#[ignore]`） | 真实 `sshd` 上跑，断言状态与字节流 |

`App::new` **不 spawn 任何进程**，所以整个键盘/表单/导入逻辑都能当纯函数测。这是把 key handler 搬出 `main.rs` 的主要理由。

## 覆盖重点 / What is worth testing

优先测**错了会静默出问题**的地方，不追行覆盖率：

| 类别 | 例子 |
|---|---|
| **时序类** | 退出事件与最后几行日志可能乱序 ⇒ 失败原因随 `Exited` 传；测试直接调 `on_exit` / `on_line` |
| **判定类** | `TunnelStatus::Up` 只能经由 `TunnelEvent::Ready` 到达（`on_ready`），任何日志文本不得改变状态 |
| **门控类** | 确认框必须在动作**之前**弹出，且确认前不得产生任何副作用；表驱动穷举全部风险操作 |
| **边界类** | 1×1 终端、4000 字符的行、CJK 宽度、空配置、损坏的状态文件、越界选中索引 |
| **不变式类** | 帮助覆盖双向、`HitTarget` 每变体被说明、keymap 覆盖全部 token、`0600`/`0700` |

## 变异测试 / Mutation check

补完测试后，**故意改坏关键逻辑**确认测试变红。以下变异历史上全部被捕获：

| 变异 | 捕获它的测试 |
|---|---|
| 交换 `-R` 的 `forward_spec` 参数顺序 | `forward_spec_for_each_kind` |
| 配置权限 0600 → 0644（两处都改） | `save_is_0600` |
| 就绪改由日志文本触发而非 `Ready` 事件 | `happy_path_starting_to_up_is_driven_by_the_driver` |
| 停用不弹确认直接执行 | `all_risky_operations_ask_first` + `space_stops_only_after_confirmation` |
| 删除不弹确认直接执行 | 4 个测试同时失败 |
| 让 `clamp_selection` 失效 | `selection_survives_a_filter_that_shrinks_the_list` |

教训：一次"变异存活"其实是因为 `sed` 没匹配上。用精确替换重做才真正生效——**验证测试本身有效，比增加测试数量更重要**。

## 测试发现过的真实缺陷 / Real bugs caught by tests

| 缺陷 | 类型 |
|---|---|
| 表单导航态按 `esc` 静默丢失输入 | 加确认闸时只改了编辑态分支 |
| 过滤后 `selected` 越界 → 选中行消失、操作落空 | 缺 `clamp_selection` 前置夹紧 |
| 编辑态 `ctrl+s` 把 `s` 打进字段（静默数据损坏） | 帮助反向断言抓到"文档和代码都错" |
| `arm_retry` 占位记录导致启动瞬间无限重连 | 集成测试 |
| 多跳 `ProxyJump` 只用第一跳 | 端到端 |
| `-R` 假就绪（`Entering interactive session.` 早于真正成功） | 结构性修复，见 [runtime.md](runtime.md#就绪判定--readiness) |

## 测试的硬边界 / What tests must not do

- **不许**为了变绿而放宽断言（尤其 `help_coverage` 的双向断言语义）。白名单只能是**常量 + 写明理由的设计决策**。
- **不许**在单元测试里依赖真实 `~/.ssh/config`、网络或 `sshd`——那是 e2e 的事。
- **不许**用真实时间 sleep 等待状态；用 `wait_for` 轮询断言（`tests/e2e.rs` 的写法）。
- 单元测试里的 `.unwrap()` / `.expect()` 是**好断言**，`tests/safety_guards.rs` 会屏蔽 `#[cfg(test)]` 区域。

## 新增测试的清单 / Checklist

1. 测试放**离被测代码最近**的文件。
2. 名字说清**行为**，不是"test_foo"：`a_drop_after_up_is_a_warning_while_a_failed_start_is_an_error`。
3. 断言消息带上下文（把实际值打进 `assert!` 消息），否则 CI 红了还得本地复现。
4. 如果加了按键/可点控件/事件，同步 [keymap.md](keymap.md) / `HitTarget` / [logging/events.md](logging/events.md)。
5. 迁移/拆分时**测试数量必须守恒**：`cargo test --lib -- --list | wc -l` 不能减少。
6. 跑 `cargo test --lib <模块前缀>` 与 `cargo test --test help_coverage`，两条都绿再交。

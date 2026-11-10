# sshpf — AI agent 入口 / Entry point for AI agents

本仓库的核心读者是 **AI agent**，其次是**人**。所有代码、文档的组织方式都是为了让你（agent）在**不看完整文件**的情况下也能定位、修改、验证。

This repository is written for AI agents first and humans second. Everything below exists so you can locate, change and verify code without reading whole files.

## 从这里开始 / Start here

| 你的任务 | 打开 |
|---|---|
| **任何任务的第一步** | [`docs/index.md`](docs/index.md) — 任务 → 文档 → 代码 的查找表 |
| **要改代码（新特性 / 修 bug）** | 先开 worktree：见下节「一特性一 worktree」 |
| 不知道新功能该放哪个模块 | [`docs/architecture.md`](docs/architecture.md) |
| 要改按键 / 帮助文案 | [`docs/agents.md`](docs/agents.md#加按键--add-a-key) + [`docs/keymap.md`](docs/keymap.md) |
| 要改渲染 / 加面板 | [`docs/tui.md`](docs/tui.md) |
| 要改隧道启停 / 重试 | [`docs/runtime.md`](docs/runtime.md) |
| 要加日志事件 | [`docs/logging/events.md`](docs/logging/events.md) + [`docs/agents.md`](docs/agents.md#加日志事件--add-a-log-event) |
| 要写测试 / 跑测试 | [`docs/testing.md`](docs/testing.md) |
| 想知道「为什么不用 unwrap / unsafe 在哪」 | [`docs/security.md`](docs/security.md) |
| 提交前自检 | [`docs/agents.md`](docs/agents.md#提交前自检--pre-flight-checklist) |

## 一特性一 worktree / One worktree per change

**开发新特性、修复问题，先新开一个 worktree + 分支再动手**。不在共享 checkout 里改代码：未提交改动、git index、`target/` 都是共享的，几个人（agent 或人）在里面并行，谁 checkout 一下、谁 `cargo fmt --all` 一下，就把别人的工作打断了。

```bash
git worktree add .agents/worktrees/feat/my-feature -b feat/my-feature origin/main
# …开发、自检、提交、push、PR、CI 绿、合并…
# PR 合并后必须清理：worktree 和分支都不留
git worktree remove .agents/worktrees/feat/my-feature
git branch -d feat/my-feature && git push origin --delete feat/my-feature
```

残留的 worktree / 分支会被当成「这条线还在进行中」，所以**合并即清理**是硬要求，不是收尾礼貌。目录命名、完整生命周期与清理清单见 [`docs/agents.md`](docs/agents.md#一特性一-worktree--one-worktree-per-change)。

## 六条硬约定 / Six hard rules

1. **每个源文件开头必须有 `//!` 模块文档**，说清「什么属于这里 / 什么不属于」，这是 agent 的地图。`tests/safety_guards.rs` 会强制。
2. **生产代码不许 `.unwrap()` / `.expect()` / `panic!`**。失败要么返回 `Result`/`Option`，要么记日志 + 显示在状态条。测试代码豁免。
3. **`unsafe` 只允许出现在 `src/logging/crash.rs`**（崩溃写入器），每处必须有 `// SAFETY:`。
4. **依赖只许向下**：`main → app/keys/ui → tunnel/config → ssh → model/text/logging/sys`。反向依赖是缺陷。
5. **一个文件一个职责，单文件生产代码 ≤400 行，单函数 ≤60 行**。超了要拆，不是加注释。**测试不占这份预算**：小模块的测试留在文件尾，大模块的测试搬到 `src/<module>/tests.rs`（开头 `#![cfg(test)]`），别为了迁就测试去删测试或放宽断言。
6. **不改变可观察行为**：`event=` 名称、帮助文案、键位、渲染文本逐字保持不变（重构 ≠ 改需求）。

以上 1/2/3/5 都是 [`tests/safety_guards.rs`](tests/safety_guards.rs) 里的**可执行断言**，不是文档里的口号。

## 本地验证 / Verify

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --lib                                   # 单元测试
cargo test --test safety_guards                    # 安全/体量政策
cargo test --test help_coverage                    # 帮助覆盖（机器校验）
cargo test --test logging -- --test-threads=1      # 崩溃路径（真二进制）
cargo test --test panic_hardening -- --test-threads=1
cargo test --test e2e -- --ignored --test-threads=1  # 需要 sshd
```

## 项目一句话 / What it is

`sshpf` 是一个 SSH 端口转发管理器（TUI），支持 `-L` / `-R` / `-D`，纯 Rust 实现（russh，无 `ssh` 子进程），Linux + Windows 同一套代码路径。

用户文档见 [`README.md`](README.md)；`docs/research/` 是历史调研，**不是当前事实来源**。

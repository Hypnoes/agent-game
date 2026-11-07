# docs/issues

本目录记录待实施改动的规划。每个文件对应一个 GitHub Issue，文件名形如 `<编号>-<英文短名>.md`。

规划与 Issue 的关系：先在本目录写清背景、设计与验收标准，再据此创建 GitHub Issue 并把编号回填到文件头部；实施在 `issue-<编号>-<短名>` 分支的 worktree 中进行。

## 清单

| 编号 | GitHub Issue | 标题 | 前置 | 状态 |
|---|---|---|---|---|
| [001](./001-blocking-state-wait.md) | [#1](https://github.com/Hypnoes/agent-game/issues/1) | 阻塞式对局等待 | — | 已合并 |
| [002](./002-mcp-http-transport.md) | [#2](https://github.com/Hypnoes/agent-game/issues/2) | 迁移到 MCP over HTTP 并移除本地客户端 | 001 | 已合并 |
| [003](./003-full-remediation.md) | — | 全量修复：正确性、健壮性、测试框架与工具链 | 001、002 | 已实施 |
| [004](./004-riir.md) | — | RIIR：用 Rust 重写整个服务端 | 001–003 | 已实施 |
| [005](./005-core-layering.md) | [#5](https://github.com/Hypnoes/agent-game/issues/5) | core 分层重排，每层抽成独立 crate | 004 | 已合并 |
| [006](./006-delete-dead-code.md) | [#8](https://github.com/Hypnoes/agent-game/issues/8) | 删除死代码与失效的协议面 | 005 | 规划中 |
| [007](./007-testing-gate.md) | [#9](https://github.com/Hypnoes/agent-game/issues/9) | 测试替身不许进发布产物 | 005 | 规划中 |
| [008](./008-liars-dice-modelling.md) | [#10](https://github.com/Hypnoes/agent-game/issues/10) | LiarsDice 建模收紧 | 005 | 规划中 |
| [009](./009-phase-enum.md) | [#11](https://github.com/Hypnoes/agent-game/issues/11) | phase 枚举化，消除框架硬编码 | 008 | 规划中 |

005 之后这批的依赖关系：**006 与 007 互相独立**（一个删死代码、一个管测试替身的
编译门控），可以并行；**008 依赖 005**（要改 `LiarsDiceEngine` 的内部表示）；
**009 依赖 008**（同一批文件，先改语义再改类型，否则两个 PR 会反复冲突）。

006 处理的 `games/state_machine.rs` 在 005 之前就存在，但它属于"死代码清理"而
不是"结构重排"，所以没有并入 005。

001 可以在现有 REST 通道上独立验证等待行为；002 把同一行为搬到 MCP 传输层，并删除本地客户端包。两者的分支与 PR 采用堆叠方式：002 基于 001 的分支开发，PR 先指向 001 分支，待 001 合并后再把 base 改为 `master`。两个 Issue 均已实现并合入 `master`，实施中与原规划的偏差记在各自文档末尾的「实施结果」一节。

004 是语言级重写，把 001–003 的全部行为搬到 Rust，并在三处刻意改了设计（`game_name` → `display_name`、秘密信息改为构造、等待循环上移到框架）。旧实现永久保存在标签 `python-v0.3.0` 与 git 历史里。端点映射见 [`../legacy/migration.md`](../legacy/migration.md)。

审查记录在 [`../reviews/`](../reviews/)，都是 Python 时代的两份专项审查（代码缺陷 + 测试框架），结论已并入 003 与 004。

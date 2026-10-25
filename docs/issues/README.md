# docs/issues

本目录记录待实施改动的规划。每个文件对应一个 GitHub Issue，文件名形如 `<编号>-<英文短名>.md`。

规划与 Issue 的关系：先在本目录写清背景、设计与验收标准，再据此创建 GitHub Issue 并把编号回填到文件头部；实施在 `issue-<编号>-<短名>` 分支的 worktree 中进行。

## 清单

| 编号 | GitHub Issue | 标题 | 前置 | 状态 |
|---|---|---|---|---|
| [001](./001-blocking-state-wait.md) | [#1](https://github.com/Hypnoes/agent-game/issues/1) | 阻塞式对局等待 | — | 已合并 |
| [002](./002-mcp-http-transport.md) | [#2](https://github.com/Hypnoes/agent-game/issues/2) | 迁移到 MCP over HTTP 并移除本地客户端 | 001 | 已合并 |

001 可以在现有 REST 通道上独立验证等待行为；002 把同一行为搬到 MCP 传输层，并删除本地客户端包。两者的分支与 PR 采用堆叠方式：002 基于 001 的分支开发，PR 先指向 001 分支，待 001 合并后再把 base 改为 `master`。两个 Issue 均已实现并合入 `master`，实施中与原规划的偏差记在各自文档末尾的「实施结果」一节。

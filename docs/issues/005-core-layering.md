# core 按 infra/domain/games 三层重排目录

| 项 | 值 |
|---|---|
| 本地编号 | 005 |
| GitHub Issue | [#5](https://github.com/Hypnoes/agent-game/issues/5) |
| 分支 | `issue-5-reorganize-layers` |
| 状态 | 实施中 |

## 背景

`crates/agent-game-core` 有 16 个扁平模块，`lib.rs` 只是一个按字母序排列的
`pub mod` 列表。分层完全隐含，没有任何地方表达"谁是底层、谁是上层"。

实测依赖图（生产代码，排除 `#[cfg(test)]`）：

```
budget.rs / config.rs / error.rs / prompt.rs / signal.rs   ← 无 crate 内依赖（叶子）
                          ↑ 被上层直接 use
engine.rs      (419 行, 100% 生产)  → error, prompt, signal
registry.rs    (167 行生产)         → config, engine
manager.rs     (529 行生产)         → config, engine, error, registry
wait.rs        (52 行生产)          → signal
identity.rs    (67 行生产)          → error
games/*.rs     (1529 行生产)        → config, engine, error, identity, prompt, signal
```

这里是"分层存在但不可见"：依赖方向本身是对的（没有循环），但没有任何地方
把"`budget`/`config`/`error` 是底层、`engine`/`registry`/`manager` 是中间层、
`games` 是上层"这件事写下来。新人打开 `lib.rs` 看到的只是 16 行 `pub mod`。

三个可量化的问题：

### 1. `engine.rs` 一个文件装 5 类东西

| 行 | 内容 |
|---|---|
| L28 / L31 | `DEFAULT_FINISHED_STATES` / `DEFAULT_SENSITIVE_KEYS` 常量 |
| L35 | `PlayerMode` 枚举 |
| L49–104 | `CreateParams` + ~150 行 JSON 取值 |
| L108–209 | `ActionParams` + ~100 行 JSON 取值 |
| L213–281 | `PlayerView` / `TimeoutEvent` 两个 DTO |
| L287 | `GameEngine` trait |
| L394 | `ToolSpec` |

419 行、零测试。参数解析（~250 行）与 trait 定义毫无关系，却挤在一起。

### 2. `manager.rs` 是 5 个职责的并集

| 行区间 | 职责 | 行数 |
|---|---|---|
| L40–95 | `Session` 生命周期（poison / finished_at） | 56 |
| L97–148 | 快照 DTO（`TimeoutRecord` / `SessionSnapshot`） | 52 |
| L149–214 | `GameManager` 定义 + 访问器 | 66 |
| L215–305 | **匹配逻辑**（重连 → 并桌 → 新建） | 91 |
| L306–470 | 快照 / 指纹 / 超时结算 / 去重 | 165 |
| L471–530 | 动作分发 | 60 |

"匹配"与"超时结算"各有独立不变量（AGENTS.md 第 3 条 vs 第 9/10 条），
却共享同一个 `impl`。改匹配的人必须读完超时结算才能确认没有副作用。

### 3. 领域概念在代码里没有落点

| 概念 | 现在住哪 | 可发现性 |
|---|---|---|
| Lobby | `manager.rs` L215–305 的私有方法 `create()` | 要读注释才知道这里在做匹配 |
| GameAbstraction | 名为 `engine.rs` 的文件第 287 行 | 文件名说的是 engine，不是抽象 |
| GameImpl | `games/` 两个文件，合计 1529 行生产代码 | 正常 |

## 目标

按 `infra → domain → games` 三层重排目录，让依赖方向**可见且可测试**。

```
crates/agent-game-core/src/
├─ lib.rs              只有 re-export + 分层文档
├─ domain/             你说的三个概念落在这里
│   ├─ game.rs         GameAbstraction：trait + 4 个 DTO
│   ├─ lobby.rs        Lobby：匹配（重连 / 并桌 / 新建）
│   ├─ session.rs      Session 生命周期
│   ├─ snapshot.rs     SessionSnapshot / TimeoutRecord
│   └─ supervisor.rs   GameManager 剩余：动作分发 + 超时结算
├─ games/              GameImpl（不动，仅改 use 路径）
│   ├─ liars_dice.rs
│   └─ guess_number.rs
└─ infra/              被所有人 use、自己不 use 上层
    ├─ params/         从 engine.rs 抽出 ~250 行
    │   ├─ create.rs
    │   └─ action.rs
    ├─ budget.rs   config.rs   error.rs   signal.rs   wait.rs
    ├─ prompt.rs   identity.rs registry.rs
```

## 设计

### 约束一：纯移动

`git mv` + 改 `use` 路径，不改任何逻辑。判据是 `git diff -M --stat` 应把
全部被移动的文件识别为 rename（相似度 ≥ 90%）。测试数保持 518 不变。

**这是刻意选择的边界**：不在同一个 PR 里叠加语义改动，否则 review 时分不清
哪一行是"跟着移动走的"、哪一行是"真的改了行为"。语义问题（`alive` 与 `dice`
双真相、`Call` 的哨兵值、`phase` 字符串）由 008/009 单独处理。

### 约束二：分层要可执行，不只是文档

单靠目录名和一段说明，下一个人仍可能从 `infra/` 里 `use crate::domain`。
新增 `tests/layering.rs`：解析 `infra/` 下每个 `.rs` 的源码，断言其中不出现
`use crate::domain` 或 `use crate::games`。

这让 AGENTS.md 可以新增一条**有看门用例的**约束，而不是又一段无人执行的注释
（对应 AGENTS.md 第 14 条：每条约束都要有对应的看门用例）。

### 约束三：顺带降 trait 默认实现

`GameEngine` 现在 19 个方法、11 个必须实现。拆文件时顺手把三个可以给默认值的
改成有默认实现，**不改任何签名**：

| 方法 | 默认值 | 理由 |
|---|---|---|
| `player_mode()` | `PlayerMode::Single` | 单人游戏占多数 |
| `check_timeout()` | `Ok(None)` | 没有回合制超时的引擎不需要写 |
| `admin_view()` | 转调 `public_view()` | 没有秘密字段的引擎不需要写两遍 |

`DummyEngine` 因此能从 126 行缩到更短。这是"按需实现"的一半（另一半是真正
拆子 trait，留给 009）。

## 测试

| 用例 | 位置 | 断言 |
|---|---|---|
| `infra_never_depends_on_domain` | `tests/layering.rs` | `infra/*.rs` 无对 `domain` 的依赖 |
| `infra_never_depends_on_games` | `tests/layering.rs` | `infra/*.rs` 无对 `games` 的依赖 |
| `domain_never_depends_on_games` | `tests/layering.rs` | `domain/*.rs` 无对 `games` 的依赖 |
| `the_guard_actually_detects_a_violation` | `tests/layering.rs` | 反向验证：同一检查函数必须能抓到 `games` 的真实依赖 |
| `the_guard_catches_*`（4 条） | `tests/layering.rs` | `pub(crate) use`、`as` 别名、分组、跨行分组四种写法 |
| `comments_do_not_create_false_positives` | `tests/layering.rs` | 注释里的 use 不算 |
| `every_public_module_path_still_resolves` | `tests/api_paths.rs` | 12 个历史模块路径逐个 import |
| `top_level_reexports_still_resolve` | `tests/api_paths.rs` | 顶层 22 个再导出名字逐个点名 |
| 现有 518 条 | 全部 | 全绿 |

## 验收标准

- `cargo test --workspace`：**530 passed**（基线 518 + 10 条分层看门 + 2 条 API 路径）
- `cargo fmt --all -- --check` 零差异
- `cargo clippy --all-targets -- -D warnings` 零告警
- `git diff -M --stat` 对全部移动文件识别为 rename
- 分层看门与 API 路径看门都做过**手工变异验证**（见下）

### 手工变异验证

四条规则各自注入一次违规，确认套件变红后恢复：

| 注入 | 预期 | 实测 |
|---|---|---|
| `infra/error.rs` 加 `use crate::domain::game::PlayerMode;` | `layering` 红 | 红 |
| 同上但写成 `pub(crate) use ...` | 红 | 红 |
| 同上但写成 `use crate::domain as dom;` | 红 | 红 |
| 同上但写成 `use crate::{\n domain::X,\n};` | 红 | 红 |
| `domain/session.rs` 加 `use crate::games::...` | 红 | 红 |
| 删掉 `lib.rs` 的 `manager` 兼容模块 | `api_paths` 编译失败 | 失败 |

## 已知边界

- **diff 很大**：几乎每个文件的 `use` 头部都会变。这是纯移动的固有代价，
  不是失控。用 `git diff -M` 而不是 `git diff` 来看。
- **不做**：目录名与包名不一致（`agent-game-mcp` → `agent-game-server-mcp`）
  维持现状——改名会打断 `Cargo.lock` 与外部引用，收益不抵成本。改为在
  `crates/agent-game-mcp/src/lib.rs` 模块文档首行补一句角色声明。
- **不做**：`sensitive_keys` 保留（AGENTS.md 第 8 条：它是测试期望清单）。
- **不做**：`map_or("?")` 保留，在 008 里收敛。

## 不在本次范围

- `games/state_machine.rs` 的删除（归 006）
- `testing.rs` 的 cfg 修正（归 007）
- `LiarsDice` 的 `alive` / `Call` / 索引建模（归 008）
- `phase` 枚举化（归 009）

## 依据

本轮 review 的结论：现有结构不直观。该判断由上面的依赖图与职责聚类数据
支撑，不是风格偏好——这也是它从"优先级不高下次再做"提升为优先项的原因。

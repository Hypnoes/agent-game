# 删除死代码与失效的协议面

| 项 | 值 |
|---|---|
| 本地编号 | 006 |
| GitHub Issue | [#8](https://github.com/Hypnoes/agent-game/issues/8) |
| 分支 | `issue-8-delete-dead-code` |
| 状态 | 实施中 |

## 背景

`agent-game-core` 里有两处**没有任何生产调用者**的代码。它们不是"暂时没人用"，
而是从重构里遗留下来、会让下一个人误以为存在的死代码。

### 1. `games/state_machine.rs` 整个模块

`Transitions` 提供 `new` / `allowed_from` / `can_transition` / `check` 四个方法，
用一个 `Vec<(&'static str, Vec<&'static str>)>` 表达状态转移表。

**整个 workspace 里除了它自己的 3 条单元测试，没有任何引用。** 实测：

```
$ grep -rnE "Transitions|state_machine" crates/
crates/agent-game-core/src/games/mod.rs:5:pub mod state_machine;
crates/agent-game-core/src/games/state_machine.rs:6://! [`Transitions`] 里写一张表...
crates/agent-game-core/src/games/state_machine.rs:12:pub struct Transitions {
...
```

注意 `-E`：POSIX 的 `grep` 默认用 BRE，`|` 在那里是**普通字符**，
`grep -rn "Transitions|state_machine"` 会返回空——看起来像"什么都没找到"，
掩盖了真实结果。

而两个引擎都用 `phase: &'static str` 直接赋值、用字符串字面量比较：

```rust
// liars_dice.rs
157:  phase: "waiting",
397:  self.phase = "bidding";
544:  self.phase = "finished";
765:  if self.phase == "finished"
856:  if self.phase == "waiting"
865:  if self.phase == "bidding"
898:  if self.phase == "finished" { ... }
901:  self.phase == "bidding" && self.current_turn == player_index
```

模块文档第 5-6 行还写着「每个引擎在 `Transitions` 里写一张表，`transition_to`
校验后返回新状态」——**`transition_to` 这个函数在仓库里不存在**。文档描述的是
一次从未发生的重构。

比"数据结构选错"更严重的是：变异测试**从 0 存活**，而这个模块一次都没被调用过。
无调用者的代码即使写错也不会有任何测试变红，`cargo mutants` 也发现不了。

### 2. `GameEngine::setup_params()`

这是 trait 的 11 个必实现方法之一。生产路径**零调用**——`Session` 已经持有
同一份 `CreateParams`（`session.setup`），匹配逻辑用的是它（`manager.rs:280`
的 `compatible(&session.setup, &setup)`）。

可导入性由 `tests/api_paths.rs` 的探针印证：`engine.setup_params()` 只在
`tests/api_paths.rs` 与几个集成测试里被调用，没有一处生产代码。

## 目标

删掉这两处，让"存在即有人用"重新成立。

## 设计

### 删除 `state_machine.rs`

| 动作 | 位置 |
|---|---|
| 删文件 | `crates/agent-game-core/src/games/state_machine.rs`（83 行） |
| 删声明 | `crates/agent-game-core/src/games/mod.rs:5` |
| 删 3 条单元测试 | 随文件一起消失 |

### 兼容性影响：必须说清楚

`Transitions` 与 `setup_params()` 都是 `pub`，所以删除**是源兼容性破坏**。

我能证明的只有"workspace 内零引用"。**外部消费者范围未知**——这个仓库是公开的，
理论上可能有人依赖。所以要交代清楚而不是含糊过去：

| 符号 | 性质 | 我的判断 |
|---|---|---|
| `Transitions` | `pub struct` + 4 个方法 | workspace 零引用；语义（状态转移表）在两个引擎里从未被使用。**但它从未在任何文档 / README / migration 里出现过**，外部使用者只能靠读源码发现 |
| `GameEngine::setup_params()` | trait 的必实现方法 | 移除会让所有 impl 少一个方法。**workspace 内**的实现全部由我们维护，编译期会报错；**外部**实现者（如果存在）会编译失败 |

**版本策略**：

- 项目当前是 `0.4.0`（`Cargo.toml` 的 `workspace.package.version`），未发布到
  crates.io。0.x 版本按 semver 允许破坏性变更。
- 若在 `0.4.0` 基础上发布，**按 0.x 约定应升到 `0.5.0`**，而不是 patch。
  提交信息里写 `BREAKING:` 是记录手段，不能替代版本号——两者都要做。
- 在 `docs/issues/` 记录，这是这个项目当前唯一的变更公告渠道。

**迁移说明**（写给可能的外部使用者）：

| 删掉的符号 | 替代方式 |
|---|---|
| `GameEngine::setup_params()`（调用方） | 读 `Session.setup`——`GameManager` 一直用它做匹配判断，是同一份数据 |
| `GameEngine::setup_params()`（实现者） | 删掉该方法的 impl 即可。trait 不再要求它 |
| `Transitions` | **没有等价替代**。使用者要么移除这个依赖、要么在自己的引擎里实现状态转移。009 会给每个引擎做一层 `transition_to`，但那是引擎私有的，不打算重新导出成通用设施 |

如果要更保守，可以先发一个 `0.4.x` patch 把两者标 `#[deprecated]`，等一版再删。
**我的建议是直接删**：`Transitions` 是纯粹的死代码，给它加 deprecation 警告等于
为一个从没被用过的东西写迁移指引。

### 删除 `setup_params()`

| 动作 | 位置 |
|---|---|
| 删 trait 方法 | `crates/agent-game-domain/src/game.rs`（`fn setup_params`） |
| 删实现 | 两个引擎 + `DummyEngine` |
| 迁移断言 | `crates/agent-game-core/tests/boundaries.rs` 里那两条测 `setup_params`
  的用例，改为直接验 `Session.setup` |

**为什么不是"保留但标注 deprecated"**：`Session.setup` 已经是唯一真相。留着
`setup_params()` 会让人以为引擎需要额外暴露一份配置，而两份配置一旦不同步就是
一个没人会发现的 bug——这正是死代码最典型的危害形态。

## 测试

| 用例 | 位置 | 断言 |
|---|---|---|
| 现有全量 | 全部 | 全绿 |
| 删掉的 3 条 | — | 随模块消失 |
| 迁移的 2 条 | `boundaries.rs` | 改为验 `Session.setup`，语义不变 |

**关键判据不是"测试还是绿的"，而是"删掉之后有没有报警示"**：删 `setup_params`
会让 trait 从 11 个必实现降到 10 个，所有 impl 都少一个方法。**workspace 内**如果
某个引擎其实依赖它，编译期就会报错——这个保证只对仓库内的实现成立，外部实现者
会得到同样的编译错误，但那是他们的构建失败，不是我们的。

## 验收标准

- `cargo test --workspace`：527 passed（删 3 条 `state_machine` 用例后应为 524，
  若数字不符说明有别的测试也依赖它们）
- `grep -rn "Transitions\|setup_params" crates/` 为空
- `cargo fmt` / `cargo clippy -D warnings` 零问题
- `api_paths.rs` 的文件头注释更新：`Transitions` 的"刻意例外"说明应改成
  "已删除"

## 已知边界

- **不动** `phase` 的字符串表示。把它改成枚举是 009 的范围，两者不该混在一个 PR
- **不动** `sensitive_keys()`。AGENTS.md 第 8 条明确它是测试期望清单，生产不读
  是设计而非缺陷

## 不在本次范围

- `phase` 枚举化（009）
- `LiarsDice` 的建模问题（008）
- `testing` 模块的 cfg 修正（007）

---

## 实施记录

### 实际删掉的东西

比规划多删了两个字段——它们是被 `setup_params()` "养活"的僵尸：

| 项 | 位置 | 说明 |
|---|---|---|
| `state_machine` 模块 | `crates/agent-game-core/src/games/state_machine.rs` | 规划内 |
| `GameEngine::setup_params()` | `crates/agent-game-domain/src/game.rs` | 规划内 |
| 9 处 impl | 两个引擎 + `DummyEngine` + 2 处测试桩 + mcp/server 测试桩 | 随 trait 方法消失 |
| **`LiarsDiceEngine::setup` 字段** | `liars_dice.rs` | 规划外 |
| **`GuessNumberEngine::setup` 字段** | `guess_number.rs` | 规划外 |

后两个是 `cargo build` 报出来的：`field 'setup' is never read`。它们是
`setup_params()` 唯一的读者——删掉方法之后，这两个字段变成纯僵尸。**只靠
grep 看不出这层关系**，这条经验写进了 AGENTS.md 第 18 条。

### 测试数变化

- 基线 527
- 删 `state_machine` 的 3 条单元测试 → 524
- `boundaries.rs` 的 3 条 `setup_params` 测试**改造**为验 `Session.setup`
  （数量不变）→ 仍是 524

与规划里"527 - 3 = 524"的预期一致。

### 改造的测试

原来测 `engine.setup_params()`，现在测 `session.setup`：

- `the_session_keeps_the_creation_configuration`
- `the_session_setup_never_includes_the_player_token`
- `the_session_carries_the_configuration`

新增 `manager()` 辅助函数（`boundaries.rs`），造一个注册了两个内置游戏的
`GameManager`——只有经由 `create()` 才会产生 `Session`。

### 验收

- 524 passed、fmt 零差异、clippy 零告警
- `grep -rnE "Transitions|state_machine|setup_params" crates/` 只剩
  `without()` 的测试用例与文档措辞

### review 后的两个决定

**1. 保留 `CreateParams::without()`。** 它现在没有生产调用点（原本是
`setup_params()` 的配套），但仍是一个公开、通用的「复制并移除键」API。删除它会
扩大本次源兼容性破坏范围，而且并非删除 `setup_params()` 所必需。如果将来要清理，
应作为单独的 API 兼容性决策，而不是因为"仓库内只剩测试用"就顺手删。

**2. 提交标题带 `BREAKING:`。** review 指出「0.x 允许破坏性变更」不等于「发布时
可以保持 0.4.0」——版本号要升到 0.5.0，提交标题也应明确标出，两者不互相替代。

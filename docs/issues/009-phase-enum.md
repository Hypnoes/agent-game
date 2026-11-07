# phase 枚举化，消除框架硬编码的游戏状态名

| 项 | 值 |
|---|---|
| 本地编号 | 009 |
| GitHub Issue | [#11](https://github.com/Hypnoes/agent-game/issues/11) |
| 分支 | `issue-11-phase-enum` |
| 状态 | 规划中 |

## 背景

README 宣称「零改动加游戏：新引擎实现 `GameEngine` trait，注册一个 `GameMeta`，
MCP 工具面与 admin 展示名全部自动生效」。但这个承诺有一处缺口。

### 框架硬编码了 `"waiting"`

`domain/lobby.rs:84`（多人游戏并桌的判据）：

```rust
let waiting = engine.phase() == "waiting" && !engine.is_finished();
```

`phase()` 返回 `&'static str`，trait 上**没有** `is_waiting()` 之类的可覆盖判定。
后果：一个把大厅态命名为 `lobby` **或任何其它名字**的新引擎，永远匹配不到桌子——
每个玩家都会各开一局。

这不是假设。`finished_states()` 是**可覆盖**的（trait 上有默认值），说明设计者
意识到"终态名应该由引擎自报"；但"大厅态"漏了同样处理。

### phase 是散落的字符串

生产代码里 `phase` 相关表达 **96 处**，测试 **80 处**。两个引擎各自用
`phase: &'static str` 赋值、用字面量比较：

```rust
// liars_dice.rs
phase: "waiting",
self.phase = "bidding";
self.phase = "finished";
if self.phase == "finished"
if self.phase == "waiting"
if self.phase == "bidding"
```

没有任何编译期检查能阻止 `self.phase = "bididng"`（拼错）或
`self.phase = "finished"` 出现在非法转移处。006 删掉的 `state_machine.rs`
本来想解决这个问题，但它从未被接入。

## 目标

1. 每个引擎的状态由**枚举**表达，非法转移写不出来
2. "大厅态"可由引擎自报，框架不再硬编码具体名字
3. **JSON 契约不变**——admin 页面与 Agent prompt 看到的 `phase` 值仍是现在的字符串

## 设计

### a. 每引擎私有枚举

```rust
// liars_dice.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase { Waiting, Bidding, Finished }

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Bidding => "bidding",
            Self::Finished => "finished",
        }
    }
}

// guess_number.rs
enum Phase { InProgress, Won, Lost }
```

`GameEngine::phase()` 的实现变成 `self.phase.as_str()`，**对外签名不变**。

### b. 大厅态由引擎自报

trait 新增一个有默认值的方法：

```rust
/// 这个引擎的"等待玩家入场"阶段叫什么。默认 `"waiting"`——
/// 多数多人游戏会这么叫，但引擎可以覆盖。
fn waiting_phase(&self) -> &'static str {
    "waiting"
}
```

`lobby.rs:84` 改为：

```rust
let waiting = engine.phase() == engine.waiting_phase() && !engine.is_finished();
```

**为什么是返回字符串而不是 `bool`**：`lobby.rs` 的判据本质是"当前阶段是不是
大厅态"，返回字符串让引擎只声明一次名字，框架自己做比较；返回 `bool` 则每个
引擎都要写一个 `fn is_waiting(&self) -> bool`，多一行样板，且与 `finished_states()`
的风格不一致（后者也是自报名字列表）。

### c. 非法转移写不出来

`self.phase = Phase::Finished` 只能从合法位置写；要禁止 `Waiting → Finished`
这类转移，在每个枚举上加一个 `fn can_transition_to(self, next: Self) -> bool`，
在赋值点断言。**注意**：这会把 006 删掉的 `state_machine.rs` 以更轻的形式
请回来——但这次是**每个引擎自己的 3 个变体**，不是通用的 `Vec<(&str, Vec<&str>)>`
数据结构。

## 测试

| 用例 | 断言 |
|---|---|
| 契约快照（insta） | 公开视图的 `phase` 仍是 `waiting`/`bidding`/`finished`/`in_progress`/`won`/`lost` |
| 新 `a_game_with_its_own_lobby_name_still_matches` | 大厅态叫 `lobby` 的引擎能并桌——证明"零改动加游戏"成立 |
| 新 `illegal_transition_is_rejected` | 每个引擎的非法转移 |
| 现有 80 处 phase 断言 | 全绿（它们比的是 JSON 里的字符串，不该变） |

**契约快照不能省**：`admin.html:323` 用 `state.phase ?? state.status` 归类
「进行中/已结束」。枚举化如果改了序列化值，页面会静默把对局归类错。

## 验收标准

- `cargo test --workspace` 全绿，测试数不减
- `grep 'phase() == "waiting"' crates/` 为空
- `grep '"waiting"\|"bidding"\|"finished"' crates/agent-game-domain/src/lobby.rs` 为空
- insta 快照锁定 6 个 phase 值不变
- 变异测试（`-p core`）仍 0 存活

## 已知边界

- 依赖 008（同一批文件）。008 动 `LiarsDiceEngine` 的内部表示，009 动它的
  `phase` 类型；分开做会让第二个 PR 反复冲突
- **不改** `finished_states()` 的既有设计，只是让 `waiting` 有同等表达力
- **不改** JSON 字段名或值

## 不在本次范围

- `PlayerMode` 的扩展
- 状态机可视化 / 调试输出

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

### c. 非法转移：枚举不够，要集中到返回 Result 的转移方法

**第一版规划在这里说错了。** 它写的是"非法转移写不出来"，理由是枚举约束了
取值。这是不成立的：`enum Phase { Waiting, Bidding, Finished }` **只约束取值，
不阻止在任何位置写 `self.phase = Phase::Finished`**。从 `Waiting` 直接跳到
`Finished` 照样能编译。

而且第一版提议"在赋值点 `assert!`"。那更糟：

- `assert!` 是 panic。引擎 panic 会被 `GameManager::act` 的 `catch_unwind`
  捕获并**毒化整个对局**（`supervisor.rs`），而不是返回一个可恢复的错误
- `check_timeout` 里的 panic 会触发 AGENTS.md 第 10 条的隔离路径——那是给
  "引擎内部 bug" 兜底的，不该用来做常规的非法转移检查

**正确做法**：把转移收进一个方法，返回 `Result<(), EngineError>`：

```rust
impl LiarsDiceEngine {
    fn transition_to(&mut self, next: Phase) -> Result<(), EngineError> {
        let legal = match (self.phase, next) {
            (Phase::Waiting, Phase::Bidding) => true,
            (Phase::Bidding, Phase::Finished) => true,
            _ => false,
        };
        if legal {
            self.phase = next;
            Ok(())
        } else {
            Err(EngineError::invalid(format!(
                "非法状态转移：{:?} → {:?}", self.phase, next
            )))
        }
    }
}
```

`phase` 字段私有化，**所有**赋值走 `transition_to`。这样"非法转移"变成
运行时可检出、且走 `EngineError` 的正常错误路径，不是 panic。

### 每个引擎要写清的合法转移

迁移时不能漏掉现有赋值点对应的语义。实测当前 5 处赋值：

| 位置 | 场景 | 目标状态 |
|---|---|---|
| `guess_number.rs:193` | `reset()` 重新开始 | `InProgress` |
| `guess_number.rs:205` | `end()` 的通用出口 | `Won` / `Lost`（由参数决定） |
| `guess_number.rs:439` | 超时判负 | `Lost` |
| `liars_dice.rs:398` | `start_game()` 满员开局 | `Bidding` |
| `liars_dice.rs:545` | `finish_if_over()` 只剩一人 | `Finished` |

注意 `guess_number::reset()` 允许**从任意阶段**重新开始——这是它和
`liars_dice` 的区别（后者开局后不允许追加玩家）。所以 `transition_to` 的合法集
每个引擎不同：

- `LiarsDice`：`Waiting → Bidding`、`Bidding → Finished`
- `GuessNumber`：`InProgress → Won`、`InProgress → Lost`、**任意 → InProgress**（重置）

`reset()` 这条要单独确认：如果 `Won → InProgress` 被判为非法，重置功能会坏。

注意：这确实把 006 删掉的 `state_machine.rs` 以另一种形式请了回来。区别是这次
在**每个引擎自己的 impl 里**用 `match` 写死 3 个变体的合法转移，不是通用的
`Vec<(&str, Vec<&str>)>` 数据结构 + 字符串比较。

## 测试

| 用例 | 断言 |
|---|---|
| 契约快照（insta） | 公开视图的 `phase` 仍是 `waiting`/`bidding`/`finished`/`in_progress`/`won`/`lost` |
| 新 `a_game_with_its_own_lobby_name_still_matches` | 大厅态叫 `lobby` 的引擎能并桌——证明"零改动加游戏"成立 |
| 新 `illegal_transition_returns_an_error` | 每个引擎的非法转移返回 `Err(EngineError::invalid(..))`，**不 panic**（用 `catch_unwind` 或返回类型断言） |
| 现有 80 处 phase 断言 | 全绿（它们比的是 JSON 里的字符串，不该变） |

**契约快照不能省**：`admin.html:323` 用 `state.phase ?? state.status` 归类
「进行中/已结束」。枚举化如果改了序列化值，页面会静默把对局归类错。

## 验收标准

- `cargo test --workspace` 全绿，测试数不减
- `grep 'phase() == "waiting"' crates/` 为空
- `grep '"waiting"\|"bidding"\|"finished"' crates/agent-game-domain/src/lobby.rs` 为空
- insta 快照锁定 6 个 phase 值不变
- `grep -n 'self.phase =' crates/agent-game-core/src/games/*.rs` 的每一处都在某个
  `transition_to` 内部。**实测当前有 5 处**（`guess_number.rs:193/205/439`、
  `liars_dice.rs:398/545`），全部要改走转移方法。验收不是"剩一处"，是
  "每一处都在转移方法里"
- 非法转移是 `Err` 而不是 panic：`check_timeout` 路径上用一个会产生非法转移的桩验证它不会毒化对局
- **非法转移不留部分更新**：`transition_to` 在返回 `Err` 时不能已经改动了
  `winner` / `loser` / `last_action` / `current_call` 等旁路状态。测试要构造
  "校验失败但旁路已被改"的场景并断言它们仍是原值——否则对局会进入一个
  "状态没变但数据变了"的错乱态
- 变异测试（`-p core`）仍 0 存活

## 已知边界

- 依赖 008（同一批文件）。008 动 `LiarsDiceEngine` 的内部表示，009 动它的
  `phase` 类型；分开做会让第二个 PR 反复冲突
- **不改** `finished_states()` 的既有设计，只是让 `waiting` 有同等表达力
- **不改** JSON 字段名或值

## 不在本次范围

- `PlayerMode` 的扩展
- 状态机可视化 / 调试输出

# LiarsDice 建模收紧：消除双真相、哨兵值与裸索引

| 项 | 值 |
|---|---|
| 本地编号 | 008 |
| GitHub Issue | [#10](https://github.com/Hypnoes/agent-game/issues/10) |
| 分支 | `issue-10-liars-dice-modelling` |
| 状态 | 规划中 |

## 背景

这是四个后续项里**唯一会真正产生错误状态**的一个。其余几项是结构与整洁问题，
这一项是"存了两个真相，而它们可以被违反"。

### 1. `alive` 与 `dice` 是两个独立的真相

规则是「骰子掉光即出局」（`player.dice.is_empty()` ⇒ 出局），但代码里 `alive`
是一个独立的 `bool`：

```rust
// liars_dice.rs:53
pub alive: bool,
```

`eliminate()` 里两处分别维护（L514-525）：

```rust
let sudden_death = self.alive_indices().len() <= 2;
if let Some(player) = self.players.get_mut(player_index) && player.alive {
    if sudden_death {
        player.dice.clear();
    } else {
        player.dice.pop();
    }
    if player.dice.is_empty() {
        player.alive = false;
    }
}
```

存两个真相就意味着它们可以被违反。**测试里就真的这样违反过**：

```rust
// liars_dice.rs:1412
engine.players[1].alive = false;
```

骰子还在，但标记为出局。因为 `players` 是 `pub`（L115），外部能绕过所有不变量。

### 2. `Call` 用哨兵值区分两种语义

```rust
// L93-104
/// 非叫数的历史记录（开盅、超时判负），`quantity == 0` 是标记。
fn note(player_idx: usize, message: String) -> Self {
    Self { player_idx, quantity: 0, face: 0, message }
}
pub fn is_bid(&self) -> bool { self.quantity > 0 }
```

用"业务上有意义的值"（0 颗骰子）编码"这是一个不同类型的事件"，是枚举被拆成
struct 的典型症状。消费端因此必须过滤：

```rust
self.call_history.iter().filter(|call| call.is_bid())
```

### 3. 裸 `usize` 索引贯穿全局

`player_idx`、`current_turn`、`winner`、`loser` 全是无类型的 `usize`。
`map_or("?", ...)` 兜底出现 **5 次**（L554/569/592/633/681），全部是
"索引查不到玩家名"时的降级——类型系统没有表达"这个索引一定有效"的代价。

## 目标

让不变量由类型表达，而不是由纪律维持。

## 设计

### a. `alive` 改成 `dice` 的派生属性 —— 但这不成立，需要另一种设计

**第一版规划在这里是错的，必须记下来。** 它写的是：

```rust
impl Player {
    /// 骰子掉光即出局。
    pub fn is_alive(&self) -> bool { !self.dice.is_empty() }
}
```

**这个推导在"尚未首次发骰"的状态上崩掉**：

```rust
// liars_dice.rs:57-64  Player::new
Self { name, token, dice: Vec::new(), alive: true }
```

新玩家入场时 `dice` 是空的、`alive` 是 `true`——他还没发到骰子，不是出局。
而 `start_game()` 只给 `alive` 的玩家发骰（L389）：

```rust
player.dice = if player.alive {
    (0..self.dice_per_player).map(...).collect()
} else {
    Vec::new()
};
```

改成 `is_alive() == !dice.is_empty()` 之后，**所有新玩家都被判定为出局**，
`start_game()` 跳过所有人，对局永远开不了。重连分支（`add_player` 里
`return Ok(existing)`）也不会经过 `start_game`，所以没有补救路径。

**正确的做法：区分"未发骰"与"已淘汰"两种零骰状态。** 三个选项：

| 方案 | 表达 | 代价 |
|---|---|---|
| A. `dice: Option<Vec<i64>>` | `None` = 未发骰，`Some([])` = 已淘汰 | 每个读 `dice` 的地方都要处理 `None`；`dice.clear()` 变成 `dice = Some(vec![])` |
| B. 三态枚举 `Status { WaitingForDeal, Alive(usize), Eliminated }` | 状态显式 | `Alive` 里存骰数，与 `dice` 长度仍是双真相 |
| C. 保留 `alive: bool`，但**只由 `dice` 的变更路径写** | 私有字段 + 单一写入口 | 仍是两个字段，但违反了"可被违反"就变成"没人能绕过去" |

**倾向方案 C**：`alive` 改成私有，只允许 `eliminate()` / `start_game()` 通过方法
修改。它不消灭双真相，但消灭了"外部随手改"这个真正的危害——

```rust
// liars_dice.rs:1412  测试里就真的这样干过
engine.players[1].alive = false;
```

方案 C + `players` 私有化（下一条）能把这类越权写全部挡掉，而代价远小于 A/B。
这是个"够用"的选择，不是"最优雅"的选择——要写清楚它是权衡。

如果将来真要做到"一个真相"，方案 A 是对的，但那是独立一轮重构，不该塞进这个
issue。

### b. `players` 私有化

```rust
pub fn players(&self) -> &[Player]
```

测试里那几处直接改 `alive` 的地方改用公开方法（比如 `set_dice` 已有，
再加一个测试专用的出局构造器）。`#[doc(hidden)]` 的测试入口模式已在
`set_dice`/`set_current_turn`/`set_last_action` 上用过，沿用。

### c. `Call` 拆成枚举

```rust
pub enum HistoryEvent {
    Bid { player_idx: usize, quantity: i64, face: i64, message: String },
    Challenge { player_idx: usize, actual: i64, face: i64 },
    TurnTimeout { loser: usize },
}
```

消费端从 `.filter(is_bid)` 变成 `filter_map(|e| if let Bid { .. } = e)`，
类型系统保证"这里不该出现开盅记录"。

### d. 索引收敛

新增 `fn player_name(&self, idx: usize) -> &str`，替代 5 处 `map_or("?")`。
**这是最小改动**：不引入 `PlayerId` 新类型（那会扩散到整个 API 与 JSON 契约），
只是把重复的降级逻辑收成一处。

## 测试

| 用例 | 断言 |
|---|---|
| 新 `a_new_player_can_still_be_dealt_dice` | **最重要的一条**：`Player::new` 之后 `dice` 为空但玩家不该算淘汰；入场 → 满员 → `start_game` 后每人拿到 `dice_per_player` 颗。第一版规划的缺陷正是漏了这条 |
| 新 `players_cannot_be_mutated_from_outside` | 编译期（私有字段） |
| 新 `history_separates_bids_from_events` | 旧 `filter(is_bid)` 与新的 `filter_map(Bid)` 结果一致 |
| 改造 4 条 | L1372/1384/1404/1412 四条淘汰用例 |
| 回归 | 全量 527 绿；`full_game.rs` 固定种子跑到底 |

## 验收标准

- `cargo test --workspace` 全绿，测试数不减
- `grep "pub alive" crates/` 为空
- `grep 'quantity: 0' crates/` 为空
- 变异测试（`-p core`）仍 0 存活
- AGENTS.md 第 9 条（超时与开盅一致性）的看门用例仍绿——本改会碰
  `check_timeout` 的淘汰路径，那条约束不能破

## 已知边界

- **不改 JSON 契约**。`build_public_view()` 里 `"alive"` 字段仍要输出。**等待大厅
  阶段的语义要保住**：还没发骰的玩家现在输出 `alive: true`，改完之后必须仍是
  `true`（他是"在等开局"，不是"被淘汰"）。这条如果弄错，Agent 会在 `join` 的
  返回值里看到自己已经出局，行为会完全跑偏
- **不引入 `PlayerId` 新类型**（见设计 d）
- 1515 行的文件会动到主体，建议拆两个 commit：先 `alive` 派生 + `players`
  私有化（行为等价改写），再 `HistoryEvent` 枚举化（有结构变化）。第一个
  commit 后应能单独跑绿

## 不在本次范围

- `phase` 枚举化（009）
- 提示词渲染逻辑（`build_prompt` 里那 5 处 `map_or("?")` 属于本 issue 的 d 项，
  但提示词文案本身不动）

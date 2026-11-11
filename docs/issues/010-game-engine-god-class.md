# 010 GameEngine 的上帝类问题

| 项 | 值 |
|---|---|
| 来源 | 代码评审 6 条问题的第 2 条：「GameEngine 建模为啥这么复杂啊，有点上帝类的感觉」 |
| GitHub Issue | 待建 |
| 状态 | 规划中 |

---

## 一、问题陈述

`GameEngine` 是**一个** trait，19 个方法，承载四类互不相同的关注点：

| 类别 | 方法 | 谁关心 |
|---|---|---|
| 元数据 | `game_type` / `player_mode` | 注册表、路由 |
| 生命周期 | `phase` / `waiting_phase` / `finished_states` / `is_finished` | 大厅匹配、保留期回收 |
| 规则 | `handle_action` / `check_timeout` | 游戏本身 |
| 视图 | `public_view` / `admin_view` / `player_view` / `sensitive_keys` | 传输层、admin 页面 |
| 工具面 | `describe_tools` | MCP 适配层 |
| 提示词 | `prompts` / `render` | 提示词热重载 |
| 并发原语 | `signal` / `is_ready_for` | 等待循环 |
| 身份 | `find_player_by_token` / `player_tokens` | 断线重连、鉴权 |

一个**游戏规则引擎**同时要懂"怎么渲染提示词""怎么通知等待方""怎么按 token
找玩家"——后三件是框架的事，不是游戏规则。

## 二、实测数据

### trait 形状

| 项 | 数量 |
|---|---|
| 方法总数 | 19 |
| 必须实现（无默认体） | 6（`game_type` / `phase` / `public_view` / `player_view` / `signal` / `prompts`） |
| 有默认实现 | 13 |

### 两个引擎的实现量

| 引擎 | 实现方法数 | impl 块行数 | 文件总行数 |
|---|---|---|---|
| `liars_dice` | 16 / 19 | 327 | 2,209 |
| `guess_number` | 13 / 19 | 157 | 1,054 |

### `liars_dice` 的 impl 块内部构成

| 方法 | 行数 | 性质 |
|---|---|---|
| `describe_tools` | **103** | **工具面声明**——不是游戏规则 |
| `handle_action` | 55 | 规则 |
| `player_view` | 41 | 视图 |
| `check_timeout` | 37 | 规则 |
| `admin_view` | 29 | 视图 |
| `is_ready_for` | 9 | 并发 |
| `player_tokens` | 8 | 身份 |
| `public_view` | 4 | 视图（转调 `build_public_view`） |
| 其余 8 个 | 各 3 | 纯一行元数据 |

**`describe_tools` 一个方法占 103 行 / 327 行（31%）**，而它是 MCP 工具声明，
与游戏规则无关。

### 前置调研的结果（已核实）

| 问题 | 结果 |
|---|---|
| `liars_dice::describe_tools` 是否读实例字段？ | **不读**（103 行里零 `self.` 引用） |
| `guess_number::describe_tools` 多少行？ | 35 行，同样不读 `self.` |
| 有没有引擎需要运行时才确定的工具表？ | 没有（两个引擎都是纯静态） |

**另发现一处真实浪费**：`agent-game-mcp/src/tools.rs:36` 的 `specs_of()` 为了
拿工具列表，**构造了一个临时引擎**：

```rust
let engine = (meta.factory)(&CreateParams::default(), &Config::default()).ok()?;
Some(engine.describe_tools())
```

而 `all_tools_for()` 被 `agent-game-server/src/games.rs` 的**三个 HTTP 端点**
调用（L86 / L285 / L308）——**每个请求都构造一个用完即弃的引擎**，只为读一串
静态的 JSON Schema。

### 框架调用点分布

19 个方法**几乎全被框架调用**（引擎外的调用点）：

| 方法 | 引擎外调用点 |
|---|---|
| `handle_action` | 78 |
| `phase` | 38 |
| `player_view` | 34 |
| `check_timeout` | 27 |
| `is_finished` | 19 |
| `signal` | 15 |
| `public_view` | 15 |
| `render` | 12 |
| `prompts` | 12 |
| 其余 | 3–7 |

这否定了"某些方法是引擎自己用的、可以删掉"这个方向。**19 个都是接口的一部分。**

## 三、判断：哪些是真问题，哪些只是形式

### 真问题 1：`describe_tools` 占了 31% 的 impl 代码，且与规则无关

`liars_dice` 的 `describe_tools` 是 103 行 JSON Schema 字面量。它：

- 只被 `agent-game-mcp/src/tools.rs:43` 一处调用
- 内容与游戏规则完全无关（是"MCP 工具怎么声明"）
- 每次加一个 action 就要同步改这个 103 行的表

**这是真复杂度**：加一个动作要改两处（`handle_action` 的 match + `describe_tools`
的 schema），两处不同步就会出现"工具列表里有、执行时报未知动作"。

### 真问题 2：视图构造与规则状态耦合

`liars_dice` 里 `public_view` / `admin_view` / `player_view` / `build_prompt`
都直接读 `self.` 的字段。`#10` 刚把 `players`、`alive` 私有化，但视图代码仍在
同一个 `impl` 里，能直接摸到全部内部状态。

这不算错（同一模块内），但**"规则改了要跟着改视图"这件事没有边界**。

### 形式问题：方法数量本身

19 个方法听起来多，但实测**其中 8 个各只有 3 行**（一行元数据）。把它们拆到
另一个 trait 里，总数不变，只是分门别类。

**拆分的价值应该体现在"加一个新游戏要做的事变少"**，而不是"每个 trait 方法少"。

## 四、候选方案

### 方案 A：只把 `describe_tools` 挪出去（最小改动）

工具声明不该由引擎提供，改由**注册时声明**（与 `GameMeta` 同类）：

```rust
// registry.rs
pub struct GameMeta {
    ...
    pub tools: Vec<ToolSpec>,   // 注册时给一次，不是每次问引擎要
}
```

- 引擎的 `impl` 块少 103 行（`liars_dice` 从 327 → 224）
- 工具表与引擎实现**不在同一个文件里**，但这是好事：schema 是"对外契约"，
  应该与注册信息放一起
- 代价：`describe_tools` 要能读到运行时状态（现在它读 `self.` 吗？需要核实）

**待核实**：`liars_dice::describe_tools` 是否读了实例字段。如果读，就不能简单
挪到注册时。

### 方案 B：拆成三个 trait

```rust
trait GameMeta_ { game_type / player_mode }                    // 元数据
trait GameRules: GameMeta_ { phase / handle_action / ... }     // 规则
trait GameView: GameMeta_ { public_view / player_view / ... }  // 视图
trait GameRuntime { signal / is_ready_for / find_player_... }  // 并发与身份
trait GameEngine: GameRules + GameView + GameRuntime { }       // 组合
```

- `BoxedEngine` 仍是 `Box<dyn GameEngine>`，调用方**零改动**
- 但实现方要写 4 个 `impl` 块而不是 1 个——**样板变多**
- 必须实现的方法数从 6 变成 6（分散到 4 个 trait），**总数没少**

**这个方案没有解决真问题**，只是把 19 个方法分类。

### 方案 C：A + 把视图构造抽成独立函数

`public_view(engine) -> Value` 这类自由函数，读引擎的只读访问器。好处是视图
代码不能摸私有字段，坏处是只读访问器要够用（可能要加不少 `pub fn`）。

## 五、建议

**先做方案 A，不做方案 B。**

理由：

1. 方案 A 解决的是**真实且可量化**的两个问题：
   - `impl GameEngine` 块少 103 行（`liars_dice` 327 → ~224）
   - 消除"每 HTTP 请求构造一个临时引擎只为读静态 schema"的浪费
2. 方案 B 是形式重组。实测：8 个方法各只有 3 行，拆到别的 trait 里总数不变，
   必须实现的方法数仍是 6 个——**不让"加游戏"变简单**
3. `#10`、`#11` 刚动过这个 trait，方案 B 的影响面覆盖两个引擎的全部 impl 块，
   风险显著高于前四个 issue

**方案 A 的具体形状**：

```rust
// registry.rs
pub struct GameMeta {
    pub game_type: &'static str,
    pub display_name: &'static str,
    pub player_mode: PlayerMode,
    pub waiting_phase: &'static str,
    pub tools: Vec<ToolSpec>,        // 新增：注册时声明一次
    pub factory: Arc<dyn Fn(&CreateParams, &Config) -> Result<BoxedEngine, String> + Send + Sync>,
}
```

`GameMeta::new()` 默认空表，加 `with_tools()` 构建器。两个引擎把
`describe_tools` 的 103 + 35 行搬到各自的注册处。

`specs_of()` 变成 `registry.get(game_type).map(|m| m.tools.clone())`——
**不再构造引擎**。

需要留意的兼容性：`GameEngine::describe_tools()` 现在是 `pub` trait 方法，
`api_paths.rs` 可能探它。删除属于源兼容性破坏（与 006 同类），要写进提交信息。

**前置调研**（动手前必须做）：

- [x] `liars_dice::describe_tools` 是否读实例字段？→ **不读**
- [x] `guess_number::describe_tools` 多少行？→ **35 行**
- [x] 有没有引擎需要运行时才确定的工具列表？→ **没有**
- [ ] 把工具声明挪到 `GameMeta` 后，`mcp/src/tools.rs` 的改动量？→ 待动手时算

**结论：方案 A 可行。** 工具表是纯静态数据，挪到注册元数据即可，顺带消掉
"每请求构造一个临时引擎"的浪费。

## 六、验收标准

- `liars_dice` 的 `impl GameEngine` 块从 327 行降到 ~220 行
- 加一个 action 不再需要改两处（工具 schema 与 match 分支）
- 557 条测试全绿
- release 产物不受影响（`DummyEngine` 仍在门控后面）

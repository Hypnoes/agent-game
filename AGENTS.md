# AGENTS.md

改动本仓库时必须遵守的硬约束。每一条都来自实测中踩过的坑，不是风格偏好。

Rust 重写（见 `docs/issues/004-riir.md`）保留了全部旧约束的**意图**，但多数约束
的形态变了：旧版靠文档纪律与 `except Exception` 兜住的东西，现在由类型与结构表达。
每条都标了"旧版怎么守"和"现在怎么守"，以及看门用例在哪个文件。

---

## 1. `/mcp` 不能重定向到 `/mcp/`

MCP 的 Streamable HTTP 客户端不会跟随 307。旧版用 `Mount("/mcp", ...)` 会 307 到
`/mcp/`，裸 HTTP 客户端直接失败；旧版因此强制用 `Route` + `session_manager.asgi_app`。

**现在**：`axum::routing::any_service(StreamableHttpService)` 挂在 `/mcp` 上，
`nest_service` 不加尾斜杠重定向。

**看门用例**：`crates/agent-game-server/tests/mcp_transport.rs::initialize_returns_a_session_id_without_redirecting`
（真实 `POST initialize`，断言 200 且带 `Mcp-Session-Id`）。

---

## 2. 消费端超时必须大于服务端的等待预算

服务端预算 55 秒（`Config::block_budget`），README 里配的 MCP 客户端超时是 70 秒。
反过来的话客户端会在服务端返回之前先超时，Agent 看到的是传输层错误而不是对局状态。

**看门用例**：`crates/agent-game-infra/src/config.rs::client_timeout_leaves_room_for_the_server`
（断言 70 > 55 且余量 ≥ 10）。

---

## 3. 身份来自 `player_token`，`Mcp-Session-Id` 只是连接缓存键

`Mcp-Session-Id` 描述连接，不描述玩家。带 `player_token` 的请求先在所有活动对局里
按 token 查找，找到即复用；找不到才走匹配/新建。

违反这条的后果是可复现的：每个 MCP 连接各持一局，两名玩家被分进两局。

**看门用例**：
- `mcp_transport.rs::the_same_token_returns_to_the_same_game_across_connections`
- `crates/agent-game-server/src/state.rs::two_states_do_not_share_sessions`

新增多人游戏时，引擎必须实现 `find_player_by_token()`——身份复用依赖它。

---

## 4. 多人引擎的 `bid` / `challenge` / `state` 必须强制要求 `player_token`

`player_token` 在这些动作上是必填项，不是可选的加固。缺 token 直接拒绝。

**症状（曾经真实存在）**：缺 token 被当成"跳过校验"，于是任何人传一个
`player_index` 就能读到对手的骰子，还能替对手叫数。大话骰的全部玩法建立在私有
信息上，这条一松，游戏就没有意义了。

**现在**：校验在 `infra::identity::check_player_identity()`，REST 与 MCP 共用。
多人引擎通过 `player_tokens()` 暴露 token 列表，`PlayerMode::Multi` 决定是否强制。

只有 `join` 允许不带 token（新玩家入场）。

**看门用例**：
- `crates/agent-game-core/src/games/liars_dice.rs::actions_require_player_token`
- `crates/agent-game-server/tests/http_contract.rs::the_player_view_needs_the_token_header`

---

## 5. 校验必须在等待之前完成

`state` 分支的顺序是固定的：解析入参 → 校验玩家身份 → 才进入等待。

**症状**：校验挪到等待之后，一个越界的 `player_index` 或错误的 `player_token`
会先白等满整个预算（55 秒），然后在 Agent 以为"对手在思考"之后才报错。

旧版只有 REST 分支遵守这条，MCP 分支相反——同一个错 token 的调用，一条通道立刻
报错，另一条白等 55 秒。

**看门用例**：
- `http_contract.rs::a_bad_token_fails_fast_instead_of_waiting`
- `mcp_transport.rs::a_bad_token_fails_fast_over_mcp`
  （都断言 < 500ms，远小于一个预算）

---

## 6. 等待唤醒不等于轮到我，必须重判

3 人以上时任何一名玩家叫数都会广播。醒来就返回会让 Agent 以为自己可以行动，
于是发出一个必然被 `check_turn` 拒绝的动作。

`infra::wait::wait_for` 是唯一实现：每轮先判条件、再等剩余预算，被唤醒只是回到
循环顶部。引擎只需提供 `is_ready_for()`。

**顺序也必须固定**：先订阅、再判条件、再等。反过来会让"检查"与"订阅"之间发生的
变更丢掉唤醒，白等满整个预算。

**看门用例**：
- `crates/agent-game-infra/src/wait.rs::a_spurious_wakeup_keeps_waiting`
- `crates/agent-game-infra/src/wait.rs::no_lost_wakeup_when_notified_before_the_first_check`
- `crates/agent-game-infra/src/signal.rs::no_lost_wakeup_between_subscribe_and_wait`

---

## 7. `timeout` 解析只有一份实现：`infra::budget::resolve_budget`

| 传入 | 语义 |
|---|---|
| 省略 / `null` | 用满当前 `Config::block_budget` |
| 数值 | 夹到 `[0, cap]`；`0` 表示**本次不等待**，立即返回 |
| `bool` / 字符串 / `NaN` / `inf` | 错误 |

**症状**：两条传输曾经各有各的写法，语义正好相反——MCP 侧
`float(args.pop("timeout", None) or BLOCK_BUDGET)` 让 `timeout=0` 被 `or` 吞成 55 秒；
REST 侧 `min(float(...), BLOCK_BUDGET)` 让 `0` 就是 `0`。同一个客户端在两条通道上
等到的时长不一样。

**现在**：解析器只有一个公开入口，其它地方拿到的是 `f64` 预算而不是原始 JSON，
写不出第二份解析。

**看门用例**：`crates/agent-game-infra/src/budget.rs` 全部用例，加
`http_contract.rs::a_non_numeric_timeout_is_rejected` 与
`mcp_transport.rs::timeout_zero_does_not_block_over_mcp`。

---

## 8. 秘密信息靠**构造**，不靠剥离

`public_view()` 与 `admin_view()` 由引擎分别构造。运行时没有"按名字删键"这一步。

**症状**：旧版是 `{k: v for k, v in state.items() if k not in secrets}`。秘密名单
拼错一个字母（`all_dices`），秘密原样漏给 Agent，编译期与运行期都不会有任何反应。

只有 `/api/v1/admin/*` 能看到秘密。新增带秘密字段的引擎必须同时写 `public_view()`，
并在 `sensitive_keys()` 里声明那些字段——后者是**测试的期望清单**，生产路径不读它。

**看门用例**：`crates/agent-game-server/tests/invariants.rs::every_registered_engine_keeps_its_secrets_out_of_the_public_view`
（遍历注册表里**每一个**引擎）。

---

## 9. 回合超时与开盅对"输掉本轮"的处理必须一致

`check_timeout` 的回合超时分支与 `challenge` 都必须 `self.current_call = None`。

**症状（曾经真实存在）**：超时分支只掉骰、换回合，不清本轮叫数。下家面对一个
无法再加码的僵尸叫数（数量可能已超过掉骰后的总数），既不能加码也不能弃权，
只能替别人的超时开盅——胜负由别人的超时决定。3 人局可稳定复现。

**看门用例**：
- `crates/agent-game-core/src/games/liars_dice.rs::turn_timeout_clears_the_round_bid`
- `invariants.rs::a_timeout_clears_the_round_bid_like_a_challenge_does`
- `crates/agent-game-core/tests/full_game.rs::a_timeout_in_the_middle_does_not_lock_the_next_player`

---

## 10. `check_timeouts` 必须按引擎隔离异常

per-session 循环必须隔离错误，记日志后继续下一个。

**症状**：一个引擎的 `check_timeout` 抛异常会中断整轮结算，其余对局不再超时。
REST 的 `state` 分支在请求内直接调用 `check_timeouts()`，所以一个坏引擎能把
**无关对局**打成 500。

**现在**：`check_timeout` 返回 `Result<Option<TimeoutEvent>, EngineError>`，
`Err` 分支只记 `warn!` 并继续。类型表达了"这里可能失败且必须隔离"。

**看门用例**：`crates/agent-game-domain/src/supervisor.rs::a_broken_engine_does_not_stop_the_others`。

---

## 11. Admin token 永远存在，且绝不交给未出示它的人

token 在启动时解析一次：有 `AGENT_GAME_ADMIN_TOKEN` 就用它，没有就随机生成。
没有"不设 token 就不校验"这个分支。

`GET /admin` 只在请求方**已经出示**正确 token 时才把它注入页面；两者都没有或都不对，
仍返回页面骨架，但页面发起的 admin API 调用会 403。

**症状**：把 token 注入给匿名访问者，等于把 `/api/v1/admin/games` 的全部秘密信息对
任何能连上端口的人敞开；反过来退化成"没配 token 就不校验"更糟，那连"配置错了"都
不会有人发现。

比较按 UTF-8 字节做恒定时间比较（`subtle::ConstantTimeEq`）。旧版专门处理过非 ASCII
的 `TypeError`——Rust 里不存在编码问题，token 就是字节。

`/api/v1/admin/ws` 同样要鉴权：浏览器 WebSocket 不能设自定义头，token 走 `?token=`，
不匹配返回 403 / `close(1008)`。

**看门用例**：
- `http_contract.rs::every_admin_endpoint_requires_a_token`
- `mcp_transport.rs::the_admin_websocket_rejects_missing_and_wrong_tokens`
- `crates/agent-game-server/src/state.rs::token_comparison_is_exact`

---

## 12. 身份校验必须与传输无关，且在等待之前

校验逻辑在 `infra::identity::check_player_identity()`。REST 与 MCP 共用同一份判断，
两条传输各写一个**翻译函数**（`ApiError::from_engine` 与 `engine_error_text`），
但分类只在 `EngineError::is_client_error()` 里定义。

**症状**：两条传输各自实现一遍时语义分叉了——REST 先校验再等待，MCP 先等待再校验。

**看门用例**：`http_contract.rs` 与 `mcp_transport.rs` 里成对出现的 fast-fail 用例。

---

## 13. 测试不准用 `skip` 给"能力缺失"让路

核心能力被删掉时套件必须红。

Rust 里没有 `skip` 这个概念（`#[ignore]` 只用于"慢"，不用于"能力缺失"），所以这条
从"禁止某种写法"变成"不要用 `#[ignore]` 掩盖失败"。CI 里跑 `cargo test` 时
`#[ignore]` 的用例不执行——把能力验收用例标上它，等于删掉它。

**看门**：CI 不传 `--ignored`，且 `crates/agent-game-server/tests/` 下没有任何
`#[ignore]`。

---

## 14. 每条约束都要有对应的看门用例

约束写在这份文档里、如果没有任何用例对应它，它实际上只是一段注释。

**看门**：本文档每一条都显式写了"看门用例"路径。新增约束时同步补用例与这一行，
而不是只加一段说明。

---

## 15. 规则用"边界"测，不用"能跑"测

第一轮变异测试在 `core` 上跑出 144 个存活变异体，其中绝大多数是同一个原因：
**既有用例只验证"这条路径能跑通"，没有验证"差一个单位时结果应该不同"。**

具体形态：

| 变异 | 为什么最初的套件抓不到 |
|---|---|
| `>` 改成 `>=`（超时判定） | 用例只测"很早就超时"与"完全没超时"，没测恰好等于时限 |
| `*` 改成 `+`（跨 1 的叫数换算） | 用例只测"明显够"与"明显不够"，没测下界本身 |
| `public_view` 返回 `Default` | 用例只断言"秘密不在里面"，没断言"别的东西在里面" |
| `session_ids` 返回 `vec![]` | 用例只测"新建之后能找到"，没测空表返回空 |
| `signal()` 返回新 `Arc` | 用例只测"通知能唤醒"，没测两次取到的是不是同一份 |

因此补了三类用例，它们现在都在 `crates/agent-game-core/tests/` 下：

| 文件 | 守什么 |
|---|---|
| `boundaries.rs` | 每个比较符的两侧：下界成立、下界外不成立 |
| `params.rs` | 参数访问器的契约：缺键、`null`、`bool`、浮点、覆盖、不变的副本 |
| `routing.rs` | 管理器访问器返回真实那份、身份复用的类型匹配、保留期差一个单位 |

**新增规则时的自检**：写下 assert 之后，问一遍"把对应的比较符改一个单位，这条会红吗"。
不会红，就还没测到位。

---

## 16. 分层边界必须由 crate 强制，不用源码扫描

五个 crate，依赖只能向上：

| crate | 放什么 | 不许依赖 |
|---|---|---|
| `agent-game-server-infra` | 预算、配置、错误、信号、等待、提示词、身份、参数解析 | `domain`、`core` |
| `agent-game-server-domain` | 引擎协议（`GameAbstraction`）、大厅匹配（`Lobby`）、会话生命周期、注册表、测试替身 | `core` |
| `agent-game-server-core` | 具体规则（`GameImpl`：大话骰、猜数字） | — |
| `agent-game-server-mcp` / `-server` | 传输层 | 只依赖上面 |

每条反向依赖都会让 cargo 在循环依赖检查处失败：

```
error: cyclic package dependency: package `agent-game-server-core` depends on itself
```

**这条约束曾经用源码扫描守过，四代都被绕过。** 每一代都合法可编译却漏检：

| 代 | 写法 | 为什么扫描器漏 |
|---|---|---|
| 1 | `pub(crate) use crate::domain::X;` | 不以 `use ` 开头 |
| 1 | `use crate::domain as dom;` | 别名，不是 `::` 后缀 |
| 1 | `use crate::{domain::X};` | 分组导入 |
| 2 | `pub(in crate::infra) use crate::domain::X;` | 可见性修饰更复杂 |
| 2 | `use crate::{infra::X, domain};` | 分组里的裸模块项 |
| 3 | `use super::super::domain::game::X;` | 相对路径，不含 `crate::` |
| 3 | `use crate :: domain :: game :: X;` | token 间空格 |
| 3 | `fn f(_: crate::domain::game::X) {}` | 根本不是 `use` |
| 4 | `use crate as root; use root::games::X;` | 别名后引用，前缀完全不匹配 |

第四代是决定性的：**只要还能用任意别名到达上层，任何文本前缀检查都有下一代替换。**
字符串匹配无法穷尽一种语言的模块引用语法。

**现在的结论**：`infra`、`domain`、`core` 各自独立成 crate，方向由编译器执行，
不存在"某种写法绕过"的余地。`use crate as root` 在 `domain` crate 里只能到达
`domain` 自己，`root::games` 根本不存在，编译期报
`cannot find 'games' in 'root'`。

**看门用例**（`crates/agent-game-core/tests/layering.rs`）：
- `infra_does_not_depend_on_domain` / `infra_does_not_depend_on_core` /
  `domain_does_not_depend_on_core`——断言 manifest 里没有反向依赖
- `the_dependency_chain_is_core_to_domain_to_infra`——正向断言，防止
  "拆开但不建立依赖"也能全绿
- `cargo_really_rejects_a_cyclic_dependency`——**真的调用 cargo**：在临时目录
  搭一个两个 crate 互相依赖的最小 workspace，断言 cargo 报 `cyclic`。前四条只读
  文本，这一条证明约束是构建系统执行的，不是我们在测试里模拟的。
  （第一版直接改仓库的 `infra/Cargo.toml`、跑完恢复，CI 上红——nextest 每个测试
  跑在独立进程里，另一个用例读到了注入的中间状态。这是测试竞态，不是产品缺陷。）

**新增规则时的自检**：往 `crates/agent-game-infra/Cargo.toml` 的
`[dependencies]` 加一行 `agent-game-server-domain = { path = "../agent-game-domain" }`，
`cargo build -p agent-game-server-infra` 必须报 `cyclic package dependency`。

---

## 17. 目录重排不许破坏公开导入路径

重排是"纯内部"的改动，但编译期只在**调用方**那里才报错。core 自己的测试会跟着
改，所以套件全绿也可能已经把外部使用者打断——这正是本 PR 第一版真实犯的错：
把 `manager.rs` 拆成四个文件后，`core::manager::{GameManager, Session, ...}`
整条路径消失了，而 518 条测试全绿，因为没有任何用例从外部位置 import 它。

**现在**：`lib.rs` 用 `pub mod <旧名> { pub use <新位置>::*; }` 保留全部 12 个
历史模块路径，并显式转发通配带不到的类型（`ActionParams` / `CreateParams` 曾是
`engine` 下的公开名字，但它们是 `infra::params` 的类型，`pub use domain::game::*`
带不出来）。

**看门用例**：`crates/agent-game-core/tests/api_paths.rs`
- `every_public_module_path_still_resolves`——12 个模块路径与各模块内的历史
  公开符号逐个 import
- `nested_public_symbols_still_resolve`——子模块路径上的符号
- `version_is_still_exported`——`VERSION` 不在任何模块里，容易被漏
- `top_level_reexports_still_resolve`——顶层 `pub use` 的名字逐个点名

对照重排前的 51 个公开符号，探 50 个。差的那个是
`games::state_machine::Transitions`——死代码，已在 **006（#8）** 里删除。
同一轮还删了 `GameEngine::setup_params()`：生产路径零调用，`Session.setup`
才是大厅匹配真正消费的那份配置。

它不测行为，测的是"路径还在"。删掉 `manager` 兼容模块后，这个文件**编译失败**，
而不是行为断言失败。

**新增规则时的自检**：删掉 `lib.rs` 里任意一个兼容模块，`cargo test -p
agent-game-server-core --test api_paths` 必须编译失败。

---

## 18. 删代码前先证明它真没人用

006（#8）删掉了两处「看起来有用、实际零调用」的代码，方法值得复用。

**证据不能是"grep 没搜到"**，要三层：

1. **调用点**：`grep -rnE` 搜符号名。注意 `-E`——POSIX `grep` 默认 BRE，
   `"A|B"` 里的 `|` 是普通字符，返回空，看起来像"什么都没找到"（006 的规划
   第一版就踩了这个坑）
2. **`dead_code` 警告**：删掉唯一的读者之后，`cargo build` 会报
   `field 'setup' is never read`。这条比 grep 硬——它是编译器说的
3. **删除后的编译**：删 trait 方法会让所有 impl 少一个方法。workspace 内如果
   真有人用，编译期就红

`setup_params()` 的案例说明第 2 层为什么必要：删掉它之后，编译器立刻指出
`LiarsDiceEngine::setup` 与 `GuessNumberEngine::setup` 两个字段从未被读取。
**那个方法就是它们唯一的读者**——只靠 grep 看不出这层关系，会留下两个
僵尸字段。

**新增规则时的自检**：删任何 `pub` 项前，先跑一次 `cargo build` 看 dead_code，
确认没有"被它养活的字段 / 函数"还留着。

---

## 19. 测试替身不许进发布产物

`domain::testing::DummyEngine` 的 `admin_view()` 返回 `{"phase": ..., "secret": "admin-only"}`。
这串假"秘密"如果出现在发布产物里，说明整个模块没被裁掉。

**007（#9）之前它就是无条件编译的。** 模块注释写着"只在 `cfg(test)` 与
`feature = "testing"` 下编译"，但全 workspace `grep "cfg(feature"` 零命中——
那个 cfg 从没存在过。实测 `libagent_game_server_domain.rlib`（646,090 字节）
里能串扫到 `DummyEngine` 与 `admin-only`。

**现在**，四处缺一不可：

| # | 位置 | 做法 |
|---|---|---|
| 1 | `domain/src/lib.rs` | `#[cfg(any(test, feature = "testing"))] pub mod testing;` |
| 2 | `core/src/lib.rs` | 兼容模块 `pub mod testing` 同样加 cfg——**domain 门控了不代表 core 的再导出也门控** |
| 3 | `core/Cargo.toml` | `testing = ["agent-game-server-domain/testing"]`——必须**转发**，空 `testing = []` 什么也不做 |
| 4 | 各 crate 的 `[dependencies]` | **不许**开 `testing`。workspace 的 feature 是 union 的，一处漏开会把替身拖进 release |

第 4 条最隐蔽：`server` 曾经在**生产** `[dependencies]` 里写了
`features = ["testing"]`。它单独看"只是多编译一个模块"，但 feature union 会让
整条链都打开。

**集成测试怎么拿到替身**：`crates/*/tests/*.rs` 是独立 crate，`cfg(test)` 对它们
不生效。用自引用 dev-dependency：

```toml
[dev-dependencies]
agent-game-server-core = { path = ".", features = ["testing"] }
```

**看门用例**：
- `crates/agent-game-core/tests/testing_gate.rs`——五条：domain 门控存在、
  core 再导出门控存在、core feature 是转发不是空、生产依赖无 `testing`、
  反向验证（喂一个明确违规的段必须抓到）
- `.github/workflows/ci.yml` 的「产物里无测试替身」——真的构建 release、
  串扫每个 rlib

两道防线各管一段：源码用例跑得快、改错了立刻红；CI 那条慢，但测的是最终事实。

**两个不能犯的错**：

- **不要加 `--all-features`**：那会主动打开 `testing`，自己把替身塞回去
  （实测 643,286 字节的 rlib，`DummyEngine` 在）
- **不要只构建单个 crate**：`-p core` 漏掉 domain，`-p domain` 漏掉经过 core
  再导出的路径。必须 `--workspace`

**新增规则时的自检**：往 `crates/agent-game-server/Cargo.toml` 的
`[dependencies]` 加一行 `agent-game-server-core = { workspace = true, features = ["testing"] }`，
`cargo test -p agent-game-server-core --test testing_gate` 必须红。

### 验产物必须用全新的 target 目录

这条来自 009（#11）一次真实的误判：我当时在同一个 `target/` 里反复做手工变异
实验（改源文件 → 构建 → 恢复），扫到 `DummyEngine` 就断定"新代码导致泄露"，
把一项能力从 PR 里回退了，还写进了 issue。

后来用干净目录复核：**那些改动全部干净**。当初扫到的是**上一次包含
`testing` feature 的旧 rlib**——`cargo clean -p <crate>` 不保证把它删掉重编。

正确做法：

```bash
CARGO_TARGET_DIR=$(mktemp -d) cargo build --release --workspace
strings "$CARGO_TARGET_DIR"/release/libagent_game*.rlib | grep -E 'DummyEngine|admin-only'
```

或者 PowerShell：

```powershell
$env:CARGO_TARGET_DIR = "D:\tmp\target-fresh"
Remove-Item -Recurse -Force $env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
cargo build --release --workspace
Get-ChildItem "$env:CARGO_TARGET_DIR\release\libagent_game*.rlib" | ForEach-Object {
  $t = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($_.FullName))
  if ($t -match 'DummyEngine|admin-only') { "LEAK: $($_.Name)" }
}
```

**判据**：产物类问题只在全新 target 目录里下结论。复用做过滤镜实验的目录时，
任何"泄露"都先怀疑是旧产物。

---

## 附：写作与编码约定

| 项 | 约定 |
|---|---|
| 语言 | 注释、文档、错误消息、提示词一律中文 |
| `unsafe` | workspace 级 `forbid` |
| `unwrap` / `expect` | 非测试代码禁止（`clippy.toml` 里对测试放行） |
| 行宽 | 交给 `rustfmt`；中文注释按显示宽度算会误报，因此不设 `max_width` |
| 错误 | 请求路径上的所有失败都走 `Result`，不 `panic` |
| 时间 | 代码里用 `Instant`；只有跨进程的展示值才转成毫秒 |
| 测试替身 | `domain::testing::DummyEngine`，门控在 `feature = "testing"` 后面。**默认不编译**；各 crate 的测试通过自己的 `[dev-dependencies]` 打开它。见第 19 条 |

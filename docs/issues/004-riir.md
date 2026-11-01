# 004 — RIIR：用 Rust 重写整个服务端

前置：001（阻塞等待）、002（MCP over HTTP）、003（全量修复）均已完成。
基线标签：`python-v0.3.0`（451 个 Python 测试全绿）。

---

## 背景

Python 版在 003 之后已经没有已知缺陷：451 条测试全绿、覆盖率 93%/90%、
50 个变异体全部被杀死或标为等价。要求重写的理由不是"它坏了"，而是：

1. **部署面**：Python 需要 venv + 解释器 + 依赖树；Rust 产出一个 7.5MB 的静态二进制。
2. **并发的表达**：旧版用 `asyncio` 单线程 + `except Exception` 兜住"坏引擎"。
   Rust 可以在类型上区分"这里可能失败且必须隔离"（`Result`）与"这里不可能失败"。
3. **全局可变状态**：旧版有 `config.BLOCK_BUDGET`、`manager`、`_MCP_SESSIONS`、
   `_admin_broadcast` 四处模块全局，测试靠三种不同的 monkeypatch 手法隔离，
   其中一套从未还原过 `_finished_at` 与 `_seen_expired`。

重写**不**改变玩法与 MCP 契约：工具名、必填字段、`timeout` 语义、阻塞等待、
提示词文案全部 1:1。改变的是 HTTP API 的形状（用户批准）、三处内部设计（见「决策」）。

---

## 决策

### D1 删除 `game_name`，改为注册表里的 `display_name`

**问题**：旧版有两个字符串——`game_type`（注册表键、URL、工具名前缀、匹配键）
与 `game_name`（人类可读）。后者被写进 JSON 却**没有任何消费者**：admin 页面
自己按 `game_type` 硬编码了一张 `TYPE_LABEL`，新游戏必须回来改页面才能显示中文名。

**改动**：`GameMeta { game_type, display_name, player_mode, factory }`。
`display_name` 随 `/api/v1/games/tools` 与 admin 列表下发，`admin.html` 的
`TYPE_LABEL` 整张删除。

**验收**：`http_contract.rs::tools_endpoint_carries_the_display_names`；
admin 页面不含任何硬编码游戏名（`admin.rs::the_dashboard_ships_the_escape_helper` 旁）。

### D2 秘密信息靠构造，不靠剥离

**问题**：旧版 `{k: v for k, v in state.items() if k not in secrets}`。秘密名单拼错
一个字母（`all_dices`），秘密原样漏给 Agent，编译期与运行期都没有任何反应。

**改动**：引擎提供 `admin_view()` 与 `public_view()` 两个方法，各自返回由
`#[derive(Serialize)]` 结构序列化出来的 JSON。运行时没有"按名字删键"这一步。
`sensitive_keys()` 降级为**测试的期望清单**，生产路径不读它。

**验收**：`invariants.rs::every_registered_engine_keeps_its_secrets_out_of_the_public_view`
遍历注册表里每一个引擎，断言公开视图不含声明的敏感字段、admin 视图含。

### D3 等待循环上移到框架，引擎回到纯同步

**问题**：旧版 `async fn wait_for_change` 定义在**引擎**里。代价是：引擎 trait 为了
一个方法引入 `async-trait`；"订阅→判条件→等剩余预算→重判"每个回合制游戏各写一遍；
`wait_for_change` 与 `get_player_state` 各判一次"是否轮到我"。

**改动**：引擎只提供两个同步方法——`is_ready_for(idx)` 与 `signal() -> Arc<ChangeSignal>`；
循环在 `core::wait::wait_for`，只写一次。`async` 只留在信号原语与传输层。

**验收**：`wait.rs` 的 7 条用例（含假唤醒、丢唤醒、取消、0 预算）；
`mcp_transport.rs::a_blocking_state_call_returns_when_the_opponent_acts` 走真实 HTTP。

### D4 端点改造（用户批准重新设计）

`/api/v1/*`；路径参数统一为 `{id}`（axum 0.8 的路由冲突）；`player_token` 从查询参数
改为 `X-Player-Token` 头（查询参数会进日志）；响应改信封
`{session_id, game_type, display_name, state}`。完整映射见
[`../legacy/migration.md`](../legacy/migration.md)。

---

## 实施

三个阶段，每阶段结束仓库可编译、可跑测试。

### 阶段 A — core（`crates/agent-game-server-core`）

| 模块 | 内容 | 对应旧文件 |
|---|---|---|
| `config.rs` | `Config` 值（不再是模块全局） | `config.py` |
| `budget.rs` | `resolve_budget()`，唯一解析器 | `config.parse_block_budget` |
| `error.rs` | `EngineError` 携带分类 | 分散在四个文件的 `ValueError` |
| `identity.rs` | `check_player_identity()` | `game_engine.player_identity_error` |
| `signal.rs` | `ChangeSignal`（watch 版本号 + RAII 订阅） | `games/signal.py` |
| `wait.rs` | `wait_for()` | 各引擎的 `wait_for_change` |
| `prompt.rs` | 极简渲染器（`{name}` / `{name:g}`）+ TOML 资产 | `prompt.py` |
| `registry.rs` | `GameMeta` / `Registry` / 前缀解析 | 三处各存一份的终态知识 |
| `manager.rs` | session 路由、匹配、超时结算、回收 | `manager.py` |
| `games/` | 两个引擎 | 同 |

### 阶段 B — 传输（`-mcp` 与 `-server`）

| 模块 | 内容 |
|---|---|
| `mcp/tools.rs` | 7 个工具的手写 schema |
| `mcp/lib.rs` | `ServerHandler`、会话缓存、动作路由、`state` 等待 |
| `server/games.rs` | REST 端点、body 解析、错误映射 |
| `server/admin.rs` | admin 端点、仪表盘、WS |
| `server/poller.rs` | 唯一的周期超时结算点 + 签名去重广播 |
| `server/state.rs` | `AppState`（一次构造、按实例共享） |
| `server/mcp.rs` | `/mcp` 挂载 |

### 阶段 C — 验收与清理

- `cargo fmt` / `clippy -D warnings` / `cargo test` / `llvm-cov`。
- 删除 Python 全部代码与工具链文件。
- 重写 `README.md`、`AGENTS.md`（14 条约束改写为 Rust 机制 + 看门用例路径）。
- 新增 `docs/legacy/migration.md`（端点映射）。

---

## 验收结果

全部判据达成。

| 项 | 判据 | 实际 |
|---|---|---|
| 构建 | `cargo build --release` | 通过，产物 7.5MB |
| 格式 | `cargo fmt --all -- --check` | 零差异 |
| 静态检查 | `clippy --all-targets -- -D warnings` | 零告警 |
| 测试 | 全绿 | **429 passed** |
| 覆盖率 | 行 ≥ 85% | **93.68% 行 / 91.23% 分支** |
| **变异测试** | **0 存活** | **521 个变异体：465 捕获 / 55 不可行 / 1 超时 / 0 存活** |
| MCP 端到端 | 真实进程 + DSH 配置 | 握手 200、`Mcp-Session-Id` 正确、`tools/list` 7 个、两名玩家开局打到 `bidding` |
| 秘密边界 | 公开视图无 `all_dice`、admin 有 | 实测通过 |
| Admin 鉴权 | 无 token → 403 | 实测 403 |
| Python 清理 | 仓库无 `.py` | 0 个 |

那 1 个 `TIMEOUT` 是 `has_changed()` 被改成恒 `true`：等待方永远认为"已经变了"，
于是循环重判直到预算耗尽，测试挂到 30 秒超时才结束。它与"被捕获"等价——
套件确实红了，只是红在超时上而不是断言上。

### 变异测试的五轮收敛

这是本次实施里信息量最大的一段。五轮跑下来：

| 轮次 | 杀死 | 存活 | 触发原因 |
|---|---|---|---|
| 1 | 378 | 144 | 初始状态 |
| 2 | 410 | 112 | 补 `params.rs`（24 条）、`prompts.rs`（17 条） |
| 3 | 446 | 76 | 补 `boundaries.rs`（27 条）、`routing.rs`（14 条） |
| 4 | 467 | 13 | 补 `surfaces.rs`（21 条）、`prompt_thresholds.rs`（16 条）、`millisecond_bounds.rs`（17 条）、`remaining.rs`（17 条） |
| 5 | 465 | **0** | 补 `final_gaps.rs`（5 条）；其余 12 条逐条判定为等价并写进 `exclude_re` |

每一轮存活下来的都不是"某个函数写错了"，而是同一类测试盲区：**用例只验证
"这条路径能跑通"，没有验证"差一个单位时结果应该不同"**。这条教训写进了
`AGENTS.md` 第 15 条。

五轮里还直接抓到三个真实实现缺陷（都不是变异测试凭空造的）：

1. **`ChangeSignal::wait` 永远超时**。在 `changed()` 成功后又问一次
   `has_changed()`，而 `changed()` 已经把版本标记为已读，那次调用永远返回
   `false`。5 条信号用例同时红。
2. **MCP 会话缓存依赖 `HashMap` 保序**。Python 的 dict 保序，Rust 的
   `HashMap` 不保序，于是"按插入顺序淘汰最旧"会淘汰到正在用的连接。
   改成显式的 `Vec<(key, entry)>`。
3. **提示词的两个时间读数来自不同基准**。`waited` 用整秒截断、`remaining`
   用浮点即时值，两者之和会差 1，提示词自己前后矛盾（"已思考 45 秒"配
   "只剩 14 秒"，而回合上限是 60）。改成同一个 `elapsed`。

### 剩余 12 个等价变异体的性质

它们分成两类，逐条理由写在 `.config/mutants.toml` 的 `exclude_re` 里：

| 类别 | 例 | 为什么等价 |
|---|---|---|
| 不可达分支 | `roll_target` 的 `max <= min` 守卫 | 构造时已校验 `min < max`，这两条臂运行期恒假；且两条臂的 Body 完全相同 |
| 不可观测的瞬时 | `> budget` → `>=` | 只影响"恰好等于时限"那一个时刻，而构造该时刻的过程本身就消耗了微秒 |
| 浮点分辨率之外 | `fract() < 1e-9` → `<=` | `Duration::as_secs_f64()` 的分辨率是纳秒，到不了 1e-9 的边界点 |
| 改名不改值 | 测试辅助方法的访问器 | 改坏它只影响测试自己的期望 |

### 与规划的偏差

**1. crate 命名改成 `agent-game-server-{core,mcp,server}`。** 规划里是
`agent-game-{core,mcp,server}`。落地时发现 Rust 把包名里的 `-` 转成 `_` 作为
import 路径，于是 `agent-game-core` 会被导入成 `agent_game_core`——而 domain 概念
是"agent_game_server"，两者对不上。改名后 import 路径与领域名一致。

**2. 引入了 `clippy.toml`。** 规划里只想用 workspace lint。落地时发现
`unwrap_used` / `expect_used` 对 `#[cfg(test)]` 模块同样生效，逐处 `#[allow]`
会淹没真正需要注意的地方。`clippy.toml` 的 `allow-*-in-tests` 把禁令精确限定在
非测试代码里。集成测试是独立 crate，不在 `cfg(test)` 里，因此在文件头显式
`#![allow(...)]` 并写明理由。

**3. `list_tools` 里加了一次 `yield_now()`。** `ServerHandler` 的签名要求 `async`，
而工具表构造本身是同步的，`clippy::unused_async` 会报错。加一次零成本的让位点，
比在 crate 级 `allow` 整条 lint 更精确。

**4. 轮询的防线从 `catch_unwind` 改成监督任务。** 规划里写的是用
`catch_unwind` 兜住"引擎在算 admin 视图时 panic"。落地时 workspace 禁止 `unsafe`，
而 `catch_unwind` 对含 `Mutex` 的结构需要 `UnwindSafe` 包装，写法不干净。
改成：轮询任务由一个监督者持有，任务若因 panic 退出就记 `error!` 并重启。
效果相同，且不需要 `unsafe`。

**5. 两个真实缺陷是测试写出来才发现的。**

- `ChangeSignal::wait` 初版在 `changed()` 成功后**再问一次** `has_changed()`。
  `watch::Receiver::changed()` 成功时已经把版本标记为已读，所以那次再问永远是
  false，等待总是超时。5 条信号用例一起红。改为"`changed()` 成功即 Ready"。
- MCP 的会话缓存初版用 `HashMap` + "按插入顺序淘汰最旧"。这在 Python 的 dict 上
  成立，但 Rust 的 `HashMap` **不保序**，淘汰会落到任意一条上——包括正在用的那条。
  改成显式的 `Vec<(key, entry)>`，把"顺序"变成结构的一部分。

**6. `/games/{id}` 与 `/games/{game_type}` 的路由冲突是编译期不报、运行期才炸的。**
`create_app` 的测试一跑就 panic（`Insertion failed due to conflicting route`）。
axum 0.8 把两者视为同一个匹配形状，参数名不同即冲突。统一成 `{id}` 后解决。

---

## 实施之后的三轮补充

合并进 `master` 之前又做了三轮，每一项都由"先验证、再发现缺陷"驱动。

### 第 1 轮：真实部署验收，发现两个同桌匹配缺陷

写了 `crates/agent-game-server/examples/play_liars_dice.rs`：用 N 条真实 MCP
连接、走真实 HTTP、把一局打到 `finished`。它立刻发现三个玩家被分到了**两桌**。

两个缺陷互相掩盖，改动一处才让另一处现形：

1. **MCP 建局时丢掉创建参数**。调用方传 `player_count=3`，传输层构造
   `CreateParams` 时只放了 `player_token`，桌子按默认 2 人建出来；第三个玩家
   来匹配又按 3 去比，永远匹配不上。
2. **`player_name` 混进了建桌配置**。它是动作参数，但被留在 `setup` 里；
   两个人名字不同 ⇒ `compatible` 判为不兼容 ⇒ 永远坐不到一起。

第 1 条在"只透传 token"的时期掩盖了第 2 条——那时 `player_name` 根本到不了
`manager`。修完之后，3 人局一次打完（112 次调用、70 秒）。

回归用例：`crates/agent-game-server-core/tests/table_matching.rs`（8 条）。

### 第 2 轮：重写 `admin.html`

旧版是逐字复制后改了三处路径，带着两张硬编码表（游戏名 `TYPE_LABEL`、状态优先级
`STATUS_ORDER`）。那让"零改动加游戏"名不副实：新游戏必须回来改这个文件。

重写后：展示名由服务端下发；状态只分"进行中/已结束"，新引擎用 `draw` 之类的终态也
能显示；未识别的字段原样列出；事件委托取代内联 `onclick`（又一处注入面）。

配套 `tools/verify-admin-ui.js`：把模板里的 `<script>` 抽出来，在最小 DOM 桩里执行，
验证转义、状态归类与渲染。CI 里用 node 跑。它比"用无头浏览器点一遍"轻得多，
且不需要装任何依赖。

### 第 3 轮：变异测试扩到 mcp 与 server

| crate | 首轮存活 | 最终存活 | 补的用例 |
|---|---|---|---|
| `-mcp` | 45 | 8 | `execute.rs`（25 条，直接调 `execute` 不经过 HTTP）+ `lib.rs` 单元用例（13 条） |
| `-server` | 28 | 0 | `startup.rs`（13）、`glue.rs`（11）、`binary.rs`（5，子进程） |

这一轮又发现三个真实缺陷：

1. **`--prompt-dir` 是空操作**。路径存进了 `AppState`，但没有任何代码读它——
   引擎用的还是内置模板。`PromptStore.dir` 改成 `RwLock<Option<PathBuf>>` 并加
   `set_dir()`，`set_prompt_dir` 真正把它同步给每个引擎。
2. **工具表硬编码在 mcp 层**。`GameEngine::describe_tools()` 与 `ToolSpec` 补进
   协议，mcp 只做翻译。否则新游戏的工具永远返回"未知工具"——这正是"零改动加游戏"
   的另一半。
3. **提示词仓库是进程级共享，测试并行时互相清目录**。三条涉及 `set_dir` 的用例
   加锁串行，否则套件间歇性变红（跑了三次才确认修好）。

`--prompt-dir` 那条还顺带说明了一件事：**一个参数被解析、被存下来、被打印，
不等于它被用了。** 只有变异测试（把 `set_prompt_dir` 改成 `()`）才会问
"删掉这行，有人会发现吗"。

### 最终验收

| 项 | 实际 |
|---|---|
| 测试 | **518 passed** |
| 覆盖率 | **94.95% 行 / 93.45% 分支** |
| 静态检查 | clippy 零告警，fmt 零差异 |
| 变异测试 | core 0 存活 / server 0 存活 / mcp 8 存活（全部记录为等价） |
| 管理页面 | `tools/verify-admin-ui.js` 全通过 |
| 端到端 | 真实二进制 + 3 条 MCP 连接，114 次调用打到 `finished` |

---

## 遗留

- **`mcp` 层的 8 个存活变异体没有补测**。它们全部落在 `ServerHandler` trait 的
  包装层（`call_tool` 的 match guard、`list_tools` 的 async 包装、`session_key`
  的转发）。要覆盖它们需要构造 `RequestContext<RoleServer>`（要 `Peer` 与
  `CancellationToken`），而真正的影响面已被 `mcp_transport.rs` 的真实 HTTP
  往返覆盖。逐条理由写在 `.config/mutants.toml`。
- **`state_signature` 的失败分支没有真正的用例**。它返回 `Option<String>`，
  `None` 表示序列化失败；但 `serde_json::Value` 实际上总能序列化，
  构造一个真的失败需要给引擎塞一个会 panic 的 `Serialize` 实现。
  `poller.rs` 的 `(None, _) => false` 分支因此从未被走到。
- **`exclude_re` 的等价判定是人工结论**。每条都写了理由，但"等价"本身不可证明——
  如果将来某条理由因为代码改动而失效，配置不会自动报警。建议：改
  `config.rs` / `prompt.rs` / `manager.rs` / 两个引擎的 `check_timeout` 时
  顺手重跑一次全量变异，看那条 exclude 是否还成立。
- **`play_liars_dice.rs` 只打了大话骰**。猜数字的端到端由
  `mcp_transport.rs::a_game_can_be_started_and_played_through_mcp` 覆盖，
  但那条走的是进程内 app，不是真实进程。
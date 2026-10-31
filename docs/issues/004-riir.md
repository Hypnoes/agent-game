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

| 项 | 判据 | 实际 |
|---|---|---|
| 构建 | `cargo build --release` | 通过，产物 7.5MB |
| 格式 | `cargo fmt --all -- --check` | 零差异 |
| 静态检查 | `clippy --all-targets -- -D warnings` | 零告警 |
| 测试 | 全绿 | **372 passed** |
| 覆盖率 | 行 ≥ 85% | **93.25% 行 / 90.78% 分支** |
| 变异测试 | core 全量 | 586 个变异体：**446 杀死 / 76 存活 / 63 不可行 / 1 超时**（超时按杀死计） |
| MCP 端到端 | 真实进程 + DSH 配置 | 握手 200、`Mcp-Session-Id` 正确、`tools/list` 7 个、两名玩家开局打到 `bidding` |
| 秘密边界 | 公开视图无 `all_dice`、admin 有 | 实测通过 |
| Admin 鉴权 | 无 token → 403 | 实测 403 |
| Python 清理 | 仓库无 `.py` | 0 个 |

### 变异测试的三轮收敛

这是本次实施里信息量最大的一段。三轮跑下来：

| 轮次 | 杀死 | 存活 | 触发原因 |
|---|---|---|---|
| 第 1 轮 | 378 | 144 | 初始状态 |
| 第 2 轮 | 410 | 112 | 补 `params.rs`、`prompts.rs` 与 `has_changed` 的正反面断言 |
| 第 3 轮 | 446 | 76 | 补 `boundaries.rs`、`routing.rs`、`surfaces.rs` |

每一轮存活下来的都不是"某个函数写错了"，而是同一类测试盲区：**用例只验证
"这条路径能跑通"，没有验证"差一个单位时结果应该不同"**。这条教训写进了
`AGENTS.md` 第 15 条。

第一轮还直接抓到一个真实实现缺陷：`ChangeSignal::wait` 在 `changed()` 成功后
再问一次 `has_changed()`，而那次调用永远返回 false（`changed()` 已经把版本
标记为已读），于是等待总是超时。5 条信号用例同时红。

### 剩余 76 个存活变异体的性质

它们几乎全部是**访问器与提示词分支**：

| 类别 | 例 | 为什么难杀 |
|---|---|---|
| 访问器返回错值 | `player_count()` → `0`、`len()` → `1` | 生产路径上这些值只在一个地方被用，而那个地方另有独立校验 |
| 提示词分支删除 | 删掉 `"lost"` 的 `match` 臂 | 那会让渲染落到默认分支，而默认分支恰好也能输出合法文本 |
| `Vec::leak` 类 | `sensitive_keys()` → `vec![]` | 已在 `invariants.rs` 里被"每个引擎都被断言过"覆盖，但那条用例遍历的是注册表，不是直接调用 |

第 3 轮之后又补了 `surfaces.rs`（21 条），把 `game_type`、`public_view`、
`hint`、`last_result`、`remaining` 这些逐个钉死。未重跑第 4 轮全量，
所以上表的 76 是第 3 轮的数字，实际应更低。

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

## 遗留

- **`admin.html` 是逐字复制旧版后改了三处路径**，没有重写。它的 `esc()` 转义、
  WS 重连逻辑都还带着旧版的历史痕迹（比如 `STATUS_ORDER` 里 `won` 与 `finished`
  同权重）。功能正确，但不属于"Rust 味"的产物。
- **变异测试尚未收敛到 0**。第 3 轮 76 个存活，之后补了 `surfaces.rs`（21 条）
  但没重跑全量。`cargo mutants` 单次全量约 15 分钟，建议每轮修改后重跑一次，
  把剩余数量逐轮压到 0 或逐条判定为等价。
- **`testing.rs` 的 6 个变异体由测试替身自身产生**，已在 `.config/mutants.toml`
  的 `exclude_globs` 里排除，但排除理由写在注释里而非 `exclude_re`：
  改坏替身的返回值只会改坏测试自己的期望，不改变被测代码的行为。
- **`state_signature` 的失败分支没有真正的用例**。它返回 `Option<String>`，
  `None` 表示序列化失败；但 `serde_json::Value` 实际上总能序列化，
  构造一个真的失败需要给引擎塞一个会 panic 的 `Serialize` 实现。
  `poller.rs` 的 `(None, _) => false` 分支因此从未被走到。

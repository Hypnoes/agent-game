# 端点与行为映射：Python → Rust

这份表记录 RIIR（Rewrite It In Rust）之后，旧版（`python-v0.3.0` 标签）与新版
之间的对应关系。它有两个用途：

1. **审计**：确认每个旧端点都有明确的去向，没有静默消失的能力。
2. **迁移**：手上有旧客户端或旧笔记的人，能查到新地址与新字段名。

旧实现永久保存在 git 标签 `python-v0.3.0` 与 `master` 的历史里，不再维护。

---

## 1. HTTP 端点

| 旧（Python） | 新（Rust） | 变化说明 |
|---|---|---|
| `GET /` | `GET /health` | 改为探活语义：`{ok, version, games, sessions}`。旧路径也保留，返回同一个体。 |
| `GET /games/tools` | `GET /api/v1/games/tools` | 响应加 `display_name` 与 `player_mode`；工具定义按游戏分组。 |
| — | `GET /api/v1/games/tools/{game_type}` | 新增：单个游戏的工具面。 |
| `POST /games/{game_type}` | `POST /api/v1/games/{id}` | 路径参数统一叫 `id`（见下方"为什么要改参数名"）。空 body 仍是 `{}`，畸形/非对象 body 一律 400。 |
| `POST /games/{session_id}/action` | `POST /api/v1/games/{id}/actions` | 单数 `action` → 复数 `actions`。`state` 的阻塞等待语义不变。 |
| `GET /games/{session_id}` | `GET /api/v1/games/{id}` | 响应改信封：`{session_id, game_type, display_name, state}`，`state` 已是公开视图。 |
| `GET /games/{sid}/state/{idx}?player_token=` | `GET /api/v1/games/{id}/players/{idx}` + `X-Player-Token` 头 | **token 从查询参数改为请求头**：查询参数会进访问日志与 `Referer`。 |
| `GET /admin` | `GET /admin?token=` | 不变。仍然只在已出示正确 token 时才注入 `window.ADMIN_TOKEN`。 |
| `GET /admin/games` | `GET /api/v1/admin/games` | 响应 `{games: [...]}`，每行是信封。 |
| `GET /admin/games/{id}` | `GET /api/v1/admin/games/{id}` | 秘密信息（`target` / `all_dice`）只在 `state` 里。 |
| `POST /admin/reload-prompts` | `POST /api/v1/admin/prompts/reload` | 路径改为资源式。 |
| `WS /admin/ws?token=` | `WS /api/v1/admin/ws?token=` | 鉴权方式不变（浏览器不能给 WS 设自定义头）。 |
| `POST /mcp` | `POST /mcp` | **路径不变**。DSH 的 MCP 配置无需改动。 |

### 为什么要改路径参数名

`POST /games/{game_type}` 与 `GET /games/{session_id}` 在 axum 0.8 里是**同一个
匹配形状**，不同参数名会让其中一条注册失败（实测报
`Insertion failed due to conflicting route`）。两条路由共用 `{id}`，语义由 HTTP
方法区分：`POST` 时它是游戏类型，`GET` 与 `.../actions` 时它是 session id。

---

## 2. MCP 工具面（刻意不变）

| 项 | 变化 |
|---|---|
| 工具名（7 个） | **不变**。`guess_number_start/guess/state`、`liars_dice_join/state/bid/challenge`。 |
| 必填字段 | **不变**。 |
| `timeout` 参数语义 | **不变**：省略=满预算、`0`=不等待、负数夹 0、超上限夹 55、非数值报错。 |
| 阻塞等待 | **不变**：非当前回合的 `state` 在服务端挂起。 |
| 身份模型 | **不变**：`player_token` 决定对局归属，`Mcp-Session-Id` 只是连接缓存键。 |
| 服务端名称 | `agent-game`（不变）。 |
| 协议版本 | 协商 `2025-11-25`（与旧版一致）。 |

工具名与玩法契约是 Agent 侧最直接的接口，改名等于改玩法，所以刻意一字不动。

---

## 3. 响应字段

| 旧字段 | 新位置 |
|---|---|
| `game_name` | 顶层 `display_name`（改名，且**有了消费者**：admin 页面与工具定义） |
| `session_id` | 信封顶层（不变） |
| `game_type` | 信封顶层（不变） |
| `target`（admin 才有） | `state.target` |
| `all_dice`（admin 才有） | `state.all_dice` |
| `phase` / `status` | `state.phase` / `state.status`（继承引擎自己的命名） |
| 动作响应的 `prompt` | 顶层 `prompt`（不变） |
| 动作响应的 `public` / `private` | 顶层（不变） |

错误响应统一为：

```json
{"error": {"code": "invalid_input", "message": "…"}}
```

`code` 取自固定集合：`invalid_input` / `unauthorized` / `forbidden` /
`not_found` / `conflict` / `poisoned` / `internal`。旧版只有一段中文 `detail`，
客户端只能靠字符串匹配区分错误类别。

---

## 4. 行为差异（全部是有意的）

| 差异 | 原因 |
|---|---|
| `X-Player-Token` 头取代 `player_token` 查询参数 | 查询参数会进日志 |
| 引擎 panic 后对局标记 `poisoned`，后续请求 409 | 旧版会 500 到连接断开为止 |
| 公开视图由引擎**分别构造**，不再运行时按名删键 | 旧版 `secret_keys` 拼错一个字母就静默泄密 |
| `display_name` 由服务端下发 | 旧版 admin 页面自己硬编码 `TYPE_LABEL`，新游戏必须回来改页面 |
| 等待循环上移到框架（`wait_for`），引擎回到纯同步 | 旧版每个引擎各写一遍"订阅→判条件→等剩余预算→重判" |
| MCP 会话缓存按**显式顺序**淘汰 | 旧版依赖 dict 保序；Rust 的 `HashMap` 不保序，照搬会淘汰正在用的连接 |

---

## 5. 没有迁移的东西

| 旧物 | 处置 |
|---|---|
| `config.BLOCK_BUDGET` 模块全局 + monkeypatch | 换成注入的 `Config` 值。测试构造小预算实例，不需要任何全局改写。 |
| `GameEngine` 协议的 `if TYPE_CHECKING:` 桩 | Rust 用默认方法实现表达"可选能力"，不存在 `hasattr` 永远为真的问题。 |
| `get_prompts` 的"探测不到就跳过"分支 | 所有引擎都必须提供 `prompts()`，编译期保证。 |
| Python 版 `str.format` 的全部格式说明符 | 只保留 `{name}` 与 `{name:g}`。`:.0f`、`!r`、`{0}`、`{x[y]}` 在提示词里从未用上。 |
| `test_meta.py` 的"版本号两处一致" | `CARGO_PKG_VERSION` 只有一个来源，那条测试失去存在意义。 |
| `scripts/mutation_check.py` | 换成 `cargo-mutants`（见 `.config/mutants.toml`）。 |

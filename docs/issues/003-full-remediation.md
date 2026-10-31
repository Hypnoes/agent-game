# 003 — 全量修复：正确性、健壮性、测试框架与工具链

前置：无（基于 `master` @ `a1d765c`）。

本计划合并两份审查的结论并一次性落地：

- `docs/reviews/2026-10-31-code-review.md` — 代码缺陷（P0×1、P1×3、P2×5、P3×7）
- `docs/reviews/2026-10-31-test-review.md` — 测试框架问题（11/35 变异体存活）

分七批实施，每批自带验收。批次之间无强依赖，但顺序按"缺陷严重度 + 后续批次的依赖"排。

---

## 背景

两轮审查的结论可以压成一句话：**代码里有一处规则错误和若干一致的健壮性缺口，
测试却把力气花在了实现细节上。** 因此这次修复同时动生产代码和测试框架——
只修代码不修测试，同类问题还会再漏；只修测试不修代码，264 条全绿也仍然是错的。

修复的验收判据只有一条：**变异测试全杀**。覆盖率门槛做不到这件事，
它只能说明代码被跑过，不能说明规则被守住。

---

## 批次 A — 正确性（P0 + P1）

### A1. 回合超时后清零本轮叫数

**问题**（代码审查 P0-1）：`check_timeout` 的超时分支只掉骰、换回合，不清 `current_call`；
`_challenge` 会清。两条"输掉本轮"的路径不一致，导致超时后桌面留着一个已无法再加码的叫数，
下家既不能加码也不能弃权，只能替别人开盅。

**改动**：超时分支与 `_challenge` 对齐 —— `self.current_call = None`，并在历史里记一条
"超时判负"记录（与开盅记录同构，`quantity=0` 表示不是叫数）。

**验收**：3 人局，`bid 6×3` → 某人超时 → 下家能正常加码。

### A2. 身份校验统一到一处，MCP 先校验后等待

**问题**（代码审查 P1-2，对应 `AGENTS.md` #5）：REST 分支"解析 → 校验 → 等待"，
MCP 分支"等待 → 校验"。预算 55 秒时，错 token 的 MCP 调用白等 55 秒才报错。

**根因**：校验逻辑写在 `game_server._state_player_error()`（HTTP 专用，返回 `HTTPException`），
MCP 侧没有等价物，只好依赖引擎自己的 `_validate_player`——而那要等到 `handle_action` 才跑。

**改动**：把"身份是否合法"抽成与传输无关的纯函数，放在 `games/game_engine.py`：

```python
def player_identity_error(engine, player_index, player_token) -> str | None
```

返回错误描述或 `None`。REST 用它生成 `HTTPException`，MCP 用它生成 `ValueError`，
两条传输都在等待之前调用。

**验收**：两条传输上，错 token / 越界索引都在 0.1 秒内报错，且 `wait_for_change` 未被调用。

### A3. `check_timeouts` 按引擎隔离，`list_sessions` 返回副本

**问题**（代码审查 P1-3 / P1-4）：

- `check_timeouts` 的 per-session 循环没有异常隔离。一个引擎的 `check_timeout` 抛异常，
  整个循环中断，其余对局不再结算超时；REST 的 `state` 分支在请求内调用它，于是无关对局 500。
- `list_sessions` 直接往 `get_public_state()` 的返回值里塞键。当前靠"两个引擎都返回新 dict"
  这个巧合安全。

**改动**：

- `check_timeouts` 内层 `try/except Exception`，记 warning 日志后继续下一个 session。
  调用方拿到的 `events` 只包含成功的那些。
- `list_sessions` 改为 `state = dict(engine.get_public_state())`。

**验收**：注册一个 `check_timeout` 必抛的引擎 + 一个正常引擎，正常的那个仍被结算，
REST `state` 仍返回 200；返回内部 dict 的引擎，`list_sessions` 后其内部字典不被污染。

---

## 批次 B — 输入健壮性（P2）

### B1. 非对象 JSON body → 400

`game_action` 里 `await request.json()` 之后统一校验 `isinstance(body, dict)`。
`create_game` 同样。

### B2. 畸形 JSON → 400，不再静默建局

`create_game` 当前把 `ValueError` 一律当"空参数"。改为：

- body 为空（`b""`）→ 视为空参数，允许 create（既有行为）
- body 非空但解析失败 → 400

### B3. `Prompt` 兜底不再漏 `AttributeError`

`_render_template` 与 `_render_field` 的 except 加上 `AttributeError`。
模块契约是"提示词层不能把一局游戏 500 掉"，`{x.y}` 而 `x` 是 `int` 必须降级为
`{unrendered:x.y}`，而不是抛给调用方。

### B4. Admin 页面转义

`admin.html` 里所有拼接进 `innerHTML` 的外部字符串（玩家名、blame message、会话 ID）
统一走一个 `esc()` 转义函数。这些字段来自 Agent 传入的 `player_name`，
而 admin 页面是唯一能看秘密信息的地方，注入等于泄密。

### B5. `/admin/ws` 要求 token

WebSocket 握手时读 `?token=` 或 `Sec-WebSocket-Protocol` 之外的 `X-Admin-Token` 不可行
（浏览器 WS API 不能设自定义头），因此用 `?token=`：不匹配则 `close(code=1008)`。

---

## 批次 C — 清理（P3）

- 删除 `game_server.py` 重复的 `@router.post("/admin/reload-prompts")`（路由实际注册 2 次）。
- 删除 `game_server.py` 里未使用的 `BLOCK_BUDGET` 导入。
- 删除 `guess_number.py` 里未使用的 `_FINISHED_STATES`。
- 删除 `liars_dice.py` 里重复的 `self.turn_timeout = 60` 及其注释。
- 修正 `mcp_server._engine_phase` 的过期注释（`GuessNumberEngine` 已显式重写 `phase`）。
- 版本号单一来源：`pyproject.toml` 用 hatch 的动态版本读 `config.__version__`，
  并加一条测试断言两者一致（在当前布局下更简单的是保留两处 + 一致性测试）。

---

## 批次 D — 架构

### D1. 终态与秘密键收进引擎协议

**问题**：`SECRET_STATE_KEYS`、`mcp_server._FINISHED_PHASES`、`manager._is_finished` 三处
各存一份"终态名/秘密键"知识，且写法不同。`README` 的"零改动加游戏"因此名不副实。

**改动**：协议新增两个**可选**类属性（运行时用 `getattr` 探测 + 默认值）：

| 属性 | 默认 | 说明 |
|---|---|---|
| `finished_states` | `frozenset({"finished", "won", "lost"})` | 该引擎的终态名集合 |
| `secret_keys` | `frozenset({"target", "all_dice"})` | 公开状态里需剥离的键 |

`manager._is_finished`、`mcp_server._engine_finished`、`game_server.game_public_state`
三处都改成读引擎自己声明的值。默认值保持现有行为，两个既有引擎不用改。
`SECRET_STATE_KEYS` 常量保留为"默认集合"并改名 `DEFAULT_SECRET_STATE_KEYS`。

**验收**：新增一个终态叫 `draw`、藏 `solution` 字段的测试引擎，不碰框架代码就能正确
判定结束与剥离秘密。

### D2. `create_app()` 变成真工厂

**问题**：`manager` 是模块全局，`create_app()` 不注入新实例，导致三个测试文件三套隔离手法，
其中一套只清三个字典、`_finished_at` 与 `_seen_expired` 从不还原。

**改动**：

```python
def create_app(manager: GameManager | None = None) -> FastAPI
```

- 参数缺省时用模块级 `manager`（保持 `uvicorn` 与既有测试路径不变）。
- 传入时，app 内的路由、MCP server、`AdminBroadcast` 全部用传入的实例。
- 路由函数通过 `request.app.state.manager` 取 manager，而不是直接引用模块全局。

这是本批最大的改动：需要把 `router` 上的处理函数从"读模块全局"改成"读 `request.app.state`"。

**验收**：两个 app 各带独立 manager 同时运行，互不干扰（已有 `test_poller.py` 的实例隔离用例扩展）。

---

## 批次 E — 测试框架

### E1. 新建 `tests/conftest.py`

收敛当前散落在各文件的 5 种"造一局大话骰"写法与 3 套 manager 隔离手法：

| fixture | 作用 |
|---|---|
| `anyio_backend` | 全局只跑 asyncio（现在只在 `test_mcp_server.py` 有） |
| `make_manager` | 造一个注册了真实引擎的独立 `GameManager` |
| `app_client` | 用 `create_app(make_manager())` 造 TestClient（依赖 D2） |
| `dice_game` | 造一局已开局的 2 人或 3 人大话骰，自动补齐 token |
| `dice_script` | 把全场骰子摆成指定形状（替代 `set_all_dice`，支持"1 混在非 1 里"） |
| `mcp_harness` | 从 `test_mcp_server.py` 提到 conftest |

### E2. 去掉会放行的 `skip`

`test_mcp_server.py` 两处 `if not hasattr(LiarsDiceEngine, "wait_for_change"): pytest.skip(...)`。
改为在模块级断言该能力存在——功能被删除时套件必须红，不能绿。

### E3. 删除重复定义的用例

`test_manager.py` 的 `test_finished_game_reclaimed_after_retention` 定义两次（169 / 350 行）。
保留第一份，删除第二份。加 ruff `F811` 防止再犯。

### E4. 补规则层用例（对应 6 个存活变异体）

| 用例 | 杀的变异体 |
|---|---|
| 1 是百搭：`[1,2,3]` 里 1 计入 3 的计数；叫 1 时 1 只算 1 | `_count_face` 去掉 `d == 1` |
| `join` 在对局开始后被拒绝，且不重摇骰子 | `join` 去掉阶段检查 |
| 超时判负唤醒等待者 | `check_timeout` 去掉 notify |
| 入座后唤醒大厅等待者 | `_add_player` 去掉 notify |
| 开盅后唤醒对手 | `_challenge` 去掉 notify |
| `turn_timeout < BLOCK_BUDGET`（判负必须先于等待预算发生） | `turn_timeout` 改值 |
| 工具 schema 的 `required` 与 `describe_tools()` 声明一致 | schema 丢 `required` |
| `/admin/reload-prompts` 无 token → 403 | 去掉 `_check_admin_token` |
| `Prompt` 的 `{x.y}` 降级而非抛异常 | 兜底改 `raise` |

### E5. 端到端整局序列用例

从 `join ×3` 打到 `finished`，覆盖"叫数 → 超时 → 下家加码 → 开盅 → 掉骰 → 淘汰"，
用固定随机种子。这是唯一能照到 A1 那类跨动作缺陷的层次。

### E6. 契约层：两条传输同一份用例集

参数化 `(通道, 请求构造器)`，对同一批 `(动作, 参数)` 分别打 REST 与 MCP，
比较归一化后的 `(成功/错误类别, 是否 fast-fail)`。

### E7. 断言从文案转向结构

把 44 处中文文案断言里的"流程判定"类改掉。保留的只有真正属于提示词契约的
（"不得出现 `{missing:` 标记"、"不得出现原始花括号"）。

判据：改一句提示词措辞，套件应保持绿；改一个状态字段，套件应变红。

---

## 批次 F — 工具链

- `pyproject.toml` 加 `[tool.ruff]`：`select = ["F", "E", "W", "I", "B", "UP"]`，
  `line-length = 100`；加 `[tool.ruff.lint.per-file-ignores]` 放行测试里的长行。
- pytest 配置加 `--strict-markers`、`filterwarnings = ["error"]`。
- `dev` 依赖加 `ruff`、`coverage`、`httpx`。
- 新增 `scripts/mutation_check.py`：内置 35 个变异体，逐个应用 → 跑套件 → 还原，
  输出存活清单，存活则退出码非零。
- 新增 `.github/workflows/ci.yml`：`pytest` + `ruff check` + `mutation_check.py`。
- 新增 `tests/test_meta.py`：断言测试函数名唯一、`config.__version__ == pyproject` 版本。

---

## 批次 G — 文档同步

- `README.md`：更新端点表（`/admin/ws` 加 token）、结构树（新增 `conftest.py`、`scripts/`）、
  "零改动加游戏"改为说明两个可选类属性。
- `AGENTS.md`：把 9 条硬约束补上"由哪个测试守"的映射；新增两条（超时清叫数、坏引擎隔离）。
- `docs/issues/README.md`：登记 003。

---

## 验收

全部批次完成后，按下表逐项取证：

| 项 | 判据 |
|---|---|
| 单元测试 | `pytest` 全绿，用例数 ≥ 当前 264（去重后不减） |
| 静态检查 | `ruff check .` 无告警 |
| 覆盖率 | 语句 ≥ 92%，分支 ≥ 90% |
| **变异测试** | **35 个变异体全部杀死** |
| 契约层 | REST 与 MCP 对同一批非法输入的错误类别与 fast-fail 行为一致 |
| 端到端 | 3 人局从 join 打到 finished，含超时介入路径 |
| 手工 | `uv run main.py` 启动后 `/mcp` 握手成功、`/admin` 需 token |

---

## 实施记录

全部七个批次已落地。与规划的偏差如下。

### 验收结果

| 项 | 判据 | 实际 |
|---|---|---|
| 单元测试 | 全绿，用例数不减 | 451 passed（原 264） |
| 静态检查 | `ruff check .` 无告警 | All checks passed |
| 覆盖率 | 语句 ≥ 92%，分支 ≥ 90% | 93% / 90% |
| 变异测试 | 全部杀死 | **48 杀死 / 0 存活 / 2 等价**，退出码 0 |

### 与规划的偏差

**A2 的落点比规划更靠下。** 规划写的是"MCP 先调用引擎方法校验"，实际做成了
`games/game_engine.py::player_identity_error()` 这个与传输无关的纯函数，返回
`IdentityError(kind, message)`，两条传输各自翻译成 `HTTPException` 或 `ValueError`。
translation 留在传输层，判断逻辑只有一份。

**D1 的两个属性实现在 `game_engine.py` 的函数里，而不是管理器里。**
规划只写了"引擎声明、管理器探测"，落地时把探测收敛成 `finished_states_of()` /
`secret_keys_of()` / `is_finished()` 三个函数，管理器与 MCP 层都调它们——
否则又是三处各写一遍探测。

**D2 的改动范围比预估大。** 真工厂要求所有路由从"读模块全局"改成"读
`request.app.state`"，涉及 8 个处理函数。顺带发现 `game_public_state` 在改签名时
漏了 `request` 参数，被测试立刻抓到。

**E1 的 conftest 让两个测试文件的辅助函数成了死代码。** `test_mcp_server.py` 里
约 130 行的 SSE/harness 与 `isolated_state` 夹具整体删除，改为 `McpHarness`
携带自己那份 manager——隔离从"手工清全局字典"变成"每个用例新建 manager"。
原来的夹具只清三个字典，`_finished_at` 与 `_seen_expired` 从来没被还原过。

**E4 里"删掉整个 wait_for_change"这一条改成了双向断言。** 规划要求"功能删除时
套件必须红"，落地时同时保留了 `test_wait.py`（7 条）与两处硬 `assert hasattr`。
删掉方法后是 6 failed 而不是 6 failed + 2 skipped。

**E6 的契约层用 ASGI 内存传输同时跑两条通道，而不是 `TestClient` + async。**
`TestClient` 自带后台线程与独立事件循环，和 lifespan 混用会触发
"future belongs to a different loop"。

**F 的变异体从 35 个扩到 50 个。** 新增的覆盖面：终态/秘密键声明、坏引擎隔离、
audit 各鉴权出口、prompt 的三层降级、`list_sessions` 副本、CLI 启动路径。

**契约层发现了一个规划外的缺陷。** MCP 侧把 `KeyError`（缺必填参数）归进
"非预期异常"，返回"服务端内部错误: KeyError"；REST 侧同一输入是 400。
`_EXPECTED_ERRORS` 因此加上 `KeyError` 与 `TypeError`。

### 两个等价变异体的原因

1. **「解析器不再读实时预算」**：把 `budget = BLOCK_BUDGET` 改成 `budget = 55.0`
   之后，末尾的 `min(budget, BLOCK_BUDGET)` 仍把 55.0 钳回当前预算，行为不变。
   要杀掉它得同时改钳制，而那正是另一个变异体「timeout 不再夹到上限」覆盖的事。

2. **「降级链最后一层改成抛异常」**：`_render_lenient` 内部已接住每个字段的异常，
   它自己只在解析器出问题时抛 `ValueError`（由内部 try 处理）。因此最外层兜底
   当前不可达——它是防御性的第二道锁。删掉不改变行为，保留是为了 `_render_lenient`
   哪天被改坏时也不会 500。这一点写进了 `prompt.py` 的注释，避免下次有人把它
   当死代码删掉后误以为仍有保护。

### 遗留

- `scripts/mutation_check.py` 串行跑，约 10 分钟。CI 里单独一个 job。
- `test_poller.py` 仍有 4 条用例各睡 1.2 秒（占套件 11 秒里的 6 秒）。它们测的是
  轮询节奏，改成假时钟需要把 `_poll_and_broadcast` 的 `asyncio.sleep` 注入化，
  本轮未做。
- 提示词文案断言仍有约 40 处保留，但都在 `test_prompts.py` 里、且断言的是
  "不得出现 `{missing:` / 原始花括号"这类渲染健康度，不是流程措辞。
  `test_mcp_server.py` 里的 4 处流程文案断言已改为结构断言。


# 代码审查记录（2026-10-31）

审查对象：`master` @ `a1d765c`。审查范围是全仓库源码、测试与文档。
环境：Windows 10，Python 3.12（`.venv`），`mcp 2.0.0`，`fastapi 0.141.1`，`starlette 1.6.0`。

基线与现状：

- 264 个测试全绿，9–10 秒。
- 语句覆盖率 90%，分支覆盖率 89%（`coverage run --branch --source=src`）。
- 无 CI（仓库没有 `.github/`），无 lint/type 检查配置。

下文每一条问题都在本机复现过，复现方式写在该条目里。严重度按影响面定：
**P0** 破坏对局规则；**P1** 破坏安全/一致性不变量；**P2** 输入健壮性；**P3** 死代码与文档漂移。

---

## P0：对局规则缺陷

### 1. 回合超时后本轮叫数不清零，下家被锁死成"必开"

**位置**：`src/agent_game_server/games/liars_dice.py:341-350`

超时分支只掉骰、换回合，没有清 `current_call`；而 `_challenge` 路径（同文件 `:460`）会清零。
两条"输掉本轮"的路径行为不一致，模块头部注释明确写的是"输掉本轮"。

**复现**（3 人局，每人 2 骰，总 6 颗）：

```python
e = LiarsDiceEngine(player_count=3, dice_per_player=2)
for n in "ABC": e.handle_action("join", player_name=n)
e.current_turn = 0
e.handle_action("bid", player_index=0, player_token=e.players[0].token, quantity=6, face=3)
e.current_turn = 1
e.last_action = time.time() - 61
e.check_timeout(time.time())   # {'type': 'turn_timeout', 'loser': 1}，总骰数 6 → 5
# P2 想加码：
e.handle_action("bid", player_index=2, player_token=e.players[2].token, quantity=7, face=3)
# ValueError: 叫数 7 超过场上骰子总数 5
e.handle_action("bid", player_index=2, player_token=e.players[2].token, quantity=6, face=4)
# ValueError: 叫数 6 超过场上骰子总数 5
```

**后果**：超时把一手"已无法再加码"的叫数留在桌上，下家既不能加码也不能弃权，只能替别人开盅。
胜负由别人的超时决定，不是由自己的判断决定。这是规则级 bug。

**修复方向**：超时分支与 `_challenge` 对齐——`self.current_call = None`，并考虑记一条历史。

---

## P1：校验顺序与错误隔离

### 2. MCP 侧 `state` 先等待、后校验身份（与 AGENTS.md #5 相悖）

**位置**：`src/agent_game_server/mcp_server.py:220-238`（`_run_action`）

REST 分支（`game_server.py:329-338`）严格做到"解析 → 校验 → 等待"，
MCP 分支顺序相反：先 `await waiter(...)`，等待结束才由 `handle_action` 校验 token 与索引。

**复现**（预算压到 2 秒）：

```
wrong token   -> ValueError player_token 不匹配     elapsed 2.02s
bad index 99  -> ValueError 无效的玩家编号: 99      elapsed 2.02s
```

**后果**：生产预算 55 秒时，token 打错的 Agent 先白等满 55 秒才拿到"player_token 不匹配"。
两条传输对同一类非法输入的错误时延差 55 秒。AGENTS.md #5 只约束了 REST 分支。

**修复方向**：把身份校验提取成引擎方法（如 `validate_player(player_index, player_token)`），
`_run_action` 在等待前调用它。

### 3. 一个坏引擎会让所有对局的超时结算与 `state` 请求崩掉

**位置**：`src/agent_game_server/manager.py:171-204`（`check_timeouts` 循环无异常隔离）

**复现**：

```
bad.check_timeout 抛 RuntimeError
-> RuntimeError 直接冒出；good.check_timeout 未被调用（calls == 0）
```

后台轮询能吞掉异常（`_poll_and_broadcast` 的 `except Exception`），
但 REST 的 `state` 分支在请求内直接调用 `manager.check_timeouts()`（`game_server.py:344`）。
带一个坏引擎时，**无关的猜数字对局** `state` 直接 500：

```
state action with poisoned check_timeouts: 500 Internal Server Error
```

**后果**：AGENTS.md 与 `test_poller.py` 都宣称"一个坏引擎不能杀死整个服务"，
该保证只在轮询路径成立。单引擎异常会扩散成全服务故障。

**修复方向**：在 `check_timeouts` 的 per-session 循环内 `try/except Exception`，记日志后继续。

### 4. `list_sessions` 会往引擎自己的状态字典里塞键

**位置**：`src/agent_game_server/manager.py:119-127`

`state = engine.get_public_state()` 之后直接 `state["session_id"] = sid`。
当前两个引擎每次返回新 dict，所以暂时安全；这是靠巧合，不是靠契约。
`game_public_state`（`game_server.py:370-374`）特意写了"在副本上剥键，别改动引擎自己维护的状态对象"。

**复现**（引擎返回内部 dict）：

```
engine internal keys after list_sessions: [... 'game_name','game_type','session_id']
```

**修复方向**：`state = dict(engine.get_public_state())`。

---

## P2：输入健壮性

### 5. 非对象 JSON body 让 `action` 端点 500

**位置**：`src/agent_game_server/game_server.py:325`（`body.pop("action", None)`）

```
'hello' -> 500     5 -> 500     true -> 500
[1,2]   -> 400（被 TypeError 兜住，但 detail 暴露了 create() 签名）
```

**修复方向**：`await request.json()` 之后统一 `if not isinstance(body, dict): 400`。

### 6. 创建端点把畸形 JSON 静默当成"空参数"

**位置**：`src/agent_game_server/game_server.py:263-265`

```
malformed json -> 200 {"session_id":"47e8e559",...}   sessions: 0 -> 1
empty body     -> 200                                 sessions: 2
```

**后果**：JSON 拼错、本该 400 的请求得到一个全默认参数的真实对局，并且会占住匹配池。

**修复方向**：`request.json()` 的 `ValueError` 分支区分"body 为空"与"body 非法"，后者返回 400。

### 7. 提示词渲染器会漏抛 `AttributeError`

**位置**：`src/agent_game_server/prompt.py:167-178`、`:211-213`

兜底链只接 `KeyError/IndexError/ValueError/TypeError`：

```
render("{x.y}", x=1) -> AttributeError 'int' object has no attribute 'y'   （冒出到调用方）
```

与模块头部"永不对调用方抛异常——提示词层不能把一局游戏 500 掉"的契约冲突。
内置模板不触发；但模板支持热重载，一次手滑就是 500。

**修复方向**：两处 except 加上 `AttributeError`（或直接 `except Exception`）。

### 8. Admin 页面把玩家名当 HTML 拼

**位置**：`src/agent_game_server/templates/admin.html:399-417`、`:445-459`

用 `innerHTML` 拼接 `g.players` 与 `c.message`，这些字段直接来自 Agent 传入的 `player_name`。

```
join player_name = "<img src=x onerror=alert(1)>"
/admin/games -> players: ['<img src=x onerror=alert(1)>', 'B', 'C']
```

**后果**：影响面是运维打开的 admin 页；但那是唯一能看到秘密信息（`target` / `all_dice`）的地方，
被注入的脚本可以读 `window.ADMIN_TOKEN` 把秘密全部取走。

**修复方向**：改成 `textContent`，或对插值做转义。

### 9. `/admin/ws` 完全无鉴权

**位置**：`src/agent_game_server/game_server.py:225-237`

```
unauthenticated ws connected OK
```

只推 `{"type":"state_changed"}`，不泄密；但它是无需凭证即可占用的长连接入口，
与"admin 全部强制 token"的口径不一致。

**修复方向**：要求 `?token=` 或用首帧出示 token。

---

## P3：一致性、死代码与文档漂移

| 位置 | 问题 |
|---|---|
| `game_server.py:476-477` | `@router.post("/admin/reload-prompts")` 写了两遍，路由实际注册 2 次（`Counter == 2`） |
| `game_server.py:33` | 导入 `BLOCK_BUDGET` 但全文未用 |
| `guess_number.py:18` | `_FINISHED_STATES` 定义后从未引用 |
| `liars_dice.py:99-101` / `:111-113` | `self.turn_timeout = 60` 及其四行注释重复两遍 |
| `mcp_server.py:159-167` | 注释称"GuessNumberEngine 从协议继承了 phase"，但 `guess_number.py:95-98` 明确重写了 `phase`，注释已过期 |
| REST vs MCP | 非 `state` 动作：MCP 把 `timeout` 从参数里 pop 掉，REST 原样并进 `**body` 传给引擎（实测 200，靠引擎 `**params` 宽容） |
| `config.py:11` / `pyproject.toml:3` | 版本号两处手写（均 0.2.2），无测试绑定 |

---

## 架构层面

**"零改动加游戏"目前名不副实。** `README.md:35` 的承诺被三处硬编码打破：

- `SECRET_STATE_KEYS`（`game_server.py:53`）
- `mcp_server._FINISHED_PHASES`（`mcp_server.py:36`）
- `manager._is_finished`（`manager.py:142-152`）

三处各存一份"终态名/秘密键"知识，且写法不同（一个认 `phase == "finished"`，
一个认 `{"finished","won","lost"}`）。新游戏只要终态叫 `draw`、或想藏一个 `solution` 字段，
就必须回来改框架。把它们收进引擎协议（如 `finished_states: frozenset`、`secret_keys: frozenset`，
管理器探测属性并提供默认值）才配得上 README 的说法。

**等待语义分散在三处。** `config.BLOCK_BUDGET`（预算）、`parse_block_budget`（解析）、
`liars_dice.wait_for_change`（循环重判）、REST/MCP 各自调用。AGENTS.md 用 #2/#5/#6/#7 四条约束
守这堆不变量，说明抽象还不够收敛。把"解析 + 校验 + 等待 + 结算"合成一个
`await engine.wait_turn(player_index, player_token, budget)`，两条传输只做协议转换，
AGENTS.md 里至少三条可以删掉。

---

## 建议补的回归用例

1. 3 人局超时后 `current_call is None`，且下家能正常加码（对应 P0-1）。
2. MCP `state` 带错 token / 越界索引时耗时 `< 0.1s`，且 `wait_for_change` 未被调用（P1-2）。
3. `check_timeouts` 里一个引擎抛异常，其它引擎仍被结算；REST `state` 仍返回 200（P1-3）。
4. `POST /games/{sid}/action` 传 `"str"` / `5` / `true` body 返回 400（P2-5）。
5. 畸形 JSON 建局返回 400 且不新增 session（P2-6）。
6. `Prompt.render("{x.y}", x=1)` 返回 `{unrendered:...}` 而非抛异常（P2-7）。

---

## 附：本次审查用到的复现脚本

全部为一次性脚本，未写入仓库。要点：

- **覆盖率**：`uv pip install coverage` 后 `python -m coverage run --branch --source=src -m pytest`。
- **变异**：在临时副本上改源码、跑 `pytest`、还原。脚本逐个断言了锚点存在，避免改错位置。
- **非对象 body / 畸形 JSON**：`TestClient(app, raise_server_exceptions=False)` 才能看到 500 而不是异常。

审查期间工作区始终保持干净，`git status` 只有 `.coverage`（已删除）。

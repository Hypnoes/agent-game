# AGENTS.md

改动本仓库时必须遵守的硬约束。每一条都来自实测中踩过的坑，不是风格偏好。

## 1. MCP 端点只能用 `Route` + `session_manager.asgi_app` 挂载

```python
app.router.routes.append(
    Route("/mcp", endpoint=session_manager.asgi_app, methods=["GET", "POST", "DELETE"])
)
```

三种错误写法及其后果：

| 写法 | 后果 |
|---|---|
| `Mount("/mcp", app=...)` | 返回 307 重定向到 `/mcp/`，裸 HTTP 客户端失败 |
| `async def` 包装后 `return None` | `TypeError: 'NoneType' object is not callable` |
| 忘了 `methods=[...]` | GET/DELETE 请求被拒 |

另外 `StreamableHTTPSessionManager.run()` 在同一个实例上只能进入一次，所以 `game_server.py` 用 `create_app()` 工厂生产 app。测试必须每个用例调用一次 `create_app()`，复用模块级 `app` 会在第二个用例抛 `RuntimeError`。

## 2. 阻塞等待预算上限是 55 秒，消费端超时必须大于它

`liars_dice_state` 在非当前回合时会挂起，服务端预算是 55 秒（`config.BLOCK_BUDGET`，MCP 层与 REST 层共用这一个常量）。请求里传更大的值会被截断。

任何 MCP 消费端的工具调用超时必须超过 55 秒。DSH 的 `@deepseek-ai/dsh-mcp-client` 默认 `toolCallTimeoutMs: 60000` 勉强够用，配置里写的是 `70000`。调低到 60 秒以下会在服务端返回之前先触发传输层超时，Agent 看到的是超时错误而不是对局状态。

## 3. 游戏身份来自 `player_token`，`mcp-session-id` 只是连接缓存键

`mcp-session-id` 描述的是连接，不是玩家。带 `player_token` 的请求先在所有活动对局里按 token 查找，找到即复用；找不到才走匹配/新建。

违反这条的后果是可复现的：每个 MCP 连接各持一局，两名玩家被分进两局，双方调用 `state` 都得到"轮到你"。`tests/test_mcp_server.py::test_player_token_identity_wins` 就是这个缺陷的回归用例，不要删。

新增游戏时，多人的引擎需要实现 `_find_player_by_token(token)`，身份复用依赖它。

## 4. 多人引擎的 `bid` / `challenge` / `state` 必须强制要求 `player_token`

`player_token` 在这些动作上是必填项，不是可选的加固。`LiarsDiceEngine._validate_player` 缺 token 直接抛错；服务端侧 `_state_player_error` 同样在缺 token 时返回 403。

**症状（曾经真实存在）：** 缺 token 被当成"跳过校验"，于是任何人传一个 `player_index` 就能读到对手的骰子，还能替对手 `bid` / `challenge`。大话骰的全部玩法建立在私有信息上，这条一松，游戏就没有意义了。

只有 `join` 允许不带 token（新玩家入场）。

## 5. 校验必须在等待之前完成

`POST /games/{session_id}/action` 的 `state` 分支顺序是固定的：解析入参 → 校验玩家身份 → 才进入 `wait_for_change`。

**症状：** 校验挪到等待之后，一个越界的 `player_index` 或错误的 `player_token` 会先白等满整个 55 秒预算，然后在 Agent 以为"对手在思考"之后才报错。无效输入必须立刻报错；`_state_player_error()` 在 `wait_for_change` 之前被调用，就是为了这个。

`GET /games/{sid}/state/{idx}` 走同一套校验但不进入任何等待——它是调试与管理界面的查询入口。

## 6. `wait_for_change` 在循环里重判回合谓词

`ChangeSignal.notify()` 是全局广播。3 人及以上的对局里，任何一名玩家叫数都会唤醒所有等待者——包括还没轮到的第三方。

**症状：** 醒来就返回会让 Agent 以为自己可以行动，于是发出一个必然被 `_check_turn` 拒绝的 `bid`，或者在 `state` 里拿到一段"还没轮到你"的文本后再次紧循环。

因此 `wait_for_change` 用一个 `while True` 循环：每轮先 `subscribe()`、再判 `_is_my_turn_or_over()`、再 `wait(剩余预算)`；被唤醒只是回到循环顶部重新判断，剩余预算耗尽才返回 `"timeout"`。`tests/test_wait.py` 覆盖这条，别把它简化回"任何一次唤醒就返回"。

## 7. `timeout` 解析只有一份实现：`config.parse_block_budget`

`timeout` 参数的语义由 `config.parse_block_budget(raw, default=None)` 统一决定，MCP 层（`mcp_server._run_action`）与 REST 层（`game_server.game_action`）都必须调用它：

| 传入 | 语义 |
|---|---|
| 省略 / `None` | 用满当前 `BLOCK_BUDGET`（55 秒） |
| 数值 | 截断到 `[0, 55]`；`0` 表示**本次不等待**，立即返回 |
| `str` / `bool` / `NaN` / `inf` | `ValueError` |

**症状：** 两条传输曾经各有各的写法，语义正好相反——MCP 侧是 `float(args.pop("timeout", None) or BLOCK_BUDGET)`，`timeout=0` 被 `or` 吞成 55 秒；REST 侧是 `min(float(...), BLOCK_BUDGET)`，`0` 就是 `0`。同一个客户端在两条通道上等到的时长不一样。

同理别把 `BLOCK_BUDGET` 写成函数默认值：默认值在 import 时求值一次，之后运维或测试改模块全局就不生效了。`parse_block_budget` 在调用时读，`config.BLOCK_BUDGET` 才能被 monkeypatch。

## 8. `GET /games/{session_id}` 必须剥掉 `SECRET_STATE_KEYS`

`get_public_state()` 是给 admin 用的**全量**视图，含 `target`（猜数字的答案）与 `all_dice`（大话骰全场骰子）。`game_public_state()` 在副本上按 `SECRET_STATE_KEYS` 过滤后才返回给 Agent。

**症状：** 直接把 `get_public_state()` 回给 Agent，等于把答案和对手的骰子一起送出去，游戏当场失去意义。

秘密信息只有 `/admin/games/{id}` 一个出口（过 `_check_admin_token` 之后）。新增带秘密字段的引擎时，在自己的类上声明 `secret_keys`——那是权威来源；`SECRET_STATE_KEYS` 只是"引擎没声明"时的默认集合。

## 9. 回合超时与开盅对"输掉本轮"的处理必须一致

`check_timeout` 的回合超时分支与 `_challenge` 都必须 `self.current_call = None`。

**症状（曾经真实存在）：** 超时分支只掉骰、换回合，不清本轮叫数。下家面对一个
无法再加码的僵尸叫数（数量可能已超过掉骰后的总数），既不能加码也不能弃权，
只能替别人的超时开盅——胜负由别人的超时决定。3 人局可稳定复现。

## 10. `check_timeouts` 必须按引擎隔离异常

per-session 循环要 `try/except Exception`，记日志后继续下一个。

**症状：** 一个引擎的 `check_timeout` 抛异常会中断整轮结算，其余对局不再超时。
REST 的 `state` 分支在请求内直接调用 `check_timeouts()`，所以一个坏引擎能把
**无关对局**打成 500。后台轮询有兜底，请求内路径没有。

## 11. Admin token 永远存在，且绝不交给未出示它的人

`_ADMIN_TOKEN` 在导入 `game_server` 时解析一次：有 `AGENT_GAME_ADMIN_TOKEN` 就用它，没有就 `secrets.token_urlsafe(24)` 当场生成。没有"不设 token 就不校验"这个分支。

`GET /admin` 只在请求方**已经出示**正确 token（`?token=` 或 `X-Admin-Token` 头）时才把 token 注入页面；两者都没有或都不对，仍返回页面骨架，但页面发起的 admin API 调用会 403。

**症状：** 把 token 注入给匿名访问者，等于把 `/admin/games` 的全部秘密信息对任何能连上端口的人敞开；反过来退化成"没配 token 就不校验"更糟，那连"配置错了"都不会有人发现。

`_token_matches()` 按 UTF-8 字节做 `secrets.compare_digest`：`str` 版遇到非 ASCII 会抛 `TypeError`，一个带非 ASCII 的 `X-Admin-Token` 头就能把 403 变成 500。

`/admin/ws` 同样要鉴权：浏览器 WebSocket 不能设自定义头，token 走 `?token=`，
不匹配以 1008 关闭。

## 12. 身份校验必须与传输无关，且在等待之前

校验逻辑在 `games/game_engine.py::player_identity_error()`，REST 与 MCP 共用。
两条传输都必须在进入 `wait_for_change` **之前**调用它。

**症状：** 两条传输各自实现一遍时语义分叉了——REST 先校验再等待，MCP 先等待
再校验。同一个 token 打错的 `state` 调用，REST 立刻 400，MCP 白等满 55 秒。

## 13. 测试不准用 `skip` 给"能力缺失"让路

核心能力被删掉时套件必须红。`pytest.skip` 让"功能删了"和"功能正常"在退出码上
一样——曾经守住阻塞等待的两条主验收用例就是这样写的，删掉整个 `wait_for_change`
之后它们变成 skipped，套件仍然全绿。

## 14. 每条约束都要有对应的看门用例

约束写在本文档里、如果没有任何用例对应它，它实际上只是一段注释。
`tests/test_meta.py::GUARDED_INVARIANTS` 维护这张映射表，新增约束时同步登记。

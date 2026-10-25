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

`liars_dice_state` 在非当前回合时会挂起，服务端预算是 55 秒（`MAX_BLOCK_BUDGET`）。请求里传更大的值会被截断。

任何 MCP 消费端的工具调用超时必须超过 55 秒。DSH 的 `@deepseek-ai/dsh-mcp-client` 默认 `toolCallTimeoutMs: 60000` 勉强够用，配置里写的是 `70000`。调低到 60 秒以下会在服务端返回之前先触发传输层超时，Agent 看到的是超时错误而不是对局状态。

## 3. 游戏身份来自 `player_token`，`mcp-session-id` 只是连接缓存键

`mcp-session-id` 描述的是连接，不是玩家。带 `player_token` 的请求先在所有活动对局里按 token 查找，找到即复用；找不到才走匹配/新建。

违反这条的后果是可复现的：每个 MCP 连接各持一局，两名玩家被分进两局，双方调用 `state` 都得到"轮到你"。`tests/test_mcp_server.py::test_player_token_identity_wins` 就是这个缺陷的回归用例，不要删。

新增游戏时，多人的引擎需要实现 `_find_player_by_token(token)`，身份复用依赖它。

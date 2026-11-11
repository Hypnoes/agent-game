# Agent 接入指南

这份文档写给**要接入本服务的一方**——人或 Agent。内容只有"怎么连、怎么发请求、
怎么读懂回复"，不需要读源码。

本文不涉及服务端实现，也不假设你用什么语言。所有例子都是可以直接发送的
HTTP 请求。

---

## 一、两种接入方式

| 方式 | 适合 | 入口 |
|---|---|---|
| **MCP**（推荐） | 支持 MCP 的 Agent 运行时 | `http://127.0.0.1:8800/mcp` |
| **REST** | 自己写 HTTP 客户端 / 调试 | `http://127.0.0.1:8800/api/v1/...` |

两条通道**行为一致**：同一个动作、同一份校验、同一份错误分类。区别只在报文格式。

> 仓库里**没有**客户端可执行文件。不需要装、不需要跑中间层——Agent 运行时
> 自带的 MCP 客户端直接连服务端的 `/mcp`。

---

## 二、MCP 接入（推荐）

### 配置

```yaml
mcp:
  servers:
    agent_game:
      transport: streamable-http
      url: http://127.0.0.1:8800/mcp
      toolCallTimeoutMs: 70000
```

三个字段都不能少：

| 字段 | 值 | 为什么 |
|---|---|---|
| `transport` | `streamable-http` | 服务端的 `/mcp` 只接 Streamable HTTP；原 stdio 客户端已在 002 里删除 |
| `url` | 结尾**不要**加尾斜杠 | `/mcp/` 会 307，而 MCP 客户端不跟随重定向 |
| `toolCallTimeoutMs` | **≥ 70000** | 服务端的等待预算是 55 秒，客户端必须留出余量 |

`toolCallTimeoutMs` 配小了会怎样：服务端还在等，客户端先超时，你拿到的是
**传输层错误**而不是对局状态——看起来像服务挂了，其实只是"对手还没动"。

### 工具清单

工具名带 `mcp__agent_game__` 前缀（由 `serverName: agent_game` 决定），
去掉前缀后是：

| 游戏 | 工具 |
|---|---|
| `guess_number` | `guess_number_start` / `guess_number_guess` / `guess_number_state` |
| `liars_dice` | `liars_dice_join` / `liars_dice_state` / `liars_dice_bid` / `liars_dice_challenge` |

也可以直接问服务端要最新清单：

```bash
curl http://127.0.0.1:8800/api/v1/games/tools
```

### 最小可跑的例子（猜数字，单人）

```
1. guess_number_start          → 开一局，拿到 session_id 与提示词
2. guess_number_guess number=50 → 返回 too_low / too_high / correct
3. guess_number_state          → 随时查当前状态
```

每次调用的返回值里都带一段 `prompt`，那是**给 Agent 读的下一步指示**——
直接读它，不需要自己解析状态字段。

---

## 三、REST 接入

### 端点

| 方法与路径 | 作用 |
|---|---|
| `GET /health` | 存活与游戏清单 |
| `GET /api/v1/games/tools` | 全部游戏的工具声明 |
| `GET /api/v1/games/tools/{game_type}` | 某个游戏的工具声明 |
| `POST /api/v1/games/{game_type}` | 开一局，返回 `session_id` |
| `GET /api/v1/games/{session_id}` | 公开状态 |
| `GET /api/v1/games/{session_id}/players/{index}` | 玩家视角（**要带 token 头**） |
| `POST /api/v1/games/{session_id}/actions` | 执行一个动作 |

### 请求形状

```bash
# 开一局
curl -X POST http://127.0.0.1:8800/api/v1/games/guess_number \
  -H 'content-type: application/json' -d '{}'

# 执行动作
curl -X POST http://127.0.0.1:8800/api/v1/games/<session_id>/actions \
  -H 'content-type: application/json' \
  -d '{"action": "guess", "number": 50}'

# 看自己的骰子（多人游戏必须带 token 头）
curl http://127.0.0.1:8800/api/v1/games/<session_id>/players/0 \
  -H "x-player-token: <你的 token>"
```

---

## 四、五个最容易搞错的点

这五条都来自真实踩过的坑，每条都附了症状。

### 1. `session_id` 不是身份，`player_token` 才是

| | 是什么 |
|---|---|
| `Mcp-Session-Id` | **连接**的缓存键。换一条连接就变了 |
| `session_id` | **对局**的标识 |
| `player_token` | **你是谁** |

症状：把 `Mcp-Session-Id` 当身份用，每开一个新连接就各持一局，两名玩家被分进
两局——服务端显示"开了 2 局，每局 1 个人"。

正确做法：`join` 时把服务端返回的 `player_token` **存下来**，之后所有
`bid` / `challenge` / `state` 都带上它。带已有 token 的请求会先在所有活动对局
里按 token 查找，找到就复用——这就是断线重连。

### 2. 多人游戏的动作必须带 `player_token`

`liars_dice` 的 `bid` / `challenge` / `state` 缺 token **直接拒绝**，不是"跳过校验"。

只有 `join` 允许不带 token（新玩家入场）。

症状：以为"不传 token 就是不校验"，于是任何人传一个 `player_index` 就能读到
对手的骰子，还能替对手叫数。

### 3. `state` 会阻塞，慢是正常的

`state` 会等到**有变化**或**预算耗尽**（默认 55 秒）才返回。

症状：拿到"还没轮到你"的回复就立刻再调一次，形成没有退避的紧循环轮询，
一个回合内消耗几十次工具调用。

正确做法：一次 `state` 调用等满就行。它返回的原因只有两个——轮到你/对局结束，
或者预算用完了。看到后面这种，继续等就好。

### 4. `timeout` 参数的三条语义

| 传入 | 含义 |
|---|---|
| 省略 / `null` | 用满服务端预算（55 秒） |
| 数值 | 夹到 `[0, 55]`；**`0` 表示本次不等待**，立即返回 |
| `bool` / 字符串 / `NaN` / `inf` | 错误 |

症状：以为 `timeout=0` 等同于"省略"，实际它立刻返回——拿到的是中间状态。

### 5. 工具参数名与动作参数名不一样

MCP 工具的参数是动作参数的**平铺**，没有 `action` 字段（工具名已经说明动作）。
REST 的 `/actions` 则要显式带 `"action": "..."`。

---

## 五、错误怎么读

所有失败都是结构化的，不要靠匹配错误文本。

| HTTP 状态 | 含义 | 你该做什么 |
|---|---|---|
| 400 | 参数错（缺字段、类型错、值越界） | 修参数重发 |
| 403 | 身份错（token 不匹配、缺 token） | 检查 token，**不要重试** |
| 404 | 会话或玩家不存在 | 会话可能被回收了，重新开局 |
| 500 | 服务端内部错误 | 可以重试；持续失败则查服务端日志 |

**403 与 400 的区别**：403 是"你不该做这件事"，重试没用；400 是"你这么写不对"，
改了就能过。

---

## 六、Admin（一般用不到）

只在需要看秘密信息时用（比如排查"骰子到底发了什么"）。

| 端点 | 作用 |
|---|---|
| `GET /admin` | 管理页面（浏览器） |
| `GET /api/v1/admin/games` | 全部对局 + 秘密信息 |
| `GET /api/v1/admin/games/{id}` | 单局详情 |
| `GET /api/v1/admin/ws` | 实时推送（WebSocket，token 走 `?token=`） |
| `POST /api/v1/admin/prompts/reload` | 重载提示词 |

所有 admin 端点都要 `x-admin-token`。token 由服务端启动时决定：设了
`AGENT_GAME_ADMIN_TOKEN` 就用它，没设则随机生成并打印在启动日志里。

**没有"不配 token 就不校验"这个分支。**

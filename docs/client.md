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

> **下面是语义示意，不是可直接粘贴的配置。** 各 Agent 运行时的 MCP 配置结构
> 不同——DeepSeek Harness 用的是插件列表（`id` / `name: '@deepseek-ai/dsh-mcp-client'`
> / `config`），完整写法见仓库 README。

```yaml
# 语义示意
serverName: agent_game
transport: streamable-http
url: http://127.0.0.1:8800/mcp
toolCallTimeoutMs: 70000
```

四个字段的**含义**是通用的：

| 字段 | 值 | 为什么 |
|---|---|---|
| `serverName` | `agent_game` | 决定工具名前缀（`mcp__agent_game__...`） |
| `transport` | `streamable-http` | 服务端的 `/mcp` 只接 Streamable HTTP；原 stdio 客户端已在 002 里删除 |
| `url` | 结尾**不要**加尾斜杠 | `/mcp/` 会 307，而 MCP 客户端不跟随重定向 |
| `toolCallTimeoutMs` | **大于服务端等待预算** | 见下 |

**`toolCallTimeoutMs` 配小了会怎样**：服务端还在等，客户端先超时，你拿到的是
**传输层错误**而不是对局状态——看起来像服务挂了，其实只是"对手还没动"。

服务端的等待预算**默认 55 秒，可配置**（`Config::block_budget`）。
`70000` 是默认配置下的安全值，不是普遍下限——如果你的服务端调大了预算，
客户端超时要跟着调。

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
1. guess_number_start           → 开一局
2. guess_number_guess number=50 → 猜一个数
3. guess_number_state           → 随时查当前状态
```

**MCP 工具只返回一段 `prompt` 文本**（给 Agent 读的下一步指示），
`session_id` 留在服务端缓存里，不保证出现在返回值中。所以：

- 不需要自己解析状态字段——直接读 `prompt`
- 也不需要自己传 `session_id`——同一连接上服务端会记住它
- 只有在**换连接**或**明确想指定某局**时，才需要自己传 `session_id`

想拿到结构化的 `session_id` 与状态字段，用下面第三节的 REST。

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
| `GET /api/v1/games/{session_id}/players/{index}` | 玩家视角（**多人游戏**要带 token 头） |
| `POST /api/v1/games/{session_id}/actions` | 执行一个动作 |

### 完整走一局（大话骰，两人）

这是唯一需要自己管身份的路径——**`join` 返回的 `player_token` 必须存下来**，
之后每个动作都带上它。

```bash
# 1. 开一局（或并进已有的等待桌）
SID=$(curl -s -X POST http://127.0.0.1:8800/api/v1/games/liars_dice \
  -H 'content-type: application/json' \
  -d '{"player_count": 2, "dice_per_player": 3}' | jq -r .session_id)

# 2. 入场，**把 token 存下来**
TOKEN=$(curl -s -X POST http://127.0.0.1:8800/api/v1/games/$SID/actions \
  -H 'content-type: application/json' \
  -d '{"action": "join", "player_name": "Alice"}' | jq -r .private.player_token)

# 3. 等轮到自己（这个调用会阻塞到轮到你或预算耗尽）
curl -s -X POST http://127.0.0.1:8800/api/v1/games/$SID/actions \
  -H 'content-type: application/json' \
  -d "{\"action\": \"state\", \"player_index\": 0, \"player_token\": \"$TOKEN\"}"

# 4. 叫数（quantity 个 face 点）
curl -s -X POST http://127.0.0.1:8800/api/v1/games/$SID/actions \
  -H 'content-type: application/json' \
  -d "{\"action\": \"bid\", \"player_index\": 0, \"player_token\": \"$TOKEN\",
       \"quantity\": 2, \"face\": 3}"

# 5. 或者开上一手盅
curl -s -X POST http://127.0.0.1:8800/api/v1/games/$SID/actions \
  -H 'content-type: application/json' \
  -d "{\"action\": \"challenge\", \"player_index\": 0, \"player_token\": \"$TOKEN\"}"
```

跑第二次时把 `SID` 与 `TOKEN` 都复用——带已有 token 的 `join` 会把你送回原对局
（断线重连），不会新开一局。

> MCP 通道下**不用**这么麻烦：同一连接上服务端记住 `session_id`，
> 工具返回值里直接带 `prompt`。需要精确控制时再用上面的 REST。
### 请求形状

```bash
# 开一局
curl -X POST http://127.0.0.1:8800/api/v1/games/guess_number \
  -H 'content-type: application/json' -d '{}'

# 执行动作
curl -X POST http://127.0.0.1:8800/api/v1/games/<session_id>/actions \
  -H 'content-type: application/json' \
  -d '{"action": "guess", "number": 50}'

# 看自己的骰子（多人游戏必须带 token 头；单人游戏不需要）
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

**只有 `POST .../actions` 且 `action=state` 会等待。** 两个 GET 状态端点
（`GET .../games/{id}` 与 `GET .../players/{index}`）**立即返回**。

`state` 的返回条件是三选一：

- 轮到了**你**（不是"有任何变化"——别人叫了一手但还没轮到你时，它继续等）
- 对局已结束
- 预算耗尽

症状：拿到"还没轮到你"的回复就立刻再调一次，形成没有退避的紧循环轮询，
一个回合内消耗几十次工具调用。

正确做法：一次 `state` 调用等满就行。

### 4. `timeout` 参数的三条语义

| 传入 | 含义 |
|---|---|
| 省略 / `null` | 用满服务端**当前**预算（默认 55 秒，可配置） |
| 数值 | 夹到 `[0, 当前预算]`；**`0` 表示本次不等待**，立即返回 |
| `bool` / 字符串 / `NaN` / `inf` | 错误 |

症状：以为 `timeout=0` 等同于"省略"，实际它立刻返回——拿到的是中间状态。

### 5. 工具参数名与动作参数名不一样

MCP 工具的参数是动作参数的**平铺**，没有 `action` 字段（工具名已经说明动作）。
REST 的 `/actions` 则要显式带 `"action": "..."`。

---

## 五、错误怎么读

**这一节只适用于 REST。** MCP 通道的失败不用 HTTP 状态码——它以
`isError: true` 的工具结果返回，错误文本在内容里。客户端应该检查 `isError`，
不要解析文本。

REST 的**业务错误**（由 API handler 返回的）都是结构化的
（`{"error": {"code": "...", "message": "..."}}`），不要靠匹配错误文本。

> 框架层的拒绝——路由没匹配、路径或查询参数提取失败——不保证用同一个
> envelope。这类错误通常连 handler 都没进，看到的是框架自己的格式。

| HTTP 状态 | 含义 | 你该做什么 |
|---|---|---|
| 400 | 参数错：缺字段、类型错、值越界、**玩家索引越界**、未知 action | 修参数重发 |
| 403 | 身份错：缺 `player_token`、token 不匹配 | 检查 token，**不要重试** |
| 404 | 框架层找不到东西（如对局已被回收） | 重新开局 |
| 409 | 引擎已毒化（之前的 panic 被隔离，对局不可继续） | 重开一局，不要重试 |
| 500 | 服务端内部错误 | **不要盲目重放动作**——先确认它是否已执行；持续失败查日志 |

三条容易搞错的：

- **玩家索引越界是 400，不是 404。** 引擎报 `InvalidIndex` / `NotFound` 时，
  框架统一映射成 400——那是"你这个参数不对"，不是"资源不存在"。
- **403 与 400 的区别**：403 是"你不该做这件事"，重试没用；400 是"你这么写
  不对"，改了就能过。
- **这张表是常见状态，不是完整清单。**



---

## 六、Admin（一般用不到）

只在需要看秘密信息时用（比如排查"骰子到底发了什么"）。

**五个**路由，鉴权方式**不统一**：

| 端点 | 作用 | 凭证 |
|---|---|---|
| `GET /admin` | 管理页面（浏览器） | **可选**，见下 |
| `GET /api/v1/admin/games` | 全部对局 + 秘密信息 | `x-admin-token` 头 |
| `GET /api/v1/admin/games/{id}` | 单局详情 | `x-admin-token` 头 |
| `POST /api/v1/admin/prompts/reload` | 重载提示词 | `x-admin-token` 头 |
| `GET /api/v1/admin/ws` | 实时推送（WebSocket） | `?token=`（浏览器不能设自定义头） |

**`GET /admin` 是特例**：没有凭证也返回页面骨架（HTML 本身不含秘密），
但只有带了正确的 `?token=` 或 `x-admin-token` 时才注入 `window.ADMIN_TOKEN`，
页面才能拉到数据。没带或带错时，页面发起的 API 调用会 403。

token 由服务端启动时决定：设了 `AGENT_GAME_ADMIN_TOKEN` 就用它，没设则随机
生成并打印在启动日志里。**没有"不配 token 就不校验"这个分支。**

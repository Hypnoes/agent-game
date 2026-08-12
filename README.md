# agent-game

AI Agent 游戏框架 — 新增游戏只需写一个 Engine 文件，MCP 工具、API 路由、Admin 查询全部自动生成。

## 架构

```mermaid
flowchart TB
    subgraph Agent["Agent (Claude / Cursor / Hermes)"]
        LLM[LLM]
    end

    subgraph Client["MCP Client (agent-game-mcp)"]
        ML["动态工具加载\nGET /games/tools"]
        PF["Prompt 透传\n(引擎自生成)"]
    end

    subgraph Server["FastAPI Game Server :8800"]
        GM[GameManager\nsession 路由 + 生命周期]
        subgraph Games["Games"]
            GN[GuessNumberEngine]
            LD[LiarsDiceEngine]
        end
        API["/games/*\n/admin/*\nWebSocket /admin/ws"]
    end

    LLM <-->|"stdio JSON-RPC"| Client
    Client <-->|"HTTP"| Server
    GM --> GN
    GM --> LD
    API --> GM

    style Agent fill:#1f6feb22,stroke:#58a6ff
    style Client fill:#3fb95022,stroke:#3fb950
    style Server fill:#d2991d22,stroke:#d2991d
```

**零改动加游戏:** 新 Engine 实现自描述协议 → `GameManager.register()` → 全部自动生效。

## 项目结构

```
agent-game/
├── main.py                            # 启动入口
├── src/
│   ├── agent_game_mcp/                # Agent 侧客户端
│   │   ├── __init__.py
│   │   └── client.py                  # 动态加载工具 + Prompt 透传
│   └── agent_game_server/             # 服务端
│       ├── __init__.py                # 包 re-export
│       ├── manager.py                 # GameManager（session 路由）
│       ├── game_server.py             # Game API + Admin + WebSocket (port 8800)
│       ├── cli.py                     # agent-game-server CLI
│       ├── prompt.py                  # 模板/Prompt 渲染
│       ├── templates/
│       │   └── admin.html             # 管理仪表盘
│       ├── assets/
│       │   ├── guess_number_prompts.py
│       │   └── liars_dice_prompts.py
│       └── games/
│           ├── __init__.py
│           ├── game_engine.py         # Engine 协议
│           ├── state_machine.py       # 通用状态机
│           ├── guess_number.py        # 猜数字引擎
│           └── liars_dice.py          # 大话骰引擎
├── tests/
│   ├── test_engine.py
│   ├── test_liars_dice.py
│   ├── test_manager.py
│   └── test_matching.py
├── pyproject.toml
├── README.md
└── pytest/
```

## 引擎自描述协议

新增游戏只需实现以下接口（duck typing，不强制基类）：

| 属性/方法 | 说明 |
|-----------|------|
| `game_type: str` | 唯一标识，如 `"my_game"` |
| `game_name: str` | 人类可读名称 |
| `player_mode: str` | `"single"` 或 `"multi"` |
| `describe_tools()` | 返回 MCP Tool 列表 `[{name, description, input_schema}]` |
| `handle_action(action, **params)` | 执行动作，返回 `{prompt, ...}` |
| `get_public_state()` | Admin 可见的公开状态 |
| `get_player_state(player_index)` | 玩家视角状态 + prompt 文本 |
| `check_timeout(now)` | 超时检查 |

## 快速开始

```bash
# 安装依赖
uv sync

# 启动服务端（Game API + Admin，端口 8800）
uv run main.py

# 或安装为全局命令
uv tool install .
agent-game-server
```

Agent MCP 配置：
```json
{"mcpServers": {"agent-game": {"command": "agent-game-mcp"}}}
```

默认环境变量：
- `AGENT_GAME_SERVER`：Game Server 地址，默认 `http://localhost:8800`
- `AGENT_GAME_ADMIN_TOKEN`：Admin 鉴权令牌，未显式设置时会随机生成

## 当前游戏

| 游戏 | 类型 | 说明 |
|------|------|------|
| 猜数字 | 单人 | 1-100 范围内猜目标数字 |
| 大话骰 | 多人 | 吹牛/撒谎骰子，轮流叫数，1 百搭 |

## 管理后台

http://localhost:8800/admin

- 提供 Admin Dashboard HTML
- 提供 `/admin/games` 列表接口
- 提供 `/admin/games/{session_id}` 详情接口
- 提供 WebSocket `/admin/ws` 实时推送状态更新
- 需要通过 `X-Admin-Token` 头校验权限；启动时默认自动生成 token

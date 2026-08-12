"""
agent-game Client (MCP)

Agent 侧客户端。启动时从 Game Server 动态拉取工具列表。
自动管理 session_id — Agent 不需要手动传。
**不包含任何游戏逻辑** — 零 engine 引用，纯转发。

配置：
    环境变量 AGENT_GAME_SERVER  默认 http://localhost:8800
"""

import asyncio
import json
import os
import signal as _signal
import sys as _sys

import httpx
from mcp import types
from mcp.server.lowlevel import Server
from mcp.server.stdio import stdio_server

# Ctrl+C 优雅退出
_signal.signal(_signal.SIGINT, lambda s, f: _sys.exit(0))

GAME_SERVER = os.environ.get("AGENT_GAME_SERVER", "http://localhost:8800")

_TOOLS: list[types.Tool] = []
_SESSIONS: dict[str, str] = {}  # game_type → session_id (automatic tracking)


async def _load_tools():
    global _TOOLS
    async with httpx.AsyncClient(timeout=5.0) as c:
        resp = await c.get(f"{GAME_SERVER}/games/tools")
        resp.raise_for_status()
        _TOOLS = [types.Tool(**t) for t in resp.json()]


def _parse_tool_name(name: str) -> tuple[str, str]:
    """解析 {game_type}_{action}"""
    parts = name.split("_")
    # 尝试前两段作为 game_type（如 guess_number）
    known = set()
    for t in _TOOLS:
        prefix = "_".join(t.name.split("_")[:2])
        known.add(prefix)
    for n in range(min(3, len(parts)), 0, -1):
        candidate = "_".join(parts[:n])
        if candidate in known:
            return candidate, "_".join(parts[n:])
    # fallback
    return parts[0], "_".join(parts[1:])


# ═══════════════════════════════════════════
# Handlers
# ═══════════════════════════════════════════


async def handle_list_tools(ctx, params) -> types.ListToolsResult:
    return types.ListToolsResult(tools=_TOOLS)


async def handle_call_tool(
    ctx, params: types.CallToolRequestParams
) -> types.CallToolResult:
    name = params.name
    args = dict(params.arguments or {})

    async with httpx.AsyncClient(timeout=10.0) as client:
        try:
            game_type, action = _parse_tool_name(name)

            # 自动管理 session_id
            session_id = args.pop("session_id", None) or _SESSIONS.get(game_type)
            if not session_id:
                # 新建 session
                resp = await client.post(f"{GAME_SERVER}/games/{game_type}")
                if resp.status_code != 200:
                    text = f"❌ 创建游戏失败: {resp.text}"
                    return types.CallToolResult(
                        content=[types.TextContent(type="text", text=text)]
                    )
                session_id = resp.json()["session_id"]

            # 记住 session_id（后续调用自动注入）
            _SESSIONS[game_type] = session_id

            # 执行 action
            body = {"action": action, "session_id": session_id, **args}
            resp = await client.post(
                f"{GAME_SERVER}/games/{session_id}/action",
                json=body,
            )
            if resp.status_code >= 400:
                text = f"❌ {resp.json().get('detail', resp.text)}"
            else:
                data = resp.json()
                text = data.get("prompt", json.dumps(data))

        except httpx.ConnectError:
            text = f"❌ 无法连接 Game Server ({GAME_SERVER})。请先启动 Game Server。"
        except (httpx.HTTPError, ValueError, TypeError) as e:
            text = f"❌ 错误: {e}"

    return types.CallToolResult(content=[types.TextContent(type="text", text=text)])


# ═══════════════════════════════════════════
# Server 实例 + CLI 入口
# ═══════════════════════════════════════════

server = Server(
    "agent-game",
    on_list_tools=handle_list_tools,
    on_call_tool=handle_call_tool,
)


async def _run():
    await _load_tools()
    async with stdio_server() as (read_stream, write_stream):
        await server.run(
            read_stream, write_stream, server.create_initialization_options()
        )


def main():
    if len(_sys.argv) > 1 and _sys.argv[1] in ("-h", "--help"):
        print("agent-game-mcp — AI Agent 游戏客户端 (MCP)")
        print()
        print("MCP 服务器，通过 stdio 与 AI Agent 通信。")
        print("启动时从 Game Server 动态加载已注册的游戏工具。")
        print("自动管理 session_id — Agent 不需要手动传。")
        print()
        print("配置：")
        print('  {"mcpServers": {"agent-game": {"command": "agent-game-mcp"}}}')
        print()
        print("环境变量：")
        print("  AGENT_GAME_SERVER    Game Server 地址 (默认 http://localhost:8800)")
        return

    if _sys.stdin.isatty():
        print("agent-game-mcp — AI Agent 游戏客户端 (MCP)")
        print()
        print("⚠️  不是交互式命令行工具。")
        print("通过 stdio 与 AI Agent 通信。用 --help 查看说明。")
        print()

    asyncio.run(_run())


if __name__ == "__main__":
    main()

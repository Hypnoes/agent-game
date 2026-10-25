"""
MCP over HTTP 适配层。

把 GameManager 的工具描述与动作路由翻译成 MCP 协议原语，不含任何游戏逻辑。
game_server.py 用 StreamableHTTPSessionManager 把它挂到 /mcp 端点。

身份约束：游戏归属由 player_token 决定，mcp-session-id 只是连接级缓存键。
同一个 player_token 无论从哪条 MCP 连接进来，都必须回到它原来所在的对局。
"""

import json
from typing import Any

from mcp import types
from mcp.server.lowlevel import Server

from agent_game_server.manager import GameManager

# state 动作的等待预算上限（秒）。001 在服务端把传入值截断到这个数，
# 使游戏服务端总是先于 MCP 消费端的工具调用超时产生响应。
MAX_BLOCK_BUDGET = 55.0

# mcp-session-id → {game_type: session_id}
# 仅作连接级缓存；它描述连接，不描述玩家。
_MCP_SESSIONS: dict[str, dict[str, str]] = {}

# 非 HTTP 传输（ctx.request 为 None）下的回落键，避免每次调用都新建缓存。
_FALLBACK_SESSION_KEY = "__no_http_session__"

# 判定对局已结束的状态名。不同引擎的终态名不同，统一在这里识别。
_FINISHED_PHASES = frozenset({"finished", "won", "lost"})


# ═══════════════════════════════════════════
# 工具名解析
# ═══════════════════════════════════════════


def _tool_prefixes(tools: list[dict]) -> set[str]:
    """从已注册工具名收集 game_type 前缀（前两段）。"""
    return {"_".join(tool["name"].split("_")[:2]) for tool in tools}


def _parse_tool_name(name: str, known_prefixes: set[str]) -> tuple[str, str]:
    """把 {game_type}_{action} 解析成 (game_type, action)。

    用已注册工具名的前缀集合做最长优先匹配，避免 guess_number 与假设中的
    guess 游戏互相误判。前缀集合为空时回落到按第一段切分。
    """
    parts = name.split("_")
    for n in range(min(3, len(parts)), 0, -1):
        candidate = "_".join(parts[:n])
        if candidate in known_prefixes:
            return candidate, "_".join(parts[n:])
    return parts[0], "_".join(parts[1:])


# ═══════════════════════════════════════════
# 会话与身份
# ═══════════════════════════════════════════


def _session_key(ctx: Any) -> str:
    """取当前连接的 mcp-session-id；拿不到时回落到固定键。"""
    request = getattr(ctx, "request", None)
    if request is None:
        return _FALLBACK_SESSION_KEY
    return request.headers.get("mcp-session-id") or _FALLBACK_SESSION_KEY


def _engine_phase(engine: Any) -> str | None:
    """引擎当前状态名。liars_dice 用 phase，guess_number 用 status。"""
    return getattr(engine, "phase", getattr(engine, "status", None))


def _engine_finished(engine: Any) -> bool:
    return _engine_phase(engine) in _FINISHED_PHASES


def _find_active_session(
    manager: GameManager, game_type: str, player_token: str
) -> str | None:
    """在所有该类型的活动对局中按 player_token 查找。找到返回 session_id。"""
    for session_id, engine in list(manager._engines.items()):
        if getattr(engine, "game_type", "") != game_type:
            continue
        if _engine_finished(engine):
            continue
        finder = getattr(engine, "_find_player_by_token", None)
        if finder is None:
            continue
        if finder(player_token) is not None:
            return session_id
    return None


# ═══════════════════════════════════════════
# 动作执行
# ═══════════════════════════════════════════


async def _run_action(engine: Any, action: str, args: dict) -> Any:
    """执行单个动作。

    state 动作在引擎提供 wait_for_change 时先阻塞等待：非当前回合的调用会挂起，
    直到对手行动、对局结束或预算耗尽。等待结束后仍交给 handle_action，
    保持玩家身份校验等原有语义。引擎没有该方法时立即返回。
    """
    if action == "state":
        # timeout 只是传输层的等待预算，不传给 handle_action
        timeout = float(args.pop("timeout", None) or MAX_BLOCK_BUDGET)
        waiter = getattr(engine, "wait_for_change", None)
        if waiter is not None:
            # 截断到上限，确保服务端先于消费端超时产生响应
            budget = min(timeout, MAX_BLOCK_BUDGET)
            if budget > 0:
                await waiter(int(args.get("player_index") or 0), budget)
    return engine.handle_action(action, **args)


async def _execute(manager: GameManager, ctx: Any, name: str, args: dict) -> str:
    """一次工具调用的完整流程：解析工具名 → 定位对局 → 执行 → 取文本。"""
    game_type, action = _parse_tool_name(name, _tool_prefixes(manager.describe_all_tools()))

    cache = _MCP_SESSIONS.setdefault(_session_key(ctx), {})

    explicit_session_id = args.pop("session_id", None)
    player_token = args.get("player_token")
    create_kwargs: dict[str, Any] = {}
    if player_token:
        create_kwargs["player_token"] = player_token

    # start 强制换新对局：先清缓存，且不做身份复用
    starting = action == "start"
    if starting:
        cache.pop(game_type, None)

    session_id = explicit_session_id
    if not session_id and player_token and not starting:
        # 身份优先：同一 player_token 永远回到它所在的对局，而不是各连接各开一局
        session_id = _find_active_session(manager, game_type, player_token)

    if not session_id:
        session_id = cache.get(game_type)
        # join 前先看缓存的会话是否还在、是否已结束
        if session_id and action == "join":
            engine = manager.get(session_id)
            if engine is None or _engine_finished(engine):
                cache.pop(game_type, None)
                session_id = None

    if not session_id:
        session_id, _engine = manager.create(game_type, **create_kwargs)

    engine = manager.get(session_id)
    if engine is None and explicit_session_id is None and not starting:
        # 缓存的会话已被回收（等价于 404）：重建后重试一次
        cache.pop(game_type, None)
        session_id, engine = manager.create(game_type, **create_kwargs)
    if engine is None:
        raise LookupError(f"会话不存在: {session_id}")

    cache[game_type] = session_id

    data = await _run_action(engine, action, args)
    if isinstance(data, dict) and data.get("prompt"):
        return str(data["prompt"])
    return json.dumps(data, ensure_ascii=False, default=str)


# ═══════════════════════════════════════════
# Server 构造
# ═══════════════════════════════════════════


def build_mcp_server(manager: GameManager) -> Server:
    """构造 MCP Server。工具定义直接复用各引擎的 describe_tools()。"""

    async def handle_list_tools(ctx, params) -> types.ListToolsResult:
        return types.ListToolsResult(
            tools=[types.Tool(**tool) for tool in manager.describe_all_tools()]
        )

    async def handle_call_tool(ctx, params: types.CallToolRequestParams) -> types.CallToolResult:
        try:
            text = await _execute(manager, ctx, params.name, dict(params.arguments or {}))
        except Exception as exc:  # 进程内异常转成 MCP 错误结果，不抛给传输层
            return types.CallToolResult(
                content=[types.TextContent(type="text", text=f"❌ {exc}")],
                isError=True,
            )
        return types.CallToolResult(
            content=[types.TextContent(type="text", text=text)],
            isError=False,
        )

    return Server(
        "agent-game",
        version="0.2.2",
        on_list_tools=handle_list_tools,
        on_call_tool=handle_call_tool,
    )

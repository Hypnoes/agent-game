"""
MCP over HTTP 适配层。

把 GameManager 的工具描述与动作路由翻译成 MCP 协议原语，不含任何游戏逻辑。
game_server.py 用 StreamableHTTPSessionManager 把它挂到 /mcp 端点。

身份约束：游戏归属由 player_token 决定，mcp-session-id 只是连接级缓存键。
同一个 player_token 无论从哪条 MCP 连接进来，都必须回到它原来所在的对局。
"""

import json
import logging
from typing import Any

from mcp import types
from mcp.server.lowlevel import Server

from agent_game_server.config import (
    MAX_MCP_SESSION_CACHE,
    __version__,
    parse_block_budget,
)
from agent_game_server.manager import GameManager

logger = logging.getLogger(__name__)

# mcp-session-id → {game_type: session_id}
# 仅作连接级缓存；它描述连接，不描述玩家。容量由 MAX_MCP_SESSION_CACHE 限制，
# 插入顺序即淘汰顺序（见 _remember_session）。
_MCP_SESSIONS: dict[str, dict[str, str]] = {}

# 非 HTTP 传输（ctx.request 为 None）下的回落键，避免每次调用都新建缓存。
_FALLBACK_SESSION_KEY = "__no_http_session__"

# 判定对局已结束的状态名。不同引擎的终态名不同，统一在这里识别。
_FINISHED_PHASES = frozenset({"finished", "won", "lost"})

# 只有这些动作可以凭空调出一个新会话。其它动作没有可解析的会话时直接报错，
# 否则像 liars_dice_bid 这种漏传会话的调用会先建出一个空大厅再失败。
_STARTING_ACTIONS = frozenset({"start", "join"})


class SessionNotFound(LookupError):
    """调用方给的 session_id 找不到对应对局。

    单独一个类型而不用裸 LookupError：LookupError 同时是 KeyError/IndexError 的
    基类，拿它当"可预期的输入错误"会把越界访问这类真 bug 一并静默掉。
    """


# 可预期的调用方错误：message 直接回给调用方，不写堆栈。
_EXPECTED_ERRORS = (ValueError, SessionNotFound)


# ═══════════════════════════════════════════
# manager 私有字段的访问替身
# ═══════════════════════════════════════════


def _iter_engines(manager: GameManager) -> list[tuple[str, Any]]:
    """活动对局列表 [(session_id, engine)]。

    GameManager 目前没有公开的迭代接口，这里读它的私有字典。所有对
    manager._engines 的访问都收在这一个函数里：等 manager.py 下次被改动时，
    把它换成 GameManager 的公开方法即可，本模块其余部分不用动。
    返回浅拷贝，避免遍历过程中有对局被移除。
    """
    return list(manager._engines.items())


def _iter_registry(manager: GameManager) -> list[tuple[str, type[Any]]]:
    """已注册游戏 [(game_type, engine_cls)]。同 _iter_engines，隔离私有字段。"""
    return list(manager._registry.items())


# ═══════════════════════════════════════════
# 工具名解析
# ═══════════════════════════════════════════


def _tool_index(manager: GameManager) -> dict[str, tuple[str, str]]:
    """{tool_name: (game_type, action)}，由注册表里的引擎自己声明。

    不按切分位置猜：工具名由引擎的 describe_tools() 定义，游戏类型的段数不受
    约束（nim 是一段，mega_liars_dice 是三段），"取前两段当前缀"必然出错。
    这里只做一件事：把引擎声明的 game_type 前缀从工具名上剥掉，剩下的就是 action。
    """
    index: dict[str, tuple[str, str]] = {}
    for game_type, engine_cls in _iter_registry(manager):
        describe = getattr(engine_cls, "describe_tools", None)
        if describe is None:
            continue
        prefix = f"{game_type}_"
        for tool in describe():
            tool_name = str(tool.get("name", ""))
            if not tool_name.startswith(prefix):
                continue
            action = tool_name[len(prefix) :]
            if action:
                index[tool_name] = (game_type, action)
    return index


def _parse_tool_name(manager: GameManager, name: str) -> tuple[str, str]:
    """把工具名解析成 (game_type, action)。未知工具名抛 ValueError。

    首选注册表精确匹配。只有注册表没命中时才按已注册的 game_type 前缀兜底
    （保持既有工具名的解析结果不变）；未注册的前缀一律报错，不拿拼错的工具名
    去新建对局。
    """
    index = _tool_index(manager)
    resolved = index.get(name)
    if resolved is not None:
        return resolved

    # 兜底：按已注册的 game_type 前缀剥离。最长优先，避免同时注册 guess 与
    # guess_number 时把 guess_number_state 判给 guess。
    for game_type, _engine_cls in sorted(
        _iter_registry(manager), key=lambda item: len(item[0]), reverse=True
    ):
        prefix = f"{game_type}_"
        if name.startswith(prefix) and len(name) > len(prefix):
            return game_type, name[len(prefix) :]

    raise ValueError(f"未知工具: {name}。已注册: {sorted(index)}")


# ═══════════════════════════════════════════
# 会话与身份
# ═══════════════════════════════════════════


def _session_key(ctx: Any) -> str:
    """取当前连接的 mcp-session-id；拿不到时回落到固定键。"""
    request = getattr(ctx, "request", None)
    if request is None:
        return _FALLBACK_SESSION_KEY
    return request.headers.get("mcp-session-id") or _FALLBACK_SESSION_KEY


def _remember_session(key: str) -> dict[str, str]:
    """取出某条连接的缓存（没有就建），并把它记为最近使用。

    这份缓存按 mcp-session-id 记账，键不会自己消失：SDK 的
    session_idle_timeout 只回收 SDK 自己的会话，不会通知这里，所以一次性连接
    留下的小字典会一直留到进程结束。因此加一个容量上限，超出时按插入顺序淘汰
    最旧的一条（dict 保序）。被访问的连接会移到末尾，淘汰落在真正闲置最久的
    连接上，正在用的连接不会被挤掉。
    """
    cache = _MCP_SESSIONS.pop(key, None)
    if cache is None:
        cache = {}
    _MCP_SESSIONS[key] = cache
    while len(_MCP_SESSIONS) > MAX_MCP_SESSION_CACHE:
        _MCP_SESSIONS.pop(next(iter(_MCP_SESSIONS)))
    return cache


def _engine_phase(engine: Any) -> str | None:
    """引擎当前状态名。liars_dice 用 phase，guess_number 用 status。

    不能写成 getattr(engine, "phase", getattr(engine, "status", None))：
    GameEngine 协议声明了 phase 属性（函数体是 ...，取值恒为 None），
    GuessNumberEngine 从协议继承了它，于是 phase 这个属性"存在但为 None"；
    而 getattr 的默认值只在属性缺失时生效，属性为 None 时不会回退到 status。
    结果已结束的猜数字对局（status == "won"）被当成进行中。
    因此逐个取，拿到字符串才算数。
    """
    for attr in ("phase", "status"):
        value = getattr(engine, attr, None)
        if isinstance(value, str):
            return value
    return None


def _engine_finished(engine: Any) -> bool:
    return _engine_phase(engine) in _FINISHED_PHASES


def _is_multiplayer(engine: Any) -> bool:
    """引擎是否多人对局。约定：多人引擎持有一个 players 列表。"""
    return isinstance(getattr(engine, "players", None), list)


def _find_active_session(
    manager: GameManager, game_type: str, player_token: str
) -> str | None:
    """在所有该类型的活动对局中按 player_token 查找。找到返回 session_id。"""
    for session_id, engine in _iter_engines(manager):
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


def _missing_session_hint(manager: GameManager, game_type: str, action: str) -> str:
    """没有可解析的会话时给调用方的提示：先开局，再动作。"""
    for registered_type, engine_cls in _iter_registry(manager):
        if registered_type != game_type:
            continue
        entry = "join" if getattr(engine_cls, "player_mode", "single") == "multi" else "start"
        return (
            f"{game_type} 还没有对局：请先调用 {game_type}_{entry}，"
            f"再调用 {game_type}_{action}"
        )
    return f"未知游戏类型: {game_type}"


# ═══════════════════════════════════════════
# 动作执行
# ═══════════════════════════════════════════


async def _run_action(engine: Any, action: str, args: dict) -> Any:
    """执行单个动作。

    state 动作在引擎提供 wait_for_change 时先阻塞等待：非当前回合的调用会挂起，
    直到对手行动、对局结束或预算耗尽。等待结束后仍交给 handle_action，
    保持玩家身份校验等原有语义。引擎没有该方法时立即返回。

    timeout 只是传输层的等待预算，对任何动作都不传给 handle_action。预算为 0
    表示"本次不等待"：直接跳过 wait_for_change（连协程都不创建），而不是拿 0
    去等一次——后者要白白经历一次 asyncio 调度。只有 outcome 是"等"的动作才会
    走到这里，所以该解析失败即报错，不会静默降级成默认预算。
    """
    timeout = args.pop("timeout", None)
    if action == "state":
        budget = parse_block_budget(timeout)
        waiter = getattr(engine, "wait_for_change", None)
        if waiter is not None and budget > 0:
            await waiter(int(args.get("player_index") or 0), budget)
    return engine.handle_action(action, **args)


async def _execute(manager: GameManager, ctx: Any, name: str, args: dict) -> str:
    """一次工具调用的完整流程：解析工具名 → 定位对局 → 执行 → 取文本。"""
    game_type, action = _parse_tool_name(manager, name)

    cache = _remember_session(_session_key(ctx))

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
            cached_engine = manager.get(session_id)
            if cached_engine is None or _engine_finished(cached_engine):
                cache.pop(game_type, None)
                session_id = None

    engine = manager.get(session_id) if session_id else None

    if engine is None and explicit_session_id is not None:
        raise SessionNotFound(f"会话不存在: {explicit_session_id}")

    if engine is not None and getattr(engine, "game_type", game_type) != game_type:
        # 显式 session_id 指到了另一款游戏的对局：拒绝，避免把它缓存成这种游戏的
        # 会话、后续调用落进不认识的引擎。
        raise ValueError(
            f"会话 {session_id} 属于 {getattr(engine, 'game_type', '?')}，"
            f"不能用于 {game_type}"
        )

    if engine is None:
        # 缓存的会话已被回收（等价于 404）。只有 start/join 允许重建会话：
        # 其余动作在没有对局时静默建一个空引擎，等于制造无人认领的孤儿对局。
        if session_id:
            cache.pop(game_type, None)
        if action not in _STARTING_ACTIONS:
            raise ValueError(_missing_session_hint(manager, game_type, action))
        session_id, engine = manager.create(game_type, **create_kwargs)

    if explicit_session_id is not None and not player_token and _is_multiplayer(engine):
        # 显式指定会话 + 无 token 等于按索引直取别人的对局：get_player_state 会返回
        # 私有骰子，bid/challenge 还能替别人出手。身份只能来自 player_token。
        raise ValueError(
            "多人对局指定 session_id 时必须同时提供 player_token："
            "身份来自 player_token，session_id 只定位对局不证明身份"
        )

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

    async def handle_call_tool(
        ctx, params: types.CallToolRequestParams
    ) -> types.CallToolResult:
        try:
            text = await _execute(manager, ctx, params.name, dict(params.arguments or {}))
        except _EXPECTED_ERRORS as exc:
            # 调用方输入问题：短文本回给调用方，不进堆栈
            logger.info("MCP 调用被拒绝 tool=%s: %s", params.name, exc)
            return types.CallToolResult(
                content=[types.TextContent(type="text", text=f"❌ {exc}")],
                isError=True,
            )
        except Exception as exc:  # 进程内异常一律转成 MCP 错误结果，不抛给传输层
            # 非预期异常：堆栈进日志，调用方只看到一句短话。真实 bug 不该只留下
            # 一行"❌ ..."文本，否则排查时完全看不到发生在哪一行。
            logger.exception("MCP 工具调用异常 tool=%s", params.name)
            return types.CallToolResult(
                content=[
                    types.TextContent(
                        type="text",
                        text=f"❌ 服务端内部错误: {type(exc).__name__}（详见服务端日志）",
                    )
                ],
                isError=True,
            )
        return types.CallToolResult(
            content=[types.TextContent(type="text", text=text)],
            isError=False,
        )

    return Server(
        "agent-game",
        version=__version__,
        on_list_tools=handle_list_tools,
        on_call_tool=handle_call_tool,
    )

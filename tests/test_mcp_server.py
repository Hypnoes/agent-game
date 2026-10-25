"""MCP over Streamable HTTP 端点测试。

走 ASGI 内存传输，不监听 socket：httpx.ASGITransport + 手动进入 app 的 lifespan
（session_manager.run() 只在 lifespan 内有效）。

每个用例用 create_app() 新建 app —— StreamableHTTPSessionManager.run() 只能在同一
实例上进入一次，复用 app 会在第二个用例抛 RuntimeError。
"""

import asyncio
import json
from contextlib import asynccontextmanager

import httpx
import pytest

from agent_game_server import game_server, mcp_server
from agent_game_server.games.guess_number import GuessNumberEngine
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.game_server import create_app

PROTOCOL_VERSION = "2025-11-25"

# 每个请求都必须带这两个头，否则 SDK 拒绝建立会话
BASE_HEADERS = {
    "Accept": "application/json, text/event-stream",
    "Content-Type": "application/json",
}

EXPECTED_TOOLS = {
    "guess_number_start",
    "guess_number_guess",
    "guess_number_state",
    "liars_dice_join",
    "liars_dice_state",
    "liars_dice_bid",
    "liars_dice_challenge",
}


# ═══════════════════════════════════════════
# SSE 与 JSON-RPC 辅助
# ═══════════════════════════════════════════


def parse_sse(text: str) -> list[dict]:
    """把响应体里的 SSE 帧还原成 JSON-RPC 消息列表。"""
    messages: list[dict] = []
    buffer: list[str] = []
    for line in text.splitlines():
        if line.startswith("data: "):
            buffer.append(line[6:])
        elif not line.strip() and buffer:
            messages.append(json.loads("".join(buffer)))
            buffer = []
    if buffer:
        messages.append(json.loads("".join(buffer)))
    return messages


class McpHarness:
    """对 /mcp 的最小 JSON-RPC 客户端。"""

    def __init__(self, client: httpx.AsyncClient):
        self._client = client
        self._next_id = 0
        self._last_session_id: str | None = None

    async def open_session(self) -> dict[str, str]:
        """完成 initialize 握手，返回带 mcp-session-id 的请求头。"""
        message = await self._post(
            BASE_HEADERS,
            {
                "jsonrpc": "2.0",
                "id": self._take_id(),
                "method": "initialize",
                "params": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "pytest", "version": "1"},
                },
            },
        )
        assert message["result"]["serverInfo"]["name"] == "agent-game"
        assert self._last_session_id, "initialize 响应缺少 mcp-session-id 头"

        session_headers = {
            **BASE_HEADERS,
            "mcp-session-id": self._last_session_id,
        }
        resp = await self._client.post(
            "/mcp",
            headers=session_headers,
            json={"jsonrpc": "2.0", "method": "notifications/initialized"},
        )
        # 通知没有响应体，SDK 回 200 或 202 都算成功
        assert resp.status_code < 400, resp.text
        return session_headers

    async def request(self, session: dict[str, str], method: str, params: dict) -> dict:
        return await self._post(
            session,
            {"jsonrpc": "2.0", "id": self._take_id(), "method": method, "params": params},
        )

    async def list_tools(self, session: dict[str, str]) -> list[dict]:
        return (await self.request(session, "tools/list", {}))["result"]["tools"]

    async def call_tool(self, session: dict[str, str], name: str, arguments: dict) -> dict:
        return await self.request(
            session, "tools/call", {"name": name, "arguments": arguments}
        )

    async def call_tool_text(
        self, session: dict[str, str], name: str, arguments: dict
    ) -> str:
        """调用工具并断言成功，返回响应文本。"""
        result = (await self.call_tool(session, name, arguments))["result"]
        text = result["content"][0]["text"]
        assert not result.get("isError"), text
        return text

    # ── 内部 ──

    def _take_id(self) -> int:
        self._next_id += 1
        return self._next_id

    async def _post(self, headers: dict[str, str], payload: dict) -> dict:
        resp = await self._client.post("/mcp", headers=headers, json=payload)
        assert resp.status_code == 200, resp.text
        if "mcp-session-id" in resp.headers:
            self._last_session_id = resp.headers["mcp-session-id"]
        return parse_sse(resp.text)[0]


@asynccontextmanager
async def mcp_harness():
    """构建独立 app，进入 lifespan，产出驱动器。"""
    app = create_app()
    async with app.router.lifespan_context(app):
        transport = httpx.ASGITransport(app=app)
        async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
            yield McpHarness(client)


# ═══════════════════════════════════════════
# 夹具
# ═══════════════════════════════════════════


@pytest.fixture
def anyio_backend():
    """只跑 asyncio 后端，避免 anyio 插件再参数化到未安装的 trio。"""
    return "asyncio"


@pytest.fixture(autouse=True)
def isolated_state():
    """每个用例独占全新的对局表与 MCP 会话缓存。

    GameManager 与 _MCP_SESSIONS 都是模块级单例（test_game_server.py 依赖
    game_server.manager 这个模块级名字），这里只清空内容，不替换对象。
    """
    engines = dict(game_server.manager._engines)
    created_at = dict(game_server.manager._created_at)
    sessions = dict(mcp_server._MCP_SESSIONS)

    game_server.manager._engines.clear()
    game_server.manager._created_at.clear()
    mcp_server._MCP_SESSIONS.clear()
    try:
        yield
    finally:
        game_server.manager._engines.clear()
        game_server.manager._engines.update(engines)
        game_server.manager._created_at.clear()
        game_server.manager._created_at.update(created_at)
        mcp_server._MCP_SESSIONS.clear()
        mcp_server._MCP_SESSIONS.update(sessions)


# ═══════════════════════════════════════════
# 断言辅助
# ═══════════════════════════════════════════


def cached_session(session: dict[str, str], game_type: str) -> str | None:
    """读取某条 MCP 连接为某个游戏缓存的 session_id。"""
    return mcp_server._MCP_SESSIONS.get(session["mcp-session-id"], {}).get(game_type)


def engine_with_player(name: str):
    """返回包含指定玩家的引擎。"""
    for engine in game_server.manager._engines.values():
        for player in getattr(engine, "players", []):
            if player.name == name:
                return engine
    raise AssertionError(f"没有找到包含玩家 {name} 的对局")


def token_of(engine, name: str) -> str:
    for player in engine.players:
        if player.name == name:
            return player.token
    raise AssertionError(f"对局中没有玩家 {name}")


# ═══════════════════════════════════════════
# 用例
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_initialize_and_list_tools():
    """握手成功，7 个工具全部返回且 schema 完整。"""
    async with mcp_harness() as mcp:
        session = await mcp.open_session()
        tools = await mcp.list_tools(session)

    assert len(tools) == 7
    assert {tool["name"] for tool in tools} == EXPECTED_TOOLS
    for tool in tools:
        assert tool["description"]
        schema = tool["inputSchema"]
        assert schema["type"] == "object"
        assert isinstance(schema["properties"], dict)
        assert isinstance(schema["required"], list)

    # 引擎原有的 describe_tools() 结构被原样翻译过来
    engine_tools = {t["name"]: t for t in LiarsDiceEngine.describe_tools()}
    served = {tool["name"]: tool for tool in tools}
    for name, original in engine_tools.items():
        assert served[name]["inputSchema"] == original["input_schema"]
        assert served[name]["description"] == original["description"]


@pytest.mark.anyio
async def test_tool_call_routes_to_engine():
    """工具名解析后落到对应引擎。"""
    async with mcp_harness() as mcp:
        session = await mcp.open_session()

        text = await mcp.call_tool_text(
            session, "liars_dice_join", {"player_name": "Alice"}
        )
        assert "你是玩家 0" in text

        await mcp.call_tool_text(session, "guess_number_start", {})

    liars = engine_with_player("Alice")
    assert isinstance(liars, LiarsDiceEngine)
    assert liars.phase == "waiting"

    guess_engines = [
        e for e in game_server.manager._engines.values() if isinstance(e, GuessNumberEngine)
    ]
    assert len(guess_engines) == 1

    # 两个游戏各自占用一条缓存条目，互不覆盖
    assert cached_session(session, "liars_dice") != cached_session(session, "guess_number")


@pytest.mark.anyio
async def test_session_isolation():
    """两个 MCP 会话各自缓存，互不干扰。"""
    async with mcp_harness() as mcp:
        first = await mcp.open_session()
        second = await mcp.open_session()
        assert first["mcp-session-id"] != second["mcp-session-id"]

        await mcp.call_tool_text(first, "guess_number_start", {})
        first_sid = cached_session(first, "guess_number")
        await mcp.call_tool_text(first, "guess_number_guess", {"number": 1})
        assert game_server.manager.get(first_sid).guess_count == 1

        await mcp.call_tool_text(second, "guess_number_start", {})
        second_sid = cached_session(second, "guess_number")

        assert second_sid != first_sid
        assert game_server.manager.get(second_sid).guess_count == 0

        # 回到第一条连接：仍然指向它自己的对局，未被第二条影响
        await mcp.call_tool_text(first, "guess_number_state", {})
        assert cached_session(first, "guess_number") == first_sid
        assert game_server.manager.get(first_sid).guess_count == 1
        assert game_server.manager.get(second_sid).guess_count == 0


@pytest.mark.anyio
async def test_player_token_identity_wins():
    """同一 token 换一条 MCP 连接重连，回到原对局。

    这是本次改动的核心回归用例：身份由 player_token 决定，mcp-session-id
    只是连接级缓存键。最初按连接分对局的实现会让两名玩家各持一局。
    """
    async with mcp_harness() as mcp:
        alice_conn = await mcp.open_session()
        await mcp.call_tool_text(alice_conn, "liars_dice_join", {"player_name": "Alice"})

        bob_conn = await mcp.open_session()
        await mcp.call_tool_text(bob_conn, "liars_dice_join", {"player_name": "Bob"})

        engine = engine_with_player("Bob")
        assert isinstance(engine, LiarsDiceEngine)
        assert engine.phase == "bidding"
        original_sid = cached_session(bob_conn, "liars_dice")
        assert cached_session(alice_conn, "liars_dice") == original_sid
        alice_token = token_of(engine, "Alice")

        # 全新连接，只带 player_token
        reconnect = await mcp.open_session()
        text = await mcp.call_tool_text(
            reconnect,
            "liars_dice_state",
            {"player_index": 0, "player_token": alice_token, "timeout": 0.1},
        )

        assert cached_session(reconnect, "liars_dice") == original_sid
        assert "你的骰子" in text
        # 没有为同一个人再开一局
        assert len(game_server.manager._engines) == 1
        assert game_server.manager.get(original_sid) is engine

        # 再换一条连接、同一个 token：仍然回到原对局
        again = await mcp.open_session()
        await mcp.call_tool_text(
            again,
            "liars_dice_state",
            {"player_index": 0, "player_token": alice_token, "timeout": 0.1},
        )
        assert cached_session(again, "liars_dice") == original_sid
        assert len(game_server.manager._engines) == 1

        # 对照：不带 token 的新连接不会白捡现有对局，身份来自 token 而非连接
        stranger = await mcp.open_session()
        await mcp.call_tool_text(stranger, "liars_dice_join", {"player_name": "Carol"})
        assert cached_session(stranger, "liars_dice") != original_sid
        assert len(game_server.manager._engines) == 2


@pytest.mark.anyio
async def test_state_blocks_until_opponent_moves():
    """端到端等待语义：非当前回合的 state 挂起，对手行动后返回。"""
    if not hasattr(LiarsDiceEngine, "wait_for_change"):
        pytest.skip("blocking wait not present")

    async with mcp_harness() as mcp:
        alice_conn = await mcp.open_session()
        await mcp.call_tool_text(alice_conn, "liars_dice_join", {"player_name": "Alice"})
        bob_conn = await mcp.open_session()
        await mcp.call_tool_text(bob_conn, "liars_dice_join", {"player_name": "Bob"})

        engine = engine_with_player("Bob")
        assert engine.phase == "bidding"
        connections = {0: alice_conn, 1: bob_conn}
        tokens = {i: engine.players[i].token for i in range(len(engine.players))}

        turn = engine.current_turn
        waiting = 1 - turn

        # 非当前回合者发起 state，应挂起
        blocked = asyncio.create_task(
            mcp.call_tool_text(
                connections[waiting],
                "liars_dice_state",
                {
                    "player_index": waiting,
                    "player_token": tokens[waiting],
                    "timeout": 2,
                },
            )
        )
        await asyncio.sleep(0.2)
        assert not blocked.done(), "非当前回合的 state 应当挂起等待"

        # 对手行动将其唤醒
        await mcp.call_tool_text(
            connections[turn],
            "liars_dice_bid",
            {
                "player_index": turn,
                "player_token": tokens[turn],
                "quantity": 2,
                "face": 3,
            },
        )

        text = await asyncio.wait_for(blocked, timeout=5)

    assert "轮到你了" in text


@pytest.mark.anyio
async def test_finished_game_not_reused():
    """对局结束后重新加入得到新对局。"""
    async with mcp_harness() as mcp:
        alice_conn = await mcp.open_session()
        await mcp.call_tool_text(alice_conn, "liars_dice_join", {"player_name": "Alice"})
        bob_conn = await mcp.open_session()
        await mcp.call_tool_text(bob_conn, "liars_dice_join", {"player_name": "Bob"})

        engine = engine_with_player("Bob")
        connections = {0: alice_conn, 1: bob_conn}
        tokens = {i: engine.players[i].token for i in range(len(engine.players))}

        # 走完一局：当前回合者叫数，对手开盅
        turn = engine.current_turn
        await mcp.call_tool_text(
            connections[turn],
            "liars_dice_bid",
            {
                "player_index": turn,
                "player_token": tokens[turn],
                "quantity": 1,
                "face": 3,
            },
        )
        challenger = 1 - turn
        await mcp.call_tool_text(
            connections[challenger],
            "liars_dice_challenge",
            {"player_index": challenger, "player_token": tokens[challenger]},
        )
        assert engine.phase == "finished"

        old_sid = cached_session(alice_conn, "liars_dice")
        assert game_server.manager.get(old_sid) is engine

        # 同一个玩家带着原 token 重新加入：不能复活已结束的对局
        await mcp.call_tool_text(
            alice_conn,
            "liars_dice_join",
            {"player_name": "Alice", "player_token": tokens[0]},
        )

        new_sid = cached_session(alice_conn, "liars_dice")

    assert new_sid != old_sid
    assert game_server.manager.get(old_sid).phase == "finished"
    new_engine = game_server.manager.get(new_sid)
    assert isinstance(new_engine, LiarsDiceEngine)
    assert new_engine.phase == "waiting"
    assert len(new_engine.players) == 1
    assert new_engine.players[0].name == "Alice"


@pytest.mark.anyio
async def test_unknown_tool_returns_error():
    """未知工具返回错误结果，而不是 500。"""
    async with mcp_harness() as mcp:
        session = await mcp.open_session()
        message = await mcp.call_tool(session, "nonexistent_tool", {})

    result = message["result"]
    assert result["isError"] is True
    assert result["content"][0]["text"].startswith("❌")


def test_create_app_lifespan_can_be_reentered():
    """create_app() 每次给出独立的 app 与 session manager。

    StreamableHTTPSessionManager.run() 只能在实例上调用一次，所以测试必须每个
    用例新建 app。这里验证新建的 app 各自都能正常进入一次 lifespan。
    """

    async def enter_and_exit(app):
        async with app.router.lifespan_context(app):
            return True

    assert asyncio.run(enter_and_exit(create_app())) is True
    assert asyncio.run(enter_and_exit(create_app())) is True

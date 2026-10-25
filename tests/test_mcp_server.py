"""MCP over Streamable HTTP 端点测试。

走 ASGI 内存传输，不监听 socket：httpx.ASGITransport + 手动进入 app 的 lifespan
（session_manager.run() 只在 lifespan 内有效）。

每个用例用 create_app() 新建 app —— StreamableHTTPSessionManager.run() 只能在同一
实例上进入一次，复用 app 会在第二个用例抛 RuntimeError。
"""

import asyncio
import json
import math
import time
from contextlib import asynccontextmanager

import httpx
import pytest

from agent_game_server import config, game_server, mcp_server
from agent_game_server.config import BLOCK_BUDGET, MAX_MCP_SESSION_CACHE, parse_block_budget
from agent_game_server.games.guess_number import GuessNumberEngine
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.game_server import create_app
from agent_game_server.mcp_server import (
    _engine_finished,
    _engine_phase,
    _parse_tool_name,
    _remember_session,
    _run_action,
)

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


# ═══════════════════════════════════════════
# 等待预算：两个传输共用一个解析器
# ═══════════════════════════════════════════


def test_parse_block_budget_contract():
    """None→默认，数值夹到 [0, BLOCK_BUDGET]，非数值与 NaN/inf 报错。"""
    assert parse_block_budget(None) == BLOCK_BUDGET
    assert parse_block_budget() == BLOCK_BUDGET
    assert parse_block_budget(0) == 0
    assert parse_block_budget(-5) == 0
    assert parse_block_budget(999) == BLOCK_BUDGET
    assert parse_block_budget(30) == 30
    assert parse_block_budget(1.5) == 1.5

    for bad in ("abc", True, False, [1], {}):
        with pytest.raises(ValueError):
            parse_block_budget(bad)
    for bad in (math.nan, math.inf, -math.inf):
        with pytest.raises(ValueError):
            parse_block_budget(bad)


def test_parse_block_budget_reads_budget_at_call_time(monkeypatch):
    """默认值在调用时读 BLOCK_BUDGET，不固化在函数默认参数里。"""
    monkeypatch.setattr(config, "BLOCK_BUDGET", 3.0)
    assert parse_block_budget(None) == 3.0
    assert parse_block_budget(999) == 3.0
    # 显式 default 覆盖模块常量
    assert parse_block_budget(None, default=1.0) == 1.0


@pytest.mark.anyio
async def test_run_action_zero_budget_skips_wait():
    """timeout=0 完全不调用 wait_for_change，也不创建等待协程。"""

    class RecordingEngine:
        def __init__(self):
            self.calls = []

        async def wait_for_change(self, player_index, timeout):
            self.calls.append((player_index, timeout))
            return "timeout"

        def handle_action(self, action, **params):
            return {"ok": action}

    engine = RecordingEngine()
    started = time.monotonic()
    await _run_action(engine, "state", {"player_index": 1, "timeout": 0})
    elapsed = time.monotonic() - started

    assert engine.calls == []
    assert elapsed < 0.05


@pytest.mark.anyio
async def test_run_action_passes_small_budget_through():
    """显式小预算原样传给 waiter，且不传给 handle_action。"""

    class RecordingEngine:
        def __init__(self):
            self.calls = []
            self.handled = None

        async def wait_for_change(self, player_index, timeout):
            self.calls.append((player_index, timeout))
            return "timeout"

        def handle_action(self, action, **params):
            self.handled = (action, params)
            return {"ok": action}

    engine = RecordingEngine()
    await _run_action(engine, "state", {"player_index": 1, "timeout": 0.01})

    assert engine.calls == [(1, 0.01)]
    # timeout 只是传输层的等待预算，不能漏进 handle_action 的参数
    assert engine.handled == ("state", {"player_index": 1})


@pytest.mark.anyio
async def test_run_action_strips_timeout_from_non_state_actions():
    """非 state 动作也接受 timeout，但不传给引擎（统一由传输层消化）。"""

    class RecordingEngine:
        def __init__(self):
            self.handled = None

        def handle_action(self, action, **params):
            self.handled = (action, params)
            return {"ok": action}

    engine = RecordingEngine()
    await _run_action(engine, "bid", {"player_index": 0, "timeout": 3})

    assert engine.handled == ("bid", {"player_index": 0})


@pytest.mark.anyio
async def test_run_action_null_timeout_uses_default_budget(monkeypatch):
    """timeout=null 取默认预算，而不是抛 TypeError。"""
    monkeypatch.setattr(config, "BLOCK_BUDGET", 7.0)

    class RecordingEngine:
        def __init__(self):
            self.calls = []

        async def wait_for_change(self, player_index, timeout):
            self.calls.append((player_index, timeout))
            return "ready"

        def handle_action(self, action, **params):
            return {"ok": action}

    engine = RecordingEngine()
    await _run_action(engine, "state", {"player_index": 0, "timeout": None})

    assert engine.calls == [(0, 7.0)]


@pytest.mark.anyio
async def test_run_action_rejects_non_numeric_timeout():
    """非数值 timeout 报错，而不是静默当成默认值或跳过等待。"""

    class RecordingEngine:
        calls = []

        async def wait_for_change(self, player_index, timeout):
            self.calls.append((player_index, timeout))

        def handle_action(self, action, **params):
            return {}

    with pytest.raises(ValueError):
        await _run_action(
            RecordingEngine(), "state", {"player_index": 0, "timeout": "abc"}
        )
    with pytest.raises(ValueError):
        await _run_action(
            RecordingEngine(), "state", {"player_index": 0, "timeout": math.nan}
        )
    assert RecordingEngine.calls == []


@pytest.mark.anyio
async def test_timeout_zero_returns_promptly_over_http():
    """端到端：非当前回合的 state 带 timeout=0 立刻返回，不挂起。"""
    async with mcp_harness() as mcp:
        alice_conn = await mcp.open_session()
        await mcp.call_tool_text(alice_conn, "liars_dice_join", {"player_name": "Alice"})
        bob_conn = await mcp.open_session()
        await mcp.call_tool_text(bob_conn, "liars_dice_join", {"player_name": "Bob"})

        engine = engine_with_player("Bob")
        not_turn = 1 - engine.current_turn
        token = engine.players[not_turn].token

        started = time.monotonic()
        text = await mcp.call_tool_text(
            bob_conn,
            "liars_dice_state",
            {"player_index": not_turn, "player_token": token, "timeout": 0},
        )
        elapsed = time.monotonic() - started

    assert elapsed < 0.5, f"timeout=0 应当立即返回，实际 {elapsed:.2f}s"
    assert text


def test_timeout_zero_vs_absent_end_to_end():
    """两个传输对 timeout 的理解必须一致：0 不等待，缺省用满预算。

    直接对比同一引擎上的两次调用耗时。缺省预算在测试里降到 0.2 秒，避免用例
    真的等满 55 秒。
    """
    engine = LiarsDiceEngine()
    engine.handle_action("join", player_name="Alice")
    engine.handle_action("join", player_name="Bob")
    not_turn = 1 - engine.current_turn

    async def timed(arguments):
        started = time.monotonic()
        await _run_action(engine, "state", dict(arguments))
        return time.monotonic() - started

    skipped = asyncio.run(timed({"player_index": not_turn, "timeout": 0}))
    assert skipped < 0.05

    original = config.BLOCK_BUDGET
    config.BLOCK_BUDGET = 0.2
    try:
        waited = asyncio.run(timed({"player_index": not_turn}))
    finally:
        config.BLOCK_BUDGET = original
    assert 0.15 <= waited < 1.0


# ═══════════════════════════════════════════
# 终态识别
# ═══════════════════════════════════════════


def test_engine_phase_falls_back_to_status():
    """guess_number 的 phase 属性存在但为 None，必须回落到 status。"""
    engine = GuessNumberEngine()
    engine.target = 50
    engine._guess(50)

    assert _engine_phase(engine) == "won"
    assert _engine_finished(engine) is True


def test_engine_finished_for_both_games():
    """两套引擎的终态与进行中都要判断正确。"""
    fresh = GuessNumberEngine()
    assert _engine_finished(fresh) is False

    lost = GuessNumberEngine(max_attempts=1)
    lost.target = 50
    lost._guess(1)
    assert _engine_finished(lost) is True

    liars = LiarsDiceEngine()
    liars.handle_action("join", player_name="Alice")
    assert _engine_finished(liars) is False
    liars.handle_action("join", player_name="Bob")
    assert _engine_finished(liars) is False
    liars._fsm.transition_to("finished")
    assert _engine_finished(liars) is True


def test_engine_finished_ignores_non_string_phase():
    """phase 不是字符串（比如数字编号或 None）时继续看 status。"""

    class Weird:
        phase = None
        status = "lost"

    assert _engine_finished(Weird()) is True

    class Opaque:
        phase = 3
        status = None

    assert _engine_phase(Opaque()) is None
    assert _engine_finished(Opaque()) is False


def test_finished_guess_number_is_not_reused():
    """已结束的猜数字对局不能按 token 复用（据此前的误判会复活）。"""
    token_engine = GuessNumberEngine()
    token_engine.target = 50
    token_engine._guess(50)
    assert _engine_finished(token_engine) is True


# ═══════════════════════════════════════════
# 身份：显式 session_id 必须配 token
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_explicit_session_id_without_token_is_refused():
    """新连接拿 session_id 硬指别人的对局：必须拒绝，且不泄露私有骰子。"""
    async with mcp_harness() as mcp:
        alice_conn = await mcp.open_session()
        await mcp.call_tool_text(alice_conn, "liars_dice_join", {"player_name": "Alice"})
        bob_conn = await mcp.open_session()
        await mcp.call_tool_text(bob_conn, "liars_dice_join", {"player_name": "Bob"})

        engine = engine_with_player("Bob")
        victim_index = engine.current_turn
        victim_dice = engine.players[victim_index].dice
        victim_sid = cached_session(bob_conn, "liars_dice")

        # 全新连接，无 token，直接指到受害者的对局
        attacker = await mcp.open_session()
        message = await mcp.call_tool(
            attacker,
            "liars_dice_state",
            {"session_id": victim_sid, "player_index": victim_index, "timeout": 0},
        )

    result = message["result"]
    assert result["isError"] is True
    text = result["content"][0]["text"]
    assert "player_token" in text

    # 私有骰子绝不能出现在响应里
    assert "你的骰子" not in text
    assert str(victim_dice) not in text
    for face in victim_dice:
        assert f"骰子: [{face}" not in text
    # 也没有替受害者建出新的空对局
    assert len(game_server.manager._engines) == 1


@pytest.mark.anyio
async def test_explicit_session_id_with_token_still_works():
    """带 token 的显式 session_id 正常返回私有骰子，不被新规则误伤。"""
    async with mcp_harness() as mcp:
        alice_conn = await mcp.open_session()
        await mcp.call_tool_text(alice_conn, "liars_dice_join", {"player_name": "Alice"})
        bob_conn = await mcp.open_session()
        await mcp.call_tool_text(bob_conn, "liars_dice_join", {"player_name": "Bob"})

        engine = engine_with_player("Bob")
        index = engine.current_turn
        token = engine.players[index].token
        sid = cached_session(bob_conn, "liars_dice")

        reconnect = await mcp.open_session()
        text = await mcp.call_tool_text(
            reconnect,
            "liars_dice_state",
            {"session_id": sid, "player_index": index, "player_token": token, "timeout": 0},
        )

    assert "骰子" in text
    assert str(engine.players[index].dice) in text


@pytest.mark.anyio
async def test_explicit_session_id_for_single_player_needs_no_token():
    """单人引擎没有 players 列表，不受 token 规则约束。"""
    async with mcp_harness() as mcp:
        session = await mcp.open_session()
        await mcp.call_tool_text(session, "guess_number_start", {})
        sid = cached_session(session, "guess_number")

        other = await mcp.open_session()
        text = await mcp.call_tool_text(
            other, "guess_number_state", {"session_id": sid}
        )

    assert "猜" in text


# ═══════════════════════════════════════════
# 孤儿会话
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_stray_action_creates_no_session():
    """没有 join 就直接 bid：报错，且不新建空大厅。"""
    async with mcp_harness() as mcp:
        session = await mcp.open_session()
        before = len(game_server.manager._engines)
        message = await mcp.call_tool(
            session,
            "liars_dice_bid",
            {"player_index": 0, "quantity": 2, "face": 3},
        )
        after = len(game_server.manager._engines)

    result = message["result"]
    assert result["isError"] is True
    assert before == after == 0
    # 错误提示要告诉调用方先 join
    assert "liars_dice_join" in result["content"][0]["text"]


@pytest.mark.anyio
async def test_stray_state_creates_no_session():
    """state 同样不能凭空建对局。"""
    async with mcp_harness() as mcp:
        session = await mcp.open_session()
        message = await mcp.call_tool(
            session, "liars_dice_state", {"player_index": 0, "timeout": 0}
        )

    assert message["result"]["isError"] is True
    assert len(game_server.manager._engines) == 0


@pytest.mark.anyio
async def test_join_and_start_still_auto_create():
    """start/join 保留自动建局的行为。"""
    async with mcp_harness() as mcp:
        session = await mcp.open_session()
        await mcp.call_tool_text(session, "liars_dice_join", {"player_name": "Alice"})
        await mcp.call_tool_text(session, "guess_number_start", {})

    assert len(game_server.manager._engines) == 2


# ═══════════════════════════════════════════
# 工具名解析
# ═══════════════════════════════════════════


def test_parse_tool_name_resolves_registered_tools():
    """注册表驱动的解析：所有已注册工具都落到正确的 (game_type, action)。"""
    manager = game_server.manager

    assert _parse_tool_name(manager, "liars_dice_join") == ("liars_dice", "join")
    assert _parse_tool_name(manager, "liars_dice_challenge") == ("liars_dice", "challenge")
    assert _parse_tool_name(manager, "guess_number_guess") == ("guess_number", "guess")
    assert _parse_tool_name(manager, "guess_number_state") == ("guess_number", "state")


def test_parse_tool_name_handles_single_segment_game_types():
    """游戏类型只有一段时不能被切成两段。

    `nim_take` 的旧实现在"取前两段"里得到前缀 `nim_take`，解析成
    ("nim_take", "")，随后 manager.create 报未知游戏类型。
    """

    class NimEngine:
        game_type = "nim"
        player_mode = "single"

        @classmethod
        def describe_tools(cls):
            return [{"name": "nim_take", "description": "n", "input_schema": {}}]

    class FakeManager:
        def __init__(self):
            self._registry = {"nim": NimEngine}
            self._engines = {}

    assert _parse_tool_name(FakeManager(), "nim_take") == ("nim", "take")


def test_parse_tool_name_handles_multi_word_game_types():
    """三段游戏类型同样按 game_type 前缀剥离，不按固定段数切。"""

    class MegaEngine:
        game_type = "mega_liars_dice"
        player_mode = "multi"

        @classmethod
        def describe_tools(cls):
            return [
                {"name": "mega_liars_dice_join", "description": "j", "input_schema": {}},
                {"name": "mega_liars_dice_state", "description": "s", "input_schema": {}},
            ]

    class FakeManager:
        def __init__(self):
            self._registry = {"mega_liars_dice": MegaEngine}
            self._engines = {}

    manager = FakeManager()
    assert _parse_tool_name(manager, "mega_liars_dice_join") == ("mega_liars_dice", "join")
    assert _parse_tool_name(manager, "mega_liars_dice_state") == ("mega_liars_dice", "state")


def test_parse_tool_name_rejects_unknown_game_type():
    """连游戏类型都认不出来的工具名在解析阶段就报错，不拿去建对局。"""
    with pytest.raises(ValueError):
        _parse_tool_name(game_server.manager, "nonexistent_tool")
    with pytest.raises(ValueError):
        _parse_tool_name(game_server.manager, "unknown_game_start")
    # 只有类型没有动作同样不算合法工具名
    with pytest.raises(ValueError):
        _parse_tool_name(game_server.manager, "liars_dice_")


def test_parse_tool_name_falls_back_for_known_game_types():
    """注册表没命中、但游戏类型确实注册过时按前缀兜底，交给引擎去拒绝动作。

    这是保留的旧行为：动作名是否合法由引擎的 handle_action 判定，解析层只负责
    把 game_type 认出来。
    """
    assert _parse_tool_name(game_server.manager, "liars_dice_bogus") == (
        "liars_dice",
        "bogus",
    )


def test_private_manager_access_is_isolated():
    """manager 私有字段只能经由 _iter_engines / _iter_registry 访问。

    这两个函数是将来 manager.py 补上公开接口时的唯一改动点，所以不允许其它地方
    直接摸 manager._engines。
    """
    import ast
    from pathlib import Path

    source = Path(mcp_server.__file__).read_text(encoding="utf-8")
    tree = ast.parse(source)

    leaks = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Attribute):
            continue
        if node.attr not in ("_engines", "_registry"):
            continue
        # 允许的形态：manager._engines / manager._registry，且必须出现在两个
        # 隔离函数自己的函数体里。
        leaks.append((node.lineno, node.attr))

    allowed_lines = set()
    for name in ("_iter_engines", "_iter_registry"):
        func = next(
            n
            for n in tree.body
            if isinstance(n, ast.FunctionDef) and n.name == name
        )
        for sub in ast.walk(func):
            if isinstance(sub, ast.Attribute) and sub.attr in ("_engines", "_registry"):
                allowed_lines.add(sub.lineno)

    stray = [leak for leak in leaks if leak[0] not in allowed_lines]
    assert not stray, f"manager 私有字段在这些行被直接访问: {stray}"


def test_iter_helpers_snapshot_private_dicts():
    """两个访问替身返回快照：遍历时管理器变动不会抛错。"""
    manager = game_server.manager
    sid, _engine = manager.create("guess_number")

    snapshot = mcp_server._iter_engines(manager)
    assert isinstance(snapshot, list)
    # 拿到快照之后清空管理器，快照仍然可遍历
    manager._engines.clear()
    assert [session_id for session_id, _ in snapshot] == [sid]

    registry = mcp_server._iter_registry(manager)
    assert isinstance(registry, list)
    assert {game_type for game_type, _ in registry} == {"guess_number", "liars_dice"}


@pytest.mark.anyio
async def test_unregistered_tool_returns_error_result():
    """未注册的工具名经 MCP 层返回 isError=True，而不是 500。"""
    async with mcp_harness() as mcp:
        session = await mcp.open_session()
        message = await mcp.call_tool(session, "unknown_game_start", {})
        bogus_action = await mcp.call_tool(session, "liars_dice_bogus", {})

    assert message["result"]["isError"] is True
    assert bogus_action["result"]["isError"] is True
    assert len(game_server.manager._engines) == 0


# ═══════════════════════════════════════════
# 会话缓存上限
# ═══════════════════════════════════════════


def test_session_cache_stays_bounded():
    """连接数超过上限时按插入顺序淘汰最旧的键。"""
    mcp_server._MCP_SESSIONS.clear()
    for i in range(MAX_MCP_SESSION_CACHE + 25):
        _remember_session(f"conn-{i}")

    assert len(mcp_server._MCP_SESSIONS) == MAX_MCP_SESSION_CACHE
    # 最旧的一批被淘汰，最新的一定还在
    assert "conn-0" not in mcp_server._MCP_SESSIONS
    assert f"conn-{MAX_MCP_SESSION_CACHE + 24}" in mcp_server._MCP_SESSIONS


def test_session_cache_touch_keeps_active_connection():
    """再次访问会把连接移到队尾，正在用的连接不会被自己挤掉。"""
    mcp_server._MCP_SESSIONS.clear()
    active = _remember_session("active")
    active["liars_dice"] = "sid-active"

    for i in range(MAX_MCP_SESSION_CACHE * 2):
        _remember_session(f"churn-{i}")
        # 每次 churn 之后重新访问活跃连接
        assert _remember_session("active")["liars_dice"] == "sid-active"

    assert "active" in mcp_server._MCP_SESSIONS
    assert len(mcp_server._MCP_SESSIONS) == MAX_MCP_SESSION_CACHE


# ═══════════════════════════════════════════
# 错误结果
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_blocking_call_returns_ok_not_error():
    """阻塞后正常返回的调用，isError 必须是 False。"""
    if not hasattr(LiarsDiceEngine, "wait_for_change"):
        pytest.skip("blocking wait not present")

    async with mcp_harness() as mcp:
        alice_conn = await mcp.open_session()
        await mcp.call_tool_text(alice_conn, "liars_dice_join", {"player_name": "Alice"})
        bob_conn = await mcp.open_session()
        await mcp.call_tool_text(bob_conn, "liars_dice_join", {"player_name": "Bob"})

        engine = engine_with_player("Bob")
        connections = {0: alice_conn, 1: bob_conn}
        tokens = {i: engine.players[i].token for i in range(len(engine.players))}
        turn = engine.current_turn
        waiting = 1 - turn

        blocked = asyncio.create_task(
            mcp.call_tool(
                connections[waiting],
                "liars_dice_state",
                {"player_index": waiting, "player_token": tokens[waiting], "timeout": 2},
            )
        )
        await asyncio.sleep(0.15)
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
        message = await asyncio.wait_for(blocked, timeout=5)

    result = message["result"]
    assert result["isError"] is False
    assert "轮到你了" in result["content"][0]["text"]


@pytest.mark.anyio
async def test_unexpected_error_logs_traceback(monkeypatch, caplog):
    """非 ValueError 的异常写完整堆栈，但调用方只看到一句短话。"""

    def boom(*_args, **_kwargs):
        raise RuntimeError("内部炸了")

    monkeypatch.setattr(mcp_server, "_execute", boom)

    async with mcp_harness() as mcp:
        session = await mcp.open_session()
        with caplog.at_level("ERROR", logger="agent_game_server.mcp_server"):
            message = await mcp.call_tool(session, "guess_number_start", {})

    result = message["result"]
    assert result["isError"] is True
    text = result["content"][0]["text"]
    assert "RuntimeError" in text
    assert "内部炸了" not in text  # 细节进日志，不进给调用方的文本

    records = [r for r in caplog.records if r.name == "agent_game_server.mcp_server"]
    assert records, "非预期异常必须留下日志"
    assert records[0].exc_info is not None, "日志里要带堆栈"

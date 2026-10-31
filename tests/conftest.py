"""全仓库共享的测试夹具。

这里只放**跨文件**需要的东西：造对局、摆骰子、起 app、连 MCP。放在 conftest
里而不是各文件各写一份，是因为同一种"造一局大话骰"曾经有过 5 种写法，语义
还不完全一致——每个文件各自发明一套世界，读代码的人要先重建这个文件的世界观。

约定：
- `make_manager` / `app_client` 各自造**独立**的 GameManager，测试之间不共享
  对局表。`create_app(manager)` 支持注入之后不再需要 monkeypatch 模块全局。
- `dice_game` 造的是真人玩家已全部入场的对局（phase=bidding）。
- `dice_script` 摆骰子，支持"1 混在非 1 里"这类关键组合；旧的
  `set_all_dice(face)` 只能摆全同face，那是百搭分支从未被测到的原因。
"""

from __future__ import annotations

import asyncio
import json
from contextlib import asynccontextmanager

import httpx
import pytest

from agent_game_server import game_server
from agent_game_server.game_server import create_app
from agent_game_server.games.guess_number import GuessNumberEngine
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.manager import GameManager

# ═══════════════════════════════════════════
# 异步后端
# ═══════════════════════════════════════════


@pytest.fixture
def anyio_backend():
    """只跑 asyncio 后端，避免 anyio 插件再参数化到未安装的 trio。

    放在 conftest 而不是某一个测试文件里：新加一个 async 测试文件时，
    不该因为夹具在别的文件里而报错。
    """
    return "asyncio"


# ═══════════════════════════════════════════
# 管理器与 app
# ═══════════════════════════════════════════


@pytest.fixture
def make_manager():
    """返回一个工厂：造注册了真实引擎的独立 GameManager。"""

    def factory(*engine_classes: type, **extra: type) -> GameManager:
        gm = GameManager()
        classes = [GuessNumberEngine, LiarsDiceEngine, *engine_classes]
        for engine_cls in classes:
            gm.register(engine_cls.game_type, engine_cls)
        for game_type, engine_cls in extra.items():
            gm.register(game_type, engine_cls)
        return gm

    return factory


@pytest.fixture
def app_client(make_manager):
    """一个带独立 manager 的 TestClient。

    `raise_server_exceptions=False`：把服务端未捕获异常变成 500 响应，
    这样断言"不是 500"才有意义（否则异常会直接冒到测试里）。
    """
    from fastapi.testclient import TestClient

    def factory(*engine_classes: type) -> tuple[TestClient, GameManager, object]:
        gm = make_manager(*engine_classes)
        app = create_app(gm)
        client = TestClient(app, raise_server_exceptions=False)
        return client, gm, app

    created: list[TestClient] = []

    def opener(*engine_classes: type):
        client, gm, app = factory(*engine_classes)
        client.__enter__()
        created.append(client)
        return client, gm

    yield opener

    for client in created:
        client.__exit__(None, None, None)


# ═══════════════════════════════════════════
# 大话骰：造局与摆骰子
# ═══════════════════════════════════════════


@pytest.fixture
def dice_game():
    """造一局已开局的 N 人大话骰，返回 (engine, tokens)。

    tokens 是 {index: token}，省掉每个用例都写 `e.players[i].token`。
    """

    def factory(player_count: int = 2, dice_per_player: int = 5):
        engine = LiarsDiceEngine(
            player_count=player_count, dice_per_player=dice_per_player
        )
        for i in range(player_count):
            engine.handle_action("join", player_name=f"P{i}")
        assert engine.phase == "bidding"
        tokens = {i: player.token for i, player in enumerate(engine.players)}
        return engine, tokens

    return factory


@pytest.fixture
def dice_script():
    """把全场骰子摆成指定形状。

    比 `set_all_dice(face)` 强的地方：能摆出 `[1, 2, 3]` 这种"1 与其它点数混在
    一起"的组合，那是百搭规则的唯一触发条件。旧的写法把全场骰子设成同一个数，
    于是百搭分支在整套测试里一次都没被走到。
    """

    def factory(engine: LiarsDiceEngine, faces: list[list[int]]):
        assert len(faces) == len(engine.players), "骰子形状必须与玩家数一致"
        for player, dice in zip(engine.players, faces, strict=True):
            player.dice = list(dice)
        return engine

    return factory


# ═══════════════════════════════════════════
# MCP over Streamable HTTP
# ═══════════════════════════════════════════

PROTOCOL_VERSION = "2025-11-25"

# 每个请求都必须带这两个头，否则 SDK 拒绝建立会话
MCP_BASE_HEADERS = {
    "Accept": "application/json, text/event-stream",
    "Content-Type": "application/json",
}


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
    """对 /mcp 的最小 JSON-RPC 客户端。

    `manager` 是这个 harness 所连 app 自己的对局表。用例要检查"服务端到底建了
    几局"时读它，不要读模块级 `game_server.manager`——现在每个用例都有独立
    manager，读全局会看到别的用例或者空表。
    """

    def __init__(self, client: httpx.AsyncClient, manager: GameManager):
        self._client = client
        self.manager = manager
        self._next_id = 0
        self._last_session_id: str | None = None

    async def open_session(self) -> dict[str, str]:
        """完成 initialize 握手，返回带 mcp-session-id 的请求头。"""
        message = await self._post(
            MCP_BASE_HEADERS,
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

        session_headers = {**MCP_BASE_HEADERS, "mcp-session-id": self._last_session_id}
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
async def _mcp_harness_for(gm: GameManager):
    """构建独立 app，进入 lifespan，产出驱动器。"""
    app = create_app(gm)
    async with app.router.lifespan_context(app):
        transport = httpx.ASGITransport(app=app)
        async with httpx.AsyncClient(transport=transport, base_url="http://test") as client:
            yield McpHarness(client, gm)


@pytest.fixture
def mcp_harness(make_manager):
    """给出一个 `async with mcp_harness() as mcp` 上下文。

    每个用例新建 app：StreamableHTTPSessionManager.run() 只能在实例上进入一次。
    manager 也每次新建，所以用例之间不共享对局表——不再需要手写夹具去清空
    模块全局的 `_engines` / `_created_at` / `_MCP_SESSIONS`（那份清理从来没管过
    `_finished_at` 与 `_seen_expired`）。
    """

    def factory(*engine_classes: type):
        return _mcp_harness_for(make_manager(*engine_classes))

    return factory


@pytest.fixture
def admin_token() -> str:
    """当前进程的 admin token。"""
    return game_server._ADMIN_TOKEN


@pytest.fixture(scope="session")
def event_loop_policy_placeholder():
    """占位，保持 conftest 无副作用。"""
    return None


def run(coro):
    """在同步用例里跑一个协程。"""
    return asyncio.run(coro)

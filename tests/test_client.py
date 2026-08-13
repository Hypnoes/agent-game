"""MCP client 行为测试。"""

import asyncio

from agent_game_mcp import client


class FakeResponse:
    def __init__(self, status_code=200, payload=None, text=""):
        self.status_code = status_code
        self._payload = payload or {}
        self.text = text or str(self._payload)

    def json(self):
        return self._payload


class FakeAsyncClient:
    def __init__(self, responses):
        self._responses = list(responses)
        self.requests = []

    async def __aenter__(self):
        return self

    async def __aexit__(self, exc_type, exc, tb):
        return False

    async def post(self, url, json=None):
        self.requests.append((url, json))
        if not self._responses:
            raise AssertionError("unexpected request")
        return self._responses.pop(0)

    async def get(self, url):
        self.requests.append((url, None))
        if not self._responses:
            raise AssertionError("unexpected request")
        return self._responses.pop(0)


def test_guess_number_start_forces_new_session(monkeypatch):
    original_sessions = dict(client._SESSIONS)
    try:
        client._SESSIONS = {"guess_number": "old-session"}
        fake = FakeAsyncClient(
            [
                FakeResponse(payload={"session_id": "fresh-session"}),
                FakeResponse(payload={"prompt": "new game"}),
            ]
        )
        monkeypatch.setattr(client, "_parse_tool_name", lambda name: ("guess_number", "start"))
        monkeypatch.setattr(client.httpx, "AsyncClient", lambda timeout=10.0: fake)

        result = asyncio.run(
            client.handle_call_tool(
                None,
                type("P", (), {"name": "guess_number_start", "arguments": {}})(),
            )
        )

        assert client._SESSIONS["guess_number"] == "fresh-session"
        assert fake.requests[0][0].endswith("/games/guess_number")
        assert fake.requests[1][0].endswith("/games/fresh-session/action")
        assert result.content[0].text == "new game"
    finally:
        client._SESSIONS = original_sessions


def test_stale_cached_session_is_replaced(monkeypatch):
    original_sessions = dict(client._SESSIONS)
    try:
        client._SESSIONS = {"guess_number": "stale-session"}
        fake = FakeAsyncClient(
            [
                FakeResponse(status_code=404, payload={"detail": "session not found"}),
                FakeResponse(payload={"session_id": "recreated-session"}),
                FakeResponse(payload={"prompt": "recovered"}),
            ]
        )
        monkeypatch.setattr(client, "_parse_tool_name", lambda name: ("guess_number", "guess"))
        monkeypatch.setattr(client.httpx, "AsyncClient", lambda timeout=10.0: fake)

        result = asyncio.run(
            client.handle_call_tool(
                None,
                type("P", (), {"name": "guess_number_guess", "arguments": {"number": 42}})(),
            )
        )

        assert client._SESSIONS["guess_number"] == "recreated-session"
        assert fake.requests[0][0].endswith("/games/stale-session/action")
        assert fake.requests[1][0].endswith("/games/guess_number")
        assert fake.requests[2][0].endswith("/games/recreated-session/action")
        assert result.content[0].text == "recovered"
    finally:
        client._SESSIONS = original_sessions


def test_join_with_player_token_can_reconnect(monkeypatch):
    original_sessions = dict(client._SESSIONS)
    try:
        client._SESSIONS = {}
        fake = FakeAsyncClient(
            [
                FakeResponse(payload={"session_id": "reconnected-session"}),
                FakeResponse(payload={"prompt": "joined"}),
            ]
        )
        monkeypatch.setattr(client, "_parse_tool_name", lambda name: ("liars_dice", "join"))
        monkeypatch.setattr(client.httpx, "AsyncClient", lambda timeout=10.0: fake)

        result = asyncio.run(
            client.handle_call_tool(
                None,
                type(
                    "P",
                    (),
                    {
                        "name": "liars_dice_join",
                        "arguments": {
                            "player_name": "Alice",
                            "player_token": "token-123",
                        },
                    },
                )(),
            )
        )

        assert client._SESSIONS["liars_dice"] == "reconnected-session"
        assert fake.requests[0][0].endswith("/games/liars_dice")
        assert fake.requests[0][1] == {"player_token": "token-123"}
        assert fake.requests[1][0].endswith("/games/reconnected-session/action")
        assert result.content[0].text == "joined"
    finally:
        client._SESSIONS = original_sessions


def test_finished_liars_dice_session_starts_new_game(monkeypatch):
    original_sessions = dict(client._SESSIONS)
    try:
        client._SESSIONS = {"liars_dice": "old-finished-session"}
        fake = FakeAsyncClient(
            [
                FakeResponse(payload={"status": "finished"}),
                FakeResponse(payload={"session_id": "fresh-session"}),
                FakeResponse(payload={"prompt": "joined fresh"}),
            ]
        )
        monkeypatch.setattr(client, "_parse_tool_name", lambda name: ("liars_dice", "join"))
        monkeypatch.setattr(client.httpx, "AsyncClient", lambda timeout=10.0: fake)

        result = asyncio.run(
            client.handle_call_tool(
                None,
                type("P", (), {"name": "liars_dice_join", "arguments": {"player_name": "Alice"}})(),
            )
        )

        assert client._SESSIONS["liars_dice"] == "fresh-session"
        assert fake.requests[0][0].endswith("/games/old-finished-session")
        assert fake.requests[1][0].endswith("/games/liars_dice")
        assert fake.requests[2][0].endswith("/games/fresh-session/action")
        assert result.content[0].text == "joined fresh"
    finally:
        client._SESSIONS = original_sessions


def test_state_action_uses_long_timeout(monkeypatch):
    original_sessions = dict(client._SESSIONS)
    captured = {}
    try:
        client._SESSIONS = {"liars_dice": "sid"}
        fake = FakeAsyncClient([FakeResponse(payload={"prompt": "ok"})])

        def fake_async_client(timeout=10.0):
            captured["timeout"] = timeout
            return fake

        monkeypatch.setattr(client, "_parse_tool_name", lambda n: ("liars_dice", "state"))
        monkeypatch.setattr(client.httpx, "AsyncClient", fake_async_client)

        asyncio.run(
            client.handle_call_tool(
                None,
                type("P", (), {"name": "liars_dice_state", "arguments": {"player_index": 0}})(),
            )
        )

        assert captured["timeout"] >= 55.0
    finally:
        client._SESSIONS = original_sessions


def test_non_state_action_uses_default_timeout(monkeypatch):
    original_sessions = dict(client._SESSIONS)
    captured = {}
    try:
        client._SESSIONS = {"guess_number": "sid"}
        fake = FakeAsyncClient([FakeResponse(payload={"prompt": "ok"})])

        def fake_async_client(timeout=10.0):
            captured["timeout"] = timeout
            return fake

        monkeypatch.setattr(client, "_parse_tool_name", lambda n: ("guess_number", "guess"))
        monkeypatch.setattr(client.httpx, "AsyncClient", fake_async_client)

        asyncio.run(
            client.handle_call_tool(
                None,
                type("P", (), {"name": "guess_number_guess", "arguments": {"number": 42}})(),
            )
        )

        assert captured["timeout"] == 10.0
    finally:
        client._SESSIONS = original_sessions

"""game_server 行为测试。"""

import asyncio

from agent_game_server import game_server


class _FakeWebSocket:
    def __init__(self):
        self.messages: list[str] = []

    async def send_text(self, message: str):
        self.messages.append(message)


class _FakeManager:
    def __init__(self, sessions: list[dict]):
        self._sessions = sessions
        self.timeout_calls = 0
        self.create_calls = []

    def check_timeouts(self):
        self.timeout_calls += 1
        return []

    def list_sessions(self):
        return self._sessions

    def create(self, game_type: str, **params):
        self.create_calls.append((game_type, params))
        return "sid-1234", object()


def test_refresh_admin_state_uses_full_snapshot():
    original_manager = game_server.manager
    original_clients = game_server._ws_clients
    original_signature = game_server._last_admin_state_signature

    try:
        ws = _FakeWebSocket()
        manager = _FakeManager(
            [{"session_id": "abc", "status": "waiting", "target": 1}]
        )
        game_server.manager = manager
        game_server._ws_clients = {ws}
        game_server._last_admin_state_signature = ""

        assert asyncio.run(game_server._refresh_admin_state()) is True
        assert manager.timeout_calls == 1
        assert len(ws.messages) == 1

        assert asyncio.run(game_server._refresh_admin_state()) is False
        assert manager.timeout_calls == 2
        assert len(ws.messages) == 1

        manager._sessions = [{"session_id": "abc", "status": "waiting", "target": 2}]
        assert asyncio.run(game_server._refresh_admin_state()) is True
        assert manager.timeout_calls == 3
        assert len(ws.messages) == 2
    finally:
        game_server.manager = original_manager
        game_server._ws_clients = original_clients
        game_server._last_admin_state_signature = original_signature


def test_create_game_forwards_request_params():
    original_manager = game_server.manager

    class _Request:
        async def json(self):
            return {"player_token": "token-123"}

    try:
        manager = _FakeManager([])
        game_server.manager = manager

        result = asyncio.run(game_server.create_game("liars_dice", _Request()))

        assert result["session_id"] == "sid-1234"
        assert manager.create_calls == [("liars_dice", {"player_token": "token-123"})]
    finally:
        game_server.manager = original_manager

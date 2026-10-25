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


class _WaitRecordingEngine:
    """记录 wait_for_change 被调用情况的假引擎。"""

    phase = "bidding"
    players = [object(), object()]

    def __init__(self):
        self.wait_calls = []
        self.validated = []

    async def wait_for_change(self, player_index, timeout):
        self.wait_calls.append((player_index, timeout))
        return "timeout"

    def _validate_player(self, player_index, player_token=""):
        self.validated.append((player_index, player_token))

    def get_player_state(self, player_index=0):
        return {"public": {}, "private": {}, "prompt": f"state-{player_index}"}

    def handle_action(self, action, **params):
        return {"handled": action}


class _EngineManager:
    """只实现 get()/check_timeouts() 的假 manager。"""

    def __init__(self, engine):
        self._engine = engine
        self.timeout_calls = 0

    def get(self, session_id):
        return self._engine

    def check_timeouts(self):
        self.timeout_calls += 1
        return []


class _ActionRequest:
    def __init__(self, payload):
        self._payload = payload

    async def json(self):
        return dict(self._payload)


def test_state_action_goes_through_wait_path():
    original_manager = game_server.manager

    class _Request:
        async def json(self):
            return {"action": "state", "player_index": 1, "player_token": "tok-1"}

    try:
        engine = _WaitRecordingEngine()
        manager = _EngineManager(engine)
        game_server.manager = manager

        result = asyncio.run(game_server.game_action("sid-1", _Request()))

        assert engine.wait_calls == [(1, game_server.BLOCK_BUDGET)]
        assert manager.timeout_calls == 1
        assert engine.validated == [(1, "tok-1")]
        assert result["prompt"] == "state-1"
    finally:
        game_server.manager = original_manager


def test_state_action_clamps_timeout_to_budget():
    original_manager = game_server.manager

    try:
        engine = _WaitRecordingEngine()
        game_server.manager = _EngineManager(engine)

        asyncio.run(
            game_server.game_action(
                "sid-1",
                _ActionRequest({"action": "state", "player_index": 0, "timeout": 999}),
            )
        )

        assert engine.wait_calls == [(0, game_server.BLOCK_BUDGET)]
    finally:
        game_server.manager = original_manager


def test_player_state_get_does_not_wait():
    original_manager = game_server.manager

    try:
        engine = _WaitRecordingEngine()
        game_server.manager = _EngineManager(engine)

        result = game_server.game_player_state("sid-1", 0)

        assert engine.wait_calls == []
        assert result["prompt"] == "state-0"
    finally:
        game_server.manager = original_manager


def test_player_state_get_out_of_range_is_404():
    original_manager = game_server.manager

    try:
        engine = _WaitRecordingEngine()
        game_server.manager = _EngineManager(engine)

        try:
            game_server.game_player_state("sid-1", 5)
        except game_server.HTTPException as ex:
            assert ex.status_code == 404
        else:
            raise AssertionError("越界索引应当返回 404")
    finally:
        game_server.manager = original_manager

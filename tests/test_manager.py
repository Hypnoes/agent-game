"""GameManager 测试"""

import time

import pytest

from agent_game_server.manager import GameManager

# ── 最小化 Engine 用于测试 GM 的路由和生命周期 ──


class FakeEngine:
    game_type = "fake_game"
    game_name = "测试游戏"
    player_mode = "single"

    def __init__(self, param1="default"):
        self.param1 = param1
        self.status = "in_progress"
        self.actions = []

    @classmethod
    def describe_tools(cls):
        return [
            {
                "name": "fake_game_do_thing",
                "description": "Do a thing",
                "input_schema": {"type": "object", "properties": {}, "required": []},
            },
        ]

    def get_public_state(self):
        return {"status": self.status}

    def do_thing(self, x=1):
        self.actions.append(f"did_{x}")
        return {"result": f"done_{x}"}

    def check_timeout(self, now):
        if (
            self.status == "in_progress"
            and hasattr(self, "_expire_at")
            and now > self._expire_at
        ):
            return {"type": "timeout", "expired": True}
        return None


class RejoinEngine:
    game_type = "rejoin_game"
    game_name = "重连游戏"
    player_mode = "multi"

    def __init__(self, phase="waiting"):
        self.phase = phase
        self.players = [{"token": "token-123"}]

    def get_public_state(self):
        return {"phase": self.phase}

    def _find_player_by_token(self, token):
        for idx, p in enumerate(self.players):
            if p["token"] == token:
                return idx
        return None


# ── 测试 ──


def test_register_and_create():
    gm = GameManager()
    gm.register("fake_game", FakeEngine)
    sid, engine = gm.create("fake_game", param1="hello")
    assert len(sid) == 8
    assert engine.param1 == "hello"
    assert engine.status == "in_progress"


def test_unknown_game_type():
    gm = GameManager()
    with pytest.raises(ValueError, match="未知游戏类型"):
        gm.create("nonexistent")


def test_route():
    gm = GameManager()
    gm.register("fake_game", FakeEngine)
    sid, _ = gm.create("fake_game")
    result = gm.route(sid, "do_thing", x=42)
    assert result == {"result": "done_42"}


def test_list_sessions():
    gm = GameManager()
    gm.register("fake_game", FakeEngine)
    s1, _ = gm.create("fake_game")
    s2, _ = gm.create("fake_game")
    sessions = gm.list_sessions()
    assert len(sessions) == 2
    ids = [s["session_id"] for s in sessions]
    assert s1 in ids and s2 in ids


def test_describe_tools():
    gm = GameManager()
    gm.register("fake_game", FakeEngine)
    tools = gm.describe_all_tools()
    assert len(tools) == 1
    assert tools[0]["name"] == "fake_game_do_thing"


def test_timeout():
    gm = GameManager()
    gm.register("fake_game", FakeEngine)
    sid, engine = gm.create("fake_game")
    engine._expire_at = time.time() - 1  # 已过期
    events = gm.check_timeouts()
    assert len(events) == 1
    assert events[0]["expired"] is True
    assert gm.get(sid) is None  # 已清理


def test_no_timeout_when_active():
    gm = GameManager()
    gm.register("fake_game", FakeEngine)
    sid, engine = gm.create("fake_game")
    engine._expire_at = time.time() + 3600  # 很久以后
    events = gm.check_timeouts()
    assert len(events) == 0
    assert gm.get(sid) is not None


def test_remove():
    gm = GameManager()
    gm.register("fake_game", FakeEngine)
    sid, _ = gm.create("fake_game")
    gm.remove(sid)
    assert gm.get(sid) is None
    assert len(gm.list_sessions()) == 0


def test_finished_session_is_not_reused_by_player_token():
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    finished_sid = "finished1"
    gm._engines[finished_sid] = RejoinEngine(phase="finished")
    gm._created_at[finished_sid] = time.time()

    new_sid, engine = gm.create("rejoin_game", player_token="token-123")

    assert new_sid != finished_sid
    assert engine.phase == "waiting"


def test_active_session_is_reused_by_player_token():
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    active_sid = "active1"
    active_engine = RejoinEngine(phase="bidding")
    gm._engines[active_sid] = active_engine
    gm._created_at[active_sid] = time.time()

    sid, engine = gm.create("rejoin_game", player_token="token-123")

    assert sid == active_sid
    assert engine is active_engine

"""GameManager 测试"""

import time

import pytest

from agent_game_server.manager import FINISHED_RETENTION, GameManager, game_type_of

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


class TableEngine:
    """带构造参数的多人游戏，用于验证 waiting session 的参数兼容性。"""

    game_type = "table_game"
    game_name = "开桌游戏"
    player_mode = "multi"

    def __init__(self, player_count=2, dice_per_player=5):
        self.phase = "waiting"
        self.player_count = player_count
        self.dice_per_player = dice_per_player
        self.players = []

    def get_public_state(self):
        return {"phase": self.phase, "player_count": self.player_count}

    def check_timeout(self, now):
        return None


class NoPhaseEngine:
    """只有 status、完全没有 phase 属性的引擎（模拟单机游戏）。"""

    game_type = "no_phase_game"
    game_name = "无 phase 游戏"
    player_mode = "single"

    def __init__(self, status="in_progress"):
        self.status = status

    def get_public_state(self):
        return {"status": self.status}


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


def test_remove_clears_all_bookkeeping():
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    sid, engine = gm.create("rejoin_game")
    engine.phase = "finished"
    gm.check_timeouts()
    assert sid in gm._created_at
    assert sid in gm._finished_at

    gm.remove(sid)

    assert sid not in gm._engines
    assert sid not in gm._created_at
    assert sid not in gm._finished_at


# ── 跨游戏类型的 token 复用（缺陷 1）──


def test_token_from_other_game_type_does_not_leak_session():
    """拿着 A 游戏的 token 创建 B 游戏，绝不能拿回 A 的 session。"""
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    gm.register("table_game", TableEngine)

    liars_sid, liars_engine = gm.create("rejoin_game")
    assert liars_engine._find_player_by_token("token-123") is not None

    new_sid, new_engine = gm.create("table_game", player_token="token-123")

    assert new_sid != liars_sid
    assert new_engine is not liars_engine
    assert new_engine.game_type == "table_game"
    # 原 session 没被顶掉
    assert gm.get(liars_sid) is liars_engine


def test_token_reuse_for_matching_game_type_still_works():
    """同类型游戏里，活着的 session 依然按 token 重连。"""
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    gm.register("table_game", TableEngine)
    gm.create("table_game")  # 另一类型的 waiting 桌，不该被命中

    sid, engine = gm.create("rejoin_game", player_token="token-123")
    sid2, engine2 = gm.create("rejoin_game", player_token="token-123")

    assert sid2 == sid
    assert engine2 is engine


# ── waiting session 的参数兼容性（缺陷 2）──


def test_waiting_session_with_mismatched_params_creates_new_session():
    gm = GameManager()
    gm.register("table_game", TableEngine)

    sid2, engine2 = gm.create("table_game", player_count=2)
    assert engine2.player_count == 2

    sid6, engine6 = gm.create("table_game", player_count=6)

    assert sid6 != sid2
    assert engine6 is not engine2
    assert engine6.player_count == 6
    assert engine2.player_count == 2  # 老桌配置没被改动


def test_waiting_session_with_matching_params_is_reused():
    gm = GameManager()
    gm.register("table_game", TableEngine)

    sid_a, engine_a = gm.create("table_game", player_count=2)
    sid_b, engine_b = gm.create("table_game", player_count=2)

    assert sid_b == sid_a
    assert engine_b is engine_a


def test_waiting_session_reused_when_param_matches_default():
    """不带参数创建，等于用引擎默认值；带相同值的参数应该继续复用。"""
    gm = GameManager()
    gm.register("table_game", TableEngine)

    sid_default, engine_default = gm.create("table_game")
    sid_explicit, engine_explicit = gm.create("table_game", player_count=2)

    assert sid_explicit == sid_default
    assert engine_explicit is engine_default


def test_param_not_on_engine_is_ignored_for_matching():
    """引擎没有同名属性的参数跳过比较，不影响复用。"""
    gm = GameManager()
    gm.register("table_game", TableEngine)

    sid_a, engine_a = gm.create("table_game", player_count=2)
    sid_b, engine_b = gm.create("table_game", player_count=2, unknown_knob="x")

    assert sid_b == sid_a
    assert engine_b is engine_a


def test_player_token_not_part_of_param_comparison():
    """player_token 不参与配置比较 —— 它只用来找对局。"""
    gm = GameManager()
    gm.register("table_game", TableEngine)

    sid_a, engine_a = gm.create("table_game", player_count=2)
    sid_b, engine_b = gm.create("table_game", player_count=2, player_token="tok-abc")

    assert sid_b == sid_a
    assert engine_b is engine_a


# ── 已结束对局的生命周期 ──


def test_finished_session_is_not_reused_by_player_token():
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    finished_sid = "finished1"
    gm._engines[finished_sid] = RejoinEngine(phase="finished")
    gm._created_at[finished_sid] = time.time()

    new_sid, engine = gm.create("rejoin_game", player_token="token-123")

    assert new_sid != finished_sid
    assert engine.phase == "waiting"


def test_finished_session_is_not_reused_as_waiting_table():
    """phase=finished 的老桌不能被后来者当成 waiting 桌复用。"""
    gm = GameManager()
    gm.register("table_game", TableEngine)

    done_sid, done_engine = gm.create("table_game", player_count=2)
    done_engine.phase = "finished"

    new_sid, _new_engine = gm.create("table_game", player_count=2)

    assert new_sid != done_sid


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


def test_finished_game_reclaimed_after_retention():
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    done_sid, done_engine = gm.create("rejoin_game")
    done_engine.phase = "finished"
    fresh_sid, fresh_engine = gm.create("rejoin_game")
    fresh_engine.phase = "finished"

    now = time.time()
    # 首次检查只为它们记下结束时刻，两个都还在
    gm.check_timeouts(now=now)
    assert gm.get(done_sid) is not None
    assert gm.get(fresh_sid) is not None

    # 把 done_engine 的结束时刻推到保留期之外，只回收它
    gm._finished_at[done_sid] = now - FINISHED_RETENTION - 1
    gm.check_timeouts(now=now)
    assert gm.get(done_sid) is None
    assert gm.get(fresh_sid) is not None
    assert done_sid not in gm._created_at
    assert done_sid not in gm._finished_at


def test_remove_clears_finished_at():
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    sid, engine = gm.create("rejoin_game")
    engine.phase = "finished"
    gm.check_timeouts()
    assert sid in gm._finished_at

    gm.remove(sid)
    assert sid not in gm._finished_at


def test_session_finishing_during_check_is_reclaimed():
    """一次 check 里被判负的 session：先记结束时刻，保留期内不回收，
    过了保留期由后续 check 回收。"""
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    sid, engine = gm.create("rejoin_game")
    engine.phase = "bidding"

    now = time.time()

    def finish_on_check(ts):
        engine.phase = "finished"
        return {"type": "turn_timeout", "loser": 0}

    engine.check_timeout = finish_on_check

    events = gm.check_timeouts(now=now)
    assert events == [{"session_id": sid, "type": "turn_timeout", "loser": 0}]
    # 结束时刻是最开始的 now，不是更晚的时间戳
    assert gm._finished_at[sid] == now
    assert gm.get(sid) is engine  # 保留期内还在

    gm.check_timeouts(now=now + FINISHED_RETENTION + 1)
    assert gm.get(sid) is None


def test_timeout_event_reported_once_then_session_removed():
    """expired 事件在第一次轮询上报并回收，之后不再重复上报。"""
    gm = GameManager()
    gm.register("fake_game", FakeEngine)
    sid, engine = gm.create("fake_game")
    engine._expire_at = time.time() - 1

    first = gm.check_timeouts()
    second = gm.check_timeouts()

    assert len(first) == 1
    assert first[0]["session_id"] == sid
    assert second == []
    assert (sid, "timeout") in gm._seen_expired
    assert gm.get(sid) is None


def test_non_expired_timeout_event_is_not_marked_seen():
    """未过期的超时事件（判负但不删 session）不写入 _seen_expired。"""
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    sid, engine = gm.create("rejoin_game")
    engine.phase = "bidding"
    engine.check_timeout = lambda ts: {"type": "turn_timeout", "loser": 0}

    events = gm.check_timeouts()
    assert len(events) == 1
    assert (sid, "turn_timeout") not in gm._seen_expired


def test_mark_expired_dedupes_same_event():
    """_seen_expired 只让同一个 (session, 事件类型) 上报一次。"""
    gm = GameManager()

    assert gm._mark_expired("sid-1", "lobby_timeout") is True
    assert gm._mark_expired("sid-1", "lobby_timeout") is False
    # 事件类型或 session 不同都算新事件
    assert gm._mark_expired("sid-1", "turn_timeout") is True
    assert gm._mark_expired("sid-2", "lobby_timeout") is True


def test_seen_expired_cache_is_bounded():
    """_seen_expired 不能让常驻服务无限涨内存。"""
    gm = GameManager()
    from agent_game_server.manager import _MAX_SEEN_EXPIRED

    for i in range(_MAX_SEEN_EXPIRED + 50):
        gm._mark_expired(f"sid-{i}", "lobby_timeout")

    assert len(gm._seen_expired) == _MAX_SEEN_EXPIRED
    # 最新的还在，最老的被淘汰
    assert (f"sid-{_MAX_SEEN_EXPIRED + 49}", "lobby_timeout") in gm._seen_expired
    assert ("sid-0", "lobby_timeout") not in gm._seen_expired


def test_restarted_session_clears_stale_finished_at():
    """同一 session 重新开局后，旧的结束时刻要清掉，不能立刻被回收。"""
    gm = GameManager()
    gm.register("rejoin_game", RejoinEngine)
    sid, engine = gm.create("rejoin_game")
    engine.phase = "finished"

    now = time.time()
    gm.check_timeouts(now=now)
    assert sid in gm._finished_at

    # 引擎重新开了一局
    engine.phase = "bidding"
    gm.check_timeouts(now=now)
    assert sid not in gm._finished_at
    assert gm.get(sid) is engine


# ── _is_finished / game_type_of ──


def test_is_finished_handles_phase_and_status():
    assert GameManager._is_finished(RejoinEngine(phase="finished")) is True
    assert GameManager._is_finished(RejoinEngine(phase="bidding")) is False
    assert GameManager._is_finished(NoPhaseEngine(status="won")) is True
    assert GameManager._is_finished(NoPhaseEngine(status="lost")) is True
    assert GameManager._is_finished(NoPhaseEngine(status="in_progress")) is False


def test_is_finished_tolerates_none_phase():
    """phase = None 的引擎不能崩，要回落到 status。"""
    engine = RejoinEngine(phase="waiting")
    engine.phase = None
    engine.status = "won"
    assert GameManager._is_finished(engine) is True

    engine.status = "in_progress"
    assert GameManager._is_finished(engine) is False


def test_is_finished_tolerates_missing_attributes():
    class Bare:
        pass

    assert GameManager._is_finished(Bare()) is False


def test_is_finished_is_staticmethod():
    """_is_finished 是可导出的静态方法，不需要实例。"""
    assert isinstance(
        GameManager.__dict__["_is_finished"], staticmethod
    ), "_is_finished 应该保持 @staticmethod"


def test_game_type_of_helper():
    assert game_type_of(RejoinEngine()) == "rejoin_game"
    assert game_type_of(object()) == "unknown"


# ── route() 已删除 ──


def test_route_is_gone():
    """route() 没有任何调用方，已删除，避免留下无用的转发入口。"""
    assert not hasattr(GameManager(), "route")

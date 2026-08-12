"""匹配逻辑测试"""

from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.manager import GameManager


def test_first_create_gets_new_session():
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)
    _sid1, e1 = gm.create("liars_dice")
    assert e1.phase == "waiting"
    assert len(e1.players) == 0


def test_second_create_reuses_waiting_session():
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)
    sid1, e1 = gm.create("liars_dice")
    sid2, e2 = gm.create("liars_dice")
    # 应该是同一个 session
    assert sid1 == sid2
    assert e1 is e2


def test_single_player_always_creates_new():
    from agent_game_server.games.guess_number import GuessNumberEngine

    gm = GameManager()
    gm.register("guess_number", GuessNumberEngine)
    sid1, e1 = gm.create("guess_number")
    sid2, e2 = gm.create("guess_number")
    # 单人游戏每次都是新 session
    assert sid1 != sid2
    assert e1 is not e2


def test_full_sessions_dont_match():
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)
    sid1, e1 = gm.create("liars_dice")
    # 让第一个 session 进入 bidding（非 waiting）
    e1._fsm.transition_to("bidding")
    sid2, _e2 = gm.create("liars_dice")
    # 应该创建新 session
    assert sid1 != sid2

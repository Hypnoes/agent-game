"""匹配逻辑测试"""

from agent_game_server.games.guess_number import GuessNumberEngine
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


# ── waiting 桌的参数兼容性 ──


def test_matching_player_count_reuses_waiting_session():
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)

    sid_a, engine_a = gm.create("liars_dice", player_count=2)
    sid_b, engine_b = gm.create("liars_dice", player_count=2)

    assert sid_b == sid_a
    assert engine_b is engine_a
    assert engine_b.player_count == 2


def test_mismatched_player_count_creates_new_session():
    """已有 player_count=2 的等待桌时，player_count=6 不能默默挤进去。"""
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)

    sid2, engine2 = gm.create("liars_dice", player_count=2)
    assert engine2.player_count == 2

    sid6, engine6 = gm.create("liars_dice", player_count=6)

    assert sid6 != sid2
    assert engine6 is not engine2
    assert engine6.player_count == 6
    # 老桌保持原配置，等着原来的 2 人局
    assert gm.get(sid2) is engine2
    assert engine2.player_count == 2
    assert engine2.phase == "waiting"


def test_later_matching_request_reuses_the_right_table():
    """两张桌（2 人 / 6 人）各自被对应参数的请求命中。"""
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)

    sid2, engine2 = gm.create("liars_dice", player_count=2)
    sid6, engine6 = gm.create("liars_dice", player_count=6)
    assert sid2 != sid6

    again2, engine2b = gm.create("liars_dice", player_count=2)
    again6, engine6b = gm.create("liars_dice", player_count=6)

    assert again2 == sid2 and engine2b is engine2
    assert again6 == sid6 and engine6b is engine6


def test_guess_number_token_never_returns_liars_dice_session():
    """拿着大话骰的 token 创建猜数字，必须拿到新的猜数字 session。"""
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)
    gm.register("guess_number", GuessNumberEngine)

    liars_sid, liars_engine = gm.create("liars_dice")
    # 真的加入一个玩家，token 才会存在于这个引擎里
    token = liars_engine.handle_action("join", player_name="Alice")["player_token"]
    assert liars_engine.game_type == "liars_dice"

    gn_sid, gn_engine = gm.create("guess_number", player_token=token)

    assert gn_sid != liars_sid
    assert gn_engine is not liars_engine
    assert isinstance(gn_engine, GuessNumberEngine)
    assert gn_engine.game_type == "guess_number"
    # 大话骰的桌子原封不动
    assert gm.get(liars_sid) is liars_engine
    assert len(liars_engine.players) == 1


def test_guess_number_via_route_returns_correct_engine():
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)
    gm.register("guess_number", GuessNumberEngine)

    gm.create("liars_dice")
    _sid, engine = gm.create("guess_number")

    assert isinstance(engine, GuessNumberEngine)


def test_token_rejoins_same_game_type():
    """同游戏的 token 重连语义保留：回到自己那桌。"""
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)
    gm.register("guess_number", GuessNumberEngine)

    sid, engine = gm.create("liars_dice")
    token = engine.handle_action("join", player_name="Alice")["player_token"]

    again_sid, again_engine = gm.create("liars_dice", player_token=token)

    assert again_sid == sid
    assert again_engine is engine


def test_token_does_not_rejoin_finished_game():
    """已结束的对局不能通过 token 重连回来。"""
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)
    gm.register("guess_number", GuessNumberEngine)

    sid, engine = gm.create("liars_dice")
    token = engine.handle_action("join", player_name="Alice")["player_token"]
    engine.handle_action("join", player_name="Bob")
    turn = engine.current_turn
    engine.handle_action("bid", player_index=turn, quantity=1, face=3)
    engine.handle_action("challenge", player_index=engine.current_turn)
    assert engine.phase == "finished"

    new_sid, _new_engine = gm.create("liars_dice", player_token=token)

    assert new_sid != sid

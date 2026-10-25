"""大话骰规则回归测试 — 叫数合法性、淘汰循环、身份校验。

这些用例在修复前全部失败：
- `_make_bid` 不校验数量/面值，0×3 与 99×42 都能叫，且 0×N 是必胜叫法
- `__init__` 不校验 player_count / dice_per_player
- 一次开盅就结束整局，`Player.alive` 从来没被读过
- 超时判负把"下家"当成赢家
- `get_player_state(-1)` 靠 Python 负索引拿到别人的骰子
- `_validate_player` 在缺 token 时完全不校验身份
"""

import time

import pytest

from agent_game_server.games.liars_dice import LiarsDiceEngine


# ── helpers ──


def make_engine(player_count: int = 2, dice_per_player: int = 5) -> LiarsDiceEngine:
    """建好并已开局的引擎（所有玩家入场）。"""
    e = LiarsDiceEngine(player_count=player_count, dice_per_player=dice_per_player)
    for i in range(player_count):
        e.handle_action("join", player_name=f"P{i}")
    return e


def act(e: LiarsDiceEngine, action: str, **params):
    """走 handle_action，自动补上该玩家的 token（state/bid/challenge 必填）。"""
    params.setdefault("player_token", e.players[params["player_index"]].token)
    return e.handle_action(action, **params)


def set_all_dice(e: LiarsDiceEngine, face: int):
    """把全场骰子固定成同一个点数，让"实际有几个"可预测。"""
    for p in e.players:
        p.dice = [face] * len(p.dice)


# ── 叫数校验 ──


def test_bid_quantity_zero_rejected():
    """0×3 曾被接受，而且必赢：开盅时 actual >= 0 恒真，开的人反而输。"""
    e = make_engine()
    turn = e.current_turn
    with pytest.raises(ValueError, match="数量"):
        act(e, "bid", player_index=turn, quantity=0, face=3)


def test_bid_bad_face_rejected():
    e = make_engine()
    turn = e.current_turn
    with pytest.raises(ValueError, match="面值"):
        act(e, "bid", player_index=turn, quantity=99, face=42)
    with pytest.raises(ValueError, match="面值"):
        act(e, "bid", player_index=turn, quantity=1, face=0)
    with pytest.raises(ValueError, match="面值"):
        act(e, "bid", player_index=turn, quantity=1, face=7)


def test_bid_beyond_total_dice_rejected():
    """叫数超过场上骰子总数 = 按构造永远开不赢，不该让玩家叫出来。"""
    e = make_engine(player_count=2, dice_per_player=5)
    with pytest.raises(ValueError, match="超过场上骰子总数"):
        act(e, "bid", player_index=e.current_turn, quantity=11, face=3)
    with pytest.raises(ValueError, match="超过场上骰子总数"):
        act(e, "bid", player_index=e.current_turn, quantity=99, face=3)
    # 边界：正好等于总数是合法的
    act(e, "bid", player_index=e.current_turn, quantity=10, face=3)
    assert e.current_call is not None
    assert e.current_call.quantity == 10


def test_bid_total_dice_shrinks_with_eliminations():
    """上限跟着场上骰子数走：掉骰之后能叫的最大数量也要变小。"""
    e = make_engine(player_count=3, dice_per_player=2)  # 总 6 颗
    e.players[0].dice.pop()  # 总 5 颗
    with pytest.raises(ValueError, match="超过场上骰子总数"):
        act(e, "bid", player_index=e.current_turn, quantity=6, face=3)
    act(e, "bid", player_index=e.current_turn, quantity=5, face=3)


def test_bid_non_integer_rejected():
    e = make_engine()
    turn = e.current_turn
    with pytest.raises(ValueError, match="数量"):
        act(e, "bid", player_index=turn, quantity=2.5, face=3)
    with pytest.raises(ValueError, match="数量"):
        act(e, "bid", player_index=turn, quantity=True, face=3)


# ── 构造参数校验 ──


@pytest.mark.parametrize("player_count", [0, 1, -1])
def test_player_count_must_be_at_least_two(player_count):
    """player_count=1 会开出一人自叫自开的荒唐局，0 会在 _start_game 里 randint 崩溃。"""
    with pytest.raises(ValueError, match="player_count"):
        LiarsDiceEngine(player_count=player_count)


@pytest.mark.parametrize("dice_per_player", [0, -3])
def test_dice_per_player_must_be_positive(dice_per_player):
    with pytest.raises(ValueError, match="dice_per_player"):
        LiarsDiceEngine(dice_per_player=dice_per_player)


# ── 开自己的盅 ──


def test_self_challenge_rejected():
    """一人局自开自盅时 winner 与 loser 会是同一个索引；现在直接拒绝。"""
    e = make_engine(player_count=3, dice_per_player=2)
    e.current_turn = 0
    act(e, "bid", player_index=0, quantity=1, face=3)
    assert e.current_turn == 1
    e.current_turn = 0  # 构造"轮到自己、叫数也是自己"的局面
    with pytest.raises(ValueError, match="不能开自己的盅"):
        act(e, "challenge", player_index=0)


# ── 三人局淘汰循环 ──


def test_three_player_lost_challenge_costs_one_die():
    """三人局输一轮只掉一颗骰子，对局继续。"""
    e = make_engine(player_count=3, dice_per_player=2)
    set_all_dice(e, 2)
    e.current_turn = 0

    act(e, "bid", player_index=0, quantity=1, face=3)  # 全场没有 3，必假
    act(e, "challenge", player_index=1)  # 实际 0 < 1 → 叫的人输

    assert e.phase == "bidding"  # 不是一次就结束
    assert e.winner is None
    assert e.loser == 0
    assert e.call_history[-1].message.startswith("开！")
    assert e.players[0].alive is True
    assert len(e.players[0].dice) == 1  # 只掉一颗
    assert len(e.players[1].dice) == 2  # 其他人不受影响
    assert len(e.players[2].dice) == 2
    # 本轮叫数清零，由输家先叫下一轮
    assert e.current_call is None
    assert e.current_turn == 0


def test_three_player_elimination_skips_dead_player_on_turn():
    """骰子掉光才出局，回合要跳过出局者。"""
    e = make_engine(player_count=3, dice_per_player=1)
    set_all_dice(e, 2)
    e.current_turn = 0

    act(e, "bid", player_index=0, quantity=1, face=3)
    act(e, "challenge", player_index=1)  # 0 号输掉最后一颗骰子

    assert e.players[0].alive is False
    assert e.players[0].dice == []
    assert e.phase == "bidding"  # 还剩 2 人，对局继续
    assert e.current_turn == 1  # 输家已出局 → 顺延给下一位存活玩家
    with pytest.raises(ValueError, match="已出局"):
        act(e, "bid", player_index=0, quantity=1, face=3)


def test_three_player_game_ends_only_at_one_survivor():
    """三人局要淘汰到只剩一人，winner 才是最后存活者。"""
    e = make_engine(player_count=3, dice_per_player=1)
    set_all_dice(e, 2)
    e.current_turn = 0

    act(e, "bid", player_index=0, quantity=1, face=3)
    act(e, "challenge", player_index=1)  # P0 出局
    assert e.phase == "bidding"

    act(e, "bid", player_index=1, quantity=1, face=3)
    act(e, "challenge", player_index=2)  # 只剩 2 人 → 突然死亡，P1 出局

    assert e.phase == "finished"
    assert e.winner == 2  # 最后存活者，不是"下家"
    assert e.loser == 1
    assert e.get_public_state()["alive"] == [False, False, True]
    assert e.players[2].alive is True


def test_make_bid_skips_eliminated_player():
    """直接标记出局后，_make_bid 的回合推进必须绕开他。"""
    e = make_engine(player_count=3, dice_per_player=2)
    e.players[1].alive = False
    e.players[1].dice = []
    e.current_turn = 0

    act(e, "bid", player_index=0, quantity=1, face=3)
    assert e.current_turn == 2  # 跳过 1 号


def test_start_game_deals_dice_only_to_alive_players():
    """开局只给存活玩家摇骰，先手也只能落在存活玩家身上。"""
    e = LiarsDiceEngine(player_count=3, dice_per_player=2)
    e.handle_action("join", player_name="P0")
    e.handle_action("join", player_name="P1")
    e.players[0].alive = False  # 开局前已出局
    e.handle_action("join", player_name="P2")

    assert e.phase == "bidding"
    assert e.players[0].dice == []
    assert len(e.players[1].dice) == 2
    assert len(e.players[2].dice) == 2
    assert e.current_turn in (1, 2)


# ── 两人局突然死亡 ──


def test_two_player_lost_challenge_is_sudden_death():
    """两人局保留"一局定胜负"：输一轮直接出局，哪怕手里还有 5 颗骰子。"""
    e = make_engine(player_count=2, dice_per_player=5)
    set_all_dice(e, 2)
    e.current_turn = 0

    act(e, "bid", player_index=0, quantity=1, face=3)
    act(e, "challenge", player_index=1)

    assert e.phase == "finished"
    assert e.winner == 1
    assert e.loser == 0
    assert e.players[0].alive is False
    assert e.players[0].dice == []


def test_two_player_challenge_still_scoped_to_one_round():
    """两人局开一次就结束 —— 与旧行为一致。"""
    e = make_engine(player_count=2, dice_per_player=5)
    set_all_dice(e, 2)
    e.current_turn = 0
    act(e, "bid", player_index=0, quantity=1, face=3)
    state = act(e, "challenge", player_index=1)
    assert e.phase == "finished"
    assert "winner_name" in state["private"]
    assert state["private"]["winner_name"] == "P1"


# ── 超时判负 ──


def test_three_player_timeout_eliminates_current_player():
    """超时者输掉本轮（掉骰），不是让"下家"白捡胜利。"""
    e = make_engine(player_count=3, dice_per_player=2)
    turn = e.current_turn

    e.last_action = time.time() - 61
    result = e.check_timeout(time.time())

    assert result == {"type": "turn_timeout", "loser": turn}
    assert e.phase == "bidding"  # 三人局继续
    assert e.winner is None  # 关键：不再是 (turn + 1) % 3
    assert e.loser == turn
    assert len(e.players[turn].dice) == 1  # 掉一颗
    assert e.players[turn].alive is True
    assert e.current_turn != turn  # 回合交出去，否则会一直超时


def test_three_player_timeout_can_end_the_game():
    e = make_engine(player_count=3, dice_per_player=1)
    turn = e.current_turn
    e.last_action = time.time() - 61
    e.check_timeout(time.time())
    assert e.players[turn].alive is False
    assert e.phase == "bidding"

    e.players[(turn + 1) % 3].dice = []
    e.players[(turn + 1) % 3].alive = False  # 只剩 2 → 下一跳是突然死亡
    e.last_action = time.time() - 61
    e.check_timeout(time.time())

    assert e.phase == "finished"
    assert e.winner == (turn + 2) % 3


def test_two_player_timeout_winner_is_survivor():
    e = make_engine(player_count=2, dice_per_player=5)
    turn = e.current_turn
    e.last_action = time.time() - 61
    e.check_timeout(time.time())
    assert e.phase == "finished"
    assert e.winner == 1 - turn
    assert e.loser == turn


# ── 视图与越界 ──


def test_get_player_state_rejects_negative_index():
    """`players[-1]` 会绕回最后一名玩家 —— 负索引必须当作越界。"""
    e = make_engine(player_count=2, dice_per_player=5)
    with pytest.raises(ValueError, match="无效的玩家编号"):
        e.get_player_state(-1)


def test_get_player_state_rejects_out_of_range_index():
    e = make_engine(player_count=2, dice_per_player=5)
    with pytest.raises(ValueError, match="无效的玩家编号"):
        e.get_player_state(99)


def test_negative_index_never_leaks_opponent_dice():
    """越界报错而不是返回别人骰子：确认没拿到任何 private 数据。"""
    e = make_engine(player_count=2, dice_per_player=5)
    try:
        state = e.get_player_state(-1)
    except ValueError:
        leaked = []
    else:  # 若实现没抛错，至少不能是 P1 的骰子
        leaked = state["private"]["your_dice"]
    assert leaked != e.players[1].dice


def test_remaining_time_clamped_to_zero():
    """超时之后 remaining 不能渲染成负数。"""
    e = make_engine(player_count=2, dice_per_player=5)
    e.current_turn = 0
    e.last_action = time.time() - 200  # 回合早已超时
    prompt = e.get_player_state(0)["prompt"]
    assert "⏱ 剩余 0 秒" in prompt
    assert "-" not in prompt.split("⏱")[1].split("秒")[0]


# ── 身份校验（P0：缺 token 就能替别人行动）──


@pytest.mark.parametrize("action", ["state", "bid", "challenge"])
def test_actions_require_player_token(action):
    e = make_engine(player_count=2, dice_per_player=5)
    act(e, "bid", player_index=e.current_turn, quantity=1, face=3)
    turn = e.current_turn
    kwargs = {"player_index": turn}
    if action == "bid":
        kwargs.update(quantity=2, face=3)
    with pytest.raises(ValueError, match="缺少 player_token"):
        e.handle_action(action, **kwargs)


def test_empty_player_token_rejected():
    e = make_engine(player_count=2, dice_per_player=5)
    with pytest.raises(ValueError, match="缺少 player_token"):
        e.handle_action("state", player_index=0, player_token="")


def test_foreign_token_rejected():
    e = make_engine(player_count=2, dice_per_player=5)
    turn = e.current_turn
    other = 1 - turn
    with pytest.raises(ValueError, match="player_token 不匹配"):
        e.handle_action(
            "bid",
            player_index=turn,
            player_token=e.players[other].token,
            quantity=1,
            face=3,
        )
    assert e.current_call is None  # 没替别人叫出去


def test_join_without_token_still_works():
    e = LiarsDiceEngine(player_count=2, dice_per_player=5)
    first = e.handle_action("join", player_name="Alice")
    assert first["player_index"] == 0
    assert first["player_token"]
    second = e.handle_action("join", player_name="Bob")
    assert second["player_index"] == 1
    assert e.phase == "bidding"


def test_join_with_own_token_reconnects():
    """带自己的 token 走重连：不追加玩家、不重摇骰子、不推状态机。"""
    e = make_engine(player_count=2, dice_per_player=5)
    turn = e.current_turn
    act(e, "bid", player_index=turn, quantity=1, face=3)
    dice_before = [list(p.dice) for p in e.players]
    call_before = (e.current_call.quantity, e.current_call.face)

    state = e.handle_action(
        "join", player_name="P0", player_token=e.players[0].token
    )

    assert len(e.players) == 2
    assert [list(p.dice) for p in e.players] == dice_before
    assert (e.current_call.quantity, e.current_call.face) == call_before
    assert e.phase == "bidding"
    assert state["player_index"] == 0
    assert state["dice"] == dice_before[0]
    assert state["reconnected"] is True

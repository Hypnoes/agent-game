"""端到端整局：从 join 打到 finished，中途插入超时。

这一层是其它层照不到的。规则层按"一个动作"看，契约层按"一次请求"看，而真实
缺陷常常跨动作：叫数 → 有人超时 → 下家该干什么。P0-1（超时后僵尸叫数把下家
锁死）就是这样漏掉的——任何一个单动作用例都测不到它。

用例用固定随机种子，跑几十手直到分出胜负；断言的是"这局能走完并且每一步都合法"，
不是某个具体骰子值。
"""

import random
import time

import pytest

from agent_game_server.games.liars_dice import LiarsDiceEngine


def _play_until_finished(engine, tokens, *, rng, inject_timeout=False, max_steps=200):
    """把一局打完，返回 (steps, used_timeout)。

    策略是最简单的人类打法：能加码就加一个小步（不超总数），否则开盅。
    每若干手插入一次"当前回合者超时"，用来把超时路径混进正常对局里。
    """
    steps = 0
    used_timeout = False

    while engine.phase != "finished" and steps < max_steps:
        steps += 1
        turn = engine.current_turn
        token = tokens[turn]

        if inject_timeout and not used_timeout and steps == 3:
            # 中途让当前回合者超时一次
            engine.last_action = time.time() - engine.turn_timeout - 1
            event = engine.check_timeout(time.time())
            assert event is not None, "超时没有产生事件"
            used_timeout = True
            continue

        if engine.current_call is None:
            engine.handle_action(
                "bid", player_index=turn, player_token=token, quantity=1, face=rng.randint(1, 6)
            )
            continue

        call = engine.current_call
        # 试试能不能加码：数量 +1 且不超过场上骰子总数
        if call.quantity + 1 <= engine._total_dice():
            try:
                engine.handle_action(
                    "bid",
                    player_index=turn,
                    player_token=token,
                    quantity=call.quantity + 1,
                    face=call.face,
                )
                continue
            except ValueError:
                pass

        # 加不了码就开盅
        try:
            engine.handle_action("challenge", player_index=turn, player_token=token)
        except ValueError:
            # 不能开自己的盅：换成加一个更大的点
            engine.handle_action(
                "bid",
                player_index=turn,
                player_token=token,
                quantity=call.quantity,
                face=min(6, call.face + 1),
            )

    return steps, used_timeout


@pytest.mark.parametrize("player_count", [2, 3, 4])
@pytest.mark.parametrize("seed", [1, 7, 42])
def test_full_game_reaches_finished(player_count, seed):
    """任意人数、任意骰子组合，对局都必须能走完并产生唯一赢家。"""
    rng = random.Random(seed)
    random.seed(seed)

    engine = LiarsDiceEngine(player_count=player_count, dice_per_player=2)
    for i in range(player_count):
        engine.handle_action("join", player_name=f"P{i}")
    tokens = {i: p.token for i, p in enumerate(engine.players)}

    steps, _ = _play_until_finished(engine, tokens, rng=rng)

    assert engine.phase == "finished", f"{steps} 手之后仍未结束"
    assert steps < 200, "对局没有收敛"
    assert engine.winner is not None
    assert engine.players[engine.winner].alive is True
    # 只剩一名存活玩家
    assert sum(1 for p in engine.players if p.alive) == 1


@pytest.mark.parametrize("seed", [1, 7, 42])
def test_full_game_with_a_timeout_in_the_middle(seed):
    """超时插进正常对局之后，这局仍然要能走完。

    这条直接对应 P0-1：超时留下僵尸叫数时，下家会在"加码超总数 / 开别人的盅"
    之间进退两难，对局卡死或胜负错乱。
    """
    rng = random.Random(seed)
    random.seed(seed)

    engine = LiarsDiceEngine(player_count=3, dice_per_player=2)
    for i in range(3):
        engine.handle_action("join", player_name=f"P{i}")
    tokens = {i: p.token for i, p in enumerate(engine.players)}

    steps, used_timeout = _play_until_finished(engine, tokens, rng=rng, inject_timeout=True)

    assert used_timeout, "用例没有真的注入超时"
    assert engine.phase == "finished", f"{steps} 手之后仍未结束（超时残留可能导致锁死）"
    assert engine.winner is not None


def test_every_bid_in_a_full_game_is_legal_at_the_time_it_is_made():
    """整局里每一次叫数都必须在当时合法。

    与上一条不同：那条只要求"能走完"，这条要求"走完的过程本身干净"。
    一个允许 0×N 或者超过总数叫数的实现对局也能走完，只是规则已经不成立了。
    """
    rng = random.Random(11)
    random.seed(11)

    engine = LiarsDiceEngine(player_count=3, dice_per_player=2)
    for i in range(3):
        engine.handle_action("join", player_name=f"P{i}")
    tokens = {i: p.token for i, p in enumerate(engine.players)}

    previous = None
    for _ in range(200):
        if engine.phase == "finished":
            break
        call = engine.current_call
        if call is not None:
            assert 1 <= call.quantity <= engine._total_dice(), (
                f"叫数 {call.quantity} 超过场上骰子总数 {engine._total_dice()}"
            )
            assert 1 <= call.face <= 6
            if previous is not None:
                assert call.quantity > previous.quantity or (
                    call.quantity == previous.quantity and call.face > previous.face
                ), "叫数没有比上家大"
        previous = call
        _play_until_finished(engine, tokens, rng=rng, max_steps=1)

    assert engine.phase == "finished"


def test_a_player_never_acts_out_of_turn_in_a_full_game():
    """整局里没有人能在别人回合行动。"""
    rng = random.Random(3)
    random.seed(3)

    engine = LiarsDiceEngine(player_count=3, dice_per_player=2)
    for i in range(3):
        engine.handle_action("join", player_name=f"P{i}")
    tokens = {i: p.token for i, p in enumerate(engine.players)}

    for _ in range(200):
        if engine.phase == "finished":
            break
        turn = engine.current_turn
        other = next(
            (i for i in range(3) if i != turn and engine.players[i].alive), None
        )
        if other is None:
            break

        # 用错编号（别人的）行动必须被拒。拒绝原因可能是"没轮到你"，也可能
        # 是别的（比如已出局），这里只要求"被拒绝"，不绑定具体措辞。
        with pytest.raises(ValueError):
            engine.handle_action(
                "bid", player_index=other, player_token=tokens[other], quantity=1, face=2
            )

        _play_until_finished(engine, tokens, rng=rng, max_steps=1)

    assert engine.phase == "finished"

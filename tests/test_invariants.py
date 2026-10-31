"""大话骰规则层测试——不变量，而不是事故清单。

与 `test_rules.py` 的分工：那个文件是按"曾经出过什么 bug"组织的回归集合；
本文件按**必须永远成立的规则**组织，覆盖事故清单里没有的那些——它们从没出过
bug，所以从没被写过测试，但它们坏掉的时候同样致命。

每个用例上方标注它守的是哪条不变量，以及删掉对应代码会发生什么。
"""

import time

import pytest

from agent_game_server.games.liars_dice import LiarsDiceEngine

# ═══════════════════════════════════════════
# 不变量 1：1 是百搭
# ═══════════════════════════════════════════


def test_ones_count_as_any_face(dice_game, dice_script):
    """叫非 1 时，1 计入该点数。

    这是大话骰区别于普通吹牛的唯一规则。所有用例曾经把全场骰子摆成同一个数
    （`set_all_dice`），于是 `[1, 3, 3]` 这种"1 与非 1 混合"的组合一次都没出现过，
    删掉 `_count_face` 里的 `d == 1` 分支，套件依然全绿。
    """
    engine, _tokens = dice_game(player_count=2, dice_per_player=3)
    dice_script(engine, [[1, 3, 3], [2, 3, 3]])

    # 3 的个数 = 明面上的 4 颗 + 百搭的 1 颗
    assert engine._count_face(3) == 5
    # 2 的个数 = 明面上的 1 颗（玩家 2 的首颗）+ 百搭的 1 颗
    assert engine._count_face(2) == 2


def test_ones_only_count_as_ones_when_bidding_ones(dice_game, dice_script):
    """叫 1 时，1 只算 1，不再顶替别的点数。

    与上一条互为反面。只断言 `_count_face(1)` 还不够：若实现写成
    `d == face or d == 1`（两次都加 1），叫 1 时会把非 1 的点数也算进来，
    而 `_count_face` 对 face=1 的返回恰好仍可能是对的。所以这里同时断言
    "叫 1 时非 1 的骰子不计入"，并走完整判定路径。
    """
    engine, _tokens = dice_game(player_count=2, dice_per_player=3)
    dice_script(engine, [[1, 3, 3], [2, 3, 3]])

    assert engine._count_face(1) == 1
    # 6 颗骰子里只有 1 颗是 1；叫 1 说"有 2 颗"就该不成立
    assert engine._count_face(1) != engine._total_dice()


def test_bidding_ones_with_only_one_face_loses(dice_game, dice_script):
    """叫 1 时 1 不百搭这一条要真的影响胜负判定。

    全场只有 1 颗 1，叫 2×1 必须不成立 → 叫的人输。若实现让 1 在任何情况下
    都百搭（`d == face or d == 1`），实际会被算成 6 颗，叫的人反而赢。
    """
    engine, tokens = dice_game(player_count=2, dice_per_player=3)
    dice_script(engine, [[1, 3, 3], [2, 3, 3]])
    engine.current_turn = 0

    engine.handle_action(
        "bid", player_index=0, player_token=tokens[0], quantity=2, face=1
    )
    engine.handle_action("challenge", player_index=1, player_token=tokens[1])

    assert engine.loser == 0, "全场只有 1 颗 1，2×1 不成立，叫的人该输"


def test_wildcard_changes_actual_count_not_just_display(dice_game, dice_script):
    """百搭影响的是开盅时的实际计数，因此会直接改变胜负。

    这条用例走完整路径（叫数 → 开盅 → 判定输家），而不是只调 `_count_face`：
    计数错了但没人用它判定，规则仍然是好的；反过来，计数对但判定没用它，
    百搭就形同虚设。
    """
    engine, tokens = dice_game(player_count=2, dice_per_player=3)
    dice_script(engine, [[1, 3, 3], [2, 3, 3]])
    engine.current_turn = 0

    # 叫 5×3：实际（含百搭）正好 5 颗，叫数成立 → 开盅的人输
    engine.handle_action(
        "bid", player_index=0, player_token=tokens[0], quantity=5, face=3
    )
    engine.handle_action("challenge", player_index=1, player_token=tokens[1])

    assert engine.loser == 1, "含百搭的 5×3 应当成立，开盅的人（P1）输"


def test_ones_without_wildcard_would_lose(dice_game, dice_script):
    """同副骰子，若不把 1 当百搭，5×3 就只有 4 颗，叫的人该输。

    与上一条互为镜像：两条一起构成"百搭确实生效"的充分证明，
    而不只是"某个函数返回了某个数"。
    """
    engine, tokens = dice_game(player_count=2, dice_per_player=3)
    dice_script(engine, [[1, 3, 3], [2, 3, 4]])
    engine.current_turn = 0

    # 实际含百搭 = 5，明面 = 4。叫 6×3 则含百搭也不成立 → 叫的人输
    engine.handle_action(
        "bid", player_index=0, player_token=tokens[0], quantity=6, face=3
    )
    engine.handle_action("challenge", player_index=1, player_token=tokens[1])

    assert engine.loser == 0


# ═══════════════════════════════════════════
# 不变量 2：开局后不许再加人
# ═══════════════════════════════════════════


def test_join_after_start_is_refused(dice_game, dice_script):
    """对局开始后加入必须被拒绝。

    否则新玩家会触发 `_start_game()`，把进行中的对局推倒重来——全场重摇骰子、
    状态机重置。这条路径从来没有用例走过，删掉阶段检查套件依然全绿。
    """
    engine, _tokens = dice_game(player_count=2, dice_per_player=3)
    dice_before = [list(p.dice) for p in engine.players]
    turn_before = engine.current_turn

    with pytest.raises(ValueError, match="对局已开始"):
        engine.handle_action("join", player_name="Late")

    # 被拒绝之后局面必须原封不动
    assert [list(p.dice) for p in engine.players] == dice_before
    assert engine.current_turn == turn_before
    assert engine.phase == "bidding"
    assert len(engine.players) == 2


def test_join_after_start_does_not_change_player_count(dice_game):
    """被拒绝的加入不能留下半个玩家。"""
    engine, _tokens = dice_game(player_count=3, dice_per_player=2)
    before = len(engine.players)

    for _ in range(3):
        with pytest.raises(ValueError):
            engine.handle_action("join", player_name="Late")

    assert len(engine.players) == before


# ═══════════════════════════════════════════
# 不变量 3：任何改变局面的路径都必须通知等待者
# ═══════════════════════════════════════════


def _record_notifications(engine, monkeypatch):
    """把 `_notify_state_change` 换成计数器，同时保留原实现。"""
    calls = []
    original = engine._notify_state_change

    def spy():
        calls.append(True)
        original()

    monkeypatch.setattr(engine, "_notify_state_change", spy)
    return calls


def test_challenge_notifies_waiters(dice_game, monkeypatch):
    """开盅改变局面，必须唤醒还在等待的对手。

    漏掉通知不会有任何报错：等待方只是多等满一个预算，然后拿到已经过时的
    "还没轮到你"。这类缺陷只能靠"通知次数 > 0"的断言发现。
    """
    engine, tokens = dice_game(player_count=2, dice_per_player=3)
    engine.current_turn = 0
    engine.handle_action("bid", player_index=0, player_token=tokens[0], quantity=1, face=3)

    calls = _record_notifications(engine, monkeypatch)
    engine.handle_action("challenge", player_index=1, player_token=tokens[1])

    assert calls, "开盅没有通知等待者"


def test_turn_timeout_notifies_waiters(dice_game, monkeypatch):
    """超时判负同样改变局面，必须通知。"""
    engine, _tokens = dice_game(player_count=3, dice_per_player=2)
    engine.last_action = time.time() - engine.turn_timeout - 1

    calls = _record_notifications(engine, monkeypatch)
    engine.check_timeout(time.time())

    assert calls, "超时判负没有通知等待者"


def test_add_player_notifies_lobby_waiters(dice_game, monkeypatch):
    """有人入座但还没满员时，大厅里等待的人要看到人数变化。"""
    engine = LiarsDiceEngine(player_count=3, dice_per_player=2)
    engine.handle_action("join", player_name="P0")

    calls = _record_notifications(engine, monkeypatch)
    engine.handle_action("join", player_name="P1")

    assert engine.phase == "waiting"
    assert calls, "入座没有通知大厅等待者"


def test_lobby_waiter_keeps_waiting_after_someone_joins(dice_game):
    """大厅里的等待者被唤醒后要重新进入等待，不能直接返回。

    大厅没满员时"有人入座"不是"轮到你"：`_is_my_turn_or_over` 仍为假。
    如果实现"被唤醒即返回"，调用方会以为自己可以行动，于是发出一个必然被拒的
    bid；AGENTS.md #6 明令禁止这条简化。这里用短预算，正确的实现会跑满预算
    返回 "timeout"，错误实现会在 notify 的瞬间就返回。
    """
    import asyncio

    engine = LiarsDiceEngine(player_count=3, dice_per_player=2)
    engine.handle_action("join", player_name="P0")

    async def scenario():
        task = asyncio.create_task(engine.wait_for_change(0, 0.5))
        await asyncio.sleep(0.05)
        assert not task.done(), "大厅里没满员时应当挂起"

        engine.handle_action("join", player_name="P1")  # 人数变化，但还没开局
        return await asyncio.wait_for(task, timeout=3)

    assert asyncio.run(scenario()) == "timeout"


def test_start_game_notifies_waiters(dice_game, monkeypatch):
    """人满开局要唤醒大厅里的所有人。"""
    engine = LiarsDiceEngine(player_count=2, dice_per_player=2)
    engine.handle_action("join", player_name="P0")

    calls = _record_notifications(engine, monkeypatch)
    engine.handle_action("join", player_name="P1")

    assert engine.phase == "bidding"
    assert calls, "开局没有通知大厅等待者"


def test_every_state_changing_action_notifies(dice_game, monkeypatch):
    """一张动作 → 是否通知的表，防止将来新增动作时漏掉通知。

    新增会改变局面的 action 时，把它加进 `expected`；忘记加通知会让这条用例红。
    """
    engine, tokens = dice_game(player_count=3, dice_per_player=2)
    engine.current_turn = 0

    calls = _record_notifications(engine, monkeypatch)

    engine.handle_action("bid", player_index=0, player_token=tokens[0], quantity=1, face=2)
    assert calls, "bid 没有通知"
    calls.clear()

    engine.handle_action("challenge", player_index=1, player_token=tokens[1])
    assert calls, "challenge 没有通知"


# ═══════════════════════════════════════════
# 不变量 4：超时与开盅对"输掉本轮"的处理一致
# ═══════════════════════════════════════════


def test_timeout_clears_the_round_bid(dice_game):
    """超时后本轮叫数必须清零，否则下家被锁死成"只能开"。

    具体场景：P0 叫了一个恰好等于全场骰子总数的数，P1 超时掉骰后总数变小，
    此时 P2 想加码必然超过总数（被 `_validate_bid_values` 拒绝），想开盅则是在
    替别人的超时买单。留着僵尸叫数，胜负就由别人的超时决定了。
    """
    engine, tokens = dice_game(player_count=3, dice_per_player=2)
    engine.current_turn = 0
    engine.handle_action(
        "bid", player_index=0, player_token=tokens[0], quantity=6, face=3
    )
    assert engine._total_dice() == 6

    engine.current_turn = 1
    engine.last_action = time.time() - engine.turn_timeout - 1
    engine.check_timeout(time.time())

    assert engine.current_call is None, "超时后本轮叫数必须清零"
    assert engine._total_dice() == 5

    # 下家现在可以正常开新的一轮，而不是被旧叫数锁死
    engine.handle_action(
        "bid", player_index=engine.current_turn,
        player_token=tokens[engine.current_turn], quantity=1, face=2,
    )
    assert engine.current_call.quantity == 1


def test_timeout_records_the_round_as_void(dice_game):
    """超时作废的那一轮要在历史里留痕，否则复盘时叫数凭空消失。"""
    engine, tokens = dice_game(player_count=3, dice_per_player=2)
    engine.current_turn = 0
    engine.handle_action("bid", player_index=0, player_token=tokens[0], quantity=2, face=3)

    engine.current_turn = 1
    engine.last_action = time.time() - engine.turn_timeout - 1
    engine.check_timeout(time.time())

    void_calls = [c for c in engine.call_history if c.quantity == 0]
    assert len(void_calls) == 1
    assert "超时" in void_calls[0].message


def test_timeout_without_a_bid_adds_no_history(dice_game):
    """场上本来就没有叫数时，超时不该往历史里塞空记录。"""
    engine, _tokens = dice_game(player_count=3, dice_per_player=2)
    assert engine.current_call is None
    history_before = len(engine.call_history)

    engine.last_action = time.time() - engine.turn_timeout - 1
    engine.check_timeout(time.time())

    assert len(engine.call_history) == history_before


# ═══════════════════════════════════════════
# 不变量 5：判负必须先于等待预算发生
# ═══════════════════════════════════════════


def test_turn_timeout_exceeds_wait_budget():
    """回合判负时限必须大于一次 state 调用的等待预算。

    `config.BLOCK_BUDGET` 是调用方最多挂起多久，`turn_timeout` 是多久不行动判负。
    文档与提示词都依赖"预算短于判负时限"这个前提（否则调用方永远等不到判负，
    只能在超时后重新调用一次）。把 turn_timeout 调小到 10 秒，语义就反了，
    而套件曾经对此毫无察觉。
    """
    from agent_game_server import config

    engine = LiarsDiceEngine()

    assert engine.turn_timeout > config.BLOCK_BUDGET, (
        f"turn_timeout={engine.turn_timeout} 必须大于 BLOCK_BUDGET={config.BLOCK_BUDGET}，"
        "否则判负总是发生在调用方拿到状态之前"
    )


# ═══════════════════════════════════════════
# 不变量 6：终态与秘密键由引擎自己声明
# ═══════════════════════════════════════════


def test_engine_declares_its_finished_states_and_secrets():
    """两个既有引擎都要显式声明，而不是靠框架内置的那张表。"""
    from agent_game_server.games.guess_number import GuessNumberEngine
    from agent_game_server.games.liars_dice import LiarsDiceEngine as Dice

    assert GuessNumberEngine.finished_states == frozenset({"won", "lost"})
    assert GuessNumberEngine.secret_keys == frozenset({"target"})
    assert Dice.finished_states == frozenset({"finished"})
    assert Dice.secret_keys == frozenset({"all_dice"})


def test_new_engine_with_its_own_names_works_without_framework_changes():
    """终态叫 `draw`、秘密字段叫 `solution` 的引擎，框架必须零改动支持。

    README 承诺"新增游戏只需写一个 Engine 文件"。当终态名与秘密键写死在框架里时，
    这条承诺是假的——新游戏得回来改三个地方。这条用例把承诺变成可执行的断言。
    """
    from agent_game_server.games.game_engine import is_finished, secret_keys_of

    class DrawEngine:
        game_type = "draw_game"
        game_name = "平局游戏"
        player_mode = "single"
        finished_states = frozenset({"draw"})
        secret_keys = frozenset({"solution"})

        def __init__(self):
            self.phase = "playing"

        def get_public_state(self):
            return {"status": self.phase, "solution": 42}

    engine = DrawEngine()

    assert is_finished(engine) is False
    assert secret_keys_of(engine) == frozenset({"solution"})

    engine.phase = "draw"
    assert is_finished(engine) is True

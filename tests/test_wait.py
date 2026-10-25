"""阻塞式等待测试 — ChangeSignal 原语与 LiarsDiceEngine.wait_for_change。

全部用例注入短超时，不使用真实的 55 秒等待，整个文件应在 2 秒内跑完。
"""

import asyncio
import time

from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.manager import GameManager


def _started_engine() -> LiarsDiceEngine:
    """两名玩家已加入、对局进入 bidding 的引擎。"""
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    return e


def test_ready_returns_immediately():
    """轮到自己时不进入等待。"""

    async def scenario():
        e = _started_engine()
        started = time.monotonic()
        result = await e.wait_for_change(e.current_turn, 5.0)
        return result, time.monotonic() - started

    result, elapsed = asyncio.run(scenario())
    assert result == "ready"
    assert elapsed < 0.1


def test_wakes_on_opponent_bid():
    """后台任务等待期间对手叫数并轮到它，0.1 秒内返回 "ready"。
    返回值语义：只有真正轮到自己（或对局结束）才返回 "ready"；
    别人行动导致的唤醒会重新进入等待，不会被误当成「该我行动了」。
    """

    async def scenario():
        e = _started_engine()
        waiter = (e.current_turn + 1) % 2

        async def opponent_bids():
            await asyncio.sleep(0.01)
            e.handle_action("bid", player_index=e.current_turn, quantity=2, face=3, player_token=e.players[e.current_turn].token)

        async with asyncio.TaskGroup() as tg:
            bidder = tg.create_task(opponent_bids())
            started = time.monotonic()
            result = await e.wait_for_change(waiter, 5.0)
            elapsed = time.monotonic() - started
            await bidder

        return result, elapsed

    result, elapsed = asyncio.run(scenario())
    assert result == "ready"
    assert elapsed < 0.1


def test_returns_at_wait_budget():
    """传入 0.2 秒预算时约 0.2 秒返回 "timeout"。"""

    async def scenario():
        e = _started_engine()
        waiter = (e.current_turn + 1) % 2
        started = time.monotonic()
        result = await e.wait_for_change(waiter, 0.2)
        return result, time.monotonic() - started

    result, elapsed = asyncio.run(scenario())
    assert result == "timeout"
    assert 0.15 <= elapsed < 1.0


def test_no_lost_wakeup():
    """先订阅、再叫数、再等待，立即返回——订阅与等待之间的行动不能丢。"""

    async def scenario():
        e = _started_engine()
        started = time.monotonic()

        ev = e._signal.subscribe()  # 模拟"已订阅、还没进入等待"的中间态
        e.handle_action("bid", player_index=e.current_turn, quantity=2, face=3, player_token=e.players[e.current_turn].token)
        try:
            result = await e._signal.wait(5.0, ev)
        finally:
            e._signal.unsubscribe(ev)

        return result, time.monotonic() - started

    result, elapsed = asyncio.run(scenario())
    assert result == "changed", "ChangeSignal 原语返回 changed；轮次判定在 wait_for_change 里"
    assert elapsed < 0.1


def test_cancellation_cleans_waiters():
    """取消等待任务后等待者列表为空。"""

    async def scenario():
        e = _started_engine()
        waiter = (e.current_turn + 1) % 2

        task = asyncio.create_task(e.wait_for_change(waiter, 5.0))
        await asyncio.sleep(0.01)  # 让任务真正挂起
        pending = e._signal.waiter_count

        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        return pending, e._signal.waiter_count

    pending, remaining = asyncio.run(scenario())
    assert pending == 1
    assert remaining == 0


def test_timeout_transitions_game():
    """turn_timeout 到期后 check_timeouts() 将当前回合玩家判负。"""
    gm = GameManager()
    gm.register("liars_dice", LiarsDiceEngine)
    _sid, e = gm.create("liars_dice")
    e.handle_action("join", player_name="Alice")
    e.handle_action("join", player_name="Bob")
    current = e.current_turn

    # 未到期时不应判负
    assert gm.check_timeouts(now=time.time()) == []
    assert e.phase == "bidding"

    # 推到判负时限之外
    expired_at = e.last_action + e.turn_timeout + 1
    events = gm.check_timeouts(now=expired_at)

    assert len(events) == 1
    assert events[0]["type"] == "turn_timeout"
    assert e.phase == "finished"
    assert e.loser == current
    assert e.winner == (current + 1) % 2


def test_multi_waiter_all_woken():
    """两个等待者同时被唤醒。"""

    async def scenario():
        e = _started_engine()
        waiter = (e.current_turn + 1) % 2

        tasks = [
            asyncio.create_task(e.wait_for_change(waiter, 5.0)) for _ in range(2)
        ]
        await asyncio.sleep(0.01)  # 两个都挂起
        pending = e._signal.waiter_count

        e.handle_action("bid", player_index=e.current_turn, quantity=2, face=3, player_token=e.players[e.current_turn].token)
        results = await asyncio.gather(*tasks)
        return pending, results

    pending, results = asyncio.run(scenario())
    assert pending == 2
    assert results == ["ready", "ready"]

"""轮询器 / 广播状态 / 提示词热重载的回归测试。

这里守的是三条「静默失效」的线：

1. `_poll_and_broadcast` 是仓库里**唯一**跑 `manager.check_timeouts()` 的地方。
   它一死，对局再也不会超时、session 永不回收、admin 页面永不更新，而且没有
   任何异常冒出来。所以它必须扛住任意单个引擎的异常。
2. 一个坏掉的 WebSocket 不能中断整轮广播，否则其余管理员页面一起停止更新。
3. 广播状态（连接集合 / 后台任务 / 上次签名）必须按 app 实例隔离。共享模块级
   全局时，内层 app 的 shutdown 会取消掉外层 app 的 poller。
"""

import asyncio
import logging

import pytest

from agent_game_server import game_server
from agent_game_server.games.game_engine import GameEngine
from agent_game_server.games.guess_number import GuessNumberEngine
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.manager import GameManager

# ── 测试替身 ──


class _RecordingSocket:
    """能收消息的 WebSocket 替身。"""

    def __init__(self) -> None:
        self.messages: list[str] = []

    async def send_text(self, message: str) -> None:
        self.messages.append(message)


class _BrokenSocket:
    """每次发送都抛 ConnectionError —— 模拟对端已经断开、连接被重置。"""

    def __init__(self) -> None:
        self.attempts = 0

    async def send_text(self, _message: str) -> None:
        self.attempts += 1
        raise ConnectionError("connection reset by peer")


def _manager(*engine_classes: type) -> GameManager:
    """只注册指定引擎的干净 manager，避免测试之间互相串状态。"""
    gm = GameManager()
    for engine_cls in engine_classes:
        gm.register(engine_cls.game_type, engine_cls)
    return gm


def _bare_engine(*, check_timeout=None, get_public_state=None) -> type:
    """造一个最小引擎；只覆盖用例关心的那两个方法。"""

    class _Engine:
        game_type = "bare"
        game_name = "最小引擎"
        player_mode = "single"

        def __init__(self) -> None:
            pass

        def get_public_state(self):
            return {"status": "ok"}

        def get_player_state(self, player_index: int = 0):
            return {"public": {}, "prompt": ""}

        def handle_action(self, action: str, **params):
            return {}

        def check_timeout(self, now: float):
            return None

    if check_timeout is not None:
        _Engine.check_timeout = check_timeout
    if get_public_state is not None:
        _Engine.get_public_state = get_public_state
    return _Engine


@pytest.fixture
def isolated(monkeypatch):
    """把模块级 manager 换成测试用的，并在结束后恢复。

    `AdminBroadcast()` 不传 manager 时会回落到模块级 `manager`，所以替换它
    就足以让新构造的实例指向本用例的数据。
    """

    def install(*engine_classes: type) -> GameManager:
        gm = _manager(*engine_classes)
        monkeypatch.setattr(game_server, "manager", gm)
        return gm

    return install


async def _run_poller_for(broadcast, seconds: float = 1.2):
    """跑一会儿轮询器，返回任务本身（调用方负责取消）。"""
    task = asyncio.create_task(game_server._poll_and_broadcast(broadcast))
    broadcast.track(task)
    await asyncio.sleep(seconds)
    return task


# ═══════════════════════════════════════════
# 1. 轮询器必须扛住任意单引擎异常
# ═══════════════════════════════════════════


def test_poller_survives_unserializable_engine_state(isolated):
    """引擎的公开状态里有 datetime 时，轮询器要继续跑。

    这是最危险的一条：签名算不出来曾经直接把 poller 打死，
    `_background_tasks` 里不再有它，没有任何东西会重启它。
    """
    gm = isolated(
        _bare_engine(
            # 就是不可 JSON 序列化的东西
            get_public_state=lambda self: {"when": object()}
        )
    )
    gm.create("bare")

    broadcast = game_server.AdminBroadcast(gm)

    async def scenario():
        task = await _run_poller_for(broadcast)
        try:
            assert not task.done(), "轮询器被不可序列化的状态杀死了"
            assert broadcast.last_signature == game_server._SIGNATURE_ERROR
        finally:
            await broadcast.cancel_all()

    asyncio.run(scenario())


def test_poller_keeps_polling_after_serialization_failure(isolated):
    """序列化失败之后仍然继续结算超时，而不是卡死或退出。"""
    gm = isolated(_bare_engine(get_public_state=lambda self: {"when": object()}))
    gm.create("bare")
    broadcast = game_server.AdminBroadcast(gm)

    calls = {"n": 0}
    original = gm.check_timeouts

    def counting_check_timeouts():
        calls["n"] += 1
        return original()

    gm.check_timeouts = counting_check_timeouts

    async def scenario():
        await _run_poller_for(broadcast, seconds=1.3)
        await broadcast.cancel_all()
        # 0.5s 一轮：1.3 秒至少应该跑到 2 轮
        assert calls["n"] >= 2, f"轮询只跑了 {calls['n']} 轮"

    asyncio.run(scenario())


def test_poller_survives_engine_raising_in_check_timeout(isolated):
    """一个引擎的 check_timeout 抛异常，不能带走整个轮询循环。"""

    def explode(self, now):
        raise RuntimeError("check_timeout 炸了")

    gm = isolated(_bare_engine(check_timeout=explode))
    gm.create("bare")
    broadcast = game_server.AdminBroadcast(gm)

    async def scenario():
        task = await _run_poller_for(broadcast)
        try:
            assert not task.done(), "check_timeout 抛异常把轮询器杀了"
        finally:
            await broadcast.cancel_all()

    asyncio.run(scenario())


def test_poller_logs_iteration_failure(isolated, caplog):
    """异常被记进日志，而不是被无声吞掉。"""

    def explode(self, now):
        raise RuntimeError("check_timeout 炸了")

    gm = isolated(_bare_engine(check_timeout=explode))
    gm.create("bare")
    broadcast = game_server.AdminBroadcast(gm)

    async def scenario():
        await _run_poller_for(broadcast, seconds=0.7)
        await broadcast.cancel_all()

    with caplog.at_level(logging.ERROR, logger=game_server.__name__):
        asyncio.run(scenario())

    assert any(
        "poll iteration failed" in record.getMessage() for record in caplog.records
    ), "轮询异常没有留下日志"


def test_admin_state_signature_never_raises(isolated):
    """签名函数对坏数据返回哨兵，不向上抛。"""
    gm = isolated(_bare_engine(get_public_state=lambda self: {"when": object()}))
    gm.create("bare")
    broadcast = game_server.AdminBroadcast(gm)

    # 不抛异常，且返回值不等同于任何真实签名
    assert game_server._admin_state_signature(broadcast) == game_server._SIGNATURE_ERROR


def test_signature_failure_forces_broadcast_every_round(isolated):
    """序列化一直失败时，每轮都要广播，不能只推一次就静默。"""
    gm = isolated(_bare_engine(get_public_state=lambda self: {"when": object()}))
    gm.create("bare")
    broadcast = game_server.AdminBroadcast(gm)
    ws = _RecordingSocket()
    broadcast.clients.add(ws)

    async def scenario():
        assert await game_server._refresh_admin_state(broadcast) is True
        assert await game_server._refresh_admin_state(broadcast) is True
        assert await game_server._refresh_admin_state(broadcast) is True

    asyncio.run(scenario())

    assert len(ws.messages) == 3, "签名持续失败时应该每轮都重试广播"


# ═══════════════════════════════════════════
# 2. 一个坏 socket 不能中断整轮广播
# ═══════════════════════════════════════════


def test_broadcast_survives_connection_error_and_reaches_others(isolated):
    """ConnectionError 只丢弃它自己，其余客户端照常收到消息。"""
    isolated()
    broadcast = game_server.AdminBroadcast()
    broken = _BrokenSocket()
    first = _RecordingSocket()
    second = _RecordingSocket()
    broadcast.clients.update({broken, first, second})

    asyncio.run(game_server._broadcast_admin_state(broadcast))

    assert broken not in broadcast.clients, "坏掉的连接应该被丢弃"
    assert first in broadcast.clients and second in broadcast.clients
    assert len(first.messages) == 1
    assert len(second.messages) == 1


def test_refresh_survives_connection_error(isolated):
    """广播阶段抛 ConnectionError 时，_refresh_admin_state 不把异常抛给轮询器。"""
    gm = isolated(_bare_engine())
    gm.create("bare")
    broadcast = game_server.AdminBroadcast(gm)
    broken = _BrokenSocket()
    healthy = _RecordingSocket()
    broadcast.clients.update({broken, healthy})

    assert asyncio.run(game_server._refresh_admin_state(broadcast)) is True
    assert len(healthy.messages) == 1
    assert broken not in broadcast.clients


def test_poller_survives_connection_error(isolated):
    """端到端：坏 socket 在循环里持续失败，轮询器依然活着并继续推进签名。"""
    gm = isolated(_bare_engine())
    gm.create("bare")
    broadcast = game_server.AdminBroadcast(gm)
    broadcast.clients.add(_BrokenSocket())

    async def scenario():
        task = await _run_poller_for(broadcast)
        try:
            assert not task.done(), "ConnectionError 杀死了轮询器"
            assert broadcast.clients == set(), "坏客户端应被清理干净"
        finally:
            await broadcast.cancel_all()

    asyncio.run(scenario())


def test_broadcast_drops_every_dead_socket(isolated):
    """多个坏 socket 全部清掉，健康的不受影响。"""
    isolated()
    broadcast = game_server.AdminBroadcast()
    dead = [_BrokenSocket() for _ in range(3)]
    alive = _RecordingSocket()
    broadcast.clients.update(dead)
    broadcast.clients.add(alive)

    asyncio.run(game_server._broadcast_admin_state(broadcast))

    assert broadcast.clients == {alive}
    assert len(alive.messages) == 1


# ═══════════════════════════════════════════
# 3. 广播状态按实例隔离
# ═══════════════════════════════════════════


def test_broadcast_instances_do_not_share_clients_or_tasks(isolated):
    """两个 AdminBroadcast 互看不到对方的连接和任务。"""
    isolated()
    one = game_server.AdminBroadcast()
    two = game_server.AdminBroadcast()

    socket_one = _RecordingSocket()
    one.clients.add(socket_one)

    assert two.clients == set()

    async def scenario():
        task = asyncio.create_task(asyncio.sleep(30))
        one.track(task)
        try:
            assert len(one.tasks) == 1
            assert len(two.tasks) == 0, "任务集合被两个实例共享了"
        finally:
            await one.cancel_all()

    asyncio.run(scenario())


def test_cancelling_one_broadcast_leaves_the_other_running(isolated):
    """取消 one 不能动 two 的任务——这正是旧模块级全局的故障。"""
    isolated()
    one = game_server.AdminBroadcast()
    two = game_server.AdminBroadcast()

    async def scenario():
        task_one = asyncio.create_task(asyncio.sleep(30))
        task_two = asyncio.create_task(asyncio.sleep(30))
        one.track(task_one)
        two.track(task_two)

        await one.cancel_all()

        assert task_one.cancelled() or task_one.done()
        assert not task_two.done(), "内层 shutdown 取消了外层的后台任务"

        await two.cancel_all()

    asyncio.run(scenario())


def test_independent_pollers_survive_each_others_shutdown(isolated):
    """两台「app」各自跑 poller，取消一台不影响另一台。"""
    gm_one = isolated(_bare_engine())
    broadcast_one = game_server.AdminBroadcast(gm_one)
    broadcast_two = game_server.AdminBroadcast(gm_one)

    async def scenario():
        task_one = asyncio.create_task(game_server._poll_and_broadcast(broadcast_one))
        task_two = asyncio.create_task(game_server._poll_and_broadcast(broadcast_two))
        broadcast_one.track(task_one)
        broadcast_two.track(task_two)
        await asyncio.sleep(0.7)

        assert not task_one.done() and not task_two.done()

        await broadcast_one.cancel_all()
        await asyncio.sleep(0.1)

        assert task_one.done(), "one 自己的任务应该被取消"
        assert not task_two.done(), "one 的 shutdown 波及了 two 的 poller"

        await broadcast_two.cancel_all()

    asyncio.run(scenario())


def test_track_removes_finished_tasks(isolated):
    """任务结束后自动移出集合，避免长期运行无限增长。"""
    isolated()
    broadcast = game_server.AdminBroadcast()

    async def scenario():
        task = asyncio.create_task(asyncio.sleep(0))
        broadcast.track(task)
        assert len(broadcast.tasks) == 1
        await asyncio.sleep(0.05)
        assert broadcast.tasks == set(), "完成的任务没有被移出"

    asyncio.run(scenario())


def test_nested_lifespans_do_not_cancel_outer_poller(isolated):
    """复现原始故障：外层 app 的 poller 在内层 app 关停后必须还活着。

    修复前两个 lifespan 共享模块级 `_background_tasks`，内层 shutdown 把
    外层任务一起取消（实测 poller 任务数 2 → 0），外层从此不再结算超时。
    """
    isolated(_bare_engine())

    async def scenario():
        outer = game_server.AdminBroadcast()
        inner = game_server.AdminBroadcast()

        outer_task = asyncio.create_task(game_server._poll_and_broadcast(outer))
        outer.track(outer_task)
        await asyncio.sleep(0.2)

        inner_task = asyncio.create_task(game_server._poll_and_broadcast(inner))
        inner.track(inner_task)
        await asyncio.sleep(0.2)

        # 两个 poller 同时在线
        assert len(outer.tasks) == 1
        assert len(inner.tasks) == 1

        await inner.cancel_all()  # 内层 app shutdown
        await asyncio.sleep(0.1)

        assert inner_task.done(), "内层 poller 应该被自己取消"
        assert not outer_task.done(), "内层 shutdown 取消了外层 poller（原始 bug）"

        await outer.cancel_all()
        assert outer_task.done()

    asyncio.run(scenario())


def test_broadcast_binds_explicit_manager(isolated):
    """构造时注入的 manager 优先于模块级 manager（为将来 create_app 铺路）。"""
    module_manager = isolated(_bare_engine())
    own_manager = _manager(_bare_engine())
    broadcast = game_server.AdminBroadcast(own_manager)

    assert broadcast.manager is own_manager
    assert broadcast.manager is not module_manager


# ═══════════════════════════════════════════
# 4. reload_prompts 只算真正有提示词的引擎
# ═══════════════════════════════════════════


class _NoPromptsEngine:
    """没有任何提示词模板的引擎——不实现 get_prompts。"""

    game_type = "no_prompts"
    game_name = "无提示词"
    player_mode = "single"

    @classmethod
    def describe_tools(cls):
        return []

    def get_public_state(self):
        return {}


class _CountingPrompts:
    """get_prompts() 的返回值替身，记录被 reload 了几次。"""

    def __init__(self) -> None:
        self.reloads = 0

    def reload(self) -> None:
        self.reloads += 1


def test_engine_without_own_get_prompts_has_no_attribute():
    """hasattr 必须是诚实的能力探测。

    协议基类上的 `...` 桩方法在运行时是真实可调用的，子类会继承它，
    于是 hasattr 恒为真——探测彻底失效。
    """
    assert not hasattr(_NoPromptsEngine, "get_prompts")
    assert not hasattr(_NoPromptsEngine(), "get_prompts")


def test_game_engine_protocol_has_no_runtime_stubs():
    """协议本身不该在运行时留下可选方法的桩。"""
    assert "get_prompts" not in GameEngine.__dict__
    assert "describe_tools" not in GameEngine.__dict__


def test_reload_prompts_excludes_engine_without_prompts(isolated, monkeypatch):
    """没有自己 get_prompts 的引擎不应出现在 reloaded 列表里。"""
    gm = isolated(GuessNumberEngine, _NoPromptsEngine)
    prompts = _CountingPrompts()
    monkeypatch.setattr(GuessNumberEngine, "get_prompts", classmethod(lambda cls: prompts))

    class _Request:
        headers = {"X-Admin-Token": game_server._ADMIN_TOKEN}

    result = game_server.reload_prompts(_Request())

    assert result["reloaded"] == ["guess_number"]
    assert "no_prompts" not in result["reloaded"]
    assert prompts.reloads == 1


def test_reload_prompts_includes_both_real_engines(isolated):
    """两个真实引擎都有自己的提示词，都应被重载。"""
    isolated(GuessNumberEngine, LiarsDiceEngine)

    class _Request:
        headers = {"X-Admin-Token": game_server._ADMIN_TOKEN}

    result = game_server.reload_prompts(_Request())

    assert set(result["reloaded"]) == {"guess_number", "liars_dice"}


def test_reload_prompts_is_idempotent(isolated):
    """连续两次热重载都不报错，且都返回同一份列表。"""
    isolated(GuessNumberEngine, LiarsDiceEngine)

    class _Request:
        headers = {"X-Admin-Token": game_server._ADMIN_TOKEN}

    first = game_server.reload_prompts(_Request())
    second = game_server.reload_prompts(_Request())

    assert first["ok"] is True and second["ok"] is True
    assert first["reloaded"] == second["reloaded"]


# ═══════════════════════════════════════════
# 5. 工具面在协议改动后保持不变
# ═══════════════════════════════════════════


def test_describe_all_tools_still_returns_seven():
    """协议重构不能让工具面缩水。"""
    gm = _manager(GuessNumberEngine, LiarsDiceEngine)
    tools = gm.describe_all_tools()
    assert len(tools) == 7, f"工具数变成 {len(tools)}"
    names = {tool["name"] for tool in tools}
    assert len(names) == 7, "工具名有重复"


def test_engine_tool_counts_unchanged():
    """分引擎的数量也要保持不变。"""
    assert len(GuessNumberEngine.describe_tools()) == 3
    assert len(LiarsDiceEngine.describe_tools()) == 4


def test_describe_all_tools_skips_engine_without_tools():
    """没有 describe_tools 的引擎被跳过，而不是被当成「有」然后报错。"""
    gm = _manager(GuessNumberEngine, _NoPromptsEngine)
    tools = gm.describe_all_tools()
    assert len(tools) == 3

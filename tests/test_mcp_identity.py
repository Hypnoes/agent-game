"""MCP 层的身份边界：`session_id` 只定位对局，不证明身份。

`session_id` 是一个 8 位短随机串，能被猜到或被日志带出来。如果它可以替代
`player_token`，那么任何知道 session_id 的人就能读到对手的私有骰子，还能替
对手叫数/开盅——大话骰的全部玩法建立在私有信息上，这条一松，游戏就没有意义。

`AGENTS.md` #3 把这件事写成"身份来自 player_token，mcp-session-id 只是连接
缓存键"。这里把它变成可执行的断言。
"""

import pytest

from agent_game_server import mcp_server


async def _execute(manager, tool: str, arguments: dict) -> str:
    """直接走 MCP 的 `_execute`，不需要握手就能验证身份规则。"""
    return await mcp_server._execute(manager, None, tool, dict(arguments))


# ═══════════════════════════════════════════
# 1. 显式 session_id + 无 token：必须拒绝
# ═══════════════════════════════════════════


@pytest.mark.anyio
@pytest.mark.parametrize("action", ["state", "bid", "challenge"])
async def test_explicit_session_id_requires_token(make_manager, action):
    """带 session_id 但不带 player_token 的多人动作必须被拒绝。

    这条守卫曾经在套件里没有任何用例：把它删掉，264 条测试依然全绿。
    实际行为恰好没泄密（引擎自己还有一层 token 校验），但那是**第二道**防线，
    不是这道守卫的依据——守卫失效时能挡住，不代表它还有存在的理由。
    """
    gm = make_manager()
    sid, engine = gm.create("liars_dice")
    engine.handle_action("join", player_name="Alice")
    engine.handle_action("join", player_name="Bob")

    arguments = {"session_id": sid, "player_index": engine.current_turn}
    if action == "bid":
        arguments.update(quantity=1, face=2)

    with pytest.raises(ValueError, match="player_token"):
        await _execute(gm, f"liars_dice_{action}", arguments)


@pytest.mark.anyio
async def test_explicit_session_id_with_token_is_accepted(make_manager):
    """对照组：带上正确 token 时允许显式指定 session_id（合法重连）。"""
    gm = make_manager()
    sid, engine = gm.create("liars_dice")
    engine.handle_action("join", player_name="Alice")
    engine.handle_action("join", player_name="Bob")
    turn = engine.current_turn

    text = await _execute(
        gm,
        "liars_dice_state",
        {
            "session_id": sid,
            "player_index": turn,
            "player_token": engine.players[turn].token,
            "timeout": 0,
        },
    )

    assert text


@pytest.mark.anyio
async def test_single_player_game_needs_no_token(make_manager):
    """单人引擎没有 players 列表，不受这条规则约束。"""
    gm = make_manager()
    sid, _engine = gm.create("guess_number")

    text = await _execute(gm, "guess_number_state", {"session_id": sid})
    assert text


# ═══════════════════════════════════════════
# 2. session_id 指错游戏类型要拒绝
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_session_id_of_another_game_type_is_rejected(make_manager):
    """把大话骰的 session_id 拿去调猜数字的工具，必须报错。

    否则会话会被缓存成"这种游戏的会话"，后续调用落进一个不认识的引擎。
    """
    gm = make_manager()
    dice_sid, _dice = gm.create("liars_dice")

    with pytest.raises(ValueError, match="不能用于"):
        await _execute(
            gm, "guess_number_state", {"session_id": dice_sid}
        )


# ═══════════════════════════════════════════
# 3. 未知会话
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_unknown_session_id_raises_session_not_found(make_manager):
    gm = make_manager()

    with pytest.raises(mcp_server.SessionNotFound):
        await _execute(gm, "liars_dice_state", {"session_id": "nope1234", "timeout": 0})


@pytest.mark.anyio
async def test_unknown_session_does_not_create_a_new_game(make_manager):
    """显式给了一个不存在的 session_id 时不能顺手建一局新的。"""
    gm = make_manager()

    with pytest.raises(mcp_server.SessionNotFound):
        await _execute(gm, "liars_dice_state", {"session_id": "nope1234"})

    assert not gm._engines


# ═══════════════════════════════════════════
# 4. 过期缓存
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_stale_session_does_not_silently_recreate(make_manager):
    """指向一个已被回收的对局时，明确报"会话不存在"，不静默建新局。

    这与"没有 session_id"是两种意图：给了就没有，说明调用方记错了；
    没给才是"我不知道，帮我找一个"。混为一谈会让调用方以为自己在操作旧局。
    """
    gm = make_manager()
    sid, _engine = gm.create("liars_dice")
    gm.remove(sid)

    with pytest.raises(mcp_server.SessionNotFound):
        await _execute(
            gm,
            "liars_dice_bid",
            {"session_id": sid, "player_index": 0, "player_token": "x", "quantity": 1, "face": 2},
        )

    assert not gm._engines, "不能顺手建出一局新的"


# ═══════════════════════════════════════════
# 5. 守卫本身：即使引擎另有防线，MCP 层也要拒绝
# ═══════════════════════════════════════════


@pytest.mark.anyio
@pytest.mark.parametrize("action", ["bid", "challenge"])
async def test_session_guard_rejects_before_the_engine_is_reached(make_manager, action):
    """用 bid/challenge 证明守卫在 MCP 层就生效，不依赖引擎的第二道防线。

    为什么不用 state：`_run_action` 里新加的"等待前先校验身份"已经把无 token 的
    state 挡住了，于是 state 路径上看不出这条守卫有没有用。bid/challenge 不走
    那条校验，守卫是唯一的一道——真实引擎的 `_validate_player` 会在**引擎层**
    再挡一次，所以要看清守卫本身，得让引擎不挡。

    用一个不校验 token 的引擎：把 MCP 层的 `explicit_session_id + 无 token` 守卫
    删掉，这个用例立刻红。两道防线不是一道——引擎将来放宽校验（比如加观战
    模式）时，MCP 层这条才是防止按 session_id 直取对手对局的那道。
    """

    class NoGuardEngine:
        """多人引擎，但**不**校验 token——用来暴露 MCP 层守卫是否独立生效。"""

        game_type = "no_guard"
        game_name = "不校验 token 的引擎"
        player_mode = "multi"

        def __init__(self):
            self.players = [
                {"name": "A", "token": "tok-a"},
                {"name": "B", "token": "tok-b"},
            ]
            self.phase = "bidding"

        def _find_player_by_token(self, token):
            for i, player in enumerate(self.players):
                if player["token"] == token:
                    return i
            return None

        def handle_action(self, action_name, **params):
            return {"action": action_name, "private": "SECRET-DICE"}

        def get_public_state(self):
            return {"phase": self.phase}

        def get_player_state(self, player_index=0):
            return {"private": "SECRET-DICE"}

    gm = make_manager()
    gm.register("no_guard", NoGuardEngine)
    sid, _engine = gm.create("no_guard")

    # 全新连接（缓存为空）显式指定别人的 session_id，且不带 token
    mcp_server._MCP_SESSIONS.clear()

    arguments = {"session_id": sid, "player_index": 0}
    if action == "bid":
        arguments.update(quantity=1, face=2)

    with pytest.raises(ValueError, match="player_token"):
        await _execute(gm, f"no_guard_{action}", arguments)


@pytest.mark.anyio
async def test_guard_absent_would_leak(make_manager):
    """对照组：带 token 时这只引擎正常返回，说明上面的拒绝不是因为接口坏了。"""
    gm = make_manager()

    class NoGuardEngine:
        game_type = "no_guard"
        game_name = "不校验 token 的引擎"
        player_mode = "multi"

        def __init__(self):
            self.players = [{"name": "A", "token": "tok-a"}]
            self.phase = "bidding"

        def _find_player_by_token(self, token):
            return 0 if token == "tok-a" else None

        def handle_action(self, action_name, **params):
            return {"action": action_name}

        def get_public_state(self):
            return {}

        def get_player_state(self, player_index=0):
            return {}

    gm.register("no_guard", NoGuardEngine)
    sid, _engine = gm.create("no_guard")
    mcp_server._MCP_SESSIONS.clear()

    text = await _execute(
        gm, "no_guard_challenge", {"session_id": sid, "player_index": 0, "player_token": "tok-a"}
    )

    assert "challenge" in text

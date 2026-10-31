"""契约层：REST 与 MCP 对同一批输入必须给出同一类回答。

这一层的存在理由是一个真实缺陷：REST 分支"解析 → 校验 → 等待"，MCP 分支
"等待 → 校验"。同一个 token 打错的 state 调用，REST 立刻 400，MCP 白等满
55 秒才报错。两条传输各自实现一遍同样的判断，就一定会分叉。

因此这里不测"某个函数返回什么"，只测**两条通道的回答是否同类**：
成功还是失败、失败属于"调用方输入问题"还是"服务端问题"、以及是否 fast-fail。

断言刻意避开具体状态码与中文文案：改文案、把 400 调成 422 都不该让这一层变红，
只有"两条通道开始给出不同答案"才会。
"""

import time
from contextlib import asynccontextmanager

import httpx
import pytest

from agent_game_server.game_server import create_app
from conftest import McpHarness

# ═══════════════════════════════════════════
# 两条通道
# ═══════════════════════════════════════════


class RestChannel:
    """REST 通道：一次 POST /games/{sid}/action。"""

    kind = "rest"

    def __init__(self, http: httpx.AsyncClient):
        self.http = http

    async def call(self, payload: dict, sid: str) -> tuple[bool, str]:
        body = {"action": payload["action"], **{k: v for k, v in payload.items() if k != "action"}}
        resp = await self.http.post(f"/games/{sid}/action", json=body)
        return _classify_http(resp.status_code)


class McpChannel:
    """MCP 通道：一次 tools/call。

    MCP 只有 `isError` 一个位，细节在文本里。服务端内部错误有固定前缀，
    其余 isError 都归为调用方错误——这与 REST 的 4xx/5xx 划分是同构的。
    """

    kind = "mcp"

    def __init__(self, harness: McpHarness, session, game_type="liars_dice"):
        self.harness = harness
        self.session = session
        self.game_type = game_type

    async def call(self, payload: dict, sid: str) -> tuple[bool, str]:
        tool = f"{self.game_type}_{payload['action']}"
        arguments = {k: v for k, v in payload.items() if k != "action"}
        arguments["session_id"] = sid
        result = (await self.harness.call_tool(self.session, tool, arguments))["result"]
        text = result["content"][0]["text"]
        if not result.get("isError"):
            return True, "ok"
        if "服务端内部错误" in text:
            return False, "server_error"
        return False, "client_error"


def _classify_http(status: int) -> tuple[bool, str]:
    if status == 200:
        return True, "ok"
    if 400 <= status < 500:
        return False, "client_error"
    return False, "server_error"


@asynccontextmanager
async def both_channels(make_manager):
    """打开两条通道，共用同一局对局。

    必须共用：比较两局不同的游戏没有意义。

    REST 也走 ASGI 内存传输（httpx.AsyncClient + ASGITransport），而不是
    `TestClient`：后者自带一个后台线程与独立事件循环，和这里的 lifespan
    混用会触发 "future belongs to a different loop"。两条通道跑在同一个
    事件循环上，才谈得上比较。
    """
    gm = make_manager()
    app = create_app(gm)
    sid, engine = gm.create("liars_dice")
    engine.handle_action("join", player_name="Alice")
    engine.handle_action("join", player_name="Bob")
    tokens = {i: p.token for i, p in enumerate(engine.players)}

    async with app.router.lifespan_context(app):
        transport = httpx.ASGITransport(app=app)
        async with httpx.AsyncClient(transport=transport, base_url="http://test") as http:
            harness = McpHarness(http, gm)
            session = await harness.open_session()
            yield RestChannel(http), McpChannel(harness, session), sid, tokens, engine


# ═══════════════════════════════════════════
# 非法输入：两条通道都必须 fast-fail，且同类
# ═══════════════════════════════════════════


BAD_IDENTITY_CASES = {
    "wrong-token": lambda tokens: {"action": "state", "player_index": 0, "player_token": "not-mine"},
    "missing-token": lambda tokens: {"action": "state", "player_index": 0},
    "out-of-range": lambda tokens: {"action": "state", "player_index": 99, "player_token": "x"},
    "negative-index": lambda tokens: {"action": "state", "player_index": -1, "player_token": tokens[0]},
    "string-index": lambda tokens: {"action": "state", "player_index": "zero", "player_token": tokens[0]},
}


@pytest.mark.anyio
@pytest.mark.parametrize("case", BAD_IDENTITY_CASES)
async def test_bad_identity_is_a_client_error_on_both_channels(make_manager, case):
    """非法身份在两条通道上都是"调用方错误"，不是服务器崩了。"""
    async with both_channels(make_manager) as (rest, mcp, sid, tokens, _engine):
        payload = BAD_IDENTITY_CASES[case](tokens)

        rest_result = await rest.call(payload, sid)
        mcp_result = await mcp.call(payload, sid)

    assert rest_result[0] is False, f"REST 接受了非法请求: {case}"
    assert mcp_result[0] is False, f"MCP 接受了非法请求: {case}"
    assert rest_result[1] == mcp_result[1] == "client_error", (
        f"两条通道对 {case} 的归类不同: REST={rest_result}, MCP={mcp_result}"
    )


@pytest.mark.anyio
@pytest.mark.parametrize("case", BAD_IDENTITY_CASES)
async def test_bad_identity_fails_fast_on_both_channels(make_manager, case):
    """非法身份不能先挂满等待预算才报错。

    这是 P1-2 的回归：MCP 曾经"先等待、后校验"，错 token 要白等 55 秒。
    这里把预算压到 2 秒，阈值取 0.5 秒——既远小于一个预算，又给 ASGI 往返留余量。
    两条通道都测同一个阈值，分叉会立刻暴露。
    """
    from agent_game_server import config

    original = config.BLOCK_BUDGET
    config.BLOCK_BUDGET = 2.0
    try:
        async with both_channels(make_manager) as (rest, mcp, sid, tokens, _engine):
            payload = BAD_IDENTITY_CASES[case](tokens)

            started = time.monotonic()
            await rest.call(payload, sid)
            rest_elapsed = time.monotonic() - started

            started = time.monotonic()
            await mcp.call(payload, sid)
            mcp_elapsed = time.monotonic() - started
    finally:
        config.BLOCK_BUDGET = original

    assert rest_elapsed < 0.5, f"REST 对 {case} 等了 {rest_elapsed:.2f}s 才报错"
    assert mcp_elapsed < 0.5, f"MCP 对 {case} 等了 {mcp_elapsed:.2f}s 才报错"


# ═══════════════════════════════════════════
# 合法输入：两条通道都要接受
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_valid_state_is_accepted_on_both_channels(make_manager):
    """对照组：合法请求两条通道都放行，说明上面的断言不是"一律拒绝"换来的。"""
    async with both_channels(make_manager) as (rest, mcp, sid, tokens, engine):
        payload = {
            "action": "state",
            "player_index": engine.current_turn,
            "player_token": tokens[engine.current_turn],
            "timeout": 0,
        }
        rest_result = await rest.call(payload, sid)
        mcp_result = await mcp.call(payload, sid)

    assert rest_result == (True, "ok")
    assert mcp_result == (True, "ok")


@pytest.mark.anyio
async def test_missing_required_argument_is_a_client_error_on_both_channels(make_manager):
    """"少一个必填参数"不能变成服务端错误。"""
    async with both_channels(make_manager) as (rest, mcp, sid, tokens, _engine):
        payload = {"action": "bid", "player_index": 0, "player_token": tokens[0]}

        rest_result = await rest.call(payload, sid)
        mcp_result = await mcp.call(payload, sid)

    assert rest_result[1] == "client_error", rest_result
    assert mcp_result[1] == "client_error", mcp_result


# ═══════════════════════════════════════════
# timeout 语义：两条通道对同一个值理解一致
# ═══════════════════════════════════════════


@pytest.mark.anyio
async def test_timeout_zero_means_no_wait_on_both_channels(make_manager):
    """`timeout=0` 在两条通道上都是"本次不等待"。"""
    async with both_channels(make_manager) as (rest, mcp, sid, tokens, engine):
        not_turn = 1 - engine.current_turn
        payload = {
            "action": "state",
            "player_index": not_turn,
            "player_token": tokens[not_turn],
            "timeout": 0,
        }

        started = time.monotonic()
        rest_result = await rest.call(payload, sid)
        rest_elapsed = time.monotonic() - started

        started = time.monotonic()
        mcp_result = await mcp.call(payload, sid)
        mcp_elapsed = time.monotonic() - started

    assert rest_result == (True, "ok")
    assert mcp_result == (True, "ok")
    assert rest_elapsed < 0.5, f"REST timeout=0 等了 {rest_elapsed:.2f}s"
    assert mcp_elapsed < 0.5, f"MCP timeout=0 等了 {mcp_elapsed:.2f}s"


@pytest.mark.anyio
async def test_non_numeric_timeout_is_rejected_on_both_channels(make_manager):
    """`timeout="abc"` 两条通道都不能静默当成默认预算。"""
    async with both_channels(make_manager) as (rest, mcp, sid, tokens, engine):
        payload = {
            "action": "state",
            "player_index": engine.current_turn,
            "player_token": tokens[engine.current_turn],
            "timeout": "abc",
        }

        rest_result = await rest.call(payload, sid)
        mcp_result = await mcp.call(payload, sid)

    assert rest_result[0] is False, rest_result
    assert mcp_result[0] is False, mcp_result
    assert rest_result[1] == mcp_result[1] == "client_error"

"""game_server 行为测试。"""

import asyncio
import time

import pytest
from fastapi.testclient import TestClient

from agent_game_server import game_server
from agent_game_server.config import parse_block_budget
from agent_game_server.games.guess_number import GuessNumberEngine
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.manager import GameManager


class _FakeWebSocket:
    def __init__(self):
        self.messages: list[str] = []

    async def send_text(self, message: str):
        self.messages.append(message)


class _FakeManager:
    def __init__(self, sessions: list[dict]):
        self._sessions = sessions
        self.timeout_calls = 0
        self.create_calls = []

    def check_timeouts(self):
        self.timeout_calls += 1
        return []

    def list_sessions(self):
        return self._sessions

    def create(self, game_type: str, **params):
        self.create_calls.append((game_type, params))
        return "sid-1234", object()


def test_refresh_admin_state_uses_full_snapshot():
    """指纹不变就不广播；变了才推一次。

    广播状态已收进 AdminBroadcast 实例（每个 app 一份），所以这里显式构造
    一个实例注入，而不是改模块级全局——语义不变：签名相同不广播。
    """
    ws = _FakeWebSocket()
    manager = _FakeManager([{"session_id": "abc", "status": "waiting", "target": 1}])
    broadcast = game_server.AdminBroadcast()
    broadcast.manager = manager
    broadcast.clients.add(ws)

    assert asyncio.run(game_server._refresh_admin_state(broadcast)) is True
    assert manager.timeout_calls == 1
    assert len(ws.messages) == 1

    assert asyncio.run(game_server._refresh_admin_state(broadcast)) is False
    assert manager.timeout_calls == 2
    assert len(ws.messages) == 1

    manager._sessions = [{"session_id": "abc", "status": "waiting", "target": 2}]
    assert asyncio.run(game_server._refresh_admin_state(broadcast)) is True
    assert manager.timeout_calls == 3
    assert len(ws.messages) == 2


def test_create_game_forwards_request_params():
    original_manager = game_server.manager

    class _Request:
        async def json(self):
            return {"player_token": "token-123"}

    try:
        manager = _FakeManager([])
        game_server.manager = manager

        result = asyncio.run(game_server.create_game("liars_dice", _Request()))

        assert result["session_id"] == "sid-1234"
        assert manager.create_calls == [("liars_dice", {"player_token": "token-123"})]
    finally:
        game_server.manager = original_manager


# ═══════════════════════════════════════════
# HTTP 契约 — 用独立 manager 的 TestClient 夹具
# ═══════════════════════════════════════════


@pytest.fixture
def client(monkeypatch):
    """一个干净的 manager + TestClient，用例结束后恢复模块级 manager。

    `create_app()` 每次新建 app：`StreamableHTTPSessionManager.run()` 只能在
    同一实例上进入一次，复用模块级 app 会在第二个用例抛 RuntimeError。

    raise_server_exceptions=False：把服务端未捕获异常变成 500 响应，
    这样断言"不是 500"才有意义（否则异常会直接冒到测试里）。
    """
    gm = GameManager()
    gm.register("guess_number", GuessNumberEngine)
    gm.register("liars_dice", LiarsDiceEngine)
    monkeypatch.setattr(game_server, "manager", gm)
    with TestClient(game_server.create_app(), raise_server_exceptions=False) as c:
        yield c


def _started_dice(client) -> tuple[str, str, str]:
    """开一局两人大话骰，返回 (session_id, Alice token, Bob token)。"""
    sid = client.post("/games/liars_dice", json={}).json()["session_id"]
    a = client.post(
        f"/games/{sid}/action", json={"action": "join", "player_name": "Alice"}
    ).json()["player_token"]
    b = client.post(
        f"/games/{sid}/action", json={"action": "join", "player_name": "Bob"}
    ).json()["player_token"]
    return sid, a, b


# ── 1. 玩家状态端点的身份校验 ──


def test_player_state_requires_token(client):
    sid, _a, _b = _started_dice(client)
    assert client.get(f"/games/{sid}/state/0").status_code == 403


def test_player_state_accepts_correct_token(client):
    sid, a, _b = _started_dice(client)
    resp = client.get(f"/games/{sid}/state/0", params={"player_token": a})
    assert resp.status_code == 200
    assert "private" in resp.json()


def test_player_state_rejects_wrong_token(client):
    sid, _a, _b = _started_dice(client)
    resp = client.get(f"/games/{sid}/state/0", params={"player_token": "not-mine"})
    assert resp.status_code == 403


def test_player_state_out_of_range_is_404(client):
    sid, a, _b = _started_dice(client)
    assert client.get(f"/games/{sid}/state/99", params={"player_token": a}).status_code == 404


def test_single_player_state_stays_open(client):
    """单人引擎没有 players 属性，保持原有开放语义。"""
    sid = client.post("/games/guess_number", json={}).json()["session_id"]
    assert client.get(f"/games/{sid}/state/0").status_code == 200


# ── 2. 公开状态不泄露秘密 ──


def test_public_state_hides_guess_number_target(client):
    sid = client.post("/games/guess_number", json={}).json()["session_id"]
    assert "target" not in client.get(f"/games/{sid}").json()


def test_public_state_hides_liars_dice_all_dice(client):
    sid, _a, _b = _started_dice(client)
    assert "all_dice" not in client.get(f"/games/{sid}").json()


def test_admin_detail_still_exposes_secrets(client):
    """秘密信息只在 admin 出口可见。"""
    gsid = client.post("/games/guess_number", json={}).json()["session_id"]
    dsid, _a, _b = _started_dice(client)
    headers = {"X-Admin-Token": game_server._ADMIN_TOKEN}

    assert "target" in client.get(f"/admin/games/{gsid}", headers=headers).json()
    assert "all_dice" in client.get(f"/admin/games/{dsid}", headers=headers).json()


def test_public_state_does_not_mutate_engine_state(client):
    """剥键必须作用在副本上，不能改动引擎自己的状态。"""
    sid = client.post("/games/guess_number", json={}).json()["session_id"]
    client.get(f"/games/{sid}")
    engine = game_server.manager.get(sid)
    assert "target" in engine.get_public_state()


# ── 3. 非法索引必须快速失败 ──


class _SpyPlayer:
    def __init__(self, name: str, token: str):
        self.name = name
        self.token = token


class _SpyEngine:
    """探针引擎：记录「是否等待过」「是否进了动作分发」。

    用来证明网关的玩家校验排在等待之前——非法的 state 请求绝不能
    走到 wait_for_change，也不该进到 handle_action 里。
    """

    game_type = "spy"
    game_name = "spy"
    player_mode = "multi"

    def __init__(self):
        self.waits: list[tuple[int, float]] = []
        self.actions: list[str] = []
        self.players = [_SpyPlayer("a", "tok-a"), _SpyPlayer("b", "tok-b")]
        self.status = "waiting"

    @property
    def phase(self):
        return self.status

    # 模拟 issue-1 的阻塞语义：真实实现里等待就发生在这一步。
    # 必须是 async——服务端用 await 调用它，同步函数会让 await 直接报错。
    async def wait_for_change(self, player_index: int, timeout: float):
        self.waits.append((player_index, timeout))
        return "changed"

    def handle_action(self, action: str, **params):
        self.actions.append(action)
        return {"action": action}

    def get_player_state(self, player_index: int = 0):
        return {"your_index": player_index}

    def get_public_state(self):
        return {"note": "spy", "target": 42, "all_dice": [[1]]}


def test_state_action_validates_before_any_wait(client):
    """校验先于等待：非法请求一次都不能等，也不能进引擎分发。"""
    game_server.manager.register("spy", _SpyEngine)
    sid = client.post("/games/spy", json={}).json()["session_id"]
    engine = game_server.manager.get(sid)

    started = time.perf_counter()
    resp = client.post(
        f"/games/{sid}/action",
        json={"action": "state", "player_index": 9, "player_token": "tok-a"},
    )
    elapsed = time.perf_counter() - started

    assert resp.status_code == 400
    assert elapsed < 1.0, f"非法索引等待了 {elapsed:.2f}s 才返回"
    assert engine.waits == [], "非法索引仍然进入了等待"
    assert engine.actions == [], "非法索引仍然进了引擎动作分发"


def test_state_action_valid_input_still_reaches_engine(client, monkeypatch):
    """对照组：合法请求被放行并返回该玩家的状态，说明上面的断言不是靠拒绝一切通过的。

    state 走的是快路径（校验 → 等待 → get_player_state），不经过
    `handle_action`，所以这里断言"拿到了玩家视角"而不是"进了动作分发"。
    探针引擎的 `phase` 是 "waiting"，等待会真的挂满预算，因此把预算压到
    0.05 秒，让这条用例只验证"校验放行"。
    """
    monkeypatch.setattr("agent_game_server.config.BLOCK_BUDGET", 0.05)
    game_server.manager.register("spy", _SpyEngine)
    sid = client.post("/games/spy", json={}).json()["session_id"]
    engine = game_server.manager.get(sid)

    resp = client.post(
        f"/games/{sid}/action",
        json={"action": "state", "player_index": 1, "player_token": "tok-b"},
    )
    assert resp.status_code == 200
    assert resp.json() == {"your_index": 1}
    assert engine.actions == [], "state 快路径不应再走 handle_action"


def test_state_action_with_invalid_index_fails_fast(client):
    """真实引擎上的实测耗时：远小于等待预算。"""
    sid, a, _b = _started_dice(client)
    started = time.perf_counter()
    resp = client.post(
        f"/games/{sid}/action",
        json={"action": "state", "player_index": 9, "player_token": a},
    )
    elapsed = time.perf_counter() - started
    assert resp.status_code == 400
    assert elapsed < 1.0, f"非法索引等待了 {elapsed:.2f}s 才返回"


# ── 4. 各类坏输入是 400 而不是 500 ──


@pytest.mark.parametrize(
    ("url_path", "payload"),
    [
        ("/games/{guess}/action", {"action": "guess"}),
        ("/games/{guess}/action", {"action": "guess", "number": "abc"}),
        ("/games/{guess}/action", {"action": "guess", "number": None}),
        ("/games/{dice}/action", {"action": "join"}),
        ("/games/guess_number", {"bogus": 1}),
    ],
    ids=["guess-missing-number", "guess-string", "guess-null", "join-missing-name", "create-bogus-kwarg"],
)
def test_bad_input_returns_400_not_500(client, url_path, payload):
    guess_sid = client.post("/games/guess_number", json={}).json()["session_id"]
    dice_sid, _a, _b = _started_dice(client)
    url = url_path.format(guess=guess_sid, dice=dice_sid)

    resp = client.post(url, json=payload)
    assert resp.status_code == 400
    assert resp.json().get("detail")


# ── 4b. timeout 参数契约（与 fix/mcp 的 config.parse_block_budget 同义）──


def _state_payload(sid, token, **extra):
    return {"action": "state", "player_index": 0, "player_token": token, **extra}


def test_state_timeout_null_uses_default_budget(client, monkeypatch):
    """"timeout": null 曾因 float(None) 抛 TypeError 变成 500。

    两处让它确定且快速：
    - 预算压到 0.05 秒：本用例只验证 null 不报错，无需真的等满默认预算。
    - 先手固定成令牌所属玩家：`_start_game` 用 `random.choice` 选先手，
      轮到该玩家时 `state` 立即返回，否则会真的挂满整个预算，使套件耗时
      在 9 秒与 64 秒之间随机波动。
    """
    monkeypatch.setattr("agent_game_server.config.BLOCK_BUDGET", 0.05)
    sid, a, _b = _started_dice(client)
    engine = game_server.manager.get(sid)
    engine.current_turn = engine._find_player_by_token(a)
    resp = client.post(f"/games/{sid}/action", json=_state_payload(sid, a, timeout=None))
    assert resp.status_code == 200


def test_state_timeout_zero_returns_immediately(client):
    """0 表示不等待，必须立即返回。"""
    sid, a, _b = _started_dice(client)
    started = time.perf_counter()
    resp = client.post(f"/games/{sid}/action", json=_state_payload(sid, a, timeout=0))
    elapsed = time.perf_counter() - started
    assert resp.status_code == 200
    assert elapsed < 0.5, f"timeout=0 却等了 {elapsed:.2f}s"


def test_state_timeout_negative_clamps_to_zero(client):
    """负数夹紧到 0，同样立即返回，不报错。"""
    sid, a, _b = _started_dice(client)
    started = time.perf_counter()
    resp = client.post(f"/games/{sid}/action", json=_state_payload(sid, a, timeout=-5))
    elapsed = time.perf_counter() - started
    assert resp.status_code == 200
    assert elapsed < 0.5, f"timeout=-5 却等了 {elapsed:.2f}s"


def test_state_timeout_above_budget_is_clamped(client, monkeypatch):
    """超上限夹紧到预算；调小预算以免真的等待。

    必须 patch `agent_game_server.config.BLOCK_BUDGET`：解析器在调用时读的是
    config 模块的全局，`game_server.BLOCK_BUDGET` 只是 import 时的副本。
    """
    monkeypatch.setattr("agent_game_server.config.BLOCK_BUDGET", 0.05)
    sid, a, _b = _started_dice(client)
    started = time.perf_counter()
    resp = client.post(f"/games/{sid}/action", json=_state_payload(sid, a, timeout=999))
    elapsed = time.perf_counter() - started
    assert resp.status_code == 200
    assert elapsed < 0.5, f"timeout=999 夹紧到 0.05s 却等了 {elapsed:.2f}s"


def test_state_timeout_non_numeric_is_400(client):
    """"timeout": "abc" 必须是 400，不是 500。"""
    sid, a, _b = _started_dice(client)
    resp = client.post(f"/games/{sid}/action", json=_state_payload(sid, a, timeout="abc"))
    assert resp.status_code == 400
    assert resp.json().get("detail")


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        (None, 55.0),
        (0, 0.0),
        (-5, 0.0),
        (999, 55.0),
        (10, 10.0),
        (3.7, 3.7),
    ],
)
def test_parse_timeout_numeric_contract(raw, expected):
    assert parse_block_budget(raw) == expected


@pytest.mark.parametrize(
    "raw",
    ["abc", "55", True, False, float("nan"), float("inf"), float("-inf"), [1], {"a": 1}],
    ids=["str", "numeric-str", "true", "false", "nan", "inf", "-inf", "list", "dict"],
)
def test_parse_timeout_rejects_non_numeric(raw):
    with pytest.raises(ValueError):
        parse_block_budget(raw)


def test_parse_timeout_honours_default(monkeypatch):
    """default 只在参数缺省时生效；显式数值照常解析。

    上限永远是 `BLOCK_BUDGET`（不是 default）：default 只回答「没给这个参数
    时用多少秒」，不能顺手把上限也降下来，否则 default 较小的调用方会意外
    得到比预期更短的预算。这里把预算调小来验证上限仍然是它。
    """
    monkeypatch.setattr(
        "agent_game_server.config.BLOCK_BUDGET", 2.0, raising=True
    )
    assert parse_block_budget(None, default=1.0) == 1.0
    assert parse_block_budget(0, default=1.0) == 0.0
    # 显式 99 被夹到当前预算 2.0，而不是 default 1.0
    assert parse_block_budget(99, default=1.0) == 2.0


# ── 5. Admin 鉴权 ──


def test_admin_games_requires_token(client):
    assert client.get("/admin/games").status_code == 403


def test_admin_games_accepts_valid_token(client):
    resp = client.get(
        "/admin/games", headers={"X-Admin-Token": game_server._ADMIN_TOKEN}
    )
    assert resp.status_code == 200
    assert isinstance(resp.json(), list)


def test_admin_games_rejects_wrong_token(client):
    resp = client.get("/admin/games", headers={"X-Admin-Token": "wrong"})
    assert resp.status_code == 403


def test_admin_dashboard_does_not_leak_token_without_credentials(client):
    """匿名访问 /admin 不能从 HTML 里读到 token。"""
    resp = client.get("/admin")
    assert resp.status_code == 200
    assert game_server._ADMIN_TOKEN not in resp.text


def test_admin_dashboard_inlines_token_when_authorized(client):
    """已证明持有 token 的请求才注入，页面才能正常拉数据。"""
    token = game_server._ADMIN_TOKEN
    assert token in client.get("/admin", params={"token": token}).text
    assert token in client.get("/admin", headers={"X-Admin-Token": token}).text
    assert token not in client.get("/admin", params={"token": "wrong"}).text


def test_admin_token_is_always_resolved():
    """admin token 一定有值，鉴权因此永远生效。"""
    assert game_server._ADMIN_TOKEN
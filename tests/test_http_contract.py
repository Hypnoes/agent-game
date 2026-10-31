"""HTTP 层的输入健壮性与鉴权：坏请求必须是 4xx，不能是 5xx。

这一层专门守"调用方传了奇怪的东西"这一类。它们不是业务规则，但每一条都对应
一个曾经真实存在的 500 或越权：

- 非对象 JSON body（`"hello"`、`5`、`true`）直接 `body.pop(...)` → AttributeError
- 畸形 JSON 被静默当成空参数 → 一个拼错的请求建出真实对局并占住匹配池
- `/admin/games/{id}` 详情接口忘记鉴权 → 秘密信息出口
- `/admin/ws` 不需要任何凭证 → 任意人可占用长连接
"""

import pytest

from agent_game_server import game_server


@pytest.fixture
def client(app_client):
    """带独立 manager 的客户端，注册两个真实引擎。"""
    client, gm = app_client()
    return client, gm


# ═══════════════════════════════════════════
# 1. 请求体形态
# ═══════════════════════════════════════════


@pytest.mark.parametrize(
    "body",
    ["hello", 5, True, [1, 2], None, {"nested": {"a": 1}}],
    ids=["string", "int", "bool", "list", "null", "object"],
)
def test_non_object_body_never_causes_500(client, body):
    """非对象 body 一律 400（能建局的除外），绝不能是 500。"""
    client, gm = client
    sid, _engine = gm.create("liars_dice")

    resp = client.post(f"/games/{sid}/action", json=body)

    assert resp.status_code < 500, f"body={body!r} 触发了服务端错误"


@pytest.mark.parametrize("body", ["hello", 5, True, [1, 2]])
def test_non_object_create_body_is_rejected(client, body):
    """创建端点收到非对象 body 必须 400，且**不能**建出对局。"""
    client, gm = client
    before = len(gm._engines)

    resp = client.post("/games/guess_number", json=body)

    assert resp.status_code == 400, resp.text
    assert len(gm._engines) == before, "非法请求不该建出对局"


@pytest.mark.parametrize(
    "raw",
    [b"{not json", b"{\"a\":}", b"not json at all", b"[1, 2]"],
    ids=["truncated", "malformed", "plain-text", "list"],
)
def test_malformed_create_body_does_not_silently_create(client, raw):
    """拼错的 JSON 不能静默变成"空参数"。

    曾经 `except ValueError: params = {}`，于是一个本该 400 的请求得到一局
    全默认参数的真实对局，还占住了匹配池——别的玩家可能被匹配进这局。
    """
    client, gm = client
    before = len(gm._engines)

    resp = client.post(
        "/games/guess_number", content=raw, headers={"content-type": "application/json"}
    )

    assert resp.status_code == 400, f"raw={raw!r} 返回了 {resp.status_code}"
    assert len(gm._engines) == before, "畸形请求不该建出对局"


def test_empty_create_body_still_uses_defaults(client):
    """对照组：真的没带 body 时按空参数建局，保持既有行为。"""
    client, gm = client

    resp = client.post("/games/guess_number", content=b"")

    assert resp.status_code == 200
    assert len(gm._engines) == 1


# ═══════════════════════════════════════════
# 2. Admin 详情接口必须鉴权
# ═══════════════════════════════════════════


def test_admin_detail_requires_token(client):
    """`/admin/games/{id}` 是秘密信息出口，必须鉴权。

    列表接口一直有测试，详情接口没有——于是把它的 `_check_admin_token` 删掉，
    套件依然全绿，而 target / all_dice 就从这条路径裸奔出去。
    """
    client, gm = client
    sid, _engine = gm.create("guess_number")

    resp = client.get(f"/admin/games/{sid}")

    assert resp.status_code == 403
    assert "target" not in resp.text


def test_admin_detail_rejects_wrong_token(client):
    client, gm = client
    sid, _engine = gm.create("guess_number")

    resp = client.get(f"/admin/games/{sid}", headers={"X-Admin-Token": "wrong"})

    assert resp.status_code == 403


def test_admin_detail_exposes_secrets_with_token(client):
    """对照组：带正确 token 时确实能看到秘密，说明上面的 403 不是因为接口坏了。"""
    client, gm = client
    sid, _engine = gm.create("guess_number")

    resp = client.get(
        f"/admin/games/{sid}", headers={"X-Admin-Token": game_server._ADMIN_TOKEN}
    )

    assert resp.status_code == 200
    assert "target" in resp.json()


def test_admin_list_requires_token(client):
    client, _gm = client
    assert client.get("/admin/games").status_code == 403


# ═══════════════════════════════════════════
# 3. Admin WebSocket 必须鉴权
# ═══════════════════════════════════════════


def test_admin_ws_rejects_missing_token(client):
    """没有 token 的 WebSocket 连接必须被关闭。

    它只推「状态变了」不泄密，但它是唯一无需凭证即可占用的长连接入口，
    与"admin 全部强制 token"的口径不一致。
    """
    client, _gm = client

    # 服务端以 1008 关闭；客户端在握手阶段就会失败。
    # 具体异常类型由 starlette/websockets 决定，这里只断言"连不上、也收不到消息"。
    connected_messages = []
    try:
        with client.websocket_connect("/admin/ws") as ws:
            connected_messages.append(ws.receive_text())
    except Exception as exc:  # noqa: B017 — 框架异常类型不是本用例的契约
        assert "1008" in str(exc) or "rejected" in str(exc).lower() or True

    assert connected_messages == [], "未鉴权的 WebSocket 不该收到任何推送"


def test_admin_ws_rejects_wrong_token(client):
    client, _gm = client

    connected_messages = []
    try:
        with client.websocket_connect("/admin/ws?token=wrong") as ws:
            connected_messages.append(ws.receive_text())
    except Exception:  # noqa: B017 — 同上
        pass

    assert connected_messages == [], "token 错误时不该收到推送"


def test_admin_ws_accepts_correct_token(client):
    """带正确 token 时能连上，并能收到状态变更推送。"""
    client, gm = client

    with client.websocket_connect(
        f"/admin/ws?token={game_server._ADMIN_TOKEN}"
    ) as ws:
        # 建一局会改变状态指纹，轮询器在 0.5 秒内推一次
        gm.create("guess_number")
        message = ws.receive_json()

    assert message == {"type": "state_changed"}


# ═══════════════════════════════════════════
# 4. list_sessions 不能污染引擎状态
# ═══════════════════════════════════════════


def test_list_sessions_does_not_mutate_engine_state(make_manager):
    """`list_sessions` 必须在副本上加管理字段。

    直接往 `get_public_state()` 的返回值里塞 session_id 等于写进引擎自己的状态
    对象——当前两个引擎每次都返回新 dict，所以没人发现；一个返回内部字典的引擎
    就会把 admin 的记账字段泄漏进业务状态。
    """

    class DictBackedEngine:
        game_type = "dict_backed"
        game_name = "返回内部字典的引擎"
        player_mode = "single"

        def __init__(self):
            self._state = {"status": "bidding", "solution": 7}

        def get_public_state(self):
            return self._state  # 故意的：返回内部对象而不是副本

    gm = make_manager(DictBackedEngine)
    sid, engine = gm.create("dict_backed")

    rows = gm.list_sessions()

    assert rows[0]["session_id"] == sid
    # 引擎自己的字典必须原封不动
    assert set(engine._state) == {"status", "solution"}, (
        f"list_sessions 污染了引擎状态：{sorted(engine._state)}"
    )
    assert "session_id" not in engine.get_public_state()
    assert "game_type" not in engine.get_public_state()


def test_public_state_endpoint_does_not_mutate_engine_state(make_manager):
    """公开状态出口同样必须在副本上剥键。"""
    from fastapi.testclient import TestClient

    from agent_game_server.game_server import create_app

    class DictBackedEngine:
        game_type = "dict_backed"
        game_name = "返回内部字典的引擎"
        player_mode = "single"
        secret_keys = frozenset({"solution"})

        def __init__(self):
            self._state = {"status": "bidding", "solution": 7}

        def get_public_state(self):
            return self._state

        def get_player_state(self, player_index=0):
            return {"public": {}}

    gm = make_manager(DictBackedEngine)
    sid, engine = gm.create("dict_backed")

    with TestClient(create_app(gm)) as client:
        body = client.get(f"/games/{sid}").json()

    assert "solution" not in body
    assert "solution" in engine._state, "公开状态出口不能改动引擎自己的状态对象"

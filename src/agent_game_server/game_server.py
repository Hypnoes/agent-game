"""
agent-game Game Server — 单端口统一 API。

Agent API:
    POST /mcp                            → MCP over Streamable HTTP 端点
    GET  /games/tools                    → MCP 工具定义
    POST /games/{game_type}              → 创建 session
    POST /games/{session_id}/action      → 执行动作
    GET  /games/{session_id}             → 公开状态（剥离秘密键）
    GET  /games/{session_id}/state/{idx} → 玩家视角（多人游戏需 player_token）

Admin:
    GET  /admin                          → Dashboard HTML
    GET  /admin/games                    → 对局列表
    GET  /admin/games/{id}               → 对局详情
    WS   /admin/ws                       → WebSocket 推送
    POST /admin/reload-prompts           → 热重载提示词
"""

import asyncio
import json
import logging
import os as _os
import secrets
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import APIRouter, FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import HTMLResponse
from mcp.server.streamable_http_manager import StreamableHTTPSessionManager
from starlette.routing import Route

from agent_game_server.config import __version__, parse_block_budget
from agent_game_server.games.game_engine import (
    DEFAULT_SECRET_KEYS,
    GameEngine,
    player_identity_error,
    secret_keys_of,
)
from agent_game_server.games.guess_number import GuessNumberEngine
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.manager import GameManager
from agent_game_server.mcp_server import build_mcp_server

logger = logging.getLogger(__name__)

manager = GameManager()
manager.register("guess_number", GuessNumberEngine)
manager.register("liars_dice", LiarsDiceEngine)

# 模板目录
_TEMPLATE_DIR = Path(__file__).parent / "templates"

# 默认要剥掉的秘密键。真正的名单由引擎自己声明（GameEngine.secret_keys），
# 这里是"引擎没声明时"的兜底，也是既有两个引擎的既有行为。
# Agent 能拿到的公开状态必须剥掉它们，否则游戏本身失去意义。
# 只有 /admin 系列接口（_check_admin_token 鉴权通过后）才允许看到，
# 那里是秘密信息唯一的展示出口。见 game_public_state()。
SECRET_STATE_KEYS = DEFAULT_SECRET_KEYS

# Admin 内部查询密钥。导入时解析一次，之后全程用这个值：
# - 有 AGENT_GAME_ADMIN_TOKEN 就用它（部署时显式配置）
# - 没有就当场随机生成，绝不能退化成「不设 token 就不校验」——
#   那会让 /admin/games 对任何能连上端口的人敞开。
# 随机生成的分支由 cli.py 在启动时打印出来，运维用它打开 /admin?token=...。
_ADMIN_TOKEN = _os.environ.get("AGENT_GAME_ADMIN_TOKEN") or secrets.token_urlsafe(24)


def _token_matches(presented: str) -> bool:
    """定时安全地比较 admin token。

    按 UTF-8 字节比较：str 版的 compare_digest 遇到非 ASCII 会直接抛
    TypeError，那样一个 `X-Admin-Token: 令牌` 就能把 403 变成 500。
    """
    return secrets.compare_digest(
        presented.encode("utf-8"), _ADMIN_TOKEN.encode("utf-8")
    )

# ═══════════════════════════════════════════
# WebSocket 广播
# ═══════════════════════════════════════════


class AdminBroadcast:
    """一台 app 实例自己的 admin 推送状态。

    WebSocket 连接、后台任务、上次广播的签名原先都是模块级全局变量，
    被所有 app 实例共享：两个实例同时在线时，内层 shutdown 会把外层的
    poller 一起取消掉（实测 poller 任务数 2 → 0）。收进这个对象后，
    每个 lifespan 只碰自己那一份。

    这个类不引用任何模块级全局（manager 由构造方注入），因此可以整体搬进
    未来的 create_app() 工厂：工厂里 new 一个，传给 HTTP/WS 处理函数即可。
    """

    def __init__(self, manager: GameManager | None = None) -> None:
        self.clients: set[WebSocket] = set()
        self.tasks: set[asyncio.Task] = set()
        self.last_signature: str = ""
        self._manager = manager

    @property
    def manager(self) -> GameManager:
        """本实例负责的 manager。

        显式注入的优先；没注入就回落到模块级 manager——单实例部署下两者是
        同一个对象，而将来 create_app() 工厂会显式传入自己那一个。
        """
        return self._manager if self._manager is not None else manager

    @manager.setter
    def manager(self, value: GameManager | None) -> None:
        self._manager = value

    def track(self, task: asyncio.Task) -> None:
        """登记一个后台任务，任务结束后自动移出，避免集合无限增长。"""
        self.tasks.add(task)
        task.add_done_callback(self.tasks.discard)

    async def cancel_all(self) -> None:
        """取消并等待**本实例**登记的后台任务；别的实例的任务不受影响。"""
        pending = list(self.tasks)
        for task in pending:
            task.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)


# 当前 app 的广播状态。将来 create_app() 工厂化时，这一个全局会被换成本地
# 变量，下面几个函数的签名不用再动。
_admin_broadcast = AdminBroadcast(manager)

# 模块级别名，保持与旧代码/旧测试的引用路径一致（指向当前实例的同一对象）。
_ws_clients = _admin_broadcast.clients
_background_tasks = _admin_broadcast.tasks

# 状态指纹算不出来时用的哨兵：永远不等于「上一次的签名」，
# 保证下一轮一定重试广播，而不是因为一次序列化失败就永久静默。
_SIGNATURE_ERROR = "<signature-error>"


async def _broadcast_admin_state(broadcast: AdminBroadcast | None = None) -> None:
    """向所有 admin WebSocket 客户端推一条「状态变了」。

    单个连接发不出去只影响它自己：RuntimeError（已关闭）、ConnectionError
    （连接被重置）乃至任何框架自定义异常，都就地丢弃这个客户端然后继续
    发给剩下的人——一个死 socket 不能中断整轮广播。
    """
    state = broadcast or _admin_broadcast
    payload = json.dumps({"type": "state_changed"})
    dead: set[WebSocket] = set()
    for ws in list(state.clients):
        try:
            await ws.send_text(payload)
        except Exception:
            logger.debug("admin ws send failed; dropping client", exc_info=True)
            dead.add(ws)
    state.clients.difference_update(dead)


def _admin_state_signature(broadcast: AdminBroadcast | None = None) -> str:
    """全部 session 的指纹。序列化失败返回哨兵，绝不向上抛异常。"""
    state = broadcast or _admin_broadcast
    try:
        return json.dumps(
            state.manager.list_sessions(),
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        )
    except Exception:
        # 某个引擎的 get_public_state() 塞了 datetime 之类的对象。这里返回
        # 哨兵而不是抛异常：轮询循环靠它继续跑下去（它是唯一结算超时的地方）。
        # _refresh_admin_state 见到哨兵会强制广播，不等下一次「状态真的变了」。
        logger.warning("admin state signature failed to serialize", exc_info=True)
        return _SIGNATURE_ERROR


async def _refresh_admin_state(broadcast: AdminBroadcast | None = None) -> bool:
    """结算超时 + 状态有变才广播。返回本轮是否真的广播了。"""
    state = broadcast or _admin_broadcast

    state.manager.check_timeouts()
    signature = _admin_state_signature(state)
    if signature == state.last_signature and signature != _SIGNATURE_ERROR:
        return False
    state.last_signature = signature
    await _broadcast_admin_state(state)
    return True


async def _poll_and_broadcast(broadcast: AdminBroadcast | None = None):
    """后台轮询：每 0.5 秒结算超时并按需广播。

    这是**唯一**调用 manager.check_timeouts() 的地方。循环一旦退出，所有对局
    都不再超时、session 永不回收、admin 页面永远不再更新，而且悄无声息。
    所以单轮异常必须就地吞掉、记日志、继续下一轮：一个坏引擎（比如
    get_public_state() 返回了 datetime）不能杀死整个服务。

    CancelledError 是 BaseException 的子类，不会被下面的 except Exception
    捕获，关停时照常向上传播。
    """
    state = broadcast or _admin_broadcast
    while True:
        await asyncio.sleep(0.5)
        try:
            await _refresh_admin_state(state)
        except Exception:
            logger.exception("admin state poll iteration failed; continuing")


# ═══════════════════════════════════════════
# 路由（挂在 router 上，便于 create_app 重复构建）
# ═══════════════════════════════════════════

router = APIRouter()


def _broadcast_of(app: FastAPI) -> AdminBroadcast:
    """取某个 app 实例的广播状态。

    `create_app()` 把实例挂在 `app.state.admin_broadcast` 上；没有工厂
    （直接使用模块级 `_admin_broadcast`）的旧路径回落到模块级实例。
    """
    broadcast = getattr(app.state, "admin_broadcast", None)
    if isinstance(broadcast, AdminBroadcast):
        return broadcast
    return _admin_broadcast


def _manager_of(app: FastAPI) -> GameManager:
    """取某个 app 实例自己的 GameManager。

    `create_app(manager)` 把实例挂在 `app.state.manager` 上。没有工厂时回落到
    模块级 `manager`——单实例部署下两者是同一个对象。路由一律经由这里取，
    这样两个带独立 manager 的 app 才能同时在线而互不串状态。
    """
    instance = getattr(app.state, "manager", None)
    if isinstance(instance, GameManager):
        return instance
    return manager


@router.websocket("/admin/ws")
async def admin_websocket(ws: WebSocket, token: str = ""):
    """接受一条 admin 推送连接，登记到**当前 app** 的广播实例上。

    连接前必须出示 admin token：浏览器 WebSocket 无法设置自定义头，所以走
    `?token=` 查询参数。不匹配直接以 1008（policy violation）关闭——这个端点
    只推「状态变了」不泄密，但它是唯一无需凭证即可占用的长连接入口。
    """
    if not token or not _token_matches(token):
        await ws.close(code=1008)
        return

    await ws.accept()
    broadcast = _broadcast_of(ws.app)
    broadcast.clients.add(ws)
    try:
        while True:
            await ws.receive_text()
    except WebSocketDisconnect:
        pass
    finally:
        broadcast.clients.discard(ws)


# ═══════════════════════════════════════════
# 通用
# ═══════════════════════════════════════════


@router.get("/")
def root(request: Request):
    return {"ok": True, "games": list(_manager_of(request.app)._registry.keys())}


@router.get("/games/tools")
def list_tools(request: Request):
    return _manager_of(request.app).describe_all_tools()


# ═══════════════════════════════════════════
# 游戏生命周期
# ═══════════════════════════════════════════


async def _json_object(request: Request) -> dict:
    """把请求体解析成 dict。空 body 当空参数，非空但非对象一律 400。

    `await request.json()` 有三种失败形态，必须分开处理：
    - body 完全为空：调用方没带参数，按 `{}` 处理（既有行为）
    - body 不是合法 JSON：拼错了，报 400 —— 曾经被静默当成 `{}`，
      于是一个本该失败的请求建出了一局全默认参数的真实对局并占住匹配池
    - body 是合法 JSON 但不是对象（`"hi"`、`5`、`true`）：报 400。
      曾经直接 `body.pop(...)` 触发 AttributeError，变成 500
    """
    raw = await request.body()
    if not raw.strip():
        return {}
    try:
        parsed = await request.json()
    except ValueError as ex:
        raise HTTPException(status_code=400, detail=f"请求体不是合法 JSON: {ex}") from ex
    if not isinstance(parsed, dict):
        raise HTTPException(
            status_code=400,
            detail=f"请求体必须是 JSON 对象，收到 {type(parsed).__name__}",
        )
    return parsed


@router.post("/games/{game_type}")
async def create_game(game_type: str, request: Request):
    params = await _json_object(request)
    try:
        sid, e = _manager_of(request.app).create(game_type, **params)
    except ValueError as ex:
        raise HTTPException(status_code=400, detail=str(ex)) from ex
    except TypeError as ex:
        # 参数名不对（如 {"bogus": 1}）：是调用方的输入问题，不是服务端 bug
        raise HTTPException(status_code=400, detail=f"无效的创建参数: {ex}") from ex
    return {
        "session_id": sid,
        "game_type": game_type,
        "phase": getattr(e, "phase", "started"),
    }


def _state_player_error(
    e, player_index: object, player_token: object, out_of_range_status: int
) -> HTTPException | None:
    """把共享的身份校验结果翻译成 HTTP 错误（None 表示通过）。

    校验本身在 `game_engine.player_identity_error()`：MCP 分支用同一个函数，
    两条传输因此不会对"谁是这名玩家"给出不同答案。
    `out_of_range_status` 由调用方决定：GET 是查询语义用 404，action
    走引擎的入参校验语义用 400，保持各自的既有契约。
    """
    identity = player_identity_error(e, player_index, player_token)
    if identity is None:
        return None
    if identity.kind == "not_found":
        detail = "player not found" if out_of_range_status == 404 else identity.message
        return HTTPException(status_code=out_of_range_status, detail=detail)
    if identity.kind == "invalid_index":
        return HTTPException(status_code=out_of_range_status, detail=identity.message)
    return HTTPException(status_code=403, detail=identity.message)


@router.post("/games/{session_id}/action")
async def game_action(session_id: str, request: Request):
    mgr = _manager_of(request.app)
    e = mgr.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    try:
        body = await _json_object(request)
        action = body.pop("action", None)
        if not action:
            raise HTTPException(status_code=400, detail="缺少 action 字段")
        if action == "state":
            # 顺序至关重要：先解析入参、再校验玩家身份、最后才进入等待。
            # 任何一步挪到「等对局变化」之后，非法输入都会先白等满整个
            # 等待预算（实测 55 秒）才报错。
            pi = body.pop("player_index", 0)
            token = body.pop("player_token", "")
            # timeout 只做传输层预算，不传给 handle_action
            timeout = parse_block_budget(body.pop("timeout", None))
            err = _state_player_error(e, pi, token, 400)
            if err:
                raise err
            # 校验通过后才挂起：非当前回合的调用在这里等待，Agent 只发起一次
            waiter = getattr(e, "wait_for_change", None)
            if waiter is not None and timeout > 0:
                await waiter(pi, timeout)
            # 等待期间可能有对局因超时结束，立即结算，不必等后台轮询
            mgr.check_timeouts()
            return e.get_player_state(pi)
        return e.handle_action(action, **body)
    except HTTPException:
        raise
    except ValueError as ex:
        raise HTTPException(status_code=400, detail=str(ex)) from ex
    except TypeError as ex:
        # 参数类型错误（None 参与比较、关键字对不上引擎签名）
        raise HTTPException(status_code=400, detail=f"无效的参数: {ex}") from ex
    except KeyError as ex:
        # 必填参数整个缺失（如 guess 少 number、join 少 player_name）
        raise HTTPException(status_code=400, detail=f"缺少必填参数: {ex}") from ex


@router.get("/games/{session_id}")
def game_public_state(session_id: str, request: Request):
    """Agent 可见的公开状态——秘密信息在这里被剥掉。

    get_public_state() 是给 admin 用的「全量」视图，含 target / all_dice。
    直接回给 Agent 等于把答案和全场骰子送出去，所以这个端点只输出
    去掉 SECRET_STATE_KEYS 之后的副本。需要秘密信息请走 /admin/games/{id}。
    """
    e = _manager_of(request.app).get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    if not hasattr(e, "get_public_state"):
        return getattr(e, "status", "unknown")
    state = e.get_public_state()
    if not isinstance(state, dict):
        return state
    # 在副本上剥键，别改动引擎自己维护的状态对象。
    # 剥哪些键由引擎自己声明（secret_keys），不是框架内置一张表。
    secrets = secret_keys_of(e)
    return {k: v for k, v in state.items() if k not in secrets}


@router.get("/games/{session_id}/state/{player_index}")
def game_player_state(
    session_id: str, request: Request, player_index: int = 0, player_token: str = ""
):
    """玩家视角状态，含私有骰子——必须证明自己是这名玩家，且立即返回。

    多人引擎：player_token 必填且要匹配，否则 403（缺 token 也是 403）。
    单人引擎：没有 players 属性，保持开放。
    越界索引：404（避免负索引绕回最后一名玩家）。
    这是调试与管理界面的查询入口，不进入任何等待。
    """
    e = _manager_of(request.app).get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    err = _state_player_error(e, player_index, player_token, 404)
    if err:
        raise err
    return e.get_player_state(player_index)


# ═══════════════════════════════════════════
# Admin
# ═══════════════════════════════════════════


def _check_admin_token(request: Request):
    """Admin 接口鉴权。 token 一定存在（导入时解析），所以永远强制校验。"""
    if not _token_matches(request.headers.get("X-Admin-Token", "")):
        raise HTTPException(status_code=403, detail="forbidden")


@router.get("/admin/games")
def admin_list_all(request: Request):
    _check_admin_token(request)
    return _manager_of(request.app).list_sessions()


@router.get("/admin/games/{session_id}")
def admin_detail(session_id: str, request: Request):
    """Admin 详情——秘密信息（target / all_dice）只有这里能看到。

    返回体在引擎的公开状态之上补两个字段：`session_id` 与 `game_type`。
    列表接口 `/admin/games` 一直带着它们，详情接口原先没有，导致仪表盘
    `showDetail()` 按 `g.game_type` 分派时两个分支都不进、面板永远空白。
    其余键原样保留（包括 target / all_dice 这类秘密）。
    """
    _check_admin_token(request)
    e = _manager_of(request.app).get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="not found")
    state = e.get_public_state()
    state = dict(state) if isinstance(state, dict) else {"state": state}
    state["session_id"] = session_id
    state["game_type"] = getattr(e, "game_type", "unknown")
    state["game_name"] = getattr(e, "game_name", e.__class__.__name__)
    return state


@router.get("/admin", response_class=HTMLResponse)
def admin_dashboard(request: Request, token: str = ""):
    """Dashboard 页面本身不含秘密，但也不能把 token 交给匿名访问者。

    设计：页面需要 token 才能让前端的 adminFetch 带上 X-Admin-Token 去查数据。
    模板 admin.html 只认 window.ADMIN_TOKEN，所以这里保持「往页头注入
    ADMIN_TOKEN」这一机制，但把注入条件收紧成「请求方已经证明自己持有 token」：

    - 查询参数 ?token=... 匹配，或 X-Admin-Token 头匹配 → 注入，
      页面打开即可正常拉数据（运维从 cli.py 打印的那一行复制 ?token=...）
    - 都没带或带错 → 仍然返回页面（空占位），但它发起的 admin API 调用会 403。
      这样既不会把 token 泄露给匿名访问者，又不会把未授权访问伪装成 404，
      运维一眼能看出「缺 token」而不是「服务挂了」。
    """
    html = (_TEMPLATE_DIR / "admin.html").read_text(encoding="utf-8")
    header_token = request.headers.get("X-Admin-Token", "")
    presented = token or header_token
    if presented and _token_matches(presented):
        html = html.replace(
            "</head>", f"<script>window.ADMIN_TOKEN='{_ADMIN_TOKEN}'</script></head>"
        )
    return HTMLResponse(content=html)


def _has_own_prompts(engine_cls: type) -> bool:
    """引擎**自己**定义了 get_prompts 才算有提示词模板。

    不能用 hasattr：它会把基类（GameEngine 协议）上的桩方法也算进来，
    一个没有提示词的引擎照样被当成「有」，于是 reload 报出一堆假成功。
    这里沿 MRO 往上找，遇到 GameEngine 就停——基类上的可选方法即便某天
    被加回运行时，也不会被认成子类自己的能力。
    """
    for klass in engine_cls.__mro__:
        if klass is GameEngine:
            return False
        if "get_prompts" in klass.__dict__:
            return True
    return False


@router.post("/admin/reload-prompts")
def reload_prompts(request: Request):
    """热重载所有**自己**带提示词模板的引擎。没有模板的引擎不报成功。"""
    _check_admin_token(request)
    reloaded = []
    for engine_cls in _manager_of(request.app)._registry.values():
        if not _has_own_prompts(engine_cls):
            continue
        engine_cls.get_prompts().reload()
        reloaded.append(getattr(engine_cls, "game_type", str(engine_cls)))
    return {"ok": True, "reloaded": reloaded}


# ═══════════════════════════════════════════
# App 工厂
# ═══════════════════════════════════════════


def create_app(manager: GameManager | None = None) -> FastAPI:
    """构建 FastAPI app。

    必须每次新建 app：StreamableHTTPSessionManager.run() 只能在实例上调一次，
    复用 app 会导致第二次进入 lifespan 时抛 RuntimeError。uvicorn 与测试各自
    调用本函数获得独立的 app 与 session manager。

    传入 `manager` 时整个 app（路由、MCP server、广播）都用它，因此两个 app
    可以各自带一份对局表同时在线。不传则用模块级 manager——单实例部署下两者
    是同一个对象，`uvicorn` 与旧调用路径不受影响。

    路由一律通过 `request.app.state.manager` 取实例，不再直接读模块全局；
    否则传入的 manager 只会作用于 MCP 与广播，REST 仍打到全局那一份。
    """
    mgr = manager if manager is not None else globals()["manager"]

    mcp_session_manager = StreamableHTTPSessionManager(
        app=build_mcp_server(mgr),
        stateless=False,
        session_idle_timeout=600,
    )

    # 广播状态在工厂里创建并挂到 app 上：WS 端点用 ws.app 取自己那一份，
    # 两条同时运行的 app 因此互不干扰。lifespan 只负责启动/停止它的 poller。
    broadcast = AdminBroadcast(mgr)

    @asynccontextmanager
    async def lifespan(_: FastAPI):
        task = asyncio.create_task(
            _poll_and_broadcast(broadcast), name="admin-state-poller"
        )
        broadcast.track(task)
        async with mcp_session_manager.run():
            try:
                yield
            finally:
                await broadcast.cancel_all()

    app = FastAPI(title="agent-game", version=__version__, lifespan=lifespan)
    app.state.manager = mgr
    app.state.admin_broadcast = broadcast
    app.include_router(router)

    # MCP over Streamable HTTP。
    # 必须用 Route + session_manager.asgi_app：Mount 会 307 重定向到 /mcp/，
    # 裸 HTTP 客户端失败；自定义 async 包装则因返回 None 触发 TypeError。
    app.router.routes.append(
        Route(
            "/mcp",
            endpoint=mcp_session_manager.asgi_app,
            methods=["GET", "POST", "DELETE"],
        )
    )
    return app


app = create_app()

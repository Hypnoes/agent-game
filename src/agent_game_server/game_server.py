"""
agent-game Game Server — 单端口统一 API。

Agent API:
    POST /mcp                            → MCP over Streamable HTTP 端点
    GET  /games/tools                    → MCP 工具定义
    POST /games/{game_type}              → 创建 session
    POST /games/{session_id}/action      → 执行动作
    GET  /games/{session_id}             → 公开状态
    GET  /games/{session_id}/state/{idx} → 玩家视角

Admin:
    GET  /admin                          → Dashboard HTML
    GET  /admin/games                    → 对局列表
    GET  /admin/games/{id}               → 对局详情
    WS   /admin/ws                       → WebSocket 推送
    POST /admin/reload-prompts           → 热重载提示词
"""

import asyncio
import json
import os as _os
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import APIRouter, FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import HTMLResponse
from mcp.server.streamable_http_manager import StreamableHTTPSessionManager
from starlette.routing import Route

from agent_game_server.config import BLOCK_BUDGET, __version__
from agent_game_server.games.guess_number import GuessNumberEngine
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.manager import GameManager
from agent_game_server.mcp_server import build_mcp_server

manager = GameManager()
manager.register("guess_number", GuessNumberEngine)
manager.register("liars_dice", LiarsDiceEngine)

# 模板目录
_TEMPLATE_DIR = Path(__file__).parent / "templates"


# ═══════════════════════════════════════════
# WebSocket 广播
# ═══════════════════════════════════════════

_ws_clients: set[WebSocket] = set()
_background_tasks: set[asyncio.Task] = set()
_last_admin_state_signature = ""


def _track_background_task(task: asyncio.Task) -> None:
    _background_tasks.add(task)
    task.add_done_callback(_background_tasks.discard)


def _admin_state_signature() -> str:
    return json.dumps(
        manager.list_sessions(),
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    )


async def _broadcast_admin_state() -> None:
    dead: set[WebSocket] = set()
    for ws in list(_ws_clients):
        try:
            await ws.send_text(json.dumps({"type": "state_changed"}))
        except RuntimeError:
            dead.add(ws)
    _ws_clients.difference_update(dead)


async def _refresh_admin_state() -> bool:
    global _last_admin_state_signature

    manager.check_timeouts()
    signature = _admin_state_signature()
    if signature == _last_admin_state_signature:
        return False
    _last_admin_state_signature = signature
    await _broadcast_admin_state()
    return True


async def _poll_and_broadcast():
    while True:
        await asyncio.sleep(0.5)
        await _refresh_admin_state()


# ═══════════════════════════════════════════
# 路由（挂在 router 上，便于 create_app 重复构建）
# ═══════════════════════════════════════════

router = APIRouter()


@router.websocket("/admin/ws")
async def admin_websocket(ws: WebSocket):
    await ws.accept()
    _ws_clients.add(ws)
    try:
        while True:
            await ws.receive_text()
    except WebSocketDisconnect:
        pass
    finally:
        _ws_clients.discard(ws)


# ═══════════════════════════════════════════
# 通用
# ═══════════════════════════════════════════


@router.get("/")
def root():
    return {"ok": True, "games": list(manager._registry.keys())}


@router.get("/games/tools")
def list_tools():
    return manager.describe_all_tools()


# ═══════════════════════════════════════════
# 游戏生命周期
# ═══════════════════════════════════════════


@router.post("/games/{game_type}")
async def create_game(game_type: str, request: Request):
    try:
        params = await request.json()
    except ValueError:
        params = {}
    try:
        sid, e = manager.create(game_type, **params)
    except ValueError as ex:
        raise HTTPException(status_code=400, detail=str(ex))
    return {
        "session_id": sid,
        "game_type": game_type,
        "phase": getattr(e, "phase", "started"),
    }


@router.post("/games/{session_id}/action")
async def game_action(session_id: str, request: Request):
    e = manager.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    try:
        body = await request.json()
        action = body.pop("action", None)
        if not action:
            raise HTTPException(status_code=400, detail="缺少 action 字段")
        if action == "state":
            # 非当前回合时在服务端挂起，Agent 只发起一次调用
            pi = body.pop("player_index", 0)
            timeout = min(float(body.pop("timeout", BLOCK_BUDGET)), BLOCK_BUDGET)
            token = body.pop("player_token", "")
            if hasattr(e, "wait_for_change"):
                await e.wait_for_change(pi, timeout)
            # 等待期间可能有对局因超时结束，立即结算，不必等后台轮询
            manager.check_timeouts()
            # 等待走的是快路径，绕过了 handle_action 的校验，这里补回：
            # get_player_state 会返回私有骰子，不能让无 token 的调用方按索引取走
            if hasattr(e, "_validate_player"):
                e._validate_player(pi, token)
            return e.get_player_state(pi)
        return e.handle_action(action, **body)
    except ValueError as ex:
        raise HTTPException(status_code=400, detail=str(ex))


@router.get("/games/{session_id}")
def game_public_state(session_id: str):
    e = manager.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    if hasattr(e, "get_public_state"):
        return e.get_public_state()
    return getattr(e, "status", "unknown")


@router.get("/games/{session_id}/state/{player_index}")
def game_player_state(session_id: str, player_index: int = 0):
    """立即返回，不做任何等待。调试与管理界面的查询入口。"""
    e = manager.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    # 越界索引在引擎里会抛 IndexError（500），这里统一成 404。
    # 单人游戏（如 guess_number）没有 players，不做这个检查。
    players = getattr(e, "players", None)
    if players is not None and not 0 <= player_index < len(players):
        raise HTTPException(status_code=404, detail="player not found")
    return e.get_player_state(player_index)


# ═══════════════════════════════════════════
# Admin
# ═══════════════════════════════════════════


def _check_admin_token(request: Request):
    token = _os.environ.get("AGENT_GAME_ADMIN_TOKEN", "")
    if token and request.headers.get("X-Admin-Token", "") != token:
        raise HTTPException(status_code=403, detail="forbidden")


@router.get("/admin/games")
def admin_list_all(request: Request):
    _check_admin_token(request)
    return manager.list_sessions()


@router.get("/admin/games/{session_id}")
def admin_detail(session_id: str, request: Request):
    _check_admin_token(request)
    e = manager.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="not found")
    return e.get_public_state()


@router.get("/admin", response_class=HTMLResponse)
def admin_dashboard():
    html = (_TEMPLATE_DIR / "admin.html").read_text(encoding="utf-8")
    token = _os.environ.get("AGENT_GAME_ADMIN_TOKEN", "")
    if token:
        html = html.replace(
            "</head>", f"<script>window.ADMIN_TOKEN='{token}'</script></head>"
        )
    return HTMLResponse(content=html)


@router.post("/admin/reload-prompts")
def reload_prompts(request: Request):
    _check_admin_token(request)
    reloaded = []
    for engine_cls in manager._registry.values():
        if hasattr(engine_cls, "get_prompts"):
            engine_cls.get_prompts().reload()
            reloaded.append(getattr(engine_cls, "game_type", str(engine_cls)))
    return {"ok": True, "reloaded": reloaded}


# ═══════════════════════════════════════════
# App 工厂
# ═══════════════════════════════════════════


def create_app() -> FastAPI:
    """构建 FastAPI app。

    必须每次新建 app：StreamableHTTPSessionManager.run() 只能在实例上调一次，
    复用 app 会导致第二次进入 lifespan 时抛 RuntimeError。uvicorn 与测试各自
    调用本函数获得独立的 app 与 session manager。
    """
    mcp_session_manager = StreamableHTTPSessionManager(
        app=build_mcp_server(manager),
        stateless=False,
        session_idle_timeout=600,
    )

    @asynccontextmanager
    async def lifespan(_: FastAPI):
        task = asyncio.create_task(_poll_and_broadcast(), name="admin-state-poller")
        _track_background_task(task)
        async with mcp_session_manager.run():
            try:
                yield
            finally:
                for bg_task in list(_background_tasks):
                    bg_task.cancel()
                if _background_tasks:
                    await asyncio.gather(*_background_tasks, return_exceptions=True)

    app = FastAPI(title="agent-game", version=__version__, lifespan=lifespan)
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

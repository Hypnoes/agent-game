"""
agent-game Game Server — 单端口统一 API。

Agent API:
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

import httpx
from fastapi import FastAPI, HTTPException, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import HTMLResponse

from agent_game_server.games.guess_number import GuessNumberEngine
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.manager import GameManager


@asynccontextmanager
async def lifespan(_: FastAPI):
    asyncio.create_task(_poll_and_broadcast())
    yield


app = FastAPI(title="agent-game", version="0.2.0", lifespan=lifespan)

manager = GameManager()
manager.register("guess_number", GuessNumberEngine)
manager.register("liars_dice", LiarsDiceEngine)

# 模板目录
_TEMPLATE_DIR = Path(__file__).parent / "templates"

# 内部共享密钥（启动时随机生成，防止 Agent 裸 HTTP 调 admin API）
_ADMIN_TOKEN = _os.environ.get("AGENT_GAME_ADMIN_TOKEN", "")


# ═══════════════════════════════════════════
# WebSocket 广播
# ═══════════════════════════════════════════

_ws_clients: set[WebSocket] = set()


async def _poll_and_broadcast():
    global _ws_clients

    last_hash = ""
    while True:
        await asyncio.sleep(0.5)
        try:
            async with httpx.AsyncClient(timeout=2.0) as c:
                resp = await c.get(
                    "http://localhost:8800/admin/games",
                    headers={"X-Admin-Token": _ADMIN_TOKEN},
                )
                if resp.status_code != 200:
                    continue
                games = resp.json()
        except (httpx.HTTPError, ValueError):
            continue
        h = str(len(games))
        for g in games:
            h += f"|{g.get('session_id', '')}:{g.get('status', '')}"
        if h == last_hash:
            continue
        last_hash = h
        dead: set[WebSocket] = set()
        for ws in list(_ws_clients):
            try:
                await ws.send_text(json.dumps({"type": "state_changed"}))
            except RuntimeError:
                dead.add(ws)
        _ws_clients -= dead


@app.websocket("/admin/ws")
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


@app.get("/")
def root():
    return {"ok": True, "games": list(manager._registry.keys())}


@app.get("/games/tools")
def list_tools():
    return manager.describe_all_tools()


# ═══════════════════════════════════════════
# 游戏生命周期
# ═══════════════════════════════════════════


@app.post("/games/{game_type}")
async def create_game(game_type: str):
    try:
        sid, e = manager.create(game_type)
    except ValueError as ex:
        raise HTTPException(status_code=400, detail=str(ex))
    return {
        "session_id": sid,
        "game_type": game_type,
        "phase": getattr(e, "phase", "started"),
    }


@app.post("/games/{session_id}/action")
async def game_action(session_id: str, request: Request):
    e = manager.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    try:
        body = await request.json()
        action = body.pop("action", None)
        if not action:
            raise HTTPException(status_code=400, detail="缺少 action 字段")
        return e.handle_action(action, **body)
    except ValueError as ex:
        raise HTTPException(status_code=400, detail=str(ex))


@app.get("/games/{session_id}")
def game_public_state(session_id: str):
    e = manager.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    if hasattr(e, "get_public_state"):
        return e.get_public_state()
    return getattr(e, "status", "unknown")


@app.get("/games/{session_id}/state/{player_index}")
def game_player_state(session_id: str, player_index: int = 0):
    e = manager.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="session not found")
    return e.get_player_state(player_index)


# ═══════════════════════════════════════════
# Admin
# ═══════════════════════════════════════════


def _check_admin_token(request: Request):
    token = _os.environ.get("AGENT_GAME_ADMIN_TOKEN", "")
    if token and request.headers.get("X-Admin-Token", "") != token:
        raise HTTPException(status_code=403, detail="forbidden")


@app.get("/admin/games")
def admin_list_all(request: Request):
    _check_admin_token(request)
    return manager.list_sessions()


@app.get("/admin/games/{session_id}")
def admin_detail(session_id: str, request: Request):
    _check_admin_token(request)
    e = manager.get(session_id)
    if not e:
        raise HTTPException(status_code=404, detail="not found")
    return e.get_public_state()


@app.get("/admin", response_class=HTMLResponse)
def admin_dashboard():
    html = (_TEMPLATE_DIR / "admin.html").read_text(encoding="utf-8")
    token = _os.environ.get("AGENT_GAME_ADMIN_TOKEN", "")
    if token:
        html = html.replace(
            "</head>", f"<script>window.ADMIN_TOKEN='{token}'</script></head>"
        )
    return HTMLResponse(content=html)


@app.post("/admin/reload-prompts")
def reload_prompts():
    reloaded = []
    for engine_cls in manager._registry.values():
        if hasattr(engine_cls, "get_prompts"):
            engine_cls.get_prompts().reload()
            reloaded.append(getattr(engine_cls, "game_type", str(engine_cls)))
    return {"ok": True, "reloaded": reloaded}

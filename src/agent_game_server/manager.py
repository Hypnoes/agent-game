"""
GameManager — 统一的 session 路由和生命周期管理。

职责：
- 注册游戏类型
- 创建 session（单实例 engine）
- 路由 action 到对应 engine
- 列出所有 session（供 admin 查询）
- 超时检查

不强制 Engine 基类——靠 duck typing。
"""

import time
import uuid
from agent_game_server.games.game_engine import GameEngine


class GameManager:
    def __init__(self):
        self._engines: dict[str, GameEngine] = {}
        self._created_at: dict[str, float] = {}
        self._registry: dict[str, type[GameEngine]] = {}

    # ── 注册 ──

    def register(self, game_type: str, engine_cls: type[GameEngine]):
        self._registry[game_type] = engine_cls

    # ── 创建 session ──

    def create(self, game_type: str, **params) -> tuple[str, GameEngine]:
        """创建新 session（多人游戏自动匹配到 waiting session）。返回 (session_id, engine)"""
        if game_type not in self._registry:
            raise ValueError(f"未知游戏类型: {game_type}。已注册: {list(self._registry)}")

        # 多人游戏：尝试通过 token 重连已有 session
        player_token = params.get("player_token")
        if player_token:
            for sid, eng in list(self._engines.items()):
                if hasattr(eng, '_find_player_by_token'):
                    pi = eng._find_player_by_token(player_token)
                    if pi is not None:
                        return sid, eng

        # 多人游戏：尝试加入现有 waiting session
        engine_cls = self._registry[game_type]
        if getattr(engine_cls, 'player_mode', 'single') == 'multi':
            for sid, eng in list(self._engines.items()):
                if (hasattr(eng, 'phase') and eng.phase == 'waiting'
                        and getattr(eng, 'game_type', '') == game_type):
                    return sid, eng

        # 新建 session
        session_id = str(uuid.uuid4())[:8]
        engine = engine_cls(**params)
        self._engines[session_id] = engine
        self._created_at[session_id] = time.time()
        return session_id, engine

    # ── 路由 ──

    def route(self, session_id: str, action: str, **params):
        engine = self._engines[session_id]
        method = getattr(engine, action)
        return method(**params)

    def get(self, session_id: str) -> GameEngine | None:
        return self._engines.get(session_id)

    def list_sessions(self) -> list[dict]:
        """列出所有 session 的公开状态（供 admin）"""
        results = []
        for sid, engine in list(self._engines.items()):
            state = {}
            if hasattr(engine, 'get_public_state'):
                state = engine.get_public_state()
            state["session_id"] = sid
            state["game_type"] = getattr(engine, 'game_type', 'unknown')
            state["game_name"] = getattr(engine, 'game_name', engine.__class__.__name__)
            state["created_at"] = self._created_at.get(sid, 0)
            results.append(state)
        return results

    # ── 工具描述 ──

    def describe_all_tools(self) -> list[dict]:
        """汇总所有已注册游戏的 MCP 工具定义"""
        tools = []
        for game_type, engine_cls in self._registry.items():
            if hasattr(engine_cls, 'describe_tools'):
                tools.extend(engine_cls.describe_tools())
        return tools

    # ── 超时 ──

    def check_timeouts(self) -> list[dict]:
        """检查所有 session 超时。返回超时事件列表。"""
        events = []
        now = time.time()
        for sid in list(self._engines):
            engine = self._engines[sid]
            if hasattr(engine, 'check_timeout'):
                result = engine.check_timeout(now)
                if result:
                    events.append({"session_id": sid, **result})
                    if result.get("expired"):
                        self._engines.pop(sid, None)
                        self._created_at.pop(sid, None)
        return events

    def remove(self, session_id: str):
        self._engines.pop(session_id, None)
        self._created_at.pop(session_id, None)

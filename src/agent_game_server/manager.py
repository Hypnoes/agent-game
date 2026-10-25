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
from typing import Any, cast

# 已结束的对局在内存里保留多久（秒）。过期后由 check_timeouts() 回收，
# 避免 admin/games 列表无限增长。
FINISHED_RETENTION = 300.0


class GameManager:
    def __init__(self):
        self._engines: dict[str, Any] = {}
        self._created_at: dict[str, float] = {}
        self._finished_at: dict[str, float] = {}
        self._registry: dict[str, type[Any]] = {}

    # ── 注册 ──

    def register(self, game_type: str, engine_cls: type[Any]):
        self._registry[game_type] = engine_cls

    # ── 创建 session ──

    def create(self, game_type: str, **params) -> tuple[str, Any]:
        """创建新 session（多人游戏自动匹配到 waiting session）。返回 (session_id, engine)"""
        if game_type not in self._registry:
            raise ValueError(
                f"未知游戏类型: {game_type}。已注册: {list(self._registry)}"
            )

        create_params = dict(params)

        # 多人游戏：尝试通过 token 重连已有 session
        player_token = create_params.pop("player_token", None)
        if player_token:
            for sid, eng in list(self._engines.items()):
                if hasattr(eng, "_find_player_by_token") and getattr(
                    eng, "phase", getattr(eng, "status", None)
                ) != "finished":
                    candidate = cast(Any, eng)
                    pi = candidate._find_player_by_token(player_token)
                    if pi is not None:
                        return sid, eng

        # 多人游戏：尝试加入现有 waiting session
        engine_cls = self._registry[game_type]
        if getattr(engine_cls, "player_mode", "single") == "multi":
            for sid, eng in list(self._engines.items()):
                if (
                    hasattr(eng, "phase")
                    and eng.phase == "waiting"
                    and getattr(eng, "game_type", "") == game_type
                ):
                    return sid, eng

        # 新建 session
        session_id = str(uuid.uuid4())[:8]
        engine = engine_cls(**create_params)
        self._engines[session_id] = engine
        self._created_at[session_id] = time.time()
        return session_id, engine

    # ── 路由 ──

    def get(self, session_id: str) -> Any | None:
        return self._engines.get(session_id)

    def list_sessions(self) -> list[dict]:
        """列出所有 session 的公开状态（供 admin）"""
        results = []
        for sid, engine in list(self._engines.items()):
            state = {}
            if hasattr(engine, "get_public_state"):
                state = engine.get_public_state()
            state["session_id"] = sid
            state["game_type"] = getattr(engine, "game_type", "unknown")
            state["game_name"] = getattr(engine, "game_name", engine.__class__.__name__)
            state["created_at"] = self._created_at.get(sid, 0)
            results.append(state)
        return results

    # ── 工具描述 ──

    def describe_all_tools(self) -> list[dict]:
        """汇总所有已注册游戏的 MCP 工具定义"""
        tools = []
        for engine_cls in self._registry.values():
            if hasattr(engine_cls, "describe_tools"):
                tools.extend(engine_cls.describe_tools())
        return tools

    # ── 超时 ──

    @staticmethod
    def _is_finished(engine: Any) -> bool:
        """对局是否已结束。大话骰用 phase，猜数字用 status。"""
        phase = getattr(engine, "phase", None)
        if phase is not None:
            return phase == "finished"
        return getattr(engine, "status", None) in ("won", "lost")

    def check_timeouts(self, now: float | None = None) -> list[dict]:
        """检查所有 session 超时，并回收超过保留期的已结束对局。

        返回超时事件列表。已结束对局的回收不产生事件。
        """
        events = []
        now = time.time() if now is None else now
        for sid in list(self._engines):
            engine = self._engines[sid]

            # 已结束：第一次看到时记录结束时刻，超过保留期就回收
            if self._is_finished(engine):
                finished_at = self._finished_at.setdefault(sid, now)
                if now - finished_at > FINISHED_RETENTION:
                    self.remove(sid)
                continue

            if hasattr(engine, "check_timeout"):
                result = engine.check_timeout(now)
                if result:
                    events.append({"session_id": sid, **result})
                    if result.get("expired"):
                        self.remove(sid)
                    elif self._is_finished(engine):
                        # 判负/结束发生在本次检查里，结束时刻从此刻算起
                        self._finished_at[sid] = now
        return events

    def remove(self, session_id: str):
        self._engines.pop(session_id, None)
        self._created_at.pop(session_id, None)
        self._finished_at.pop(session_id, None)

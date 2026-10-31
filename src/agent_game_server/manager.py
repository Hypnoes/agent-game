"""
GameManager — 统一的 session 路由和生命周期管理。

职责：
- 注册游戏类型
- 创建 session（单实例 engine）
- 列出所有 session（供 admin 查询）
- 超时检查 + 已结束对局的回收

不强制 Engine 基类——靠 duck typing。
"""

import logging
import time
import uuid
from typing import Any

from agent_game_server.games.game_engine import is_finished

logger = logging.getLogger(__name__)

# 已结束的对局在内存里保留多久（秒）。过期后由 check_timeouts() 回收，
# 避免 admin/games 列表无限增长。
FINISHED_RETENTION = 300.0

# 已上报过的超时事件的缓存上限。key 是 (session_id, 事件类型)，
# 只用来防重复上报，不需要长期保留，超过上限就按插入顺序淘汰最旧的。
_MAX_SEEN_EXPIRED = 512


def game_type_of(engine: Any) -> str:
    """取引擎的游戏类型。没有 game_type 属性的引擎归为 "unknown"。"""
    return getattr(engine, "game_type", "unknown")


class GameManager:
    def __init__(self):
        self._engines: dict[str, Any] = {}
        self._created_at: dict[str, float] = {}
        self._finished_at: dict[str, float] = {}
        self._registry: dict[str, type[Any]] = {}
        # (session_id, 事件类型) — 同一个超时事件不跨多次轮询重复上报
        self._seen_expired: dict[tuple[str, str], None] = {}

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

        # 断线重连：通过 token 找它所属的对局。
        # 只认同类型游戏 —— 拿着大话骰的 token 建猜数字不能拿回大话骰的 session。
        player_token = create_params.pop("player_token", None)
        if player_token:
            for sid, eng in list(self._engines.items()):
                if game_type_of(eng) != game_type:
                    continue
                if self._is_finished(eng):
                    continue
                finder = getattr(eng, "_find_player_by_token", None)
                if finder is None:
                    continue
                if finder(player_token) is not None:
                    return sid, eng

        # 多人游戏：尝试加入现有 waiting session。
        # 只有请求参数与该 session 现有配置兼容时才复用，否则另开一桌
        # （否则 player_count=6 的请求会默默挤进 player_count=2 的桌子）。
        engine_cls = self._registry[game_type]
        if getattr(engine_cls, "player_mode", "single") == "multi":
            for sid, eng in list(self._engines.items()):
                if game_type_of(eng) != game_type:
                    continue
                if not self._is_waiting(eng):
                    continue
                if not self._params_compatible(eng, create_params):
                    continue
                return sid, eng

        # 新建 session
        session_id = str(uuid.uuid4())[:8]
        engine = engine_cls(**create_params)
        self._engines[session_id] = engine
        self._created_at[session_id] = time.time()
        return session_id, engine

    @staticmethod
    def _params_compatible(engine: Any, params: dict[str, Any]) -> bool:
        """请求参数能否落在该引擎的现有配置上。

        逐个比较同名的引擎属性：引擎没有这个属性就跳过（引擎自己决定要不要），
        有就必须相等。player_token 不是配置项，不参与比较。
        """
        for key, wanted in params.items():
            if key == "player_token":
                continue
            if not hasattr(engine, key):
                continue
            try:
                if getattr(engine, key) != wanted:
                    return False
            except Exception:  # noqa: BLE001 — 属性读不出来就当不兼容
                return False
        return True

    # ── 查询 ──

    def get(self, session_id: str) -> Any | None:
        return self._engines.get(session_id)

    def list_sessions(self) -> list[dict]:
        """列出所有 session 的公开状态（供 admin）"""
        results = []
        for sid, engine in list(self._engines.items()):
            state = {}
            if hasattr(engine, "get_public_state"):
                declared = engine.get_public_state()
                # 拷一份再塞管理字段：直接改返回值等于写进引擎自己的状态对象，
                # 下一个读它的人会看到 session_id / game_type 这些不属于业务的东西。
                state = dict(declared) if isinstance(declared, dict) else {"state": declared}
            state["session_id"] = sid
            state["game_type"] = game_type_of(engine)
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

    # ── 对局状态判定 ──

    @staticmethod
    def _is_finished(engine: Any) -> bool:
        """对局是否已结束。终态名由引擎自己声明，见 game_engine.is_finished。"""
        return is_finished(engine)

    @classmethod
    def _is_waiting(cls, engine: Any) -> bool:
        """是否还在等人加入（可被新玩家复用）。"""
        return getattr(engine, "phase", None) == "waiting" and not cls._is_finished(
            engine
        )

    # ── 超时 ──

    def check_timeouts(self, now: float | None = None) -> list[dict]:
        """检查所有 session 超时，并回收超过保留期的已结束对局。

        返回超时事件列表；已结束对局的回收不产生事件。

        单个引擎的异常就地隔离：这是**唯一**结算超时的地方，一个坏引擎（比如
        get_public_state 里塞了 datetime、或 check_timeout 自己抛）不能让其余对局
        永远不再超时。REST 的 state 分支会在请求内直接调用本方法，所以隔离失败
        会把一个坏引擎的影响放大成全服务 500。
        """
        events: list[dict] = []
        now = time.time() if now is None else now

        for sid in list(self._engines):
            engine = self._engines.get(sid)
            if engine is None:  # 同一轮里被前面的分支回收了
                continue
            try:
                event = self._check_one(sid, engine, now)
            except Exception:  # noqa: BLE001 — 坏引擎只影响自己
                logger.warning(
                    "check_timeouts 跳过异常 session %s（game_type=%s）",
                    sid,
                    game_type_of(engine),
                    exc_info=True,
                )
                continue
            if event is not None:
                events.append(event)
        return events

    def _check_one(self, sid: str, engine: Any, now: float) -> dict | None:
        """结算单个 session。返回要上报的事件，或 None。"""
        # 已结束：第一次看到时记下结束时刻，超过保留期就回收
        if self._is_finished(engine):
            finished_at = self._finished_at.setdefault(sid, now)
            if now - finished_at > FINISHED_RETENTION:
                self.remove(sid)
            return None

        # 重新开局（比如 guess_number start 重置了同一 session）：
        # 旧一轮的结束时刻必须清掉，否则下一轮结束会被立刻当成过期回收。
        self._finished_at.pop(sid, None)

        checker = getattr(engine, "check_timeout", None)
        if checker is None:
            return None
        result = checker(now)
        if not result:
            return None

        event_type = str(result.get("type", ""))
        if result.get("expired"):
            # 大厅超时：事件只报一次（_seen_expired 去重），随后立刻回收 session
            marked = self._mark_expired(sid, event_type)
            self.remove(sid)
            return {"session_id": sid, **result} if marked else None

        if self._is_finished(engine):
            # 判负/结束发生在本次检查里，结束时刻从此刻算起
            self._finished_at[sid] = now
        return {"session_id": sid, **result}

    def _mark_expired(self, session_id: str, event_type: str) -> bool:
        """登记一个已上报的超时事件。

        返回 True 表示这是第一次上报（调用方应该发事件），
        False 表示同一个 (session_id, 事件类型) 之前已经报过 —— 不再重复上报。
        顺带限制缓存规模，避免长期运行后无限增长。
        """
        key = (session_id, event_type)
        if key in self._seen_expired:
            return False
        self._seen_expired[key] = None
        while len(self._seen_expired) > _MAX_SEEN_EXPIRED:
            oldest = next(iter(self._seen_expired))
            del self._seen_expired[oldest]
        return True

    def remove(self, session_id: str):
        """彻底移除一个 session，三个记账字典都要清干净。"""
        self._engines.pop(session_id, None)
        self._created_at.pop(session_id, None)
        self._finished_at.pop(session_id, None)

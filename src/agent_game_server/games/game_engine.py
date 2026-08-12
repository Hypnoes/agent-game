"""
GameEngine Protocol — 所有游戏的抽象契约。

新增游戏只需实现此协议，GameManager/API/MCP Client 全部自动生效。
"""

from typing import Optional, Protocol


class StateMachine:  # forward reference
    pass


class GameEngine(Protocol):
    """游戏引擎协议——每个游戏引擎必须实现的接口。"""

    # ── 类属性 ──
    game_type: str
    game_name: str
    player_mode: str

    # ── 状态机 ──
    _fsm: StateMachine
    @property
    def state(self) -> str: ...
    @property
    def phase(self) -> str: ...
    def is_in(self, *states: str) -> bool: ...

    # ── 工具描述 ──

    @classmethod
    def describe_tools(cls) -> list[dict]:
        """返回 MCP 工具定义列表 [{name, description, input_schema}]"""
        ...

    # ── 状态查询 ──

    def get_public_state(self) -> dict:
        """Admin 可见的公开状态（含秘密信息）"""
        ...

    def get_player_state(self, player_index: int = 0) -> dict:
        """玩家视角状态 + prompt 文本。
        返回 {"public": ..., "private": ..., "prompt": "..."}"""
        ...

    # ── 动作分发 ──

    def handle_action(self, action: str, **params) -> dict:
        """统一动作入口。action 是引擎自定义的动作名（如 "guess", "bid", "join"）。
        返回 get_player_state() 格式的字典。"""
        ...

    # ── 生命周期 ──

    def check_timeout(self, now: float) -> Optional[dict]:
        """超时检查。返回 None 表示未超时，返回 {"expired": True, "type": "..."} 表示需要清理。"""
        ...

    # ── 提示词 ──

    @classmethod
    def get_prompts(cls):
        """返回 Prompt 实例（热重载用）"""
        ...

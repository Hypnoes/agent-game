"""
GameEngine Protocol — 所有游戏的抽象契约。

新增游戏只需实现此协议，GameManager/API/MCP Client 全部自动生效。

关于「可选能力」：describe_tools / get_prompts 并非每款游戏都需要，调用方一律
用 hasattr() 探测。协议里用 `...` 写的桩方法在运行时是真实存在的可调用对象，
子类会原样继承——那会让 hasattr() 永远为真，探测形同虚设（一个没有提示词的
引擎也会被当成「有 get_prompts」）。因此可选方法的签名放在 `if TYPE_CHECKING:`
里：类型检查器照常校验，运行时不定义，子类不继承。将来若增加可选能力
（如阻塞式等待），同样放进这个块。
"""

from typing import TYPE_CHECKING, Protocol

from agent_game_server.games.state_machine import StateMachine

if TYPE_CHECKING:
    from agent_game_server.prompt import Prompt


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

    def check_timeout(self, now: float) -> dict | None:
        """超时检查。返回 None 表示未超时，返回 {"expired": True, "type": "..."} 表示需要清理。"""
        ...

    # ── 可选能力（运行时不存在，只有实现了的引擎才 hasattr 得 True）──

    if TYPE_CHECKING:

        @classmethod
        def describe_tools(cls) -> list[dict]:
            """返回 MCP 工具定义列表 [{name, description, input_schema}]。

            可选：没有工具面的引擎不实现它，manager.describe_all_tools() 会跳过。
            """
            ...

        @classmethod
        def get_prompts(cls) -> "Prompt":
            """返回 Prompt 实例（热重载用）。

            可选：没有提示词模板的引擎不实现它，reload_prompts 会跳过。
            """
            ...

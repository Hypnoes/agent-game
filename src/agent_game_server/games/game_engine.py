"""
GameEngine Protocol — 所有游戏的抽象契约。

新增游戏只需实现此协议，GameManager/API/MCP Client 全部自动生效。

关于「可选能力」：describe_tools / get_prompts 并非每款游戏都需要，调用方一律
用 hasattr() 探测。协议里用 `...` 写的桩方法在运行时是真实存在的可调用对象，
子类会原样继承——那会让 hasattr() 永远为真，探测形同虚设（一个没有提示词的
引擎也会被当成「有 get_prompts」）。因此可选方法的签名放在 `if TYPE_CHECKING:`
里：类型检查器照常校验，运行时不定义，子类不继承。

同一条规则适用于可选类属性 finished_states / secret_keys：它们也只有类型声明，
运行时不存在。探测一律走本模块的 finished_states_of() / secret_keys_of()，
那两个函数给出默认值，所以新引擎不声明也不会漏判。

身份校验（player_identity_error）与终态判定（is_finished）放在本模块，
是为了让 REST 与 MCP 两条传输共用同一份判断，而不是各自实现一遍——
它们曾经语义分叉过：REST 先校验再等待，MCP 先等待再校验。
"""

from typing import TYPE_CHECKING, NamedTuple, Protocol

from agent_game_server.games.state_machine import StateMachine

if TYPE_CHECKING:
    from agent_game_server.prompt import Prompt


# 引擎没有声明时的默认终态名。README 承诺"零改动加游戏"，但终态名是引擎自己的知识：
# 猜数字用 won / lost，大话骰用 finished。管理器与传输层都必须问引擎，而不是自己
# 内置一份列表——内置的那份永远会在新游戏加进来时过期。
DEFAULT_FINISHED_STATES = frozenset({"finished", "won", "lost"})

# 公开状态里默认要剥掉的秘密键。带秘密字段的引擎应当用 secret_keys 声明自己的。
DEFAULT_SECRET_KEYS = frozenset({"target", "all_dice"})


def finished_states_of(engine) -> frozenset[str]:
    """该引擎的终态名集合。引擎可声明 finished_states，否则用默认集合。"""
    declared = getattr(engine, "finished_states", None)
    if isinstance(declared, (frozenset, set, tuple, list)) and declared:
        return frozenset(str(name) for name in declared)
    return DEFAULT_FINISHED_STATES


def secret_keys_of(engine) -> frozenset[str]:
    """该引擎需要从公开状态里剥掉的键。"""
    declared = getattr(engine, "secret_keys", None)
    if isinstance(declared, (frozenset, set, tuple, list)):
        return frozenset(str(key) for key in declared)
    return DEFAULT_SECRET_KEYS


def is_finished(engine) -> bool:
    """对局是否已结束。终态名由引擎声明，猜数字的 status 与骰子的 phase 都走这里。

    逐个取 phase / status 且只认字符串：`GameEngine` 协议在运行时没有桩方法，
    但某些引擎会把 phase 定义成返回 None 的属性，getattr 的默认值对"存在但为 None"
    不生效，直接取会拿到 None 而漏判。
    """
    states = finished_states_of(engine)
    for attr in ("phase", "status"):
        value = getattr(engine, attr, None)
        if isinstance(value, str) and value in states:
            return True
    return False


def phase_of(engine) -> str | None:
    """引擎当前状态名，取第一个是字符串的 phase / status。"""
    for attr in ("phase", "status"):
        value = getattr(engine, attr, None)
        if isinstance(value, str):
            return value
    return None


class IdentityError(NamedTuple):
    """玩家身份校验失败的原因。

    `kind` 让调用方决定映射成什么：REST 把 not_found 映射成 404（查询语义）
    或 400（动作语义），把两个 token 错误映射成 403；MCP 一律映射成 ValueError。
    校验本身与传输无关，所以放在这里而不是任一传输层。
    """

    kind: str  # "invalid_index" | "not_found" | "missing_token" | "token_mismatch"
    message: str


def player_token_of(player) -> str | None:
    """取玩家的 token。引擎可能用对象（.token）也可能用 dict（["token"]）。"""
    if isinstance(player, dict):
        return player.get("token")
    return getattr(player, "token", None)


def player_identity_error(
    engine, player_index: object, player_token: object
) -> IdentityError | None:
    """校验"调用方是否真的是这名玩家"。返回 None 表示通过。

    多人引擎（暴露 players 列表）必须带 player_token 且与对应玩家一致；
    缺 token 一律拒绝——否则按索引就能拿到对手的私有骰子。
    单人引擎没有 players 属性，保持开放（兼容原有调用方式）。

    两条传输都必须在**进入等待之前**调用它：一个 token 打错的 state 调用如果先
    挂满 55 秒预算再报错，调用方会以为"对手在思考"。
    """
    players = getattr(engine, "players", None)
    if players is None:
        return None

    if not isinstance(player_index, int) or isinstance(player_index, bool):
        return IdentityError("invalid_index", "无效的玩家编号")
    if not 0 <= player_index < len(players):
        return IdentityError("not_found", f"无效的玩家编号: {player_index}")

    token = player_token if isinstance(player_token, str) else ""
    if not token:
        return IdentityError("missing_token", "缺少 player_token")
    if player_token_of(players[player_index]) != token:
        return IdentityError("token_mismatch", "player_token 不匹配")
    return None


class GameEngine(Protocol):
    """游戏引擎协议——每个游戏引擎必须实现的接口。"""

    # ── 类属性 ──
    game_type: str
    game_name: str
    player_mode: str

    # ── 可选类属性（运行时用 getattr 探测，缺省时取 DEFAULT_* 常量）──
    if TYPE_CHECKING:

        finished_states: frozenset[str]
        """该引擎的终态名集合。不声明时用 DEFAULT_FINISHED_STATES。"""

        secret_keys: frozenset[str]
        """公开状态里必须剥掉的键。不声明时用 DEFAULT_SECRET_KEYS。"""

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

        async def wait_for_change(self, player_index: int, timeout: float) -> str:
            """可选方法。回合制游戏实现它，让非当前回合的 state 调用在服务端挂起。

            返回 "ready"（已轮到调用方或对局已结束）、"changed"（被唤醒）或
            "timeout"（等满预算）。被取消时应让 CancelledError 向上抛出。

            单人即时游戏（如 guess_number）不实现该方法。调用方必须用
            getattr(engine, "wait_for_change", None) 探测——本协议在运行时
            不提供任何方法体，所以 hasattr 是可信的，但 getattr 写法更稳妥。
            """
            ...

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

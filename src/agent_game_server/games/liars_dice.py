"""
大话骰（Liar's Dice / 吹牛）游戏引擎 — 单 session 实例 + 自描述协议。

规则：
- 每人 5 骰（`dice_per_player` 可配置），只能看自己的
- 轮流"叫数"：宣称全场某点数至少有多少颗
- 1 是百搭（可代替任意点数），但叫 1 时 1 只算 1
- 下家可选"加码"或"开"（揭盅）
- 实际 ≥ 叫数 → 开的人输；实际 < 叫数 → 叫的人输
- 叫数必须自身合法：数量 ≥ 1、面值 1-6、数量不超过场上骰子总数
  （超过即按构造必输），且必须比上家大（见 `_is_valid_bid`）。不能开自己的盅。
- 身份校验：`state` / `bid` / `challenge` 必须带 `player_token`，只有 `join`
  可以不带（新玩家入场）。大话骰的全部玩法建立在私有信息上，缺 token 就等于
  允许替别人行动、看别人的骰子。

淘汰规则（输一轮的代价）：
- 场上还有 3 名及以上存活玩家：输家本轮只掉一颗骰子，骰子掉光才 `alive = False`，
  对局继续；本轮叫数清零，由输家先叫下一轮（输家已出局则由其下一位存活玩家先叫）。
- 场上只剩 2 名存活玩家：突然死亡——输一轮直接出局（骰子清零），保留 2 人局
  "一局定胜负"的原有体验。
- 回合超时按同样的淘汰规则处理：超时者视为输掉本轮（掉骰/出局），但回合直接交给
  下一位存活玩家——超时者没有叫数，让他接着叫下一轮只会继续空转。
- 只剩一名存活玩家时对局结束（`finished`），该玩家即 `winner`。
"""

import random
import time
import uuid
from dataclasses import dataclass, field
from datetime import UTC, datetime

# ═══════════════════════════════════════════
# 数据结构
# ═══════════════════════════════════════════


@dataclass
class Player:
    name: str
    token: str = ""
    dice: list[int] = field(default_factory=list)
    alive: bool = True
    joined_at: str = ""


@dataclass
class Call:
    player_idx: int
    quantity: int
    face: int
    message: str = ""


from agent_game_server.games.game_engine import GameEngine
from agent_game_server.games.state_machine import StateMachine
from agent_game_server.prompt import Prompt

# prompt 模板 — 修改 assets/liars_dice_prompts.py 后 POST /admin/reload-prompts 热更新
_PROMPT_MODULE = "agent_game_server.assets.liars_dice_prompts"


class _LiarsDiceFSM(StateMachine):
    def allowed_transitions(self) -> dict[str, set[str]]:
        return {
            "waiting": {"bidding"},
            "bidding": {"finished"},
            "finished": set(),
        }

    def _on_after_transition(self, to: str):
        """子类可 override：转移到 finished 后记录结束时间等"""


# ═══════════════════════════════════════════
# 引擎
# ═══════════════════════════════════════════


class LiarsDiceEngine(GameEngine):
    """大话骰引擎。每个实例管理一局游戏。"""

    game_type = "liars_dice"
    game_name = "大话骰"
    player_mode = "multi"

    _prompts: Prompt | None = None  # 类级共享，避免每个实例都加载文件

    @classmethod
    def get_prompts(cls) -> Prompt:
        if cls._prompts is None:
            cls._prompts = Prompt(_PROMPT_MODULE)
        return cls._prompts

    def __init__(self, player_count: int = 2, dice_per_player: int = 5):
        self._validate_setup(player_count, dice_per_player)
        self.player_count = player_count
        self.dice_per_player = dice_per_player
        # 回合判负时限（秒）。与 game_server 的等待预算 BLOCK_BUDGET 是两个数值：
        # 这里决定多久不行动判负，那边决定单次 state 调用最多挂起多久。
        self.turn_timeout = 60
        self._fsm = _LiarsDiceFSM("waiting")
        self.players: list[Player] = []
        self.current_turn: int = 0
        self.current_call: Call | None = None
        self.call_history: list[Call] = []
        self.winner: int | None = None
        self.loser: int | None = None
        self.created_at = datetime.now(UTC).isoformat()
        self.last_action = time.time()

    @property
    def state(self) -> str:
        return self._fsm.state

    @property
    def phase(self) -> str:
        """兼容旧 API — 引擎内部继续用 phase"""
        return self._fsm.state

    # ── 状态变更通知 ──

    def _notify_state_change(self):
        """通知所有等待者"局面变了"。

        阻塞式 state 等待（`wait_for_change` / `ChangeSignal`）依赖这个信号，
        所以每一条会改变局面的路径都必须调用它。信号对象在另一条分支上引入，
        这里用 getattr 探测：没有信号时是 no-op，合并后自动生效。
        """
        signal = getattr(self, "_signal", None)
        if signal is not None:
            signal.notify()

    def is_in(self, *states: str) -> bool:
        return self._fsm.is_in(*states)

    # ── 自描述协议 ──

    @classmethod
    def describe_tools(cls) -> list[dict]:
        return [
            {
                "name": "liars_dice_join",
                "description": "加入大话骰（吹牛/撒谎骰子）匹配队列。只需提供名字，服务器自动匹配对手。如果之前断线，传入 player_token 可重连回原有对局。每人5颗骰子，1是百搭。返回玩家编号和游戏状态。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "player_name": {"type": "string", "description": "你的名字"},
                        "player_token": {
                            "type": "string",
                            "description": "断线重连时传入之前的 token",
                        },
                    },
                    "required": ["player_name"],
                },
            },
            {
                "name": "liars_dice_state",
                "description": "查看大话骰对局当前状态。**注意：如果还没轮到你，这个调用会阻塞等待（最长约60秒）直到对手行动、游戏结束或超时。返回慢是正常的——不是出错，是服务器在等你对面的玩家决策。** 必须带上你的 player_index 和 player_token。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "player_index": {
                            "type": "integer",
                            "description": "你的玩家编号",
                        },
                        "player_token": {
                            "type": "string",
                            "description": "你的身份 token（必填）",
                        },
                    },
                    "required": ["player_index", "player_token"],
                },
            },
            {
                "name": "liars_dice_bid",
                "description": "在大话骰中叫数。宣称全场至少有多少颗某个点数的骰子。数量不能超过场上骰子总数，面值 1-6，且必须比上家大。可选附带一句话 bluff 或误导对手。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "player_index": {
                            "type": "integer",
                            "description": "你的玩家编号",
                        },
                        "player_token": {
                            "type": "string",
                            "description": "你的身份 token（必填）",
                        },
                        "quantity": {
                            "type": "integer",
                            "description": "数量（≥1，且不超过场上骰子总数）",
                            "minimum": 1,
                        },
                        "face": {
                            "type": "integer",
                            "description": "面值 1-6",
                            "minimum": 1,
                            "maximum": 6,
                        },
                        "message": {
                            "type": "string",
                            "description": "可选: bluff/误导信息",
                        },
                    },
                    "required": ["player_index", "player_token", "quantity", "face"],
                },
            },
            {
                "name": "liars_dice_challenge",
                "description": "在大话骰中开盅（挑战上家的叫数）。所有人揭骰验证。不能开自己的盅。必须带上你的 player_index 和 player_token。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "player_index": {
                            "type": "integer",
                            "description": "你的玩家编号",
                        },
                        "player_token": {
                            "type": "string",
                            "description": "你的身份 token（必填）",
                        },
                    },
                    "required": ["player_index", "player_token"],
                },
            },
        ]

    def get_public_state(self) -> dict:
        """Admin 可见的公开状态"""
        cc = None
        if self.current_call:
            cc = {
                "quantity": self.current_call.quantity,
                "face": self.current_call.face,
                "player": self.players[self.current_call.player_idx].name
                if self.current_call.player_idx < len(self.players)
                else "?",
                "message": self.current_call.message,
            }
        return {
            "status": self.phase,
            "players": [p.name for p in self.players],
            "all_dice": [list(p.dice) for p in self.players],
            "alive": [p.alive for p in self.players],
            "current_turn": self.current_turn,
            "current_call": cc,
            "call_history": [
                {
                    "player": self.players[c.player_idx].name
                    if c.player_idx < len(self.players)
                    else "?",
                    "quantity": c.quantity,
                    "face": c.face,
                    "message": c.message,
                }
                for c in self.call_history
            ],
            "winner": self.winner,
            "loser": self.loser,
            "dice_per_player": self.dice_per_player,
            "created_at": self.created_at,
        }

    def get_player_state(self, player_index: int = 0) -> dict:
        """玩家视角状态 + prompt 文本。

        `player_index` 越界（含负数）直接报错：Python 负索引会绕回最后一名玩家，
        不做校验就等于凭空拿到对手的骰子。
        """
        self._validate_index(player_index)
        public = self._build_public_view()
        private = {
            "your_dice": list(self.players[player_index].dice),
            "your_index": player_index,
        }
        if self.phase == "finished":
            private["all_dice"] = [list(p.dice) for p in self.players]
            private["winner"] = self.winner
            private["winner_name"] = (
                self.players[self.winner].name if self.winner is not None else None
            )

        prompt = self._build_prompt(player_index)
        return {"public": public, "private": private, "prompt": prompt}

    def handle_action(self, action: str, **params):
        """统一 action 入口"""
        if action == "join":
            result = self._add_player(
                params["player_name"], params.get("player_token", "")
            )
            # 返回 player_state 格式（含 prompt），同时带上 join 的元数据
            state = self.get_player_state(result["player_index"])
            state["player_index"] = result["player_index"]
            state["player_token"] = result.get("player_token", "")
            state["phase"] = result["phase"]
            state["dice"] = result["dice"]
            state["player_count"] = result["player_count"]
            state["needed"] = result["needed"]
            state["reconnected"] = result.get("reconnected", False)
            # 把 player_index 和 token 写进 prompt
            state["prompt"] = (
                f"🎲 你是玩家 {result['player_index']}，token: {result.get('player_token', '')}\n"
                f"{state['prompt']}"
            )
            return state
        elif action == "state":
            pi = params.get("player_index", 0)
            self._validate_player(pi, params.get("player_token", ""))
            return self.get_player_state(pi)
        elif action == "bid":
            pi = params["player_index"]
            self._validate_player(pi, params.get("player_token", ""))
            self._make_bid(
                pi, params["quantity"], params["face"], params.get("message", "")
            )
            return self.get_player_state(pi)
        elif action == "challenge":
            pi = params["player_index"]
            self._validate_player(pi, params.get("player_token", ""))
            self._challenge(pi)
            return self.get_player_state(pi)
        else:
            raise ValueError(f"未知 action: {action}")

    def check_timeout(self, now: float) -> dict | None:
        # 匹配超时
        if self.phase == "waiting" and now - self.last_action > 120:
            self._notify_state_change()
            return {"type": "lobby_timeout", "expired": True}
        # 回合超时：超时者按"输掉本轮"处理（掉骰/出局），而不是让下家白捡一局。
        # 回合必须交出去，否则超时者会立刻再次超时，对局永远卡住。
        if self.phase == "bidding" and now - self.last_action > self.turn_timeout:
            loser = self.current_turn
            self.loser = loser
            self.last_action = now
            if not self._eliminate(loser):
                self.current_turn = self._next_alive_index(loser)
            self._notify_state_change()
            return {"type": "turn_timeout", "loser": loser}
        return None

    # ── 游戏逻辑 ──

    def _start_game(self):
        """开局：只给存活玩家摇骰，随机选一名存活玩家先手。"""
        for p in self.players:
            p.dice = (
                [random.randint(1, 6) for _ in range(self.dice_per_player)]
                if p.alive
                else []
            )
        self._fsm.transition_to("bidding")
        alive = self._alive_indices()
        self.current_turn = random.choice(alive) if alive else 0
        self.last_action = time.time()
        self._notify_state_change()

    def _add_player(self, name: str, player_token: str = "") -> dict:
        """加一个玩家。人满自动开局。返回含 token 的元数据。

        带上已有的 `player_token` 且命中在场玩家时走重连：不追加玩家、不重摇骰子、
        不推进状态机，只把现有身份还给他。
        """
        if player_token:
            existing = self._find_player_by_token(player_token)
            if existing is not None:
                return self._player_meta(existing, reconnected=True)

        # 对局已经开始还允许追加玩家，会触发 _start_game() 重摇全场骰子、
        # 把进行中的对局推倒重来，所以非 waiting 阶段一律拒绝。
        if self.phase != "waiting":
            raise ValueError(f"对局已开始（{self.phase}），无法加入")

        token = str(uuid.uuid4())[:12]
        idx = len(self.players)
        self.players.append(
            Player(name=name, token=token, joined_at=datetime.now(UTC).isoformat())
        )
        self.last_action = time.time()

        if len(self.players) >= self.player_count:
            self._start_game()
        else:
            # 人还没满，同样是一次状态变更：大厅里等待的玩家要看到人数变化
            self._notify_state_change()

        return self._player_meta(idx)

    def _player_meta(self, idx: int, reconnected: bool = False) -> dict:
        """返回 join 的元数据（player_index / token / 开局骰子等）。"""
        return {
            "player_index": idx,
            "player_token": self.players[idx].token,
            "phase": self.phase,
            "dice": list(self.players[idx].dice) if self.phase == "bidding" else [],
            "player_count": len(self.players),
            "needed": self.player_count,
            "reconnected": reconnected,
        }

    def _find_player_by_token(self, token: str) -> int | None:
        """通过 token 查找玩家索引，用于重连。找不到返回 None。"""
        for i, p in enumerate(self.players):
            if p.token == token:
                return i
        return None

    def _make_bid(self, player_index: int, quantity: int, face: int, message: str = ""):
        self._check_turn(player_index)
        self._validate_bid_values(quantity, face)
        new_call = Call(
            player_idx=player_index, quantity=quantity, face=face, message=message
        )

        if not self._is_valid_bid(self.current_call, new_call):
            current_quantity = self.current_call.quantity if self.current_call else 0
            current_face = self.current_call.face if self.current_call else 0
            raise ValueError(
                f"叫数必须比上家大。当前: {current_quantity}×{current_face} "
                f"→ 你叫: {quantity}×{face}"
            )

        self.current_call = new_call
        self.call_history.append(new_call)
        self.current_turn = self._next_alive_index(player_index)
        self.last_action = time.time()
        self._notify_state_change()

    def _challenge(self, player_index: int):
        self._check_turn(player_index)
        if self.current_call is None:
            raise ValueError("没有可开的叫数")

        call = self.current_call
        if player_index == call.player_idx:
            raise ValueError("不能开自己的盅")

        actual = self._count_face(call.face)
        loser = player_index if actual >= call.quantity else call.player_idx

        # 开盅记录（quantity/face 为 0 表示这不是一次叫数）
        self.call_history.append(
            Call(
                player_idx=player_index,
                quantity=0,
                face=0,
                message=f"开！(实际 {actual} 个 {call.face})",
            )
        )
        self.current_call = None
        self.loser = loser
        self.last_action = time.time()

        if not self._eliminate(loser):
            # 对局继续：本轮叫数清零，由输家先叫下一轮（已出局则顺延）
            self.current_turn = self._next_turn_after(loser)
        # 掉骰、出局、结束对局都是状态变更，唤醒可能还在等待的对手
        self._notify_state_change()

    # ── 淘汰 ──

    def _alive_indices(self) -> list[int]:
        return [i for i, p in enumerate(self.players) if p.alive]

    def _next_alive_index(self, player_index: int) -> int:
        """从 player_index 往后找下一位存活玩家（跳过出局者，绕一圈）。"""
        count = len(self.players)
        for step in range(1, count + 1):
            idx = (player_index + step) % count
            if self.players[idx].alive:
                return idx
        # 没有别的存活玩家了（对局应已结束），原地不动以免乱指
        return player_index

    def _next_turn_after(self, player_index: int) -> int:
        """本轮输家先叫下一轮；若他已被淘汰，则顺延到下一位存活玩家。"""
        if 0 <= player_index < len(self.players) and self.players[player_index].alive:
            return player_index
        return self._next_alive_index(player_index)

    def _total_dice(self) -> int:
        """场上仍在使用的骰子总数。"""
        return sum(len(p.dice) for p in self.players if p.alive)

    def _count_face(self, face: int) -> int:
        """统计全场满足 face 的骰子数。1 是百搭：叫非 1 时把 1 也算进来。"""
        return sum(
            1
            for p in self.players
            for d in p.dice
            if d == face or (d == 1 and face != 1)
        )

    def _eliminate(self, player_index: int) -> bool:
        """让 player_index 输掉本轮：掉一颗骰子，掉光则出局。

        场上只剩 2 名存活玩家时是突然死亡：本轮输家直接出局，保留 2 人局
        "一局定胜负"的原有体验。返回对局是否因此结束。
        """
        self._validate_index(player_index)
        player = self.players[player_index]

        if player.alive:
            if len(self._alive_indices()) <= 2:
                player.dice.clear()
            elif player.dice:
                player.dice.pop()
            if not player.dice:
                player.alive = False

        return self._finish_if_over()

    def _finish_if_over(self) -> bool:
        """只剩一名存活玩家时结束对局，最后存活者即 winner。"""
        alive = self._alive_indices()
        if len(alive) > 1:
            return False

        self.winner = alive[0] if alive else None
        self.current_call = None
        # current_turn 只允许指向存活玩家，对局结束后停在赢家身上
        if self.winner is not None:
            self.current_turn = self.winner
        if self.phase != "finished":
            self._fsm.transition_to("finished")
        return True

    # ── 验证 ──

    @staticmethod
    def _is_int(value) -> bool:
        """bool 是 int 的子类，棋子数量/面值不接受 True/False。"""
        return isinstance(value, int) and not isinstance(value, bool)

    @classmethod
    def _validate_setup(cls, player_count: int, dice_per_player: int):
        if not cls._is_int(player_count) or player_count < 2:
            raise ValueError(f"player_count 必须是 ≥ 2 的整数，收到: {player_count!r}")
        if not cls._is_int(dice_per_player) or dice_per_player < 1:
            raise ValueError(
                f"dice_per_player 必须是 ≥ 1 的整数，收到: {dice_per_player!r}"
            )

    def _validate_index(self, player_index: int):
        if (
            not self._is_int(player_index)
            or player_index < 0
            or player_index >= len(self.players)
        ):
            raise ValueError(f"无效的玩家编号: {player_index}")

    def _validate_bid_values(self, quantity: int, face: int):
        """校验叫数本身是否合法——与"必须比上家大"是两回事。"""
        if not self._is_int(quantity) or quantity < 1:
            raise ValueError(f"叫数数量必须是 ≥ 1 的整数，收到: {quantity!r}")
        if not self._is_int(face) or not 1 <= face <= 6:
            raise ValueError(f"面值必须是 1-6 的整数，收到: {face!r}")
        total = self._total_dice()
        if quantity > total:
            raise ValueError(
                f"叫数 {quantity} 超过场上骰子总数 {total}，这样的叫数永远开不赢"
            )

    def _check_turn(self, player_index: int):
        if self.phase != "bidding":
            raise ValueError(f"游戏阶段是 {self.phase}，不能行动")
        self._validate_index(player_index)
        if not self.players[player_index].alive:
            raise ValueError(f"玩家 {player_index} 已出局，不能行动")
        if player_index != self.current_turn:
            raise ValueError(f"还没轮到你（当前是玩家 {self.current_turn}）")

    def _validate_player(self, player_index: int, player_token: str = ""):
        """验证玩家身份。token 必填——缺 token 就等于允许替别人行动。"""
        self._validate_index(player_index)
        if not player_token:
            raise ValueError("缺少 player_token")
        if self.players[player_index].token != player_token:
            raise ValueError("player_token 不匹配")

    def _is_valid_bid(self, prev: Call | None, new: Call) -> bool:
        if prev is None:
            return True
        if new.face == 1 and prev.face != 1:
            return new.quantity >= (prev.quantity // 2 + 1)
        if new.face != 1 and prev.face == 1:
            return new.quantity >= prev.quantity * 2
        return new.quantity > prev.quantity or (
            new.quantity == prev.quantity and new.face > prev.face
        )

    # ── 状态视图 ──

    def _build_public_view(self) -> dict:
        cc = None
        if self.current_call:
            caller = self.players[self.current_call.player_idx]
            cc = {
                "player": self.current_call.player_idx,
                "player_name": caller.name,
                "quantity": self.current_call.quantity,
                "face": self.current_call.face,
            }
        return {
            "phase": self.phase,
            "current_turn": self.current_turn,
            "current_call": cc,
            "last_message": self.current_call.message if self.current_call else "",
            "players": [p.name for p in self.players],
            "alive": [p.alive for p in self.players],
            "dice_per_player": self.dice_per_player,
            "call_history": [
                {
                    "player": c.player_idx,
                    "player_name": self.players[c.player_idx].name
                    if c.player_idx < len(self.players)
                    else "?",
                    "quantity": c.quantity,
                    "face": c.face,
                    "message": c.message,
                }
                for c in self.call_history
            ],
        }

    # ── Prompt 生成（模板渲染，修改 YAML 后调 reload() 热更新）──

    def _build_prompt(self, player_index: int) -> str:
        P = self.get_prompts()
        # 剩余时间只用于展示：超时后不能显示成负数
        remaining = max(0, int(self.turn_timeout - (time.time() - self.last_action)))

        if self.phase == "finished":
            winner = self.players[self.winner].name if self.winner is not None else "?"
            return P.render(
                "finished",
                winner=winner,
                all_dice=[p.dice for p in self.players],
                call_history=[
                    (c.quantity, c.face) for c in self.call_history if c.quantity > 0
                ],
            )

        if self.phase == "waiting":
            return P.render(
                "waiting",
                players=len(self.players),
                needed=self.player_count,
                waited=int(time.time() - self.last_action),
            )

        # 走到这里一定是 bidding：先挡住越界/负索引，否则负索引会把对手的骰子
        # 渲染进 prompt 文本里。
        self._validate_index(player_index)

        if self.current_turn != player_index:
            current_player = self.players[self.current_turn]
            waited = int(time.time() - self.last_action)
            patience = ""
            if waited > 20:
                patience = P.render("patience_20")
            if waited > 40:
                patience = P.render(
                    "patience_40",
                    remaining_time=max(0, int(self.turn_timeout - waited)),
                )

            call_str = (
                f"{self.current_call.quantity}×{self.current_call.face}"
                if self.current_call
                else "无"
            )
            return P.render(
                "not_your_turn",
                current_player=current_player.name,
                progress="刚刚" if waited < 10 else f"已思考 {waited} 秒",
                patience=patience,
                call_str=call_str,
                dice=self.players[player_index].dice,
                player_index=player_index,
            )

        # 轮到自己
        call = self.current_call
        if call:
            call_str = f"{call.quantity}×{call.face}（{self.players[call.player_idx].name} 叫的）"
        else:
            call_str = "无（你是第一个叫数的人）"
        msg = self.current_call.message if self.current_call else ""
        msg_line = f"\n💬 上家说: '{msg}'" if msg else ""

        return P.render(
            "your_turn",
            dice=self.players[player_index].dice,
            call_str=call_str,
            msg_line=msg_line,
            remaining=remaining,
            player_index=player_index,
        )

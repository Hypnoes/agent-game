"""
大话骰（Liar's Dice / 吹牛）游戏引擎 — 单 session 实例 + 自描述协议。

规则：
- 每人 5 骰，只能看自己的
- 轮流"叫数"：宣称全场某点数至少有多少颗
- 1 是百搭（可代替任意点数），但叫 1 时 1 只算 1
- 下家可选"加码"或"开"（揭盅）
- 实际 ≥ 叫数 → 开的人输；实际 < 叫数 → 叫的人输
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
        self.player_count = player_count
        self.dice_per_player = dice_per_player
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
                "description": "查看大话骰对局当前状态。**注意：如果还没轮到你，这个调用会阻塞等待（最长约60秒）直到对手行动、游戏结束或超时。返回慢是正常的——不是出错，是服务器在等你对面的玩家决策。** 只需提供你的 player_index。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "player_index": {
                            "type": "integer",
                            "description": "你的玩家编号",
                        },
                        "player_token": {
                            "type": "string",
                            "description": "你的身份 token",
                        },
                    },
                    "required": ["player_index"],
                },
            },
            {
                "name": "liars_dice_bid",
                "description": "在大话骰中叫数。宣称全场至少有多少颗某个点数的骰子。可选附带一句话 bluff 或误导对手。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "player_index": {
                            "type": "integer",
                            "description": "你的玩家编号",
                        },
                        "player_token": {
                            "type": "string",
                            "description": "你的身份 token",
                        },
                        "quantity": {
                            "type": "integer",
                            "description": "数量",
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
                    "required": ["player_index", "quantity", "face"],
                },
            },
            {
                "name": "liars_dice_challenge",
                "description": "在大话骰中开盅（挑战上家的叫数）。所有人揭骰验证。只需提供 player_index。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "player_index": {
                            "type": "integer",
                            "description": "你的玩家编号",
                        },
                        "player_token": {
                            "type": "string",
                            "description": "你的身份 token",
                        },
                    },
                    "required": ["player_index"],
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
            "all_dice": [p.dice for p in self.players],
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
        """玩家视角状态 + prompt 文本"""
        public = self._build_public_view()
        private = {
            "your_dice": self.players[player_index].dice
            if player_index < len(self.players)
            else [],
            "your_index": player_index,
        }
        if self.phase == "finished":
            private["all_dice"] = [p.dice for p in self.players]
            private["winner"] = self.winner
            private["winner_name"] = (
                self.players[self.winner].name if self.winner is not None else None
            )

        prompt = self._build_prompt(player_index)
        return {"public": public, "private": private, "prompt": prompt}

    def handle_action(self, action: str, **params):
        """统一 action 入口"""
        if action == "join":
            result = self._add_player(params["player_name"])
            # 返回 player_state 格式（含 prompt），同时带上 join 的元数据
            state = self.get_player_state(result["player_index"])
            state["player_index"] = result["player_index"]
            state["player_token"] = result.get("player_token", "")
            state["phase"] = result["phase"]
            state["dice"] = result["dice"]
            state["player_count"] = result["player_count"]
            state["needed"] = result["needed"]
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
            return {"type": "lobby_timeout", "expired": True}
        # 回合超时
        if self.phase == "bidding" and now - self.last_action > 60:
            self.loser = self.current_turn
            self.winner = (self.current_turn + 1) % (len(self.players) or 1)
            self._fsm.transition_to("finished")
            return {"type": "turn_timeout", "loser": self.current_turn}
        return None

    # ── 游戏逻辑 ──

    def _start_game(self):
        """开局：摇骰，随机先手"""
        for p in self.players:
            p.dice = [random.randint(1, 6) for _ in range(self.dice_per_player)]
        self._fsm.transition_to("bidding")
        self.current_turn = random.randint(0, len(self.players) - 1)
        self.last_action = time.time()

    def _add_player(self, name: str) -> dict:
        """加一个玩家。人满自动开局。返回含 token 的元数据。"""
        token = str(uuid.uuid4())[:12]
        idx = len(self.players)
        self.players.append(
            Player(name=name, token=token, joined_at=datetime.now(UTC).isoformat())
        )
        self.last_action = time.time()

        if len(self.players) >= self.player_count:
            self._start_game()

        return {
            "player_index": idx,
            "player_token": token,
            "phase": self.phase,
            "dice": self.players[idx].dice if self.phase == "bidding" else [],
            "player_count": len(self.players),
            "needed": self.player_count,
        }

    def _find_player_by_token(self, token: str) -> int | None:
        """通过 token 查找玩家索引，用于重连。找不到返回 None。"""
        for i, p in enumerate(self.players):
            if p.token == token:
                return i
        return None

    def _make_bid(self, player_index: int, quantity: int, face: int, message: str = ""):
        self._check_turn(player_index)
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
        self.current_turn = (self.current_turn + 1) % len(self.players)
        self.last_action = time.time()

    def _challenge(self, player_index: int):
        self._check_turn(player_index)
        if self.current_call is None:
            raise ValueError("没有可开的叫数")

        call = self.current_call
        all_dice = [d for p in self.players for d in p.dice]
        actual = sum(
            1 for d in all_dice if d == call.face or (d == 1 and call.face != 1)
        )

        if actual >= call.quantity:
            self.winner = call.player_idx
            self.loser = player_index
        else:
            self.winner = player_index
            self.loser = call.player_idx

        self._fsm.transition_to("finished")
        self.call_history.append(
            Call(
                player_idx=player_index,
                quantity=0,
                face=0,
                message=f"开！(实际 {actual} 个 {call.face})",
            )
        )
        self.last_action = time.time()

    # ── 验证 ──

    def _check_turn(self, player_index: int):
        if self.phase != "bidding":
            raise ValueError(f"游戏阶段是 {self.phase}，不能行动")
        if player_index != self.current_turn:
            raise ValueError(f"还没轮到你（当前是玩家 {self.current_turn}）")

    def _validate_player(self, player_index: int, player_token: str = ""):
        """验证玩家身份。token 不匹配则拒绝。"""
        if player_index < 0 or player_index >= len(self.players):
            raise ValueError(f"无效的玩家编号: {player_index}")
        p = self.players[player_index]
        if player_token and p.token != player_token:
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
        remaining = 60 - (time.time() - self.last_action)

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

        # bidding phase
        if self.current_turn != player_index:
            current_player = self.players[self.current_turn]
            waited = int(time.time() - self.last_action)
            patience = ""
            if waited > 20:
                patience = P.render("patience_20")
            if waited > 40:
                patience = P.render("patience_40", remaining_time=60 - waited)

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

"""
agent-game: 游戏引擎模块

GuessNumberEngine — 猜数字，单 session 实例 + 自描述协议。
"""

import random
import time
from datetime import UTC, datetime

from agent_game_server.games.game_engine import GameEngine
from agent_game_server.games.state_machine import StateMachine
from agent_game_server.prompt import Prompt

_PROMPT_MODULE = "agent_game_server.assets.guess_number_prompts"

# 对局结束状态（won / lost）。manager._is_finished 依赖这两个值。
_FINISHED_STATES = ("won", "lost")


class _GuessNumberFSM(StateMachine):
    def allowed_transitions(self) -> dict[str, set[str]]:
        return {
            "in_progress": {"won", "lost"},
            "won": set(),
            "lost": set(),
        }

    def _on_after_transition(self, to: str):
        # transfer ended_at from engine — handled by engine itself
        pass


class GuessNumberEngine(GameEngine):
    """猜数字游戏引擎。每个实例管理一局游戏。"""

    game_type = "guess_number"
    game_name = "猜数字"
    player_mode = "single"

    _prompts: Prompt | None = None

    @classmethod
    def get_prompts(cls) -> Prompt:
        if cls._prompts is None:
            cls._prompts = Prompt(_PROMPT_MODULE)
        return cls._prompts

    def __init__(self, min_num: int = 1, max_num: int = 100, max_attempts: int = 10):
        if isinstance(min_num, bool) or not isinstance(min_num, int):
            raise ValueError(f"min_num 必须是整数，收到 {min_num!r}")
        if isinstance(max_num, bool) or not isinstance(max_num, int):
            raise ValueError(f"max_num 必须是整数，收到 {max_num!r}")
        if min_num >= max_num:
            raise ValueError(
                f"min_num 必须小于 max_num，收到 min_num={min_num}, max_num={max_num}"
            )
        if isinstance(max_attempts, bool) or not isinstance(max_attempts, int):
            raise ValueError(f"max_attempts 必须是整数，收到 {max_attempts!r}")
        if max_attempts < 1:
            raise ValueError(f"max_attempts 必须 >= 1，收到 {max_attempts}")

        self.min = min_num
        self.max = max_num
        self.max_attempts = max_attempts
        self._reset()

    # ── 开局 / 重置 ──

    def _reset(self):
        """重置成一局全新的游戏。config（min/max/max_attempts）不变。"""
        previous_target = getattr(self, "target", None)
        self._fsm = _GuessNumberFSM("in_progress")
        self.target = self._roll_target(previous_target)
        self.guesses: list[dict] = []
        self.guess_count = 0
        self.last_result: str | None = None
        self.created_at = datetime.now(UTC).isoformat()
        self.ended_at: str | None = None
        self.last_action = time.time()

    def _roll_target(self, previous: int | None) -> int:
        """摇一个新答案。范围里还有别的数时，保证换一个（不重复上一局的答案）。"""
        if previous is None or self.max <= self.min:
            return random.randint(self.min, self.max)
        while True:
            candidate = random.randint(self.min, self.max)
            if candidate != previous:
                return candidate

    @property
    def state(self) -> str:
        return self._fsm.state

    @property
    def phase(self) -> str:
        """兼容旧 API / GameEngine 协议 — 和 state 等价。"""
        return self._fsm.state

    @property
    def status(self) -> str:
        """兼容旧 API"""
        return self._fsm.state

    def is_in(self, *states: str) -> bool:
        return self._fsm.is_in(*states)

    # ── 自描述协议 ──

    @classmethod
    def describe_tools(cls) -> list[dict]:
        return [
            {
                "name": "guess_number_start",
                "description": "开始一局新的猜数字游戏（1-100）。Agent 调用此工具创建对局并开始猜测。",
                "input_schema": {"type": "object", "properties": {}, "required": []},
            },
            {
                "name": "guess_number_guess",
                "description": "提交一个 1-100 之间的猜测数字。返回 too_high/too_low/correct 和剩余次数。游戏未结束时返回值引导你继续猜。",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "number": {
                            "type": "integer",
                            "description": "猜测数字（1-100）",
                            "minimum": 1,
                            "maximum": 100,
                        },
                    },
                    "required": ["number"],
                },
            },
            {
                "name": "guess_number_state",
                "description": "查看当前对局状态，包括所有历史猜测。不需要参数。",
                "input_schema": {
                    "type": "object",
                    "properties": {},
                    "required": [],
                },
            },
        ]

    def get_public_state(self) -> dict:
        """Admin 可见的公开状态"""
        return {
            "status": self.status,
            "guess_count": self.guess_count,
            "max_attempts": self.max_attempts,
            "target": self.target,
            "guesses": self.guesses,
            "created_at": self.created_at,
            "ended_at": self.ended_at,
        }

    def get_player_state(self, player_index=None) -> dict:
        """Agent 视角状态 + prompt 文本"""
        public = {
            "status": self.status,
            "guess_count": self.guess_count,
            "max_attempts": self.max_attempts,
            "remaining": self.max_attempts - self.guess_count,
            "last_result": self.last_result,
            "guesses": self.guesses,
        }
        prompt = self._build_prompt()
        return {"public": public, "prompt": prompt}

    def handle_action(self, action: str, **params):
        """统一 action 入口"""
        if action == "start":
            # 无论当前是什么状态，start 都开一局新的
            self._reset()
            return self.get_player_state()
        elif action == "guess":
            self._guess(params["number"])
            return self.get_player_state()
        elif action == "state":
            return self.get_player_state()
        else:
            raise ValueError(f"未知 action: {action}")

    def check_timeout(self, now: float) -> dict | None:
        if self._fsm.is_in("in_progress") and now - self.last_action > 600:
            self._fsm.transition_to("lost")
            self.ended_at = datetime.now(UTC).isoformat()
            return {"type": "session_timeout", "expired": True}
        return None

    # ── 游戏逻辑 ──

    def _guess(self, number):
        if not self._fsm.is_in("in_progress"):
            raise ValueError(f"游戏已结束 ({self._fsm.state})")

        number = self._validate_number(number)

        if number < self.target:
            result = "too_low"
        elif number > self.target:
            result = "too_high"
        else:
            result = "correct"

        self.guess_count += 1
        self.last_result = result
        self.last_action = time.time()
        self.guesses.append(
            {
                "number": number,
                "result": result,
                "timestamp": datetime.now(UTC).isoformat(),
            }
        )

        if result == "correct":
            self._fsm.transition_to("won")
            self.ended_at = datetime.now(UTC).isoformat()
        elif self.guess_count >= self.max_attempts:
            self._fsm.transition_to("lost")
            self.ended_at = datetime.now(UTC).isoformat()

    def _validate_number(self, number) -> int:
        """把外部传进来的猜测值收敛成合法整数：必须在 [min, max] 内。

        非法输入一律 ValueError（HTTP 层对应 400，而不是 500）。
        bool 是 int 的子类，但 True/False 不算有意义的猜测，同样拒掉。
        """
        if isinstance(number, bool):
            raise ValueError(f"猜测值必须是整数，收到 {number!r}")
        if isinstance(number, float):
            if not number.is_integer():
                raise ValueError(f"猜测值必须是整数，收到 {number!r}")
            number = int(number)
        elif not isinstance(number, int):
            raise ValueError(
                f"猜测值必须是 {self.min}-{self.max} 的整数，收到 {type(number).__name__}: {number!r}"
            )
        if not self.min <= number <= self.max:
            raise ValueError(f"猜测值必须在 {self.min}-{self.max} 之间，收到 {number}")
        return number

    # ── Prompt 生成（模板渲染，修改 YAML 后调 reload() 热更新）──

    def _build_prompt(self) -> str:
        P = self.get_prompts()

        if self._fsm.is_in("won"):
            return P.render(
                "won", last_guess=self.guesses[-1]["number"], count=self.guess_count
            )

        if self._fsm.is_in("lost"):
            return P.render(
                "lost",
                max_attempts=self.max_attempts,
                target=self.target,
                history=[g["number"] for g in self.guesses],
            )

        last = self.guesses[-1] if self.guesses else None
        if last:
            hint = {"too_low": "太低 ↑", "too_high": "太高 ↓"}[last["result"]]
            return P.render(
                "in_progress",
                count=self.guess_count,
                last_number=last["number"],
                hint=hint,
                remaining=self.max_attempts - self.guess_count,
                min_num=self.min,
                max_num=self.max,
                history=[g["number"] for g in self.guesses],
            )
        return P.render(
            "in_progress_first",
            remaining=self.max_attempts,
            min_num=self.min,
            max_num=self.max,
        )

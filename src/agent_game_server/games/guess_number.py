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
        self.min = min_num
        self.max = max_num
        self.max_attempts = max_attempts
        self._fsm = _GuessNumberFSM("in_progress")
        self.target = random.randint(min_num, max_num)
        self.guesses: list[dict] = []
        self.guess_count = 0
        self.last_result: str | None = None
        self.created_at = datetime.now(UTC).isoformat()
        self.ended_at: str | None = None
        self.last_action = time.time()

    @property
    def state(self) -> str:
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

    def _guess(self, number: int):
        if not self._fsm.is_in("in_progress"):
            raise ValueError(f"游戏已结束 ({self._fsm.state})")

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

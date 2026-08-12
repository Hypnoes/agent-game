"""
StateMachine — 抽象状态机，GameEngine 的核心组件。

每个游戏创建自己的 StateMachine 子类，定义 allowed_transitions()。
子类可 override _on_before_transition / _on_after_transition 添加转移时的业务逻辑。
"""

from abc import ABC, abstractmethod


class StateMachine(ABC):
    """状态机抽象基类。子类只需实现 allowed_transitions()。

    钩子方法（可 override）：
        _on_before_transition(to)  — 转移前，用于校验或准备
        _on_after_transition(to)   — 转移后，用于更新引擎状态
    """

    def __init__(self, initial: str):
        self._current = initial
        self._previous: str | None = None

    # ── 属性 ──

    @property
    def state(self) -> str:
        return self._current

    @property
    def phase(self) -> str:
        """业务的语义别名，和 state 等价"""
        return self._current

    @property
    def previous(self) -> str | None:
        """上一个状态。初始为 None。"""
        return self._previous

    def is_in(self, *states: str) -> bool:
        return self._current in states

    # ── 子类必须实现 ──

    @abstractmethod
    def allowed_transitions(self) -> dict[str, set[str]]:
        """返回 {from: {to, ...}}。子类可动态计算（比如多人游戏人数不够不能开始）。"""
        ...

    # ── 默认转移实现 ──

    def can_transition_to(self, to: str) -> bool:
        return to in self.allowed_transitions().get(self._current, set())

    def transition_to(self, to: str):
        """默认转移：校验 → before 钩子 → 更新 → after 钩子。子类可 override 添加自己的逻辑。"""
        if not self.can_transition_to(to):
            allowed = self.allowed_transitions().get(self._current, set())
            raise ValueError(
                f"状态机不能从 '{self._current}' 转到 '{to}'。允许: {allowed}"
            )
        self._on_before_transition(to)
        self._previous = self._current
        self._current = to
        self._on_after_transition(to)

    # ── 钩子（子类按需 override）──

    def _on_before_transition(self, to: str):
        """转移前执行。子类可做额外校验、日志等。"""
        pass

    def _on_after_transition(self, to: str):
        """转移后执行。子类可更新辅助数据、发事件等。"""
        pass

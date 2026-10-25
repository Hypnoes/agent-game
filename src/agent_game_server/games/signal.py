"""
ChangeSignal — 轻量的状态变更信号，供引擎的阻塞式等待使用。

语义：
- subscribe() 每次新建 asyncio.Event 并登记。Event 在首次 await 时绑定当时的事件循环，
  复用同一个 Event 跨循环会静默失效，所以绝不复用。
- notify() 先整体换出等待者列表，再逐个 set()，避免遍历过程中新来的 subscribe() 漏唤醒。
- wait() 用 asyncio.wait_for 等待，超时映射为 "timeout"；CancelledError 不吞，交给调用方处理。

wait() 允许接收调用方事先 subscribe() 得到的事件（ev 参数）。调用方按"先订阅、再检查状态、
再等待"的顺序使用，notify() 有可能恰好落在检查与进入等待之间：那个事件已被 set、也已从
登记表里换出。若 wait() 此时另起一个新事件，就会丢掉这次唤醒、白等满一整个预算。传入原
事件后 wait() 先看它是否已置位，已置位立刻返回 "changed"。事件由调用方负责 unsubscribe()。
"""

import asyncio


class ChangeSignal:
    """状态变更信号。等待者阻塞在 wait() 上，直到 notify() 或超时。"""

    def __init__(self):
        self._waiters: list[asyncio.Event] = []

    @property
    def waiter_count(self) -> int:
        """当前登记在册的等待者数量（测试与诊断用）。"""
        return len(self._waiters)

    def subscribe(self) -> asyncio.Event:
        """新建并登记一个事件，返回它。"""
        ev = asyncio.Event()
        self._waiters.append(ev)
        return ev

    def unsubscribe(self, ev: asyncio.Event) -> None:
        """移除登记。重复移除是安全的。"""
        self._waiters = [w for w in self._waiters if w is not ev]

    def notify(self) -> None:
        """唤醒全部等待者。先整体换出列表，再逐个 set()。"""
        waiters, self._waiters = self._waiters, []
        for ev in waiters:
            ev.set()

    async def wait(self, timeout: float, ev: asyncio.Event | None = None) -> str:
        """等待一次变更。返回 "changed" 或 "timeout"。被取消时向上抛出 CancelledError。

        ev 省略时内部自行订阅并在结束时移除；传入时由调用方负责 unsubscribe()。
        """
        owns_event = ev is None
        if ev is None:
            ev = self.subscribe()

        try:
            if ev.is_set():
                # 订阅之后、进入等待之前已经发生过 notify()，这次唤醒不能丢
                return "changed"
            await asyncio.wait_for(ev.wait(), timeout)
        except TimeoutError:
            return "timeout"
        finally:
            if owns_event:
                self.unsubscribe(ev)
        return "changed"

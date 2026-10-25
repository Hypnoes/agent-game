"""
服务端的共享常量与共享解析器。

单独成模块是为了避免 game_server 与 mcp_server 互相 import：两边都要用等待
预算与版本号，放在任一方的模块里都会形成循环导入。
"""

import math
from typing import Any

__version__ = "0.2.2"

# 单次 state 调用的最长挂起时间（秒）。低于 MCP 消费端默认的 60 秒工具调用
# 超时，保证超时总是由本服务先发生，Agent 收到的是状态文本而非传输层错误。
# 引擎侧另有 turn_timeout（判负时限），两者是不同的数值。
BLOCK_BUDGET = 55.0

# MCP 连接级会话缓存最多追踪多少条连接。键是 mcp-session-id，一次性连接的键
# 不会自己消失：SDK 的 session_idle_timeout 只回收 SDK 自己的会话，不会通知
# 这份缓存。按插入顺序做 FIFO 淘汰，避免长跑进程里无限增长。
MAX_MCP_SESSION_CACHE = 512


def parse_block_budget(raw: Any = None, default: float | None = None) -> float:
    """把调用方传入的 timeout 解析成一次等待的预算（秒）。

    契约：
    - 省略或 None → default；default 省略时取当前的 BLOCK_BUDGET（55）。
    - 数值 → 截断到 [0, BLOCK_BUDGET]。0 表示"本次不等待"。
    - 非数值（str、bool、NaN、inf）→ ValueError。

    两个传输必须共用这一个解析器。它们曾经语义相反：MCP 侧的
    `float(args.pop("timeout", None) or BLOCK_BUDGET)` 让 timeout=0 变成 55 秒，
    而 REST 侧的 `min(float(...), BLOCK_BUDGET)` 让 0 就是 0。

    default 用 None 作哨兵而不是直接写 `default=BLOCK_BUDGET`：函数默认值在
    import 时求值一次，之后再改 BLOCK_BUDGET 就不生效了，测试与运维都没法临时
    调小预算。这里在调用时读模块全局。

    bool 单列出来拒绝：Python 里 bool 是 int 的子类，不拒绝的话 timeout=true 会
    静默变成 1 秒。NaN 也拒绝：`nan > 0` 为假，会让等待被静默跳过而不是报错。
    """
    budget = BLOCK_BUDGET if default is None else default
    if raw is not None:
        budget = _to_finite(raw, "timeout")
    return max(0.0, min(budget, BLOCK_BUDGET))


def _to_finite(value: Any, field: str) -> float:
    """把单个值转成有限的 float，否则 ValueError。"""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{field} 必须是数字，收到 {value!r}")
    seconds = float(value)
    if not math.isfinite(seconds):
        raise ValueError(f"{field} 必须是有限数字，收到 {value!r}")
    return seconds

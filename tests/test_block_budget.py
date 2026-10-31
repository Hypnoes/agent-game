"""`config.parse_block_budget` 的契约测试。

这个解析器是 REST 与 MCP **共用**的唯一实现（AGENTS.md #7）。它曾经被两条传输
各写一遍，语义正好相反：MCP 侧的 `float(args.pop("timeout", None) or BLOCK_BUDGET)`
让 `timeout=0` 变成 55 秒，REST 侧的 `min(float(...), BLOCK_BUDGET)` 让 0 就是 0。
同一个客户端在两条通道上等到的时长不一样。

因此这里只认这一份实现，并且断言它**在调用时**读 `config.BLOCK_BUDGET`，
而不是在 import 时固化进函数默认参数。
"""

import math

import pytest

from agent_game_server import config
from agent_game_server.config import BLOCK_BUDGET, parse_block_budget

# ── 数值契约 ──


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        (None, BLOCK_BUDGET),  # 省略用满预算
        (0, 0.0),  # 0 表示本次不等待
        (-5, 0.0),  # 负数夹到 0
        (999, BLOCK_BUDGET),  # 超上限夹到预算
        (10, 10.0),
        (3.7, 3.7),
    ],
)
def test_numeric_contract(raw, expected):
    assert parse_block_budget(raw) == expected


def test_missing_argument_uses_full_budget():
    assert parse_block_budget() == BLOCK_BUDGET


# ── 拒绝非数值 ──


@pytest.mark.parametrize(
    "raw",
    ["abc", "55", True, False, math.nan, math.inf, -math.inf, [1], {"a": 1}],
    ids=["str", "numeric-str", "true", "false", "nan", "inf", "-inf", "list", "dict"],
)
def test_rejects_non_numeric(raw):
    """非数值一律报错，不静默降级成默认预算。

    `bool` 单列：Python 里 bool 是 int 的子类，不拒绝的话 `timeout=true` 会
    静默变成 1 秒。`NaN` 单列：`nan > 0` 为假，会让等待被静默跳过而不是报错。
    """
    with pytest.raises(ValueError):
        parse_block_budget(raw)


# ── 调用时读取预算，而不是 import 时固化 ──


def test_reads_budget_at_call_time(monkeypatch):
    """默认值必须在调用时读 `config.BLOCK_BUDGET`。

    写成 `def parse_block_budget(raw, default=BLOCK_BUDGET)` 会在 import 时求值一次，
    之后再改模块全局就不生效——测试没法把预算调小，运维也没法临时调紧。
    """
    monkeypatch.setattr(config, "BLOCK_BUDGET", 3.0)

    assert parse_block_budget(None) == 3.0
    assert parse_block_budget() == 3.0
    assert parse_block_budget(999) == 3.0


def test_explicit_default_overrides_budget_but_not_the_cap(monkeypatch):
    """`default` 只回答"没给参数时用多少秒"，不改变上限。

    把上限也一起降下来会让 default 较小的调用方意外得到比预期更短的预算。
    """
    monkeypatch.setattr(config, "BLOCK_BUDGET", 2.0, raising=True)

    assert parse_block_budget(None, default=1.0) == 1.0
    assert parse_block_budget(0, default=1.0) == 0.0
    # 显式 99 被夹到当前预算 2.0，而不是 default 1.0
    assert parse_block_budget(99, default=1.0) == 2.0


# ── 两条传输共用这一份实现 ──


def test_both_transports_call_the_shared_parser():
    """REST 与 MCP 都必须经由 `config.parse_block_budget`。

    用源码扫描而不是行为测试：两份实现即使当前行为一致，下一个改的人也可能
    只改一边。扫描能保证"只有一个入口"这个结构事实。
    """
    from pathlib import Path

    src = Path(__file__).parent.parent / "src" / "agent_game_server"
    for module in ("game_server.py", "mcp_server.py"):
        text = (src / module).read_text(encoding="utf-8")
        assert "parse_block_budget" in text, f"{module} 没有使用共享解析器"
        # 不允许自己再写一份 float()/min() 的临时代码
        assert "float(args" not in text, f"{module} 自己解析了 timeout"


# ── AGENTS.md #2：消费端超时必须大于服务端预算 ──


def test_budget_leaves_room_for_the_client_timeout():
    """服务端预算必须留出余量给传输层，否则客户端先超时。

    README 要求 MCP 客户端配置 `toolCallTimeoutMs: 70000`（70 秒），而预算上限
    是 55 秒。留 15 秒给协议往返与调度，服务端才能总是先返回——这样 Agent 看到的
    是状态文本，不是一个裸的超时错误。

    把 BLOCK_BUDGET 调大到 65 秒以上，这条会红：那意味着客户端会在服务端之前超时。
    """
    client_timeout_ms = 70_000  # README 与 DSH 配置里写的值

    assert client_timeout_ms / 1000 > BLOCK_BUDGET, (
        f"服务端预算 {BLOCK_BUDGET}s 必须小于客户端超时 {client_timeout_ms / 1000}s，"
        "否则客户端会在服务端返回之前先超时"
    )
    # 至少留 10 秒余量给协议往返
    assert client_timeout_ms / 1000 - BLOCK_BUDGET >= 10


def test_budget_is_positive():
    """预算为 0 会让所有 state 调用立即返回，等于关掉阻塞等待。"""
    assert BLOCK_BUDGET > 0

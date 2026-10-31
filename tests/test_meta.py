"""元测试：守住测试套件自身的结构与契约。

这些用例不测业务行为，测的是"测试套件会不会静默腐化"：

- 一个测试函数被复制粘贴定义两次，后者覆盖前者，前者永远不执行（真实发生过）。
- 版本号在两处手写，漂移了没人发现。
- 关键能力被删掉时，验收用例靠 `skip` 让路（真实发生过）。
- 覆盖率统计不到的文件悄悄长出来。
"""

import ast
import re
from collections import Counter
from pathlib import Path

import pytest

TESTS_DIR = Path(__file__).parent
SRC_DIR = TESTS_DIR.parent / "src" / "agent_game_server"


def _test_functions(path: Path) -> list[str]:
    """文件里所有 test 函数的定义名（按出现顺序，保留重复）。"""
    tree = ast.parse(path.read_text(encoding="utf-8"))
    names = []
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith(
            "test_"
        ):
            names.append(node.name)
    return names


# ═══════════════════════════════════════════
# 1. 测试函数名必须唯一
# ═══════════════════════════════════════════


@pytest.mark.parametrize("path", sorted(TESTS_DIR.glob("test_*.py")), ids=lambda p: p.name)
def test_no_duplicate_test_functions(path):
    """同一个文件里不能有两个同名 test 函数。

    Python 的后定义会覆盖前定义，第一个从此永远不执行，而且**没有任何信号**：
    pytest 不报错，覆盖率可能还更高（重复的代码行被算到了）。这不是假设——
    `test_manager.py` 的 `test_finished_game_reclaimed_after_retention` 曾经
    定义两次，名义 33 个用例、实际只收集到 32 个。
    """
    names = _test_functions(path)
    duplicates = {name: count for name, count in Counter(names).items() if count > 1}
    assert not duplicates, f"{path.name} 有重复定义的 test 函数（后者会静默覆盖前者）: {duplicates}"


# ═══════════════════════════════════════════
# 2. 不允许用 skip 给"能力缺失"让路
# ═══════════════════════════════════════════


@pytest.mark.parametrize("path", sorted(TESTS_DIR.glob("test_*.py")), ids=lambda p: p.name)
def test_no_conditional_skip_on_core_capabilities(path):
    """核心能力缺失时套件必须红，不能 skip。

    `pytest.skip` 让"功能被删掉"和"功能正常"在退出码上一样。曾经守住阻塞等待
    的两条主验收用例就是这样写的，于是整个 `wait_for_change` 被删掉之后，
    它们变成 skipped，套件仍然是绿的。
    """
    source = path.read_text(encoding="utf-8")
    offenders = []
    for node in ast.walk(ast.parse(source)):
        # 只看真实调用，不看字符串与注释：本文件自己就在文档里提到 pytest.skip
        if not isinstance(node, ast.Call):
            continue
        func = node.func
        if not (isinstance(func, ast.Attribute) and func.attr == "skip"):
            continue
        if isinstance(func.value, ast.Name) and func.value.id != "pytest":
            continue
        # 允许显式标注为"平台/依赖不可用"的跳过，但不允许探测自己实现的能力
        argtext = ast.unparse(node)
        if "platform" in argtext or "importorskip" in argtext:
            continue
        offenders.append(f"{path.name}:{node.lineno}: {argtext}")

    assert not offenders, (
        "这些 skip 会在能力缺失时把失败变成通过，请改成硬断言:\n" + "\n".join(offenders)
    )


# ═══════════════════════════════════════════
# 3. 版本号单一来源
# ═══════════════════════════════════════════


def test_version_matches_pyproject():
    """`config.__version__` 与 `pyproject.toml` 必须一致。

    两处手写版本号一定会漂移：改了一处忘了另一处，`/mcp` 握手报的版本与实际
    包版本不符，排查时先被这个假线索带偏。
    """
    from agent_game_server.config import __version__

    pyproject = (TESTS_DIR.parent / "pyproject.toml").read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', pyproject, re.MULTILINE)
    assert match, "pyproject.toml 里找不到 version"
    assert match.group(1) == __version__, (
        f"版本号漂移: pyproject={match.group(1)}, config={__version__}"
    )


# ═══════════════════════════════════════════
# 4. 协议里的可选能力不能被运行时桩污染
# ═══════════════════════════════════════════


def test_protocol_optional_attributes_are_not_runtime_stubs():
    """协议声明的可选类属性在运行时不能存在。

    一旦它们变成运行时的真属性，`getattr` 探测就永远命中，所有引擎都被当成
    "声明了自己的终态名/秘密键"，默认值形同虚设。
    """
    from agent_game_server.games.game_engine import GameEngine

    for name in ("finished_states", "secret_keys"):
        assert name not in GameEngine.__dict__, f"协议不该在运行时定义 {name}"


def test_defaults_apply_to_engines_that_declare_nothing():
    """没声明可选属性的引擎拿到默认值，不会漏判终态或漏剥秘密。"""
    from agent_game_server.games.game_engine import (
        DEFAULT_FINISHED_STATES,
        DEFAULT_SECRET_KEYS,
        finished_states_of,
        secret_keys_of,
    )

    class Bare:
        game_type = "bare"

    assert finished_states_of(Bare()) == DEFAULT_FINISHED_STATES
    assert secret_keys_of(Bare()) == DEFAULT_SECRET_KEYS


# ═══════════════════════════════════════════
# 5. 每条 AGENTS.md 硬约束都要有对应的看门用例
# ═══════════════════════════════════════════


GUARDED_INVARIANTS = {
    "#1 /mcp 用 Route 而不是 Mount": "test_mcp_server.py",
    "#2 消费端超时大于服务端预算": "test_block_budget.py",
    "#3 身份来自 player_token": "test_mcp_identity.py",
    "#4 bid/challenge/state 必填 token": "test_invariants.py",
    "#5 校验先于等待（两条传输）": "test_contract.py",
    "#6 wait_for_change 循环重判": "test_wait.py",
    "#7 parse_block_budget 唯一实现": "test_block_budget.py",
    "#8 剥离秘密键": "test_http_contract.py",
    "#9 超时与开盅对输掉本轮一致": "test_invariants.py",
    "#10 check_timeouts 按引擎隔离": "test_poller.py",
    "#11 admin token 永远生效": "test_http_contract.py",
    "#12 身份校验与传输无关": "test_contract.py",
    "#13 不准用 skip 让路": "test_meta.py",
    "#14 每条约束都有看门用例": "test_meta.py",
}


@pytest.mark.parametrize("invariant,filename", sorted(GUARDED_INVARIANTS.items()))
def test_each_hard_constraint_has_a_guard_test(invariant, filename):
    """AGENTS.md 的每条硬约束都要有文件在守。

    这是"不变量住在文档里、不进测试"的反面：约束写在 AGENTS.md，但如果没有任何
    用例对应它，它实际上只是一段注释。
    """
    path = TESTS_DIR / filename
    assert path.exists(), f"{invariant} 指定的看门文件 {filename} 不存在"
    assert _test_functions(path), f"{filename} 里没有任何 test 函数"

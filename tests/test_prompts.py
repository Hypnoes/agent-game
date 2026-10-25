"""提示词模板测试 — 时间数值必须由调用方传入，模板里一个具体秒数都不能写死。

背景（这两个数值不是一回事）：
- `config.BLOCK_BUDGET`（55 秒）：一次 `state` 调用在服务端最多挂起多久，到点返回文本。
- 引擎的 `self.turn_timeout`（60 秒）：当前回合多久不行动判负。

`not_your_turn` 同时被两种路径渲染：真的挂满预算的 `liars_dice_state`，和
`GET /games/{sid}/state/{idx}` 这种立刻返回的纯视图。后者一秒都没等，
所以模板绝不能声称"已等待 N 秒"。
"""

import time

import pytest

from agent_game_server import config
from agent_game_server.games.liars_dice import LiarsDiceEngine
from agent_game_server.prompt import Prompt

PROMPT_MODULE = "agent_game_server.assets.liars_dice_prompts"

# 整个渲染结果里都不该出现的降级标记
BAD_MARKERS = ("{missing:", "{unrendered:", "[unrendered]")


@pytest.fixture
def prompts() -> Prompt:
    return Prompt(PROMPT_MODULE)


def started(player_count: int = 3, dice_per_player: int = 5) -> LiarsDiceEngine:
    """开局后的引擎（所有玩家都已入场）。"""
    e = LiarsDiceEngine(player_count=player_count, dice_per_player=dice_per_player)
    for i in range(player_count):
        e.handle_action("join", player_name=f"P{i}")
    assert e.phase == "bidding"
    return e


def not_current(e: LiarsDiceEngine) -> int:
    """挑一个不是当前回合的玩家（必然存在：多人局）。"""
    idx = (e.current_turn + 1) % len(e.players)
    assert idx != e.current_turn
    return idx


def prompt_of(e: LiarsDiceEngine, player_index: int) -> str:
    return e.get_player_state(player_index)["prompt"]


def assert_rendered(text: str) -> None:
    """渲染成功：无 missing/unrendered 标记，无残留花括号。"""
    for marker in BAD_MARKERS:
        assert marker not in text, f"渲染降级标记 {marker!r} 泄漏到输出: {text!r}"
    assert "{" not in text, f"渲染后仍有原始占位符: {text!r}"
    assert "}" not in text, f"渲染后仍有原始占位符: {text!r}"


# ═══════════════════════════════════════════
# 模板本身：不写死任何秒数
# ═══════════════════════════════════════════


def test_templates_hardcode_no_seconds(prompts: Prompt):
    """模板里不能出现 55 / 60 —— 这些数值必须由调用方传入。"""
    for name in prompts.names():
        text = prompts.template(name)
        assert "55" not in text, f"模板 {name} 写死了等待预算 55: {text!r}"
        assert "60" not in text, f"模板 {name} 写死了回合时限 60: {text!r}"


def test_not_your_turn_placeholders(prompts: Prompt):
    """not_your_turn 必须声明 elapsed 与 wait_budget 两个占位符。"""
    fields = prompts.placeholders("not_your_turn")
    assert "elapsed" in fields
    assert "wait_budget" in fields


def test_patience_templates_declare_the_right_clock(prompts: Prompt):
    """patience_20 用 turn_timeout；patience_40 用 remaining_time（回合倒计时）。"""
    assert prompts.placeholders("patience_20") == {"turn_timeout"}
    assert prompts.placeholders("patience_40") == {"remaining_time"}
    assert "wait_budget" not in prompts.placeholders("patience_40")


# ═══════════════════════════════════════════
# not_your_turn：0 秒与 50 秒都要说真话
# ═══════════════════════════════════════════


def _ny_turn(prompts: Prompt, elapsed: int, wait_budget: float = 55.0) -> str:
    return prompts.render(
        "not_your_turn",
        strict=True,
        current_player="B",
        progress="刚刚" if elapsed < 10 else f"已思考 {elapsed} 秒",
        patience="",
        call_str="3×4",
        dice=[1, 2, 3],
        player_index=1,
        elapsed=elapsed,
        wait_budget=wait_budget,
    )


def test_not_your_turn_zero_elapsed_reports_zero(prompts: Prompt):
    """立即返回的路径：elapsed=0 时不能声称等了几十秒。"""
    text = _ny_turn(prompts, elapsed=0)
    assert_rendered(text)
    assert "对手的回合已经进行了 0 秒" in text
    # 旧模板的谎言：任何形态的"已等待 N 秒"都不允许再出现
    assert "已等待" not in text
    assert "已等待 55 秒" not in text


def test_not_your_turn_fifty_elapsed_reports_fifty(prompts: Prompt):
    """真的等了 50 秒：必须如实报出 50。"""
    text = _ny_turn(prompts, elapsed=50)
    assert_rendered(text)
    assert "对手的回合已经进行了 50 秒" in text
    assert "对手的回合已经进行了 0 秒" not in text
    assert "已等待" not in text


def test_not_your_turn_uses_passed_budget_not_literal_55(prompts: Prompt):
    """wait_budget=7 时输出 7，且不再出现 55。"""
    text = _ny_turn(prompts, elapsed=0, wait_budget=7)
    assert_rendered(text)
    assert "7" in text
    assert "55" not in text
    assert "最多挂起 7 秒" in text


def test_waiting_uses_passed_budget_not_literal_55(prompts: Prompt):
    """waiting 模板的挂起时长同样来自入参。"""
    text = prompts.render(
        "waiting", strict=True, players=1, needed=2, waited=3, wait_budget=7
    )
    assert_rendered(text)
    assert "最多阻塞 7 秒" in text
    assert "55" not in text


def test_not_your_turn_does_not_claim_caller_was_blocked(prompts: Prompt):
    """措辞要求：不能说"你被阻塞了 N 秒"——视图层不知道某次调用等了多久。

    只允许描述两件确定的事：对手回合进行了多久、state 调用最多挂起多久。
    """
    text = _ny_turn(prompts, elapsed=0)
    assert "你已等待" not in text
    assert "你已阻塞" not in text
    assert "已阻塞" not in text
    # 但必须说明 state 调用会阻塞，且阻塞上限就是传入的预算
    assert "最多挂起" in text and "55" in text


def test_patience_20_uses_turn_timeout(prompts: Prompt):
    """patience_20 的秒数是回合时限，与等待预算无关。"""
    text = prompts.render("patience_20", strict=True, turn_timeout=60)
    assert_rendered(text)
    assert "60" in text
    assert "回合上限 60 秒" in text


def test_patience_40_uses_turn_countdown_not_budget(prompts: Prompt):
    """patience_40 渲染的是回合倒计时 remaining_time，不是等待预算。"""
    text = prompts.render("patience_40", strict=True, remaining_time=15)
    assert_rendered(text)
    assert "15" in text
    # 它不需要 wait_budget；给了也不用
    with_budget = prompts.render(
        "patience_40", strict=True, remaining_time=15, wait_budget=999
    )
    assert "999" not in with_budget

    # 缺 remaining_time 时 strict 必须报错——证明它靠回合倒计时渲染
    with pytest.raises(KeyError):
        prompts.render("patience_40", strict=True, wait_budget=55)


# ═══════════════════════════════════════════
# 全模板渲染健康度
# ═══════════════════════════════════════════


FULL_CONTEXT = {
    "finished": {
        "winner": "Alice",
        "all_dice": [[1, 2, 3], [4, 5, 6]],
        "call_history": [(3, 4), (4, 4)],
    },
    "waiting": {"players": 1, "needed": 2, "waited": 3, "wait_budget": 55.0},
    "not_your_turn": {
        "current_player": "Bob",
        "progress": "已思考 12 秒",
        "patience": "",
        "call_str": "3×4",
        "dice": [1, 2, 3],
        "player_index": 1,
        "elapsed": 12,
        "wait_budget": 55.0,
    },
    "your_turn": {
        "dice": [1, 2, 3],
        "call_str": "3×4（Bob 叫的）",
        "msg_line": "\n💬 上家说: '你信吗'",
        "remaining": 42,
        "player_index": 1,
    },
    "patience_20": {"turn_timeout": 60},
    "patience_40": {"remaining_time": 15},
}


def test_full_context_covers_every_template(prompts: Prompt):
    """每个模板都有完整上下文，且没有模板被漏测。"""
    assert set(FULL_CONTEXT) == set(prompts.names())


def test_every_template_renders_clean(prompts: Prompt):
    """所有模板在字段齐全时都要渲染干净：无 missing、无原始花括号、无 unrendered。"""
    for name, ctx in FULL_CONTEXT.items():
        missing = prompts.missing_keys(name, **ctx)
        assert not missing, f"模板 {name} 的测试上下文缺字段: {sorted(missing)}"
        text = prompts.render(name, strict=True, **ctx)
        assert text, f"模板 {name} 渲染为空"
        assert_rendered(text)


# ═══════════════════════════════════════════
# 引擎接线：真实数值必须流进模板
# ═══════════════════════════════════════════


def test_immediate_get_player_state_claims_no_specific_wait():
    """立即返回的 get_player_state 不能声称自己等了多久。"""
    e = started(player_count=3)
    other = not_current(e)
    e.last_action = time.time()  # 对手刚行动完，elapsed≈0

    prompt = prompt_of(e, other)

    # 旧缺陷的原文：第一行说"刚刚"，下面却说"已等待 55 秒"
    assert "刚刚" in prompt
    assert "已等待" not in prompt
    assert "已等待 55 秒" not in prompt
    # 只允许描述对手回合的进度
    assert "对手的回合已经进行了 0 秒" in prompt


def test_engine_prompt_reflects_elapsed_turn_time():
    """elapsed 来自 last_action：50 秒前行动过就报 50。"""
    e = started(player_count=2)
    other = not_current(e)
    # -50.5：落在 50.x 秒区间，int() 截断后稳定为 50，不会被调度抖动推成 51
    e.last_action = time.time() - 50.5

    prompt = prompt_of(e, other)

    assert "对手的回合已经进行了 50 秒" in prompt
    assert "已等待" not in prompt
    # 超过 40 秒会附加回合倒计时的安慰语（remaining = 60 - 50 = 10）
    assert "只剩 10 秒" in prompt


def test_engine_reads_live_block_budget(monkeypatch):
    """模型里的预算是运行时读的 config.BLOCK_BUDGET，不是 import 时的固化副本。"""
    monkeypatch.setattr(config, "BLOCK_BUDGET", 7.0)
    e = started(player_count=2)
    other = not_current(e)
    e.last_action = time.time()

    prompt = prompt_of(e, other)

    assert "最多挂起 7 秒" in prompt
    assert "55" not in prompt


def test_engine_waiting_prompt_reads_live_block_budget(monkeypatch):
    """waiting 阶段同样读实时的 BLOCK_BUDGET。"""
    monkeypatch.setattr(config, "BLOCK_BUDGET", 7.0)
    e = LiarsDiceEngine()
    e.handle_action("join", player_name="Solo")

    prompt = e.get_player_state(0)["prompt"]

    assert "最多阻塞 7 秒" in prompt
    assert "55" not in prompt


def test_turn_timeout_does_not_leak_into_wait_budget_wording(monkeypatch):
    """回合时限与等待预算是两个数：改 turn_timeout 不该改"最多挂起"的数字。"""
    e = started(player_count=2)
    e.turn_timeout = 90
    other = not_current(e)
    e.last_action = time.time()

    prompt = prompt_of(e, other)

    assert "最多挂起 55 秒" in prompt
    assert "最多挂起 90 秒" not in prompt


def test_three_player_non_current_prompt_is_honest(monkeypatch):
    """集成：3 人局里非当前玩家的 prompt 既不谎报等待，也不残留占位符。"""
    monkeypatch.setattr(config, "BLOCK_BUDGET", 7.0)
    e = started(player_count=3)
    other = not_current(e)
    e.last_action = time.time() - 5  # 对手思考了 5 秒

    prompt = prompt_of(e, other)

    assert_rendered(prompt)
    assert "对手的回合已经进行了 5 秒" in prompt
    assert "最多挂起 7 秒" in prompt
    assert "55" not in prompt
    assert "已等待" not in prompt

    # 当前玩家的 prompt 走 your_turn 分支，仍然自洽
    mine = prompt_of(e, e.current_turn)
    assert_rendered(mine)
    assert "轮到你了" in mine

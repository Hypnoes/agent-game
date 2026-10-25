"""GuessNumberEngine 测试（单 session 实例 + 自描述协议）"""

import time

import pytest

from agent_game_server import GuessNumberEngine


def test_describe_tools():
    tools = GuessNumberEngine.describe_tools()
    assert len(tools) == 3
    names = [t["name"] for t in tools]
    assert "guess_number_start" in names
    assert "guess_number_guess" in names
    assert "guess_number_state" in names


def test_initial_state():
    e = GuessNumberEngine()
    state = e.get_player_state()
    assert state["public"]["status"] == "in_progress"
    assert state["public"]["guess_count"] == 0
    assert "prompt" in state
    assert "游戏进行中" in state["prompt"]


def test_guess_flow():
    e = GuessNumberEngine()
    # Force target for deterministic test
    e.target = 50

    result = e.handle_action("guess", number=30)
    assert e.guess_count == 1
    assert e.last_result == "too_low"
    assert "太低" in result["prompt"]

    result = e.handle_action("guess", number=60)
    assert e.guess_count == 2
    assert e.last_result == "too_high"

    result = e.handle_action("guess", number=50)
    assert e.status == "won"
    assert e.ended_at is not None
    assert "猜对了" in result["prompt"]


def test_max_attempts():
    e = GuessNumberEngine(max_attempts=3)
    e.target = 50
    for _ in range(3):
        e.handle_action("guess", number=1)  # always too_low
    assert e.status == "lost"
    assert e.ended_at is not None


def test_public_state():
    e = GuessNumberEngine()
    pub = e.get_public_state()
    assert pub["status"] == "in_progress"
    assert "target" in pub  # Admin can see target


def test_player_state_hides_target():
    e = GuessNumberEngine()
    state = e.get_player_state()
    assert "target" not in state["public"]  # Agent can't see target


def test_check_timeout():
    e = GuessNumberEngine()
    # Not timed out yet
    assert e.check_timeout(time.time()) is None
    # Force timeout
    e.last_action = time.time() - 601
    result = e.check_timeout(time.time())
    assert result is not None
    assert result["expired"] is True
    assert e.status == "lost"


# ── start 必须真的重开一局（缺陷 6）──


def test_start_resets_finished_game():
    e = GuessNumberEngine()
    e.target = 7
    e.handle_action("guess", number=1)  # too_low
    e.handle_action("guess", number=7)  # won
    assert e.status == "won"
    old_target = e.target
    e.created_at = "2020-01-01T00:00:00+00:00"  # 哨兵：便于判断是否刷新

    state = e.handle_action("start")

    assert e.status == "in_progress"
    assert e.guess_count == 0
    assert e.guesses == []
    assert e.last_result is None
    assert e.ended_at is None
    assert e.target != old_target  # 换了新答案
    assert 1 <= e.target <= 100
    assert e.created_at != "2020-01-01T00:00:00+00:00"  # created_at 已刷新
    # 返回形状不变
    assert set(state) == {"public", "prompt"}
    assert state["public"]["guess_count"] == 0
    assert state["public"]["status"] == "in_progress"
    assert "游戏进行中" in state["prompt"]


def test_start_resets_in_progress_game():
    """进行中的对局调用 start 也要重开，不能只是重渲染旧 prompt。"""
    e = GuessNumberEngine()
    e.target = 50
    e.handle_action("guess", number=20)
    e.handle_action("guess", number=30)
    assert e.guess_count == 2
    assert "第 2 次" in e.handle_action("state")["prompt"]

    state = e.handle_action("start")

    assert e.guess_count == 0
    assert e.guesses == []
    assert e.last_result is None
    assert "第 2 次" not in state["prompt"]
    assert "20" not in state["prompt"]


def test_start_on_fresh_engine_is_consistent():
    """新引擎上 start 依然给出合法的全新一局。"""
    e = GuessNumberEngine(min_num=5, max_num=8)
    state = e.handle_action("start")
    assert e.status == "in_progress"
    assert e.guess_count == 0
    assert 5 <= e.target <= 8
    assert e.min == 5 and e.max == 8  # 配置不被重置冲掉
    assert state["public"]["status"] == "in_progress"


def test_start_after_lost_resets():
    e = GuessNumberEngine(max_attempts=1)
    e.target = 50
    e.handle_action("guess", number=1)
    assert e.status == "lost"

    e.handle_action("start")

    assert e.status == "in_progress"
    assert e.ended_at is None
    assert e.guess_count == 0


def test_start_refreshes_last_action():
    """start 之后不应因为上一局的 last_action 太古早而被判超时。"""
    e = GuessNumberEngine()
    e.last_action = time.time() - 900
    e.handle_action("start")
    assert e.check_timeout(time.time()) is None


# ── 猜测值校验（缺陷 7）──


@pytest.mark.parametrize("bad", [0, -1, -5, 99999, 101, 1.5, "abc", None, [], True])
def test_guess_rejects_invalid_values(bad):
    e = GuessNumberEngine()
    with pytest.raises(ValueError):
        e.handle_action("guess", number=bad)
    # 非法输入不能污染对局状态
    assert e.guess_count == 0
    assert e.guesses == []
    assert e.status == "in_progress"


def test_guess_accepts_boundaries():
    e = GuessNumberEngine(min_num=1, max_num=100)
    e.target = 100
    e.handle_action("guess", number=1)
    assert e.guess_count == 1
    e.handle_action("guess", number=100)
    assert e.status == "won"


def test_guess_accepts_integral_float():
    """JSON 里的 42.0 是合法的整数猜测，收敛成 int 处理。"""
    e = GuessNumberEngine()
    e.target = 42
    e.handle_action("guess", number=42.0)
    assert e.status == "won"
    assert e.guesses[0]["number"] == 42
    assert isinstance(e.guesses[0]["number"], int)


def test_guess_respects_custom_range():
    e = GuessNumberEngine(min_num=10, max_num=20)
    with pytest.raises(ValueError):
        e.handle_action("guess", number=9)
    with pytest.raises(ValueError):
        e.handle_action("guess", number=21)
    e.last_result = None
    e.handle_action("guess", number=10)
    assert e.guess_count == 1


# ── 构造参数校验（缺陷 7）──


def test_invalid_range_rejected():
    with pytest.raises(ValueError):
        GuessNumberEngine(min_num=10, max_num=10)
    with pytest.raises(ValueError):
        GuessNumberEngine(min_num=20, max_num=10)


def test_invalid_max_attempts_rejected():
    with pytest.raises(ValueError):
        GuessNumberEngine(max_attempts=0)
    with pytest.raises(ValueError):
        GuessNumberEngine(max_attempts=-1)


def test_valid_custom_config():
    e = GuessNumberEngine(min_num=10, max_num=20, max_attempts=3)
    assert e.min == 10
    assert e.max == 20
    assert e.max_attempts == 3
    assert 10 <= e.target <= 20


# ── 超时 / ended_at 一致性（缺陷 8）──


def test_timeout_sets_lost_and_ended_at_once():
    e = GuessNumberEngine()
    e.last_action = time.time() - 601

    result = e.check_timeout(time.time())
    assert result == {"type": "session_timeout", "expired": True}
    assert e.status == "lost"
    assert e.is_in("lost")
    assert e.ended_at is not None
    first_ended_at = e.ended_at

    # 再查一次：已经在结束态，不再返回事件，也不改 ended_at
    assert e.check_timeout(time.time()) is None
    assert e.ended_at == first_ended_at


def test_timeout_does_not_fire_when_already_finished():
    e = GuessNumberEngine()
    e.target = 50
    e.handle_action("guess", number=50)
    assert e.status == "won"
    ended_at = e.ended_at
    e.last_action = time.time() - 601

    assert e.check_timeout(time.time()) is None
    assert e.status == "won"
    assert e.ended_at == ended_at


def test_state_and_phase_accessors_agree():
    e = GuessNumberEngine()
    assert e.state == "in_progress"
    assert e.phase == "in_progress"
    assert e.is_in("in_progress")
    assert not e.is_in("won", "lost")


def test_no_wait_for_change():
    """猜数字是单人同步游戏，不提供阻塞等待。"""
    assert not hasattr(GuessNumberEngine(), "wait_for_change")


# ── 提示词模板需要全量上下文 ──


def test_prompt_templates_render_without_missing_markers():
    """引擎产出的 prompt 不应出现缺字段标记。"""
    e = GuessNumberEngine(max_attempts=2)
    e.target = 50

    assert "{missing:" not in e.get_player_state()["prompt"]
    after_low = e.handle_action("guess", number=10)["prompt"]
    assert "{missing:" not in after_low
    assert "{unrendered" not in after_low or "history" in e.guesses

    e.handle_action("guess", number=90)  # 用完次数 -> lost
    assert e.status == "lost"
    lost_prompt = e.get_player_state()["prompt"]
    assert "{missing:" not in lost_prompt
    assert "50" in lost_prompt


def test_won_prompt_renders_cleanly():
    e = GuessNumberEngine()
    e.target = 33
    e.handle_action("guess", number=33)
    prompt = e.get_player_state()["prompt"]
    assert "{missing:" not in prompt
    assert "33" in prompt


# ── Prompt 层：缺字段可见、格式说明符可用、占位符可自检 ──


@pytest.fixture
def prompt():
    """独立 Prompt 实例（不是引擎的类级单例），测试可以安全地塞临时模板。"""
    from agent_game_server.prompt import Prompt

    return Prompt("agent_game_server.assets.guess_number_prompts")


def test_prompt_missing_key_leaves_no_raw_braces(prompt):
    """缺字段不能留下 `{变量名}` 这种看不出是拼错的原始花括号。"""
    text = prompt.render("in_progress", count=1, last_number=10)

    assert "{count}" not in text
    assert "{hint}" not in text
    assert "{missing:" in text  # 缺失的字段有显式标记
    assert "{missing:hint}" in text
    # 已知字段照常出值
    assert "第 1 次" in text
    assert "10" in text


def test_prompt_missing_key_fallback_is_not_a_crash(prompt):
    """提示词层永远不向调用方抛异常 —— 不能把一局游戏 500 掉。"""
    text = prompt.render("in_progress")
    assert isinstance(text, str)
    assert text


def test_prompt_format_spec_applies(prompt):
    """`{x:.0f}` 这类格式说明符要真的生效，不能退化成原样输出。"""
    prompt._templates["_fmt_probe"] = "剩余 {x:.0f} 秒 / {y:.2f}"
    prompt._required["_fmt_probe"] = ("x", "y")

    text = prompt.render("_fmt_probe", x=41.7, y=3.14159)

    assert text == "剩余 42 秒 / 3.14"
    assert "{" not in text


def test_prompt_format_spec_survives_partial_context(prompt):
    """缺一个字段时，另一个带格式说明符的字段仍要按格式渲染。"""
    prompt._templates["_fmt_probe"] = "{x:.0f} / {gone}"
    prompt._required["_fmt_probe"] = ("x", "gone")

    text = prompt.render("_fmt_probe", x=9.6)

    assert text.startswith("10 / ")
    assert "{missing:gone}" in text


def test_prompt_validation_reports_missing_placeholders(prompt):
    """validation 帮助函数能报出模板缺哪些字段。"""
    problems = prompt.validate("in_progress")
    assert problems
    assert any("hint" in p for p in problems)

    missing = prompt.missing_keys("in_progress", count=1)
    assert "hint" in missing
    assert "count" not in missing

    assert prompt.validate(
        "in_progress",
        count=1,
        last_number=2,
        hint="x",
        remaining=3,
        min_num=1,
        max_num=100,
        history=[],
    ) == []


def test_prompt_declares_placeholders(prompt):
    """placeholders() 能报出模板声明的字段名集合。"""
    declared = prompt.placeholders("in_progress_first")
    assert declared == {"remaining", "min_num", "max_num"}
    # 无字段的模板
    assert prompt.placeholders("missing_template_name") == set()


def test_engine_templates_all_render_with_full_context(prompt):
    """引擎的每套模板在完整上下文下都应渲染干净（模板一致性自检）。"""
    full_context = {
        "won": {"last_guess": 42, "count": 3},
        "lost": {"max_attempts": 10, "target": 42, "history": [1, 2]},
        "in_progress": {
            "count": 2,
            "last_number": 20,
            "hint": "太低 ↑",
            "remaining": 8,
            "min_num": 1,
            "max_num": 100,
            "history": [50, 20],
        },
        "in_progress_first": {"remaining": 10, "min_num": 1, "max_num": 100},
    }

    for name, ctx in full_context.items():
        assert prompt.validate(name, **ctx) == [], f"{name} 模板上下文不完整"
        text = prompt.render(name, **ctx)
        assert "{missing:" not in text, f"{name} 渲染出了缺失标记"
        assert "{unrendered" not in text, f"{name} 渲染失败"


def test_prompt_strict_mode_raises(prompt):
    """strict 模式给测试/自检用，缺字段时抛 PromptError。"""
    from agent_game_server.prompt import PromptError

    with pytest.raises(PromptError):
        prompt.render("in_progress", strict=True, count=1)

    # 上下文齐了就不抛
    ok = prompt.render("in_progress_first", strict=True, remaining=1, min_num=1, max_num=2)
    assert "游戏进行中" in ok


def test_prompt_unknown_template_is_marked(prompt):
    text = prompt.render("definitely_not_a_template")
    assert "不存在" in text
    assert prompt.validate("definitely_not_a_template")

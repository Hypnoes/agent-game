"""提示词层的容错契约：渲染失败必须降级，不能把一局游戏打成 500。

`prompt.py` 的模块文档写着"永不对调用方抛异常——提示词层不能把一局游戏 500 掉"，
但兜底链只接了 `KeyError / IndexError / ValueError / TypeError`。
`{x.y}` 而 x 是 int 时抛的是 `AttributeError`，它不在 KeyError 家族里，
于是一个点号就能把整局游戏打挂。

模板是**设计为可热重载**的资产（`/admin/reload-prompts` 就是为改它准备的），
所以"运维手滑"是正常输入，不是异常输入。
"""

import pytest

from agent_game_server.prompt import BROKEN_PREFIX, Prompt


@pytest.fixture
def prompt(tmp_path):
    """独立 Prompt 实例，可以安全地塞临时模板。"""
    return Prompt("agent_game_server.assets.guess_number_prompts")


# ═══════════════════════════════════════════
# 1. 属性访问失败要降级
# ═══════════════════════════════════════════


def test_attribute_access_on_non_object_degrades(prompt):
    """`{x.y}` 而 x 是 int：必须降级成 unrendered 标记，不能抛 AttributeError。"""
    prompt._templates["_probe"] = "值={x.y}"
    prompt._required["_probe"] = ("x",)

    text = prompt.render("_probe", x=1)

    assert isinstance(text, str)
    assert "{unrendered:x.y}" in text


def test_attribute_access_on_string_degrades(prompt):
    """字符串同样没有任意属性，`{x.missing}` 必须降级。"""
    prompt._templates["_probe"] = "值={x.nope}"
    prompt._required["_probe"] = ("x",)

    text = prompt.render("_probe", x="abc")

    assert "{unrendered:x.nope}" in text


def test_index_access_on_non_indexable_degrades(prompt):
    """`{x[0]}` 而 x 是 int：IndexError/TypeError 路径，必须降级。"""
    prompt._templates["_probe"] = "值={x[0]}"
    prompt._required["_probe"] = ("x",)

    text = prompt.render("_probe", x=5)

    assert "{unrendered:x[0]}" in text


@pytest.mark.parametrize(
    ("template", "ctx"),
    [
        ("{x.y}", {"x": 1}),
        ("{x.y.z}", {"x": 1}),
        ("{x[0]}", {"x": 5}),
        ("{x.y:.2f}", {"x": 1}),
        ("前 {x.y} 后 {ok}", {"x": 1, "ok": "好"}),
    ],
)
def test_no_template_shape_escapes_as_an_exception(prompt, template, ctx):
    """一张模板形态表，防止将来再漏一个异常类型。

    新增占位符语法时把它加进这张表；漏了会让用例红，而不是等运维踩到时才发现。
    """
    prompt._templates["_probe"] = template
    prompt._required["_probe"] = tuple(ctx)

    text = prompt.render("_probe", **ctx)

    assert isinstance(text, str)
    assert text


# ═══════════════════════════════════════════
# 2. 整条降级链仍然有效
# ═══════════════════════════════════════════


def test_missing_field_is_marked_not_raw_braces(prompt):
    prompt._templates["_probe"] = "值={absent}"
    prompt._required["_probe"] = ("absent",)

    text = prompt.render("_probe")

    assert "{missing:absent}" in text
    assert "{" not in text.replace("{missing:absent}", "")


def test_broken_template_syntax_is_prefixed(prompt):
    """花括号不配对：整段打上 [unrendered] 前缀原样返回。"""
    prompt._templates["_probe"] = "值={x"
    prompt._required["_probe"] = ()

    text = prompt.render("_probe", x=1)

    assert text.startswith(BROKEN_PREFIX)


def test_unknown_template_returns_a_message(prompt):
    text = prompt.render("definitely_not_a_template")
    assert "不存在" in text


# ═══════════════════════════════════════════
# 3. strict 模式仍然要报错
# ═══════════════════════════════════════════


def test_strict_mode_still_raises_for_tests(prompt):
    """降级只针对业务路径；测试/自检用 strict 时必须直接失败。"""
    from agent_game_server.prompt import PromptError

    with pytest.raises(PromptError):
        prompt.render("in_progress", strict=True, count=1)


def test_strict_passes_with_full_context(prompt):
    ok = prompt.render(
        "in_progress_first", strict=True, remaining=1, min_num=1, max_num=2
    )
    assert "游戏进行中" in ok


# ═══════════════════════════════════════════
# 4. 值本身会抛异常时也要降级
# ═══════════════════════════════════════════


class _Hostile:
    """所有参与渲染的协议方法都抛非清单内异常的对象。

    引擎把自定义对象塞进提示词上下文时就会遇到这种值。清单式捕获
    （只接 KeyError/ValueError/...）挡不住它：`__format__` 抛 RuntimeError
    就穿透了所有那些 except。
    """

    def __format__(self, spec):
        raise RuntimeError("hostile format")

    def __getitem__(self, key):
        raise RuntimeError("hostile getitem")

    def __getattr__(self, name):
        raise RuntimeError("hostile attr")


@pytest.mark.parametrize("template", ["{x}", "{x:.0f}", "{x[0]}", "{x.attr}"])
def test_hostile_value_degrades_instead_of_raising(prompt, template):
    """值自己会抛异常时，渲染必须降级，不能把异常交给调用方。

    这是 `prompt.py` 模块文档的硬契约："提示词层不能把一局游戏 500 掉"。
    兜底链的最后一层（`_render_template` 的 `except Exception`）就是为这种情况
    存在的——把那条 `return BROKEN_PREFIX + template` 改成 `raise`，这里必须红。
    """
    prompt._templates["_hostile"] = template
    prompt._required["_hostile"] = ("x",)

    text = prompt.render("_hostile", x=_Hostile())

    assert isinstance(text, str)
    assert text


def test_hostile_value_does_not_leak_the_exception_message(prompt):
    """降级输出里不该出现原始异常信息（那会进到 Agent 的上下文里）。"""
    prompt._templates["_hostile"] = "{x}"
    prompt._required["_hostile"] = ("x",)

    text = prompt.render("_hostile", x=_Hostile())

    assert "hostile" not in text.lower()

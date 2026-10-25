"""
Prompt 模板引擎 — 从 Python 模块加载提示词，支持运行时 reload。

用法：
    prompts = Prompt("agent_game_server.assets.liars_dice_prompts")
    text = prompts.render("waiting", players=1, needed=2, waited=5)
    prompts.reload()  # hotfix 提示词不用重启

渲染策略（分层降级，永不对调用方抛异常 —— 提示词层不能把一局游戏 500 掉）：
    1. 正常路径：str.format 整段渲染，`{x:.0f}` 这类格式说明符照常生效。
    2. 缺字段：逐字段渲染，已知字段（含格式说明符）照常出值，
       未知字段换成可见标记 `{missing:字段名}` —— 而不是留下 `{字段名}`
       这种"看起来只是没渲染、其实拼错了"的原始花括号。
    3. 模板语法本身坏了（花括号不配对等）：整段打上 `[unrendered]` 前缀原样返回，
       让坏模板在输出里一眼能看出来。
    4. 字段有值但格式说明符不适用（如 `{x:.0f}` 而 x 是字符串）：
       打上 `{unrendered:...}` 标记，保留原始占位符便于定位。

占位符自省（测试/自检用）：
    placeholders("your_turn")            → 该模板声明的字段名集合
    missing_keys("your_turn", a=1)       → 这次渲染会缺哪些字段
    validate("your_turn", a=1)           → 问题描述列表，空列表表示没问题
    render("your_turn", strict=True, ...) → 有问题直接抛 PromptError
"""

import importlib
import string
from typing import Any

# 缺字段的可见标记前缀：{missing:count}
MISSING_PREFIX = "missing"
# 字段在但渲染失败的标记前缀：{unrendered:x:.0f}
UNRENDERED_PREFIX = "unrendered"
# 整个模板无法解析时的前缀
BROKEN_PREFIX = "[unrendered]"

_FORMATTER = string.Formatter()


class PromptError(KeyError):
    """strict 模式下的模板错误。继承 KeyError，方便按字典语义捕获。"""


def _root_field(field: str) -> str:
    """从 'x' / 'x.y' / 'x[0]' 中取出根字段名。"""
    root = field
    for sep in (".", "["):
        idx = root.find(sep)
        if idx != -1:
            root = root[:idx]
    return root


def _raw_placeholder(field: str, spec: str | None, conversion: str | None) -> str:
    """把 Formatter.parse 拆开的四元组拼回原始占位符文本（不含外层花括号）。"""
    text = field
    if conversion:
        text += "!" + conversion
    if spec:
        text += ":" + spec
    return text


class Prompt:
    """加载模块的 PROMPTS 字典，render() 做变量替换，reload() 热更新。"""

    def __init__(self, module_path: str):
        self._module_path = module_path
        self._templates: dict[str, str] = {}
        self._required: dict[str, tuple[str, ...]] = {}
        self._broken: dict[str, str] = {}
        self.load()

    def load(self):
        mod = importlib.import_module(self._module_path)
        importlib.reload(mod)
        self._templates = dict(getattr(mod, "PROMPTS", {}))
        # 载入时就把每个模板的字段名和语法健康度算好，问题早于渲染暴露
        self._required = {}
        self._broken = {}
        for name, template in self._templates.items():
            try:
                parsed = list(_FORMATTER.parse(template))
            except ValueError as ex:
                self._broken[name] = str(ex)
                self._required[name] = ()
                continue
            self._required[name] = self._collect_fields(parsed)

    def reload(self):
        self.load()

    # ── 自省 ──

    @staticmethod
    def _collect_fields(parsed) -> tuple[str, ...]:
        fields: list[str] = []
        for _literal, field, _spec, _conv in parsed:
            if field is None:
                continue
            name = _root_field(field)
            # {} / {0} 这类位置字段没有名字可报
            if not name or name.isdigit():
                continue
            if name not in fields:
                fields.append(name)
        return tuple(fields)

    def names(self) -> list[str]:
        """已加载的模板名。"""
        return list(self._templates)

    def template(self, name: str) -> str | None:
        """原始模板文本（未渲染）。不存在返回 None。"""
        return self._templates.get(name)

    @staticmethod
    def required_fields_of(template: str) -> set[str]:
        """任意模板文本声明了哪些字段（按出现顺序返回集合，忽略格式说明符）。"""
        try:
            parsed = list(_FORMATTER.parse(template))
        except ValueError:
            return set()
        return set(Prompt._collect_fields(parsed))

    def placeholders(self, name: str) -> set[str]:
        """某个模板需要的字段名集合。模板不存在返回空 set。"""
        return set(self._required.get(name, ()))

    def missing_keys(self, name: str, **ctx: Any) -> set[str]:
        """这次 render 会缺哪些字段。"""
        return self.placeholders(name) - set(ctx)

    def validate(self, name: str, **ctx: Any) -> list[str]:
        """校验一次 render 的上下文。返回问题描述列表，空列表表示没问题。"""
        problems: list[str] = []
        if name not in self._templates:
            return [f"模板 '{name}' 不存在；已加载: {sorted(self._templates)}"]
        if name in self._broken:
            problems.append(f"模板 '{name}' 语法错误: {self._broken[name]}")
        missing = sorted(self.missing_keys(name, **ctx))
        if missing:
            problems.append(f"模板 '{name}' 缺少字段: {missing}")
        return problems

    # ── 渲染 ──

    def render(self, name: str, *, strict: bool = False, **ctx: Any) -> str:
        """渲染模板。缺字段 / 格式不对都不向调用方抛异常。

        strict=True 时改为抛 PromptError —— 只给测试和自检用，业务路径不要开。
        """
        if name not in self._templates:
            if strict:
                raise PromptError(
                    f"模板 '{name}' 不存在；已加载: {sorted(self._templates)}"
                )
            return f"⚠️ 模板 '{name}' 不存在"

        if strict:
            problems = self.validate(name, **ctx)
            if problems:
                raise PromptError("；".join(problems))

        return self._render_template(self._templates[name], ctx)

    def _render_template(self, template: str, ctx: dict[str, Any]) -> str:
        # 路径 1：整段严格渲染。字段齐、格式对时这就是唯一路径。
        try:
            return template.format(**ctx)
        except (KeyError, IndexError, ValueError, TypeError):
            pass

        # 路径 2/3：逐字段降级渲染
        try:
            return self._render_lenient(template, ctx)
        except Exception:  # noqa: BLE001 — 提示词层绝不能让一局游戏 500
            return BROKEN_PREFIX + template

    def _render_lenient(self, template: str, ctx: dict[str, Any]) -> str:
        """逐字段渲染：认得就渲染（含格式说明符），不认得就打成 missing 标记。"""
        try:
            parsed = list(_FORMATTER.parse(template))
        except ValueError:
            # 模板语法本身坏了，解析器给不出字段边界
            return BROKEN_PREFIX + template

        out: list[str] = []
        for literal, field, spec, conversion in parsed:
            out.append(literal)
            if field is None:
                continue
            out.append(self._render_field(field, spec, conversion, ctx))
        return "".join(out)

    @staticmethod
    def _render_field(
        field: str, spec: str | None, conversion: str | None, ctx: dict[str, Any]
    ) -> str:
        root = _root_field(field)
        if not root or root.isdigit():
            # 位置字段，模板没给出名字，原样保留
            return "{" + _raw_placeholder(field, spec, conversion) + "}"
        if root not in ctx:
            return f"{{{MISSING_PREFIX}:{root}}}"

        # 用单字段模板复用 format 的全部语义：.attr、[idx]、!r、格式说明符
        single = "{" + _raw_placeholder(field, spec, conversion) + "}"
        try:
            return single.format(**ctx)
        except (ValueError, TypeError, KeyError, IndexError):
            # 字段有值但渲染失败 —— 保留原始占位符便于定位
            return f"{{{UNRENDERED_PREFIX}:{_raw_placeholder(field, spec, conversion)}}}"

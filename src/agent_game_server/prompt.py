"""
Prompt 模板引擎 — 从 Python 模块加载提示词，支持运行时 reload。

用法：
    prompts = Prompt("agent_game_server.assets.liars_dice_prompts")
    text = prompts.render("waiting", players=1, needed=2, waited=5)
    prompts.reload()  # hotfix 提示词不用重启
"""

import importlib
from typing import Any


class Prompt:
    """加载模块的 PROMPTS 字典，render() 做变量替换，reload() 热更新。"""

    def __init__(self, module_path: str):
        self._module_path = module_path
        self._templates: dict[str, str] = {}
        self.load()

    def load(self):
        mod = importlib.import_module(self._module_path)
        importlib.reload(mod)
        self._templates = dict(getattr(mod, "PROMPTS", {}))

    def reload(self):
        self.load()

    def render(self, name: str, **ctx: Any) -> str:
        template = self._templates.get(name, f"⚠️ 模板 '{name}' 不存在")
        try:
            return template.format(**ctx)
        except KeyError:
            result = template
            for key, val in ctx.items():
                result = result.replace(f"{{{key}}}", str(val))
            return result

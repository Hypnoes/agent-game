"""变异测试：验证测试套件真的能抓到 bug。

覆盖率只回答"这行代码被执行过"，不回答"这条规则被约束住"。变异测试反过来问：
往源码里注入一个真实缺陷，套件会不会红？不会红的那些，说明它在那个位置没有看门人。

用法：

    python scripts/mutation_check.py            # 跑全部
    python scripts/mutation_check.py --list     # 只看清单
    python scripts/mutation_check.py -k timeout # 只跑名字含 timeout 的

退出码：全部被杀死为 0；有存活变异体为 1。

每个变异体包含：名字、文件、锚点（必须唯一出现）、替换文本。锚点找不到时按
"变异体失效"处理并计入失败——源码改了而脚本没跟上，等于这个位置重新失去了看门人。
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src" / "agent_game_server"


@dataclass(frozen=True)
class Mutant:
    name: str
    relative_path: str
    anchor: str
    replacement: str
    # 期望行为：被杀死（"killed"）。留一个字段以便将来记录"已知等价变异体"。
    expectation: str = "killed"

    @property
    def path(self) -> Path:
        return SRC / self.relative_path


DICE = "games/liars_dice.py"
MANAGER = "manager.py"
MCP = "mcp_server.py"
SERVER = "game_server.py"
PROMPT = "prompt.py"
CONFIG = "config.py"
ENGINE = "games/game_engine.py"


MUTANTS: list[Mutant] = [
    # ── 大话骰规则 ──
    Mutant(
        "百搭失效：1 不再顶替其它点数",
        DICE,
        "            if d == face or (d == 1 and face != 1)",
        "            if d == face",
    ),
    Mutant(
        "开盅胜负判反",
        DICE,
        "        loser = player_index if actual >= call.quantity else call.player_idx",
        "        loser = call.player_idx if actual >= call.quantity else player_index",
    ),
    Mutant(
        "叫 1 时其它点数也被算进来",
        DICE,
        "            if d == face or (d == 1 and face != 1)",
        "            if d == face or d == 1 or (face == 1 and d != 1)",
    ),
    Mutant(
        "回合推进不跳过出局者",
        DICE,
        "            if self.players[idx].alive:\n                return idx",
        "            return idx",
    ),
    Mutant(
        "开局后仍允许加入",
        DICE,
        '        if self.phase != "waiting":\n            raise ValueError(f"对局已开始（{self.phase}），无法加入")',
        '        if False:\n            raise ValueError("x")',
    ),
    Mutant(
        "超时不清零本轮叫数",
        DICE,
        "            self.current_call = None\n            if not self._eliminate(loser):",
        "            if not self._eliminate(loser):",
    ),
    Mutant(
        "超时判负不通知等待者",
        DICE,
        "            # 判负同样要唤醒等待者，否则等待方要等满整个预算才知道对局已结束\n            self._notify_state_change()",
        "            pass",
    ),
    Mutant(
        "开盅后不通知对手",
        DICE,
        "        # 掉骰、出局、结束对局都是状态变更，唤醒可能还在等待的对手\n        self._notify_state_change()",
        "        pass",
    ),
    Mutant(
        "入座后不通知大厅",
        DICE,
        "            # 人还没满，同样是一次状态变更：大厅里等待的玩家要看到人数变化\n            self._notify_state_change()",
        "            pass",
    ),
    Mutant(
        "叫数合法性检查永远通过",
        DICE,
        "        if prev is None:\n            return True\n",
        "        if prev is None:\n            return True\n        return True\n",
    ),
    Mutant(
        "两人局也走掉一颗骰子（去掉突然死亡）",
        DICE,
        "            if len(self._alive_indices()) <= 2:\n                player.dice.clear()",
        "            if len(self._alive_indices()) <= 1:\n                player.dice.clear()",
    ),
    Mutant(
        "引擎侧不校验 player_token",
        DICE,
        '        if not player_token:\n            raise ValueError("缺少 player_token")\n        if self.players[player_index].token != player_token:\n            raise ValueError("player_token 不匹配")',
        "        return",
    ),
    Mutant(
        "回合判负时限短于等待预算",
        DICE,
        "        self.turn_timeout = 60",
        "        self.turn_timeout = 10",
    ),
    Mutant(
        "负索引不再拒绝（绕回最后一名玩家）",
        DICE,
        "            or player_index < 0\n            or player_index >= len(self.players)",
        "            or player_index >= len(self.players)",
    ),
    Mutant(
        "state 返回对手的视角",
        DICE,
        '        elif action == "state":\n            pi = params.get("player_index", 0)\n            self._validate_player(pi, params.get("player_token", ""))\n            return self.get_player_state(pi)',
        '        elif action == "state":\n            pi = params.get("player_index", 0)\n            self._validate_player(pi, params.get("player_token", ""))\n            return self.get_player_state((pi + 1) % len(self.players))',
    ),
    Mutant(
        "工具 schema 丢掉必填约束",
        DICE,
        '                    "required": ["player_index", "player_token"],\n                },\n            },\n            {\n                "name": "liars_dice_bid"',
        '                    "required": [],\n                },\n            },\n            {\n                "name": "liars_dice_bid"',
    ),
    Mutant(
        "wait_for_change 醒一次就返回（假唤醒）",
        DICE,
        '            if outcome == "timeout":\n                return "timeout"\n            # 被唤醒：可能只是别人行动，循环重判，未轮到我就继续等剩下的预算',
        '            if outcome == "timeout":\n                return "timeout"\n            return "changed"\n            #',
    ),
    Mutant(
        "等待条件放宽成任何人行动都算轮到我",
        DICE,
        '        if self.phase == "finished":\n            return True\n        return self.phase == "bidding" and self.current_turn == player_index',
        '        if self.phase == "finished":\n            return True\n        return self.phase == "bidding"',
    ),
    # ── 猜数字 ──
    Mutant(
        "猜数字超时不再判负",
        "games/guess_number.py",
        '        if self._fsm.is_in("in_progress") and now - self.last_action > 600:',
        "        if False:",
    ),
    Mutant(
        "猜测值越界不再拒绝",
        "games/guess_number.py",
        "        if not self.min <= number <= self.max:",
        "        if False:",
    ),
    # ── 管理器 ──
    Mutant(
        "check_timeouts 不再回收已结束对局",
        MANAGER,
        "            if now - finished_at > FINISHED_RETENTION:\n                self.remove(sid)",
        "            if False:\n                self.remove(sid)",
    ),
    Mutant(
        "check_timeouts 不隔离坏引擎",
        MANAGER,
        "            try:\n                event = self._check_one(sid, engine, now)\n            except Exception:",
        "            try:\n                event = self._check_one(sid, engine, now)\n            except ZeroDivisionError:",
    ),
    Mutant(
        "list_sessions 污染引擎状态对象",
        MANAGER,
        "                state = dict(declared) if isinstance(declared, dict) else {\"state\": declared}",
        "                state = declared if isinstance(declared, dict) else {}",
    ),
    Mutant(
        "不再按 player_token 复用对局",
        MANAGER,
        "                if finder(player_token) is not None:\n                    return sid, eng",
        "                pass",
    ),
    # ── 终态与秘密键 ──
    Mutant(
        "终态判定忽略引擎声明的集合",
        ENGINE,
        "        if isinstance(value, str) and value in states:",
        '        if isinstance(value, str) and value in {"finished", "won", "lost"}:',
    ),
    Mutant(
        "秘密键忽略引擎声明",
        ENGINE,
        "    declared = getattr(engine, \"secret_keys\", None)\n    if isinstance(declared, (frozenset, set, tuple, list)):\n        return frozenset(str(key) for key in declared)",
        "    declared = None\n    if isinstance(declared, (frozenset, set, tuple, list)):\n        return frozenset(str(key) for key in declared)",
    ),
    # ── MCP 传输 ──
    Mutant(
        "MCP state 不先校验身份（先等待后校验）",
        MCP,
        '        identity = player_identity_error(\n            engine, args.get("player_index", 0), args.get("player_token", "")\n        )\n        if identity is not None:\n            raise ValueError(identity.message)\n',
        "",
    ),
    Mutant(
        "MCP 的 state 不再剥离 timeout 参数",
        MCP,
        '    timeout = args.pop("timeout", None)\n',
        '    timeout = args.get("timeout")\n',
    ),
    Mutant(
        "MCP 会话缓存无上限",
        MCP,
        "    while len(_MCP_SESSIONS) > MAX_MCP_SESSION_CACHE:\n        _MCP_SESSIONS.pop(next(iter(_MCP_SESSIONS)))",
        "    pass",
    ),
    Mutant(
        "MCP 不再刷新 LRU",
        MCP,
        "    cache = _MCP_SESSIONS.pop(key, None)",
        "    cache = _MCP_SESSIONS.get(key)",
    ),
    Mutant(
        "MCP 把缺参数当成服务端错误",
        MCP,
        "_EXPECTED_ERRORS = (ValueError, KeyError, TypeError, SessionNotFound)",
        "_EXPECTED_ERRORS = (ValueError, SessionNotFound)",
    ),
    Mutant(
        "MCP 未知动作也允许建会话",
        MCP,
        "        if action not in _STARTING_ACTIONS:",
        "        if False:",
    ),
    Mutant(
        "MCP 显式 session_id 不要求 token",
        MCP,
        "    if explicit_session_id is not None and not player_token and _is_multiplayer(engine):",
        "    if False:",
    ),
    # ── game_server ──
    Mutant(
        "公开状态不剥离秘密键",
        SERVER,
        "    return {k: v for k, v in state.items() if k not in secrets}",
        "    return state",
    ),
    Mutant(
        "/admin 无条件注入 token",
        SERVER,
        "    if presented and _token_matches(presented):",
        "    if True:",
    ),
    Mutant(
        "reload-prompts 不校验 token",
        SERVER,
        '    """热重载所有**自己**带提示词模板的引擎。没有模板的引擎不报成功。"""\n    _check_admin_token(request)',
        '    """热重载所有**自己**带提示词模板的引擎。没有模板的引擎不报成功。"""',
    ),
    Mutant(
        "/admin/games 不校验 token",
        SERVER,
        "def admin_list_all(request: Request):\n    _check_admin_token(request)",
        "def admin_list_all(request: Request):",
    ),
    Mutant(
        "/admin/games/{id} 不校验 token",
        SERVER,
        "    _check_admin_token(request)\n    e = _manager_of(request.app).get(session_id)",
        "    e = _manager_of(request.app).get(session_id)",
    ),
    Mutant(
        "多人 state 缺 token 也放行",
        SERVER,
        '    return HTTPException(status_code=403, detail=identity.message)',
        '    return None',
    ),
    Mutant(
        "非 dict body 不再拒绝",
        SERVER,
        "    if not isinstance(parsed, dict):",
        "    if False:",
    ),
    Mutant(
        "畸形 JSON 静默当成空参数",
        SERVER,
        '    except ValueError as ex:\n        raise HTTPException(status_code=400, detail=f"请求体不是合法 JSON: {ex}")',
        "    except ValueError:\n        return {}",
    ),
    Mutant(
        "admin WS 不校验 token",
        SERVER,
        "    if not token or not _token_matches(token):",
        "    if False:",
    ),
    Mutant(
        "poller 单轮异常直接退出",
        SERVER,
        '        except Exception:\n            logger.exception("admin state poll iteration failed; continuing")',
        '        except Exception:\n            logger.exception("x")\n            return',
    ),
    Mutant(
        "广播不再按签名去重",
        SERVER,
        "    if signature == state.last_signature and signature != _SIGNATURE_ERROR:\n        return False",
        "    if True:\n        return False",
    ),
    # ── prompt ──
    Mutant(
        "降级链最后一层改成抛异常",
        PROMPT,
        "        except Exception:  # noqa: BLE001 — 路径 3：提示词层绝不能让一局游戏 500\n            return BROKEN_PREFIX + template",
        "        except Exception:  # noqa: BLE001\n            raise",
        # 等价变异体：`_render_lenient` 内部已经把每个字段的异常都接住了，
        # 它自己只会在解析器出问题时抛 ValueError（那由内部 try 处理）。
        # 所以这层兜底当前不可达——它是防御性的第二道锁，不是活路径。
        # 删掉不会改变行为，保留是为了"哪天 _render_lenient 被改坏了也不会 500"。
        expectation="equivalent",
    ),
    Mutant(
        "单字段渲染失败不外接（异常交回调用方）",
        PROMPT,
        "        except Exception:  # noqa: BLE001\n            # 字段有值但渲染失败",
        "        except KeyError:\n            # 字段有值但渲染失败",
    ),
    Mutant(
        "整段渲染失败时不降级（直接外抛）",
        PROMPT,
        "        except Exception:  # noqa: BLE001\n            pass",
        "        except ZeroDivisionError:\n            pass",
    ),
    # ── config ──
    Mutant(
        "timeout 不再夹到上限",
        CONFIG,
        "    return max(0.0, min(budget, BLOCK_BUDGET))",
        "    return max(0.0, budget)",
    ),
    Mutant(
        "bool 不再被 timeout 拒绝",
        CONFIG,
        "    if isinstance(value, bool) or not isinstance(value, (int, float)):",
        "    if not isinstance(value, (int, float)):",
    ),
    Mutant(
        "解析器不再读实时预算",
        CONFIG,
        "    budget = BLOCK_BUDGET if default is None else default",
        "    budget = 55.0 if default is None else default",
        # 等价变异体：末尾的 `min(budget, BLOCK_BUDGET)` 会把 55.0 钳回当前预算，
        # 所以把 BLOCK_BUDGET 调小时行为不变。要杀掉它得同时改钳制，那是另一个
        # 变异体（"timeout 不再夹到上限"）已经覆盖的事。
        expectation="equivalent",
    ),
]


def run_suite() -> tuple[bool, str]:
    """跑一次测试套件。返回 (是否通过, 摘要)。"""
    result = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "--tb=no", "-p", "no:cacheprovider"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    lines = [line for line in result.stdout.splitlines() if "passed" in line or "failed" in line or "error" in line]
    summary = lines[-1] if lines else f"exit={result.returncode}"
    return result.returncode == 0, summary


def check_mutant(mutant: Mutant) -> tuple[str, str]:
    """应用一个变异体，跑套件，还原。返回 (结果, 摘要)。

    结果取值：
    - "killed"：套件变红，说明有看门人
    - "survived"：套件仍然全绿，这个位置没有看门人
    - "stale"：锚点在源码里找不到（变异体本身失效）
    """
    path = mutant.path
    original = path.read_text(encoding="utf-8")

    occurrences = original.count(mutant.anchor)
    if occurrences == 0:
        return "stale", "锚点未找到"
    if occurrences > 1:
        return "stale", f"锚点出现 {occurrences} 次，无法唯一定位"

    mutated = original.replace(mutant.anchor, mutant.replacement, 1)
    path.write_text(mutated, encoding="utf-8")
    try:
        passed, summary = run_suite()
    finally:
        path.write_text(original, encoding="utf-8")

    if passed:
        # 已知等价变异体：行为确实没变，不算缺口
        return ("equivalent" if mutant.expectation == "equivalent" else "survived"), summary
    return "killed", summary


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="变异测试：验证测试套件能否抓到注入的缺陷")
    parser.add_argument("--list", action="store_true", help="只列出变异体")
    parser.add_argument("-k", "--filter", default="", help="只跑名字包含该子串的变异体")
    args = parser.parse_args(argv)

    selected = [m for m in MUTANTS if args.filter in m.name]

    if args.list:
        for i, mutant in enumerate(selected, 1):
            print(f"{i:2d}. [{mutant.relative_path}] {mutant.name}")
        print(f"\n共 {len(selected)} 个变异体")
        return 0

    print("基线：先确认未变异的源码是通过的")
    passed, summary = run_suite()
    if not passed:
        print(f"❌ 基线不通过，先修测试再谈变异：{summary}")
        return 1
    print(f"   ✅ {summary}\n")

    killed, survived, stale, equivalent = [], [], [], []
    buckets = {
        "killed": killed,
        "survived": survived,
        "stale": stale,
        "equivalent": equivalent,
    }
    markers = {"killed": "✅", "survived": "❌", "stale": "⚠️ ", "equivalent": "➖"}
    for i, mutant in enumerate(selected, 1):
        outcome, summary = check_mutant(mutant)
        marker = markers[outcome]
        print(f"{marker} [{i:2d}/{len(selected)}] {mutant.name} | {summary}")
        buckets[outcome].append(mutant)

    print("\n" + "=" * 60)
    print(
        f"杀死 {len(killed)} / 存活 {len(survived)} / "
        f"等价 {len(equivalent)} / 失效 {len(stale)}  (共 {len(selected)})"
    )

    if survived:
        print("\n存活变异体（这些位置没有看门人）：")
        for mutant in survived:
            print(f"  - [{mutant.relative_path}] {mutant.name}")
    if stale:
        print("\n失效变异体（源码变了，脚本要跟上）：")
        for mutant in stale:
            print(f"  - [{mutant.relative_path}] {mutant.name}")

    return 0 if not survived and not stale else 1


if __name__ == "__main__":
    raise SystemExit(main())

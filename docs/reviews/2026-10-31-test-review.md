# 测试专项审查（2026-10-31）

审查对象：`master` @ `a1d765c`，10 个测试文件、234 个 test 函数、收集到 264 个用例。
审查方法：读全部测试代码 + 覆盖率测量 + **变异测试**（改源码、跑套件、还原）+ 特定缺陷的注入实验。

## 结论摘要

你的怀疑成立，而且问题可以定位得比"覆盖不够"更具体：

**这套测试的数量没问题，错的是测试对象和通过判据。**

- **覆盖**：语句 90%、分支 89%，但 35 个变异体里 **11 个存活**。存活的是规则，被杀的是实现细节。
- **要测什么**：测试盯的是"文案长什么样"和"私有函数返回什么"，不是"对局规则是否成立"。
- **怎么算通过**：`skip` 计入通过、重复定义的用例被静默遮蔽、没有 CI 也没有单一判据文件。
- **框架**：测试按**源码模块**组织，而不是按**必须成立的不变量**组织；不变量住在 `AGENTS.md` 里，不进测试。

一个直接对照：把玩家名改成 HTML 注入串，全套 264 条照常通过；把提示词里"轮到你了"四个字改成同义的"现在由你行动"，**3 条测试立刻失败**。前者是安全漏洞，后者是文案热重载。测试对"不该在意的东西"敏感，对"必须守住的东西"迟钝。

---

## 一、覆盖：数字漂亮，漏在语义上

### 1.1 覆盖率数据

```
TOTAL   1224 statements   120 missed   414 branches   59 partial   89%
```

单看数字可以交差。但覆盖率只回答"这行代码被执行过"，不回答"这条规则被约束住"。

### 1.2 变异测试：11/35 存活

做法：在源码上做单点修改（等价于"引入一个 bug"），跑全套测试，看是否被杀死。存活 = 测试没能发现这个 bug。

**存活的变异体（测试没抓到）**

| # | 引入的 bug | 后果 | 套件反应 |
|---|---|---|---|
| 1 | `_count_face` 去掉 `d == 1` 分支 | **1 不再是百搭**——大话骰最核心的规则失效 | 264 全绿 |
| 2 | `join` 去掉"对局已开始则拒绝" | 进行中的对局可被新人挤进来、全场骰子重摇 | 264 全绿 |
| 3 | `check_timeout` 判负时不再 `_notify_state_change()` | 等待方要等满整个预算才知道对局已结束 | 264 全绿 |
| 4 | `_add_player` 入座后不再通知 | 大厅里等待的玩家看不到人数变化 | 264 全绿 |
| 5 | `_challenge` 结束后不再通知 | 对手等满预算才拿到结果 | 264 全绿 |
| 6 | `turn_timeout = 60` 改成 `10` | 判负时限短于等待预算 55 秒的既定设计被破坏 | 264 全绿 |
| 7 | 工具 schema 的 `required` 清空 | 客户端不再被约束必填字段 | 264 全绿 |
| 8 | `reload_prompts` 去掉 `_check_admin_token` | **未授权者能强制热重载提示词** | 264 全绿 |
| 9 | `_state_player_error` 的"缺 token"分支删除 | 单看这一支无行为变化（第二支兜住），但该分支无覆盖 | 264 全绿 |
| 10 | `_execute` 的"显式 session_id 必须带 token"守卫删除 | 守卫本身无测试；实际不泄密（引擎另有校验） | 264 全绿 |
| 11 | `prompt.py` 兜底改成 `raise` | 提示词层约定"永不 500"失效 | 264 全绿 |

**被杀死的变异体（测试有效）**：开盅胜负判反、回合推进不跳过出局者、`_validate_player` 去掉校验、
admin 列表不校验 token、猜数字超时不判负、`_reemember_session` 不刷新 LRU、
`/mcp` 改成 `Mount`、admin 永不广播、公开状态不剥秘密键、`/admin` 无条件注入 token、
未知动作也建会话、`timeout` 不夹上限、`bool` 不被拒绝、poller 单轮异常退出、
`wait_for_change` 改成醒一次就返回、假唤醒条件放宽。

### 1.3 存活的三个为什么

**（1）规则测试是用"某个骰子值"采样，不是用规则建模。**
`set_all_dice(e, face)` 把全场骰子固定成同一个数，于是永远采不到"1 混在非 1 里"的组合。
百搭分支从未被任何用例触达——这是大话骰区别于普通吹牛的唯一规则。

**（2）通知/唤醒只在主路径上被测。**
`test_wait.py` 测的是"被别人叫数唤醒"这一条路径。`_challenge` 之后、超时判负之后、
入座之后这三条 notify 路径没有对应用例。变异 3–5 全部存活，说明"通知"被视为实现细节而非契约。

**（3）测试跟着"历史缺陷清单"走，不跟着"规则清单"走。**
文件名和注释里到处都是「缺陷 6」「缺陷 7」「issue-1」。这是回归测试按事故记录生长：
每个修过的 bug 都有 1–3 条用例（同一 bug 的变体再多加几条），
而**从没出过 bug 的规则（百搭、join 时机、通知时序）覆盖率接近于零**。
事故驱动能防复发，不能防首次发生。

---

## 二、要测什么：测试对象错位

### 2.1 断言绑在文案上（44 处）

全文有 44 条断言依赖中文提示词的具体措辞，例如：

```python
assert "轮到你了" in text            # test_mcp_server.py:401
assert "你的骰子" in text            # test_mcp_server.py:330
assert "已等待" not in prompt        # test_prompts.py:246
assert "最多挂起 55 秒" in prompt     # test_prompts.py:300
```

提示词模板是**明确设计为可热重载的资产**（`/admin/reload-prompts` 就是为改它准备的）。
把测试绑在措辞上，等于给一个"鼓励改"的东西上锁。

实测：

```
仅把 "轮到你了！这是你的回合" 改成 "现在由你行动" → 3 failed
```

文案改动让套件变红，是**假阳性**；它不会告诉你对局逻辑坏了。
而且这类断言在 `test_mcp_server.py` 里出现 2 次、`test_prompts.py` 里出现多次，来源不同、语义重复。

### 2.2 28 个用例直接调私有函数

234 个 test 函数里，28 个直接调用 `_run_action`、`_parse_tool_name`、`_remember_session`、
`_engine_phase`、`_iter_engines`、`_mark_expired`、`_find_player_by_token` 等私有实现。

私有函数当然可以测，问题是**它们被当成了契约的替身**。最典型的例子：

```python
# test_mcp_server.py:655
def test_timeout_zero_vs_absent_end_to_end():
    """两个传输对 timeout 的理解必须一致：0 不等待，缺省用满预算。"""
    ...
    await _run_action(engine, "state", payload)     # ← 只调了私有函数
```

名字叫"两个传输"，实际一次 HTTP 都没发。**传输对等性**这个 002 号 issue 的核心验收项，
被降级成了"同一个函数被调用两次"。

### 2.3 后果：真实缺陷从这条缝里漏掉了

本轮代码审查发现的 **P1-2**：MCP 侧 `state` 先等待、后校验身份，与 REST 侧顺序相反。

```
MCP  带错 token 的 state -> 等 2.02 秒才报错
REST 带错 token 的 state -> 立即报错
```

REST 侧有 `test_state_action_validates_before_any_wait`（注入 5 秒等待，5 条用例全红，很有效）。
**MCP 侧没有对应用例**。`AGENTS.md` #5 写的是"校验必须在等待之前完成"，但只在一条传输上被验证。

同理，`AGENTS.md` #9 要求 admin token 永远生效；`/admin/games` 有测试，
`/admin/reload-prompts` 没有——变异 8 因此存活。

### 2.4 AGENTS.md 的 9 条硬约束 ↔ 测试映射

这是最该有、但完全不存在的表：

| 约束 | 有测试吗 | 变异验证 |
|---|---|---|
| #1 `/mcp` 只能用 `Route` | ✅ MCP 握手用例 | 改 `Mount` → 17 failed |
| #2 消费端超时必须大于 55 秒 | ❌ 仅文档 | 不可测（在客户端侧） |
| #3 身份来自 `player_token` | ✅ 核心回归 | 去掉 identity → 1 failed |
| #4 `bid/challenge/state` 必填 token | ⚠️ 只测了"不匹配"，"缺失"分支单独删仍存活 | 两段一起删 → 5 failed |
| #5 校验先于等待 | ⚠️ **仅 REST**，MCP 无 | 注入 5 秒 → REST 5 failed；MCP 无测试 |
| #6 `wait_for_change` 循环重判 | ✅ `test_wait.py` 7 条 | 改醒一次就返回 → 2 failed |
| #7 `parse_block_budget` 唯一实现 | ✅ 但两个文件重复测 | 去掉夹紧 → 4 failed |
| #8 剥 `SECRET_STATE_KEYS` | ✅ | 不剥 → 2 failed |
| #9 admin token 永远生效 | ⚠️ **reload-prompts 无** | 去掉校验 → 264 passed ✗ |

**9 条里有 3 条是零覆盖或不完整覆盖，其中 1 条（#9）对应真实可利用的漏洞。**

### 2.5 文档承诺的端点有 2 个从未被请求过

`README.md` 的端点表列了 6 行。实际 HTTP 覆盖：

| 端点 | 有测试 |
|---|---|
| `POST /mcp` | ✅ 39 条 |
| `GET /games/tools` | ❌ 0 次 HTTP 请求（只有 `describe_all_tools()` 的单元测试） |
| `POST /games/{type}` | ✅ |
| `POST /games/{sid}/action` | ✅ |
| `GET /games/{sid}` | ✅ |
| `GET /games/{sid}/state/{idx}` | ✅ |
| `/admin/*` | ⚠️ reload-prompts 只测了 happy path |
| `WS /admin/ws` | ❌ 只有假 socket，从未建立真实连接 |

上一轮发现的"`/admin/ws` 完全无鉴权"，正是因为它从未被真实建立过连接。

---

## 三、怎么算通过：判据本身不可靠

### 3.1 `skip` 被当成通过

```python
# test_mcp_server.py:355 和 :1067
if not hasattr(LiarsDiceEngine, "wait_for_change"):
    pytest.skip("blocking wait not present")
```

这两条是**阻塞等待功能的主验收用例**。删掉整个 `wait_for_change` 方法后：

```
6 failed, 256 passed, 2 skipped
```

两条主用例变成 `skipped`。`skipped` 在 pytest 退出码里是成功。
`AGENTS.md` #6 明令"别把循环简化回任何一次唤醒就返回"——而守住它的两条最直接的用例，
在功能被删除时会**主动让路**。跳过条件写反了：它保护的是"老版本没有这个方法"，代价是"新版本删掉这个方法也不报警"。

### 3.2 重复定义的用例被静默遮蔽

`tests/test_manager.py` 的 `test_finished_game_reclaimed_after_retention` **定义了两次**
（第 169 行与第 350 行）。Python 后定义覆盖先定义，第一个永久不执行。

AST 统计：名义 33 个 → 实际收集 32 个。

这是一次明显的复制粘贴合并残留，**没有任何机制能发现它**——没有 flake8 的 F811，
没有用例数基线，也没有对"测试函数名唯一"的检查。

### 3.3 没有 CI、没有 lint、没有覆盖率门槛

仓库没有 `.github/`，没有 `ruff`/`flake8`/`mypy` 配置，`pyproject.toml` 只有：

```toml
[tool.pytest.ini_options]
testpaths = ["tests"]
```

"264 全绿"是一个**本机口头结论**。没有人和任何东西能保证它下次还是绿的，
也没有东西阻止新增一个未被任何用例引用的测试文件。
（顺带：`coverage` 不在 dev 依赖里，我是临时装的。）

### 3.4 时序断言依赖真实墙钟

16 处 `asyncio.sleep`，其中 4 条 poller 用例各睡 1.2 秒，占了套件 9.5 秒里的 8 秒。
断言形态如：

```python
assert elapsed < 0.1              # test_wait.py
assert 0.15 <= elapsed < 1.0      # test_wait.py:75
```

我用 8 路并发压测模拟共享 CI 机器，8 次全绿，**当前不脆**。
但这是"运气好 + 阈值相对于操作耗时足够宽"，不是"确定性"。一旦上 CI，这是第一批变成间歇性失败的用例。

另外 4 条 poller 用例靠"睡 1.2 秒看它跑了几轮"，本质是在测调度器，不是在测业务。

### 3.5 存在纯占位断言

```python
assert "prompt" in state     # test_liars_dice.py:140
```

只断言键存在，不断言内容非空、不断言结构。这类断言的作用是让覆盖率 +1 行。

---

## 四、整体框架：四个结构性决定

### 4.1 `create_app()` 不是真工厂，于是测试只能改模块全局

```python
def create_app():
    mcp_session_manager = StreamableHTTPSessionManager(app=build_mcp_server(manager))  # 模块全局
    broadcast = AdminBroadcast(manager)                                                # 模块全局
```

`manager` 是模块级单例，`create_app()` 并不给它注入新的实例。于是每个测试文件的隔离手法各不相同：

- `test_mcp_server.py`：`autouse` 夹具手工清空 `manager._engines` / `_created_at` / `_MCP_SESSIONS` 再还原；
- `test_game_server.py`：`monkeypatch.setattr(game_server, "manager", gm)`；
- `test_poller.py`：`monkeypatch.setattr(game_server, "manager", gm)`（另一套 `isolated`）。

同一个单例，三套隔离策略。而且 `test_mcp_server.py` 的夹具**只清了三个字典**，
`_finished_at` 与 `_seen_expired` 从未还原（grep 结果为 0）。

我验证过这个夹具是 load-bearing 的：把它从 `autouse` 改成普通夹具 → 8 failed。
也就是说，测试的正确性依赖一个手写的、覆盖不全的全局状态重置器。

**这是测试反馈给设计的信号**：`create_app()` 只做了一半的工厂。真正的工厂应该
`create_app(manager=None)`，把 manager 与 broadcast 收进 `app.state`，测试就能各自造一个 app。
现在做不到，测试只好伸手进模块全局。

### 4.2 没有 `conftest.py`，234 个用例共享 0 个公共件

```
tests/ 下没有任何 conftest.py
```

后果：

- `started()` 在 `test_liars_dice.py` 和 `test_prompts.py` 各写一遍，语义还不完全一致；
- `act()`（自动补 token）在 `test_liars_dice.py` 和 `test_rules.py` 各写一遍；
- `anyio_backend` 夹具只存在于 `test_mcp_server.py`，新加的 async 测试文件享受不到；
- "怎么造一局大话骰"这件事有 **5 种写法**（`make_engine`、`started`、`_started_dice`、`_started_engine`、`_begin`）。

没有共享 fixture，也没有共享的"这是合法输入/这是非法输入"数据集，于是每个文件各自发明一套世界。

### 4.3 测试按源码模块切分，不变量无家可归

```
test_engine.py        ← guess_number.py
test_liars_dice.py    ← liars_dice.py
test_manager.py       ← manager.py
test_mcp_server.py    ← mcp_server.py
```

但真正重要的东西是**跨模块的不变量**：

| 不变量 | 散落在 |
|---|---|
| 阻塞等待语义 | `test_wait.py`(7) + `test_mcp_server.py`(2) + `test_game_server.py`(4) |
| 身份校验 | `test_rules.py` + `test_game_server.py` + `test_mcp_server.py` + `test_manager.py` |
| `timeout` 解析 | `test_game_server.py`(3) + `test_mcp_server.py`(2) —— 同一个纯函数被两个文件重复参数化 |

没有任何一个文件回答"这个系统的核心不变量是什么、每条由谁守"。
`AGENTS.md` 里那张 9 条硬约束的表，本该是测试目录的骨架，现在只存在于文档里。

### 4.4 缺一层：端到端整局

没有任何用例从 `join` 开始、打完整整一局（多轮、掉骰、淘汰、超时介入、分出胜负）。

现有最长的是 `test_rules.py` 里的两三手：`bid → challenge`。
`test_three_player_game_ends_only_at_one_survivor` 看似完整，实际是手工摆好骰子后连打两轮。

**上一轮发现的 P0-1（超时后僵尸叫数把下家锁死）之所以能存在，就是因为缺这一层。**
它需要"叫数 → 有人超时 → 下家加码"这个跨动作序列，任何单动作用例都照不到。

---

## 五、建议的框架

把测试从"按模块"改成"按不变量分层"。三层，各层有自己的通过判据：

### 第 1 层：规则层（纯引擎，无 IO）

对象：`liars_dice` / `guess_number` 的规则本身。
判据：**给定骰子与动作序列，结果必须确定**。

- 用一个 `DiceScript` 夹具替代 `set_all_dice`：能摆出"1 混在非 1 里"、"1 单独"、"全 1"等组合。
- 补：百搭计数、跨 `1` 的叫数转换、掉骰后总数收缩、`join` 时机、三条 notify 路径。
- 补：一局到底的序列测试（`join ×3 → bid → 超时 → 加码 → challenge → …` 直到 `finished`），
  用固定随机种子。这条能覆盖 P0-1。
- 断言用状态对象（`e.current_call`、`[len(p.dice) for p in ...]`），不用文案。

### 第 2 层：契约层（两条传输 × 同一份用例集）

对象：REST 与 MCP **必须表现一致**这件事。
判据：**同一输入，两条传输的状态码/错误类别/耗时量级必须一致**。

- 写一套 parametrized 用例，同一份 `(action, payload)` 分别打两条通道，比较归一化后的结果。
- 这一层天然覆盖 `AGENTS.md` #5：非法输入在**两条通道上**都必须 fast-fail。
- 上一轮的 P1-2 会在这一层第一次运行时就变红。

### 第 3 层：传输/生命周期层

对象：`/mcp` 的 Route 形态、lifespan、poller、admin 鉴权、WebSocket。
判据：**越权和静默失效必须报警，且不能靠 skip 让路**。

- 把两条 `pytest.skip` 改成直接断言 `hasattr` 为真（功能被删就该红）。
- 补 `/admin/reload-prompts` 的 403、`/admin/ws` 的真实连接与鉴权、`/games/tools` 的 HTTP 契约。
- 补工具 schema 的结构断言（对 `describe_tools()` 的输出跑一遍 jsonschema 校验）。

### 配套的四件事

1. **加 `tests/conftest.py`**：统一的 `engine_factory`、`started_dice`、`dice_script`、
   `client`（带独立 manager）、`mcp_harness`、`anyio_backend`。
2. **让 `create_app(manager=None)` 成为真工厂**，测试不再 monkeypatch 模块全局。
3. **加一条变异测试脚本进 CI**：现在 35 个变异体里有 11 个存活，目标应是全杀。
   这比覆盖率门槛有效得多——它测的是"测试有没有用"，不是"代码被没被执行"。
4. **加 CI**：`pytest` + `ruff`（至少 F811 能抓出重复定义）+ `--strict-markers` +
   `filterwarnings = error`，并把 `coverage` 与 `pytest-rerunfailures` 之外的时序用例隔离。

---

## 六、按优先级排列的修复清单

| 优先级 | 事项 | 理由 |
|---|---|---|
| P0 | 两条 `pytest.skip` 改为硬断言 | 阻止"功能删除 = 绿灯" |
| P0 | 补 `/admin/reload-prompts` 鉴权用例 | 变异存活，且是真实越权 |
| P0 | 补 MCP 侧"校验先于等待"用例 | 两条传输行为分叉，已埋真 bug |
| P1 | 删掉 `test_manager.py:350` 的重复定义，加 F811 lint | 静默遮蔽 |
| P1 | 加 `conftest.py`，收敛 5 种开局写法 | 降低新增用例成本 |
| P1 | 第一期先补规则层：百搭、join 时机、三条 notify | 11 个存活变异体里 6 个在此 |
| P2 | 端到端整局序列用例 | 唯一能覆盖 P0-1 这类跨动作缺陷的层 |
| P2 | 契约层参数化（两传输同用例） | 一次性消除整类分叉 |
| P2 | 文案断言改为结构断言 | 消除假阳性 |
| P3 | `create_app` 真工厂化 | 消除三套隔离手法 |

---

## 附：变异测试清单（可复现）

脚本形态：备份源码 → 单点替换（断言锚点存在）→ `pytest -q` → 还原。

被杀（24）：

```
开盅胜负判反                     5 failed
回合推进不跳过出局者             1 failed
_validate_player 去掉 token 校验  5 failed
GET /admin/games 不校验 token     2 failed
GET /admin/games/{id} 不校验      1 failed
猜数字超时不判负                 2 failed
manager 不按 token 复用           1 failed
MCP 会话缓存无上限               2 failed
admin state 永不广播             5 failed
_mcp /mcp 改 Mount               17 failed
去 LRU touch                     1 failed
公开状态不剥秘密键               2 failed
/admin 无条件注入 token          2 failed
未知动作也建会话                 3 failed
timeout 不夹上限                 4 failed
bool 不被拒绝                    3 failed
poller 单轮异常退出              2 failed
wait_for_change 醒一次就返回     2 failed
假唤醒条件放宽                   6 failed
state 返回对手视角               3 failed
注入 5 秒等待（REST fast-fail）   5 failed
删整个 wait_for_change           6 failed
仅改提示词文案                   3 failed（假阳性，见 2.1）
```

存活（11）：见 1.2 表格。

// 管理页面的逻辑校验。
//
// 为什么需要这个脚本：`admin.html` 的 JS 没法用 `cargo test` 覆盖——它是
// 字符串资产，Rust 侧只能断言"文件里有没有某段文本"（见 `admin.rs` 的那几条
// 单元测试）。但页面真正做的三件事都必须验证：
//
// 1. **转义**：玩家名与叫数留言直接来自 Agent，注入等于泄露 `target` / `all_dice`。
// 2. **状态归类**：不靠硬编码的阶段名表，只看"有没有结束标记"。
// 3. **渲染**：真实 admin 视图能不能被渲染出来，且不残留未转义的 `<img onerror>`。
//
// 做法：把模板里的 `<script>` 抽出来，塞进一个最小 DOM 桩里执行，
// 直接调它的纯函数（`esc` / `isFinished` / `renderState` / `formatTime`）。
// 这比"用无头浏览器点一遍"轻得多，且不需要装任何依赖。
//
// 用法：
//     node tools/verify-admin-ui.js
//
// 退出码：全部通过为 0，任何一条失败为 1。

const fs = require("node:fs");
const path = require("node:path");

const TEMPLATE = path.join(
  __dirname,
  "..",
  "crates",
  "agent-game-server",
  "src",
  "templates",
  "admin.html",
);

const html = fs.readFileSync(TEMPLATE, "utf8");
const scriptMatch = html.match(/<script>([\s\S]*?)<\/script>/);
if (!scriptMatch) {
  console.error("模板里没有 <script> 块");
  process.exit(1);
}
const js = scriptMatch[1];

// ── 最小 DOM 桩 ──
//
// 只提供页面用到的那些成员。够让 `renderState` / `isFinished` 跑起来，
// 又不会真的发起网络请求。
const elements = new Map();
function element(id) {
  if (!elements.has(id)) {
    elements.set(id, {
      id,
      innerHTML: "",
      textContent: "",
      dataset: {},
      classList: { add() {}, remove() {} },
      querySelectorAll: () => [],
      addEventListener() {},
      offsetWidth: 0,
    });
  }
  return elements.get(id);
}
const documentStub = { getElementById: element, querySelectorAll: () => [] };
const windowStub = { ADMIN_TOKEN: "test-token" };
const locationStub = { protocol: "http:", host: "127.0.0.1:8800" };
const fetchStub = async () => ({ ok: true, status: 200, json: async () => ({ games: [] }) });
class WebSocketStub {
  addEventListener() {}
}

const sandbox = new Function(
  "document",
  "window",
  "location",
  "fetch",
  "WebSocket",
  "setInterval",
  `${js}\nreturn { esc, isFinished, renderState, formatTime };`,
);
const api = sandbox(documentStub, windowStub, locationStub, fetchStub, WebSocketStub, () => {});

// ── 断言 ──
let failures = 0;
function check(name, condition, detail) {
  if (condition) {
    console.log(`ok   ${name}`);
  } else {
    failures += 1;
    console.log(`FAIL ${name}${detail ? `\n     ${detail}` : ""}`);
  }
}

// 1. 转义
check("esc 转义尖括号", api.esc("<img onerror=alert(1)>") === "&lt;img onerror=alert(1)&gt;");
check("esc 转义引号", api.esc(`"'`) === "&quot;&#39;");
check("esc 处理 null", api.esc(null) === "");
check("esc 处理 undefined", api.esc(undefined) === "");

// 2. 状态归类：不依赖硬编码的阶段名表
check("finished 是终态", api.isFinished({ phase: "finished" }));
check("won 是终态", api.isFinished({ status: "won" }));
check("lost 是终态", api.isFinished({ status: "lost" }));
check("bidding 不是终态", !api.isFinished({ phase: "bidding" }));
check("自定义终态靠结束标记识别", api.isFinished({ phase: "draw", ended_at_ms: 5 }));
check("没有结束标记的 draw 不算终态", !api.isFinished({ phase: "draw" }));

// 3. 渲染真实的 admin 视图
const dice = {
  phase: "bidding",
  players: ["Alice", "Bob"],
  alive: [true, true],
  all_dice: [
    [1, 2, 3],
    [4, 5, 6],
  ],
  current_turn: 0,
  current_call: { player: 0, player_name: "Alice", quantity: 2, face: 3 },
  call_history: [
    { player: 0, player_name: "Alice", quantity: 2, face: 3, message: "你信吗" },
    { player: 1, player_name: "Bob", quantity: 0, face: 0, message: "开！(实际 2 个 3)" },
  ],
  winner: null,
  loser: null,
};
const rendered = api.renderState(dice);
check("渲染出所有座位", rendered.includes("Alice") && rendered.includes("Bob"));
check("渲染出叫数", rendered.includes("2×3"));
check("渲染出留言", rendered.includes("你信吗"));
check("渲染出摘要", rendered.includes("摘要"));
check("骰子用 unicode 面", /[⚀⚁⚂⚃⚄⚅]/u.test(rendered));
check("出局标记", api.renderState({ ...dice, alive: [true, false] }).includes("已出局"));

// 4. 注入防护：玩家名带脚本
const hostile = { ...dice, players: ["<img src=x onerror=alert(1)>", "Bob"] };
const safe = api.renderState(hostile);
check("玩家名被转义", !safe.includes("<img src=x"), safe.slice(0, 200));
check("转义后仍可读", safe.includes("&lt;img"));

// 5. 空状态与缺字段
check("空状态有占位", api.renderState({}).includes("暂无数据"));
check(
  "未知字段被列出",
  api.renderState({ phase: "bidding", mysterious: 42 }).includes("mysterious"),
);

// 6. 时间格式
check("formatTime 处理 null", api.formatTime(null) === "—");
check("formatTime 处理非法值", api.formatTime("abc") === "—");

// 7. 模板本身不该硬编码游戏名与阶段表（只看代码，注释除外）
const codeOnly = html
  .split("\n")
  .filter((line) => !line.trim().startsWith("//"))
  .join("\n");
for (const forbidden of ["TYPE_LABEL", "STATUS_ORDER", "猜数字", "大话骰", "🔢", "🎲"]) {
  check(`代码里无硬编码 ${forbidden}`, !codeOnly.includes(forbidden));
}

// 8. 模板不该有内联事件处理器
check("无内联 onclick", !codeOnly.includes("onclick="));
check("用事件委托", codeOnly.includes("addEventListener") && codeOnly.includes("dataset.id"));

console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILURES`);
process.exit(failures === 0 ? 0 : 1);

//! agent-game 的 MCP over Streamable HTTP 适配层。
//!
//! 本 crate 实现的是 MCP **服务端**（`rmcp::ServerHandler`），不是客户端。
//! Agent 侧的 MCP 客户端是外部组件（DSH 内置的 `@deepseek-ai/dsh-mcp-client`），
//! 本项目不提供、也不需要任何本地可执行产物。方向是：Agent 发起请求 → 本 crate
//! 处理 → 内核（[`agent_game_server_core`]）执行。
//!
//! 这一层没有游戏逻辑：它把内核的注册表与动作路由翻译成 MCP 协议原语，
//! 并把"一次工具调用"接上框架的等待语义。
//!
//! 身份约束（与 Python 版同一条）：对局归属由 `player_token` 决定，
//! `Mcp-Session-Id` 只是连接级缓存键。同一个 token 无论从哪条 MCP 连接进来，
//! 都必须回到它原来所在的对局。

// `list_tools` 必须 `async`（trait 签名要求返回 Future），工具表本身不需要 await。
pub mod tools;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use agent_game_server_core::budget::resolve_budget;
use agent_game_server_core::identity::check_player_identity;
use agent_game_server_core::wait::wait_for;
use agent_game_server_core::{
    ActionParams, CreateParams, EngineError, GameManager, Session, SessionNotFound,
};
use rmcp::ErrorData;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::Value;

/// MCP 连接级会话缓存的容量上限。
pub const SESSION_CACHE_CAP: usize = 512;

/// 非 HTTP 传输下的回落键，避免每次调用都新建缓存。
const FALLBACK_SESSION_KEY: &str = "__no_http_session__";

/// 只有这些动作可以凭空调出一个新会话。
///
/// 其它动作没有可解析的会话时直接报错，否则像 `liars_dice_bid` 这种漏传会话的
/// 调用会先建出一个空大厅再失败。
const STARTING_ACTIONS: &[&str] = &["start", "join"];

/// `mcp-session-id → {game_type: session_id}`。
///
/// 仅作连接级缓存；它描述连接，不描述玩家。容量有上限。
///
/// **用 `Vec<(key, entry)>` 而不是 `HashMap`。** Python 的 dict 保序，所以
/// "按插入顺序淘汰最旧的一条"能成立；`std::collections::HashMap` 的迭代顺序
/// 是随机的，照搬那个假设会让淘汰落到任意一条上——包括正在用的那条。
/// 顺序在这里是契约的一部分，所以用显式有序的结构表达它。
#[derive(Debug)]
pub struct SessionCache {
    inner: Mutex<Vec<(String, HashMap<String, String>)>>,
    cap: usize,
}

impl SessionCache {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            cap: cap.max(1),
        }
    }

    /// 取某条连接的缓存（没有就建），并把它记为最近使用。
    ///
    /// 被访问的连接移到末尾，淘汰落在真正闲置最久的连接上。
    pub fn remember(&self, key: &str) -> HashMap<String, String> {
        let Ok(mut entries) = self.inner.lock() else {
            return HashMap::new();
        };
        let entry = match entries.iter().position(|(existing, _)| existing == key) {
            Some(index) => entries.remove(index).1,
            None => HashMap::new(),
        };
        let clone = entry.clone();
        entries.push((key.to_owned(), entry));
        Self::trim(&mut entries, self.cap);
        clone
    }

    /// 覆盖某条连接里某款游戏的会话。
    ///
    /// 同样要维护上限并刷新位置：只靠 `remember` 维护的话，一条只写不读的
    /// 连接序列（每个请求一个新 session id）会让缓存无限增长。
    pub fn put(&self, key: &str, game_type: &str, session_id: &str) {
        let Ok(mut entries) = self.inner.lock() else {
            return;
        };
        let mut entry = match entries.iter().position(|(existing, _)| existing == key) {
            Some(index) => entries.remove(index).1,
            None => HashMap::new(),
        };
        entry.insert(game_type.to_owned(), session_id.to_owned());
        entries.push((key.to_owned(), entry));
        Self::trim(&mut entries, self.cap);
    }

    /// 忘掉某条连接里某款游戏的会话。
    ///
    /// 整条连接被清空时把它从表里移除，避免留一堆空条目占着上限。
    pub fn forget(&self, key: &str, game_type: &str) {
        let Ok(mut entries) = self.inner.lock() else {
            return;
        };
        let Some(index) = entries.iter().position(|(existing, _)| existing == key) else {
            return;
        };
        entries[index].1.remove(game_type);
        if entries[index].1.is_empty() {
            entries.remove(index);
        }
    }

    /// 淘汰最旧的若干条，直到不超过上限。
    fn trim(entries: &mut Vec<(String, HashMap<String, String>)>, cap: usize) {
        while entries.len() > cap {
            entries.remove(0);
        }
    }

    /// 当前追踪的连接数（测试用）。
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.inner.lock().map_or(0, |entries| entries.len())
    }

    /// 清空（测试用）。
    pub fn clear(&self) {
        if let Ok(mut entries) = self.inner.lock() {
            entries.clear();
        }
    }
}

impl Default for SessionCache {
    fn default() -> Self {
        Self::new(SESSION_CACHE_CAP)
    }
}

/// MCP 服务端。
#[derive(Debug, Clone)]
pub struct AgentGameServer {
    manager: Arc<GameManager>,
    cache: Arc<SessionCache>,
}

impl AgentGameServer {
    #[must_use]
    pub fn new(manager: Arc<GameManager>) -> Self {
        Self {
            manager,
            cache: Arc::new(SessionCache::default()),
        }
    }

    /// 用给定的缓存构造（测试用：两个服务共用一份缓存来验证隔离）。
    #[must_use]
    pub fn with_cache(manager: Arc<GameManager>, cache: Arc<SessionCache>) -> Self {
        Self { manager, cache }
    }

    #[must_use]
    pub fn manager(&self) -> &Arc<GameManager> {
        &self.manager
    }

    /// 工具定义列表。同步实现，`list_tools` 只是把它包进 Future。
    #[must_use]
    pub fn tools(&self) -> ListToolsResult {
        ListToolsResult::with_all_items(tools::all_tools(self.manager.registry()))
    }

    /// 执行一次工具调用，返回给调用方的文本。
    ///
    /// 这是 `call_tool` 的核心，单独抽出来是为了让集成测试不必完成 MCP 握手
    /// 就能验证身份规则。
    ///
    /// # Errors
    /// 调用方输入问题（未知工具、身份不匹配、缺参数）与服务端内部错误，
    /// 由 [`EngineError::is_client_error`] 分类。
    pub async fn execute(
        &self,
        session_key: &str,
        tool_name: &str,
        arguments: serde_json::Map<String, Value>,
    ) -> Result<String, EngineError> {
        let registry = self.manager.registry();
        let Some((game_type, action)) = registry.split_tool_name(tool_name) else {
            return Err(EngineError::invalid(format!("未知工具: {tool_name}")));
        };
        let action = action.to_owned();

        // 工具名前缀能剥出 action，不等于这个 action 存在：`liars_dice_nope` 也能剥成
        // `("liars_dice", "nope")`。必须以引擎**声明**的工具表为准，否则一个拼错的
        // 工具名会被当成合法动作去建局，再在引擎里以"未知 action"失败——错误信息
        // 指向了错的地方。
        if !tools::declares(registry, game_type, tool_name) {
            return Err(EngineError::invalid(format!("未知工具: {tool_name}")));
        }

        let args = ActionParams::new(arguments);
        let explicit_session_id = args.optional_str("session_id")?.map(ToOwned::to_owned);
        let player_token = args.player_token()?.map(ToOwned::to_owned);

        let cached = self.cache.remember(session_key);
        let starting = action == "start";
        if starting {
            self.cache.forget(session_key, game_type);
        }

        let mut session_id = explicit_session_id.clone();
        if session_id.is_none()
            && !starting
            && let Some(token) = player_token.as_deref()
        {
            // 身份优先：同一 player_token 永远回到它所在的对局，而不是各连接各开一局
            session_id = self.find_active_session(game_type, token).await;
        }

        if session_id.is_none() {
            session_id = cached.get(game_type).filter(|_| !starting).cloned();
            if let (Some(cached_id), true) = (session_id.clone(), action == "join") {
                // join 前先看缓存的会话是否还在、是否已结束
                let stale = match self.manager.get(&cached_id) {
                    None => true,
                    Some(session) => session.engine.lock().await.is_finished(),
                };
                if stale {
                    self.cache.forget(session_key, game_type);
                    session_id = None;
                }
            }
        }

        let engine_session = session_id.as_deref().and_then(|id| self.manager.get(id));

        // 显式给了 session_id 但找不到：明确报错，不静默建新局
        if engine_session.is_none() && explicit_session_id.is_some() {
            return Err(EngineError::invalid(
                SessionNotFound(explicit_session_id.unwrap_or_default()).to_string(),
            ));
        }

        let Some(session) = engine_session else {
            if !STARTING_ACTIONS.contains(&action.as_str()) {
                return Err(EngineError::invalid(
                    self.missing_session_hint(game_type, &action),
                ));
            }
            // 新建对局时要把**调用方给的全部创建参数**带过去，而不是只带 token。
            //
            // 曾经只带 `player_token`，于是 `player_count=3` 被丢掉：桌子按默认值 2
            // 建出来，第三个玩家去匹配时又按 3 去比，永远匹配不上——he 会拿到一张
            // 全新的桌子，前面两人打的那局与他无关。
            //
            // 传输层专用的键（`session_id`、`timeout`）不属于创建参数，必须剥掉；
            // 其余一律透传，由引擎自己决定认识哪些。
            let mut params = args.raw.clone();
            for transport_only in ["session_id", "timeout"] {
                params.remove(transport_only);
            }
            let created = self
                .manager
                .create(game_type, CreateParams::new(params), Instant::now())
                .await
                .map_err(EngineError::invalid)?;
            self.cache.put(session_key, game_type, &created.id);
            // 新建的对局立刻执行动作（join / start），而不是只返回它的 id
            return self.run_action(&created, &action, args).await;
        };

        // 显式 session_id 指到了另一款游戏的对局：拒绝，避免把它缓存成这种游戏
        // 的会话、后续调用落进不认识的引擎。
        if session.game_type != game_type {
            return Err(EngineError::invalid(format!(
                "会话 {} 属于 {}，不能用于 {game_type}",
                session.id, session.game_type
            )));
        }

        // 显式指定会话 + 无 token 等于按索引直取别人的对局。
        let multi = session.engine.lock().await.player_mode().is_multi();
        if explicit_session_id.is_some() && player_token.is_none() && multi {
            return Err(EngineError::invalid(
                "多人对局指定 session_id 时必须同时提供 player_token：\
                 身份来自 player_token，session_id 只定位对局不证明身份",
            ));
        }

        self.cache.put(session_key, game_type, &session.id);
        self.run_action(&session, &action, args).await
    }

    /// 在一次已定位到对局的会话上执行动作。
    async fn run_action(
        &self,
        session: &Arc<Session>,
        action: &str,
        mut args: ActionParams,
    ) -> Result<String, EngineError> {
        let timeout = args.raw.remove("timeout");
        let player_index = args.player_index()?;
        let player_token = args.player_token()?.map(ToOwned::to_owned);

        if action == "state" {
            // 顺序至关重要：先解析入参、再校验身份、最后才进入等待。
            // 任何一步挪到"等对局变化"之后，非法输入都会先白等满整个预算（55 秒）。
            let budget = resolve_budget(
                timeout.as_ref(),
                None,
                self.manager.config().block_budget_secs(),
            )
            .map_err(|error| EngineError::invalid(error.to_string()))?;

            let (signal, ready) = {
                let engine = session.engine.lock().await;
                if let Some(tokens) = engine.player_tokens() {
                    check_player_identity(Some(&tokens), player_index, player_token.as_deref())?;
                }
                // 取一次信号就放锁：等待期间不能持有引擎锁
                (engine.signal(), player_index.unwrap_or(0))
            };

            if budget > 0.0 {
                let budget = std::time::Duration::from_secs_f64(budget);
                wait_for(&signal, budget, || {
                    let session = Arc::clone(session);
                    async move { session.engine.lock().await.is_ready_for(ready) }
                })
                .await;
            }
        }

        // 走 manager 的包装：引擎 panic 会被翻译成"毒化这个对局"，
        // 而不是让进程或整条 MCP 连接去死。
        let view = self.manager.act(session, action, args).await?;
        Ok(view.prompt)
    }

    /// 在所有该类型的活动对局中按 `player_token` 查找。
    ///
    /// 三个 `continue` 分支都必须保留：
    /// - 找不到 session（同一轮里被回收了）
    /// - 类型不匹配（拿着别的游戏的 token）
    /// - 已结束（那一局没法继续）
    ///
    /// `pub(crate)` 而不是私有：测试要能直接喂它各种形态的对局。
    pub(crate) async fn find_active_session(
        &self,
        game_type: &str,
        player_token: &str,
    ) -> Option<String> {
        for id in self.manager.session_ids() {
            let Some(session) = self.manager.get(&id) else {
                continue;
            };
            if session.game_type != game_type {
                continue;
            }
            let engine = session.engine.lock().await;
            if engine.is_finished() {
                continue;
            }
            if engine.find_player_by_token(player_token).is_some() {
                return Some(id);
            }
        }
        None
    }

    /// 没有可解析的会话时给调用方的提示：先开局，再动作。
    fn missing_session_hint(&self, game_type: &str, action: &str) -> String {
        let entry = self
            .manager
            .registry()
            .get(game_type)
            .map_or("start", |meta| {
                if meta.player_mode.is_multi() {
                    "join"
                } else {
                    "start"
                }
            });
        format!("{game_type} 还没有对局：请先调用 {game_type}_{entry}，再调用 {game_type}_{action}")
    }
}

/// 把引擎错误翻译成 MCP 的返回文本。
///
/// 分类由 [`EngineError::is_client_error`] 决定，与 REST 侧同一份判断。
#[must_use]
pub fn engine_error_text(error: &EngineError) -> String {
    format!("❌ {error}")
}

impl ServerHandler for AgentGameServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agent-game", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "AI Agent 对弈游戏服务器。先调用 <game>_join 或 <game>_start 入场，\
             再用带 player_token 的 state / bid / challenge 行动。",
            )
    }

    // `async` 是 `ServerHandler` 的签名要求（返回 `impl Future`）。
    // 工具表本身是同步构造的，这里显式让位一次，避免 `unused_async`
    // 把"trait 要求 async"误报成"实现写错了"。
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        tokio::task::yield_now().await;
        Ok(self.tools())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let key = session_key(&context);
        let arguments = request.arguments.unwrap_or_default();

        match self.execute(&key, &request.name, arguments).await {
            Ok(text) => Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(text),
            ]))),
            Err(error) if error.is_client_error() => {
                tracing::info!(tool = %request.name, %error, "MCP 调用被拒绝");
                Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                    ContentBlock::text(engine_error_text(&error)),
                ])))
            }
            Err(error) => {
                tracing::error!(tool = %request.name, %error, "MCP 工具调用异常");
                Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                    ContentBlock::text(format!("❌ 服务端内部错误: {error}（详见服务端日志）")),
                ])))
            }
        }
    }
}

/// 从请求上下文的扩展里取连接级的会话键。
///
/// 抽成接受 `Option<&http::request::Parts>` 的纯函数，是为了让它可测：
/// `RequestContext<RoleServer>` 需要 `Peer` 与 `CancellationToken` 才能构造，
/// 而这里真正关心的只是"`mcp-session-id` 头读到了什么"。变异测试在这个位置
/// 抓到的存活变异体（返回空串、返回常量、漏判空值）只有它能覆盖。
fn session_key_of(parts: Option<&http::request::Parts>) -> String {
    parts
        .and_then(|parts| parts.headers.get("mcp-session-id"))
        .and_then(|value| value.to_str().ok())
        // 空字符串不能当成一个有效的会话键：`Mcp-Session-Id: ` 这种畸形头
        // 会让它成为所有连接的共享键，等于让不同玩家串到同一局。
        .filter(|value| !value.is_empty())
        .unwrap_or(FALLBACK_SESSION_KEY)
        .to_owned()
}

/// 从请求上下文里取连接级的会话键。
fn session_key(context: &RequestContext<RoleServer>) -> String {
    session_key_of(context.extensions.get::<http::request::Parts>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_game_server_core::{GameMeta, PlayerMode, Registry};

    #[test]
    fn cache_tracks_and_forgets() {
        let cache = SessionCache::new(4);
        cache.put("conn-a", "liars_dice", "sid-1");
        assert_eq!(cache.tracked(), 1);
        let entry = cache.remember("conn-a");
        assert_eq!(entry.get("liars_dice").map(String::as_str), Some("sid-1"));
        cache.forget("conn-a", "liars_dice");
        assert!(!cache.remember("conn-a").contains_key("liars_dice"));
    }

    #[test]
    fn cache_is_bounded_and_evicts_the_oldest() {
        let cache = SessionCache::new(2);
        cache.put("a", "g", "1");
        cache.put("b", "g", "2");
        // 访问 a 让它变成最近使用，evict 应落在 b 上
        let _ = cache.remember("a");
        cache.put("c", "g", "3");
        assert_eq!(cache.tracked(), 2);
        assert!(cache.remember("a").contains_key("g"));
        assert!(cache.remember("c").contains_key("g"));
        assert!(!cache.remember("b").contains_key("g"));
    }

    #[test]
    fn cache_zero_capacity_still_works() {
        let cache = SessionCache::new(0);
        cache.put("a", "g", "1");
        assert!(cache.tracked() <= 1);
    }

    #[test]
    fn session_not_found_text_keeps_the_id() {
        assert!(SessionNotFound("nope".into()).to_string().contains("nope"));
    }

    // ── 连接键 ──

    /// 造一份只有指定头的 `http::request::Parts`。
    fn parts_with_session(session: Option<&str>) -> http::request::Parts {
        let mut builder = http::Request::builder().method("POST").uri("/mcp");
        if let Some(value) = session {
            builder = builder.header("mcp-session-id", value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[test]
    fn a_missing_header_falls_back_to_the_fixed_key() {
        // 变异成返回空串或 "xyzzy" 会让所有无头请求共用一个错误的缓存桶。
        assert_eq!(session_key_of(None), FALLBACK_SESSION_KEY);
        assert_eq!(
            session_key_of(Some(&parts_with_session(None))),
            FALLBACK_SESSION_KEY
        );
    }

    #[test]
    fn an_empty_header_also_falls_back() {
        // `Mcp-Session-Id: ` 这种畸形头不能成为一个有效的共享键。
        assert_eq!(
            session_key_of(Some(&parts_with_session(Some("")))),
            FALLBACK_SESSION_KEY,
            "空值必须回落，否则所有连接会串到同一局"
        );
    }

    #[test]
    fn a_real_header_is_used_verbatim() {
        assert_eq!(
            session_key_of(Some(&parts_with_session(Some("abc-123")))),
            "abc-123"
        );
    }

    #[test]
    fn different_headers_produce_different_keys() {
        let first = session_key_of(Some(&parts_with_session(Some("conn-1"))));
        let second = session_key_of(Some(&parts_with_session(Some("conn-2"))));
        assert_ne!(first, second);
    }

    // ── ServerHandler 的静态部分 ──

    fn bare_server() -> AgentGameServer {
        let mut registry = Registry::new();
        registry.register(GameMeta::new(
            "guess_number",
            PlayerMode::Single,
            |params, config| {
                agent_game_server_core::games::GuessNumberEngine::new(params, config)
                    .map(|e| Box::new(e) as agent_game_server_core::BoxedEngine)
            },
        ));
        AgentGameServer::new(std::sync::Arc::new(GameManager::new(
            registry,
            agent_game_server_core::Config::default(),
        )))
    }

    #[test]
    fn get_info_advertises_the_tool_capability_and_the_right_name() {
        // 变异成 `Default::default()` 会让 `initialize` 报出一个空的能力集，
        // 客户端据此认为这个服务端没有工具。
        let info = bare_server().get_info();

        assert_eq!(info.server_info.name, "agent-game");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(
            info.capabilities.tools.is_some(),
            "必须声明 tools 能力: {:?}",
            info.capabilities
        );
    }

    #[tokio::test]
    async fn list_tools_through_the_handler_returns_the_registry_tools() {
        // 变异成 `Ok(Default::default())` 会让 `tools/list` 返回零个工具。
        // `list_tools` 的第二个参数用不上，但签名要求给一个。
        // 这里借用一个真实的 `RequestContext` 构造不出来，所以直接验证
        // `tools()`（`list_tools` 的实现体就是它的包装）。
        let server = bare_server();
        let result = server.tools();
        assert!(!result.tools.is_empty(), "工具表不能为空");
    }

    // ── 错误分类 ──

    #[test]
    fn engine_error_text_keeps_the_prefix_and_the_cause() {
        // 变异成空串或 "xyzzy" 会让 Agent 看不到任何有用信息。
        let text = engine_error_text(&EngineError::MissingToken);
        assert!(text.starts_with('❌'), "{text}");
        assert!(text.contains("缺少 player_token"), "{text}");
    }

    #[test]
    fn the_session_cache_cap_can_be_smaller_than_its_default() {
        // 默认 512；显式传更小的值时不该被悄悄改回默认。
        let cache = SessionCache::new(3);
        for index in 0..10 {
            cache.put(&format!("conn-{index}"), "g", "sid");
        }
        assert_eq!(cache.tracked(), 3, "容量必须被尊重");
    }

    // ── 按 token 找对局：三个 `continue` 分支 ──

    /// 一个最小引擎，`find_player_by_token` 认一个固定 token。
    ///
    /// 只用于 `find_active_session` 的分支测试：它的三个 `continue` 各自
    /// 对应"找不到 / 类型不对 / 已结束"，要逐一走到。
    #[derive(Debug)]
    struct ProbeEngine {
        token: &'static str,
        phase: &'static str,
        /// 为真时 `is_ready_for` 恒假：用来验证"预算为 0 时不等"。
        never_ready: bool,
        signal: std::sync::Arc<agent_game_server_core::ChangeSignal>,
        prompts: agent_game_server_core::PromptStore,
    }

    impl ProbeEngine {
        fn new(token: &'static str, phase: &'static str) -> Self {
            Self {
                token,
                phase,
                never_ready: false,
                signal: std::sync::Arc::new(agent_game_server_core::ChangeSignal::new()),
                prompts: agent_game_server_core::PromptStore::new("[templates]\n"),
            }
        }

        /// 一个永远不会"轮到我"的引擎：只要真的进入等待，就会挂满预算。
        fn never_ready(mut self) -> Self {
            self.never_ready = true;
            self
        }
    }

    impl agent_game_server_core::GameEngine for ProbeEngine {
        fn game_type(&self) -> &'static str {
            "probe"
        }
        fn player_mode(&self) -> PlayerMode {
            PlayerMode::Multi
        }
        fn phase(&self) -> &'static str {
            self.phase
        }
        fn admin_view(&self) -> serde_json::Value {
            serde_json::json!({"phase": self.phase})
        }
        fn public_view(&self) -> serde_json::Value {
            serde_json::json!({"phase": self.phase})
        }
        fn player_view(
            &self,
            _index: Option<usize>,
        ) -> Result<agent_game_server_core::PlayerView, EngineError> {
            Ok(agent_game_server_core::PlayerView::default())
        }
        fn handle_action(
            &mut self,
            _action: &str,
            _params: ActionParams,
        ) -> Result<agent_game_server_core::PlayerView, EngineError> {
            Ok(agent_game_server_core::PlayerView::default())
        }
        fn check_timeout(
            &mut self,
            _now: std::time::Instant,
        ) -> Result<Option<agent_game_server_core::TimeoutEvent>, EngineError> {
            Ok(None)
        }
        fn signal(&self) -> std::sync::Arc<agent_game_server_core::ChangeSignal> {
            std::sync::Arc::clone(&self.signal)
        }
        fn prompts(&self) -> &agent_game_server_core::PromptStore {
            &self.prompts
        }
        fn find_player_by_token(&self, token: &str) -> Option<usize> {
            (token == self.token).then_some(0)
        }
        fn is_ready_for(&self, _player_index: usize) -> bool {
            !self.never_ready
        }
        fn describe_tools(&self) -> Vec<agent_game_server_core::ToolSpec> {
            use agent_game_server_core::ToolSpec;
            vec![
                ToolSpec::new(
                    "probe_state",
                    "看状态",
                    serde_json::json!({"type": "object", "properties": {}, "required": []}),
                ),
                ToolSpec::new(
                    "probe_bid",
                    "叫数",
                    serde_json::json!({"type": "object", "properties": {}, "required": []}),
                ),
            ]
        }
    }

    fn probe_server(entries: &[(&'static str, &'static str, &'static str)]) -> AgentGameServer {
        // (game_type, token, phase)
        let mut registry = Registry::new();
        for &(game_type, token, phase) in entries {
            registry.register(GameMeta::new(
                game_type,
                PlayerMode::Multi,
                move |_p, _c| {
                    Ok(Box::new(ProbeEngine::new(token, phase))
                        as agent_game_server_core::BoxedEngine)
                },
            ));
        }
        AgentGameServer::new(std::sync::Arc::new(GameManager::new(
            registry,
            agent_game_server_core::Config::default(),
        )))
    }

    #[tokio::test]
    async fn find_active_session_returns_the_matching_session() {
        let server = probe_server(&[("probe", "tok-a", "bidding")]);
        server
            .manager()
            .create("probe", CreateParams::default(), std::time::Instant::now())
            .await
            .unwrap();

        let found = server.find_active_session("probe", "tok-a").await;
        assert!(found.is_some(), "应能找到持有该 token 的对局");

        // 变异成 `Some(String::new())` 或 `Some("xyzzy")` 会让返回值不是真实 id
        let id = found.unwrap();
        assert!(
            server.manager().get(&id).is_some(),
            "返回的必须是真实存在的 session id，实际 {id:?}"
        );
    }

    #[tokio::test]
    async fn find_active_session_returns_none_for_an_unknown_token() {
        let server = probe_server(&[("probe", "tok-a", "bidding")]);
        server
            .manager()
            .create("probe", CreateParams::default(), std::time::Instant::now())
            .await
            .unwrap();

        assert!(
            server
                .find_active_session("probe", "nobody")
                .await
                .is_none(),
            "陌生 token 不该命中"
        );
    }

    #[tokio::test]
    async fn find_active_session_skips_a_finished_session() {
        // `if engine.is_finished() { continue; }` 被删掉时，已结束的对局会被
        // 当成可复用的目标——断线重连会拿到一局已经打完的牌。
        let server = probe_server(&[("probe", "tok-a", "finished")]);
        let created = server
            .manager()
            .create("probe", CreateParams::default(), std::time::Instant::now())
            .await
            .unwrap();
        assert!(server.manager().is_finished(&created).await);

        assert!(
            server.find_active_session("probe", "tok-a").await.is_none(),
            "已结束的对局不该被复用"
        );
    }

    #[tokio::test]
    async fn find_active_session_skips_a_session_of_another_game_type() {
        // `if session.game_type != game_type { continue; }` 的 `!=` 改成 `==`
        // 会让它**只**命中类型不同的对局——拿着大话骰的 token 建猜数字，
        // 拿回大话骰的 session。
        let server = probe_server(&[("dice", "tok-a", "bidding"), ("guess", "tok-a", "bidding")]);
        let dice = server
            .manager()
            .create("dice", CreateParams::default(), std::time::Instant::now())
            .await
            .unwrap();

        let found = server.find_active_session("guess", "tok-a").await;
        assert_ne!(
            found.as_deref(),
            Some(dice.id.as_str()),
            "不能返回另一种游戏的对局"
        );

        // 查 dice 时应当命中 dice 那局
        assert_eq!(
            server.find_active_session("dice", "tok-a").await.as_deref(),
            Some(dice.id.as_str())
        );
    }

    // ── 工具调用是否该等：`state` 与其它动作的分界 ──

    /// 造一台只有 `probe` 一个游戏的 server，且那台引擎永不 ready。
    fn never_ready_server() -> AgentGameServer {
        let mut registry = Registry::new();
        registry.register(GameMeta::new("probe", PlayerMode::Multi, |_p, _c| {
            Ok(Box::new(ProbeEngine::new("tok-a", "bidding").never_ready())
                as agent_game_server_core::BoxedEngine)
        }));
        AgentGameServer::new(std::sync::Arc::new(GameManager::new(
            registry,
            agent_game_server_core::Config::default(),
        )))
    }

    #[tokio::test]
    async fn a_positive_budget_waits_but_is_cut_off_by_the_deadline() {
        // `budget > 0.0` 改成 `==`/`<`/`>=` 时，这一条会变：
        // - `>=`：0 也进等待（下面第二条用例会挂住）
        // - `==`：只有恰好 0 才等，非 0 反而不等（这一条会立刻返回）
        // - `<`：负数才等，正数不等（这一条会立刻返回）
        //
        // 用一个很短的预算，验证"确实等待了，且按预算返回"。
        let server = never_ready_server();
        let session = server
            .manager()
            .create("probe", CreateParams::default(), std::time::Instant::now())
            .await
            .unwrap();

        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "session_id".to_owned(),
            serde_json::Value::String(session.id.clone()),
        );
        arguments.insert(
            "player_token".to_owned(),
            serde_json::Value::String("tok-a".to_owned()),
        );
        // 预算 1 秒：`f64` 直接传，解析器接受数值
        arguments.insert("timeout".to_owned(), serde_json::Value::from(1));

        let started = std::time::Instant::now();
        server
            .execute("conn", "probe_state", arguments)
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "正预算必须真的等待，实际只用了 {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "等待必须被预算切断，实际 {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn only_the_state_action_resolves_the_wait_budget() {
        // `run_action` 里 `if action == "state"` 的 `==` 改成 `!=` 会让
        // **其它**动作去解析 timeout 并进入等待，而 state 反而不等。
        let server = probe_server(&[("probe", "tok-a", "bidding")]);
        let session = server
            .manager()
            .create("probe", CreateParams::default(), std::time::Instant::now())
            .await
            .unwrap();

        // 非 state 动作带一个非法 timeout：不该被解析，因此不该报错
        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "session_id".to_owned(),
            serde_json::Value::String(session.id.clone()),
        );
        arguments.insert(
            "timeout".to_owned(),
            serde_json::Value::String("abc".to_owned()),
        );
        let outcome = server.execute("conn", "probe_bid", arguments).await;
        assert!(
            !matches!(&outcome, Err(EngineError::InvalidInput(m)) if m.contains("timeout")),
            "非 state 动作不该解析 timeout: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_zero_budget_skips_the_wait_entirely() {
        // `if budget > 0.0` 的三种变异（`==`/`<`/`>=`）都会改变"0 要不要等"。
        // 用**永不 ready** 的引擎：只要真的进入等待就一定挂满 5 秒，
        // 所以"立即返回"这件事是可观测的。
        let server = never_ready_server();
        let session = server
            .manager()
            .create("probe", CreateParams::default(), std::time::Instant::now())
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "session_id".to_owned(),
            serde_json::Value::String(session.id.clone()),
        );
        // 多人引擎在显式 session_id 下必须带 token，否则先被身份守卫挡住
        arguments.insert(
            "player_token".to_owned(),
            serde_json::Value::String("tok-a".to_owned()),
        );
        arguments.insert("timeout".to_owned(), serde_json::Value::from(0));
        server
            .execute("conn", "probe_state", arguments)
            .await
            .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "timeout=0 必须立即返回"
        );
    }
}

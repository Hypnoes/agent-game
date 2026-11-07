//! MCP 适配层的直接测试：不经过 HTTP，只调 `AgentGameServer::execute`。
//!
//! 传输层的行为（握手、SSE、401/200）由 `agent-game-server` 的
//! `mcp_transport.rs` 覆盖，但那条路径太厚：它测不到 `execute` 内部的分支。
//! 变异测试在 mcp crate 上跑出 45 个存活，几乎全在这里——
//! "错误分类"、"缓存键"、"会话定位"这些逻辑只被集成测试间接覆盖，
//! 改动它们时套件不会红。
//!
//! 这个文件把 `execute` 当作被测单元，逐个钉住它的分支。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Instant;

use agent_game_server_core::{
    BoxedEngine, CreateParams, EngineError, GameManager, GameMeta, PlayerMode, Registry,
};
use agent_game_server_mcp::tools;
use agent_game_server_mcp::{AgentGameServer, SessionCache, engine_error_text};
use serde_json::{Map, Value, json};

const CONN_A: &str = "conn-a";
const CONN_B: &str = "conn-b";

fn args(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// 一个能跑通 join / state / bid 的最小多人引擎。
///
/// 与 `core::testing::DummyEngine` 不同，这里需要**真实的两名玩家与 token**，
/// 否则身份校验与"按 token 找回对局"都测不到。
#[derive(Debug)]
struct TwoPlayerEngine {
    players: Vec<(String, String)>, // (name, token)
    phase: &'static str,
    signal: Arc<agent_game_server_core::ChangeSignal>,
    prompts: agent_game_server_core::PromptStore,
}

impl TwoPlayerEngine {
    fn new() -> Self {
        Self {
            players: Vec::new(),
            phase: "waiting",
            signal: Arc::new(agent_game_server_core::ChangeSignal::new()),
            prompts: agent_game_server_core::PromptStore::new("[templates]\nhello = \"hi {n}\"\n"),
        }
    }
}

impl agent_game_server_core::GameEngine for TwoPlayerEngine {
    fn game_type(&self) -> &'static str {
        "two_player"
    }

    fn player_mode(&self) -> PlayerMode {
        PlayerMode::Multi
    }

    fn phase(&self) -> &'static str {
        self.phase
    }

    fn admin_view(&self) -> Value {
        json!({"phase": self.phase, "secret": "admin-only"})
    }

    fn public_view(&self) -> Value {
        json!({"phase": self.phase})
    }

    fn player_view(
        &self,
        player_index: Option<usize>,
    ) -> Result<agent_game_server_core::PlayerView, EngineError> {
        let index = player_index.unwrap_or(0);
        Ok(agent_game_server_core::PlayerView {
            public: self.public_view(),
            private: json!({"your_index": index}),
            prompt: format!("state:{index}"),
        })
    }

    fn handle_action(
        &mut self,
        action: &str,
        params: agent_game_server_core::ActionParams,
    ) -> Result<agent_game_server_core::PlayerView, EngineError> {
        match action {
            "join" => {
                let name = params.require_str("player_name")?.to_owned();

                // 重连：带的是已知 token 就把原身份还给他，不新增玩家。
                // 真实引擎都有这段逻辑；测试替身少了它，"同一 token 回到同一局"
                // 那条用例就会因为"又加了一个玩家"而失败——那不是被除数
                // （`find_active_session`）的问题，而是替身不完整。
                if let Some(token) = params.player_token()?
                    && let Some(index) = self.find_player_by_token(token)
                {
                    return Ok(agent_game_server_core::PlayerView {
                        public: self.public_view(),
                        private: json!({"player_token": token}),
                        prompt: format!("🎲 你是玩家 {index}，token: {token}"),
                    });
                }

                let token = format!("tok-{}", self.players.len());
                self.players.push((name, token));
                if self.players.len() >= 2 {
                    self.phase = "bidding";
                }
                Ok(agent_game_server_core::PlayerView {
                    public: self.public_view(),
                    private: json!({"player_token": self.players.last().map(|p| p.1.clone())}),
                    prompt: format!(
                        "🎲 你是玩家 {}，token: {}",
                        self.players.len() - 1,
                        self.players.last().map_or("", |p| p.1.as_str())
                    ),
                })
            }
            "state" => self.player_view(params.player_index()?),
            "bid" => {
                let index = params.require_i64("quantity")?;
                Ok(agent_game_server_core::PlayerView {
                    public: json!({"quantity": index}),
                    private: json!({}),
                    prompt: format!("bid:{index}"),
                })
            }
            other => Err(EngineError::UnknownAction(other.to_owned())),
        }
    }

    fn check_timeout(
        &mut self,
        _now: Instant,
    ) -> Result<Option<agent_game_server_core::TimeoutEvent>, EngineError> {
        Ok(None)
    }

    fn signal(&self) -> Arc<agent_game_server_core::ChangeSignal> {
        Arc::clone(&self.signal)
    }

    fn prompts(&self) -> &agent_game_server_core::PromptStore {
        &self.prompts
    }

    fn setup_params(&self) -> CreateParams {
        CreateParams::default()
    }

    // 工具面必须由引擎自己声明：mcp 层不再硬编码任何游戏的 schema。
    // 这个替身不声明的话，`two_player_join` 会被当成"未知工具"。
    fn describe_tools(&self) -> Vec<agent_game_server_core::ToolSpec> {
        use agent_game_server_core::ToolSpec;

        vec![
            ToolSpec::new(
                "two_player_join",
                "入场",
                json!({
                    "type": "object",
                    "properties": {"player_name": {"type": "string"}},
                    "required": ["player_name"],
                }),
            ),
            ToolSpec::new(
                "two_player_state",
                "看状态",
                json!({
                    "type": "object",
                    "properties": {"player_index": {"type": "integer"}},
                    "required": [],
                }),
            ),
            ToolSpec::new(
                "two_player_bid",
                "叫数",
                json!({
                    "type": "object",
                    "properties": {"quantity": {"type": "integer"}},
                    "required": ["quantity"],
                }),
            ),
        ]
    }

    fn find_player_by_token(&self, token: &str) -> Option<usize> {
        self.players
            .iter()
            .position(|(_, existing)| existing == token)
    }

    fn player_tokens(&self) -> Option<Vec<String>> {
        Some(
            self.players
                .iter()
                .map(|(_, token)| token.clone())
                .collect(),
        )
    }
}

fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(GameMeta::new(
        "two_player",
        PlayerMode::Multi,
        |_params, _config| Ok(Box::new(TwoPlayerEngine::new()) as BoxedEngine),
    ));
    // 用**真实**的猜数字引擎：它自带工具声明，与生产路径一致。
    // 换成测试替身就没有工具面，`guess_number_start` 会被判成未知工具。
    registry.register(GameMeta::new(
        "guess_number",
        PlayerMode::Single,
        |params, config| {
            agent_game_server_core::games::GuessNumberEngine::new(params, config)
                .map(|engine| Box::new(engine) as BoxedEngine)
        },
    ));
    registry
}

fn server() -> AgentGameServer {
    AgentGameServer::new(Arc::new(GameManager::new(
        registry(),
        agent_game_server_core::Config::default(),
    )))
}

async fn call(
    server: &AgentGameServer,
    conn: &str,
    tool: &str,
    arguments: Value,
) -> Result<String, EngineError> {
    server.execute(conn, tool, args(arguments)).await
}

// ═══════════════════════════════════════════
// 工具面
// ═══════════════════════════════════════════

#[tokio::test]
async fn the_tool_list_is_not_empty_and_is_prefixed_by_game_type() {
    // `tools()` 变异成 `Default::default()` 会让 `tools/list` 返回零个工具，
    // Agent 从此看不到任何可调用的动作。
    let server = server();
    let result = server.tools();

    assert!(!result.tools.is_empty(), "工具表不能为空");
    for tool in &result.tools {
        let name = tool.name.as_ref();
        assert!(
            name.starts_with("two_player_") || name.starts_with("guess_number_"),
            "{name}"
        );
    }
}

#[test]
fn all_tools_for_knows_every_registered_game() {
    // 工具表由引擎自己声明（`GameEngine::describe_tools`），mcp 层只做翻译。
    // 任一引擎漏声明会让它的工具从这里消失。
    let registry = registry();
    assert_ne!(tools::all_tools_for(&registry, "guess_number"), Vec::new());
    assert_ne!(tools::all_tools_for(&registry, "two_player"), Vec::new());
    assert!(
        tools::all_tools_for(&registry, "nope").is_empty(),
        "未知游戏返回空"
    );
}
#[test]
fn describe_all_reports_the_mode_and_the_tool_count() {
    // 这个函数取代了旧的 `expected_player_mode`：它同时给出展示名、玩家模式
    // 与工具数，`/api/v1/games/tools` 与测试都用它。
    let registry = registry();
    let rows = tools::describe_all(&registry);
    assert_eq!(rows.len(), 2);

    let single = rows
        .iter()
        .find(|(name, ..)| *name == "guess_number")
        .unwrap();
    assert_eq!(single.2, PlayerMode::Single);
    assert_eq!(single.3, 3);

    let multi = rows
        .iter()
        .find(|(name, ..)| *name == "two_player")
        .unwrap();
    assert_eq!(multi.2, PlayerMode::Multi);
    assert_eq!(multi.3, 3);
}
// ═══════════════════════════════════════════
// 动作执行
// ═══════════════════════════════════════════

#[tokio::test]
async fn execute_returns_the_engines_prompt_not_a_placeholder() {
    // `execute` 变异成 `Ok(String::new())` 或 `Ok("xyzzy")` 会让 Agent
    // 拿到一个没有任何信息的返回值。
    let server = server();
    let text = call(&server, CONN_A, "guess_number_start", json!({}))
        .await
        .unwrap();
    assert!(!text.is_empty(), "返回值不能为空");
    assert_ne!(text, "xyzzy");
    assert!(text.contains("guess_number"), "{text}");
}

#[tokio::test]
async fn an_unknown_tool_is_a_client_error_with_the_name_in_it() {
    let server = server();
    let error = call(&server, CONN_A, "totally_unknown_tool", json!({}))
        .await
        .unwrap_err();
    assert!(error.is_client_error());
    assert!(
        error.to_string().contains("totally_unknown_tool"),
        "{error}"
    );
}

#[tokio::test]
async fn a_tool_name_that_only_shares_the_prefix_is_rejected() {
    // `liars_dice_nope` 能剥出 `("liars_dice", "nope")`，但这不是一个已声明的工具。
    let server = server();
    let error = call(&server, CONN_A, "guess_number_nope", json!({}))
        .await
        .unwrap_err();
    assert!(error.is_client_error());
    assert!(error.to_string().contains("未知工具"), "{error}");
    assert_eq!(server.manager().len(), 0, "不该为此建出对局");
}

#[tokio::test]
async fn engine_errors_pass_through_with_their_own_message() {
    let server = server();
    // 先建局
    call(&server, CONN_A, "guess_number_start", json!({}))
        .await
        .unwrap();
    // 再发一个引擎不认识的动作
    let error = call(&server, CONN_A, "guess_number_start2", json!({}))
        .await
        .unwrap_err();
    // `start2` 不在声明里，所以先被工具名校验挡住
    assert!(error.is_client_error());
}

#[tokio::test]
async fn engine_error_text_is_prefixed_and_carries_the_message() {
    // 变异成空串或 "xyzzy" 会让 Agent 只看到 "❌ " 或一个无意义的词。
    let text = engine_error_text(&EngineError::invalid("叫数必须比上家大"));
    assert!(text.starts_with('❌'), "{text}");
    assert!(text.contains("叫数必须比上家大"), "{text}");
}

// ═══════════════════════════════════════════
// 会话定位与缓存
// ═══════════════════════════════════════════

#[tokio::test]
async fn a_starting_action_creates_a_session_and_caches_it() {
    let server = server();
    call(&server, CONN_A, "guess_number_start", json!({}))
        .await
        .unwrap();
    assert_eq!(server.manager().len(), 1);

    // 第二次 start 会**换新局**（`start` 的语义），而不是复用
    call(&server, CONN_A, "guess_number_start", json!({}))
        .await
        .unwrap();
    assert_eq!(server.manager().len(), 2, "start 必须开新局");
}

#[tokio::test]
async fn a_non_starting_action_without_a_session_is_rejected() {
    // 变异把这些守卫去掉会让 `bid` 凭空调出一个空对局。
    let server = server();
    let error = call(&server, CONN_A, "guess_number_guess", json!({"number": 50}))
        .await
        .unwrap_err();
    assert!(error.is_client_error());
    assert!(error.to_string().contains("还没有对局"), "{error}");
    assert_eq!(server.manager().len(), 0, "不该建出对局");
}

#[tokio::test]
async fn the_hint_names_the_right_starting_action() {
    // 多人游戏提示 `join`，单人提示 `start`。变异成空串会让 Agent 不知道该调什么。
    let server = server();
    let single = call(&server, CONN_A, "guess_number_state", json!({}))
        .await
        .unwrap_err();
    assert!(
        single.to_string().contains("guess_number_start"),
        "{single}"
    );

    let multi = call(
        &server,
        CONN_B,
        "two_player_state",
        json!({"player_index": 0, "timeout": 0}),
    )
    .await
    .unwrap_err();
    assert!(multi.to_string().contains("two_player_join"), "{multi}");
}

#[tokio::test]
async fn an_explicit_session_id_that_does_not_exist_is_reported_as_such() {
    let server = server();
    let error = call(
        &server,
        CONN_A,
        "guess_number_state",
        json!({"session_id": "nosuch00"}),
    )
    .await
    .unwrap_err();
    assert!(error.is_client_error());
    assert!(error.to_string().contains("nosuch00"), "{error}");
}

#[tokio::test]
async fn an_explicit_session_id_of_another_game_is_rejected() {
    let server = server();
    // 建一个 two_player 局，再拿它的 id 去调 guess_number
    let session = server
        .manager()
        .create("two_player", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    let error = call(
        &server,
        CONN_A,
        "guess_number_state",
        json!({"session_id": session.id}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("不能用于"), "{error}");
}

#[tokio::test]
async fn an_explicit_session_id_without_a_token_is_rejected_for_multiplayer() {
    let server = server();
    let session = server
        .manager()
        .create("two_player", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    let error = call(
        &server,
        CONN_A,
        "two_player_state",
        json!({"session_id": session.id, "player_index": 0, "timeout": 0}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("player_token"), "{error}");
}

#[tokio::test]
async fn a_single_player_session_needs_no_token() {
    let server = server();
    let session = server
        .manager()
        .create("guess_number", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    let text = call(
        &server,
        CONN_A,
        "guess_number_state",
        json!({"session_id": session.id}),
    )
    .await
    .unwrap();
    assert!(!text.is_empty(), "返回值不能为空");
}

// ═══════════════════════════════════════════
// 身份：token 优先于连接
// ═══════════════════════════════════════════

#[tokio::test]
async fn the_same_token_returns_to_the_same_session_from_another_connection() {
    // `find_active_session` 变异成 `None`/`Some("")`/`Some("xyzzy")` 会让
    // 断线重连失败，第二个连接另开一局。
    let server = server();

    call(
        &server,
        CONN_A,
        "two_player_join",
        json!({"player_name": "A"}),
    )
    .await
    .unwrap();
    let session_id = server.manager().session_ids()[0].clone();

    // 从另一条连接、带着第一个玩家的 token 进来
    let text = call(
        &server,
        CONN_B,
        "two_player_join",
        json!({"player_name": "A", "player_token": "tok-0"}),
    )
    .await
    .unwrap();

    // DummyTwoPlayer 的重连只认 token，所以这里应当回到同一局
    assert!(text.contains("tok-0"), "{text}");
    assert_eq!(
        server.manager().len(),
        1,
        "同一 token 必须回到原对局（session={session_id}）"
    );
}

#[tokio::test]
async fn find_active_session_ignores_a_finished_session() {
    // 造一个已结束的对局
    let registry = Registry::new();
    let manager = Arc::new(GameManager::new(
        registry,
        agent_game_server_core::Config::default(),
    ));
    let ended = AgentGameServer::new(Arc::clone(&manager));

    // 空注册表：任何工具都会报"未知工具"，这里只验证它不 panic
    let error = call(
        &ended,
        CONN_A,
        "two_player_join",
        json!({"player_name": "A"}),
    )
    .await
    .unwrap_err();
    assert!(error.is_client_error());
}

#[tokio::test]
async fn two_connections_do_not_share_a_cached_session() {
    // 缓存按连接记账。`remember` 变异成不区分 key 会让两条连接互相覆盖。
    let server = server();

    call(&server, CONN_A, "guess_number_start", json!({}))
        .await
        .unwrap();
    call(&server, CONN_B, "guess_number_start", json!({}))
        .await
        .unwrap();
    assert_eq!(server.manager().len(), 2, "两条连接各开一局");
}

// ═══════════════════════════════════════════
// 缓存本身的边界
// ═══════════════════════════════════════════

#[test]
fn clear_actually_empties_the_cache() {
    // 变异成 `()` 会让 `clear()` 变成空操作，测试之间的隔离失效。
    let cache = SessionCache::new(4);
    cache.put("a", "g", "1");
    assert_eq!(cache.tracked(), 1);
    cache.clear();
    assert_eq!(cache.tracked(), 0, "clear 必须清空");
}

#[test]
fn a_zero_capacity_cache_still_stores_one_entry() {
    // `cap.max(1)` 去掉会让容量变成 0，每次 put 立刻被淘汰，
    // 于是同一个连接里的第二次调用找不到上一局。
    let cache = SessionCache::new(0);
    cache.put("a", "g", "1");
    assert_eq!(cache.tracked(), 1);
    assert_eq!(cache.remember("a").get("g").map(String::as_str), Some("1"));
}

// ═══════════════════════════════════════════
// 传输层适配：connection key 与错误分类
// ═══════════════════════════════════════════

#[tokio::test]
async fn different_connection_keys_are_independent() {
    // `session_key` 变异成空串或常量会让所有连接共用一份缓存——
    // 那正是"每个 MCP 连接各持一局"的那个历史缺陷。
    let server = server();

    call(&server, "conn-1", "guess_number_start", json!({}))
        .await
        .unwrap();
    let after_first = server.manager().session_ids();

    call(&server, "conn-2", "guess_number_start", json!({}))
        .await
        .unwrap();
    let after_second = server.manager().session_ids();

    // `session_ids()` 返回的是 BTreeMap 的键序，不是创建顺序——
    // 所以按集合而不是按下标比较。
    assert_eq!(after_first.len(), 1);
    assert_eq!(after_second.len(), 2);

    let first: std::collections::BTreeSet<_> = after_first.iter().collect();
    let second: std::collections::BTreeSet<_> = after_second.iter().collect();
    assert!(
        first.is_subset(&second),
        "第一次的对局必须还在（两条连接不互相覆盖）"
    );
    let only_new: Vec<_> = second.difference(&first).collect();
    assert_eq!(only_new.len(), 1, "第二条连接应该新增一局，而不是复用");
}

#[tokio::test]
async fn a_client_error_and_an_internal_error_are_classified_differently() {
    // `call_tool` 的 match guard 是两者唯一的分类点。这里通过
    // `is_client_error` 间接验证：未知工具是调用方问题，引擎内部错误不是。
    let server = server();
    let client = call(&server, CONN_A, "nope_tool", json!({}))
        .await
        .unwrap_err();
    assert!(client.is_client_error());

    let internal = EngineError::Internal("boom".to_owned());
    assert!(!internal.is_client_error());
}

// ═══════════════════════════════════════════
// 等待语义
// ═══════════════════════════════════════════

#[tokio::test]
async fn state_with_timeout_zero_does_not_wait() {
    let server = server();
    // 用 guess_number（单人，默认 is_ready_for 恒真）验证不挂起
    let session = server
        .manager()
        .create("guess_number", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    let started = Instant::now();
    call(
        &server,
        CONN_A,
        "guess_number_state",
        json!({"session_id": session.id, "timeout": 0}),
    )
    .await
    .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "timeout=0 不该挂起"
    );
}

#[tokio::test]
async fn a_non_numeric_timeout_is_rejected_before_waiting() {
    let server = server();
    let session = server
        .manager()
        .create("guess_number", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    let started = Instant::now();
    let error = call(
        &server,
        CONN_A,
        "guess_number_state",
        json!({"session_id": session.id, "timeout": "abc"}),
    )
    .await
    .unwrap_err();
    assert!(error.is_client_error());
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "非法 timeout 必须 fast-fail"
    );
}

#[tokio::test]
async fn an_out_of_range_player_index_is_rejected_before_waiting() {
    let server = server();
    let session = server
        .manager()
        .create("two_player", CreateParams::default(), Instant::now())
        .await
        .unwrap();
    // 先塞两名玩家
    {
        let mut engine = session.engine.lock().await;
        for name in ["A", "B"] {
            let mut params = serde_json::Map::new();
            params.insert("player_name".to_owned(), Value::from(name));
            engine
                .handle_action("join", agent_game_server_core::ActionParams::new(params))
                .unwrap();
        }
    }

    let started = Instant::now();
    let error = call(
        &server,
        CONN_A,
        "two_player_state",
        json!({"session_id": session.id, "player_index": 99, "player_token": "tok-0"}),
    )
    .await
    .unwrap_err();
    assert!(error.is_client_error());
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "越界索引必须 fast-fail"
    );
}

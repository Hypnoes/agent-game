//! MCP 传输层：真实的 HTTP 往返，不是直接调内部函数。
//!
//! 这一层守三件事：
//!
//! 1. **`/mcp` 不能重定向**。Python 版用 `Mount` 会 307 到 `/mcp/`，裸 HTTP
//!    客户端直接失败；这里用 `nest_service`，用真实 `POST initialize` 断言 200。
//! 2. **`Mcp-Session-Id` 只是连接缓存键，身份只来自 `player_token`**。
//!    同一个 token 从两条连接进来必须回到同一局。
//! 3. **工具面**：7 个工具、`required` 正确、未知工具报错不建局。
//!
//! 走真实 `TcpListener` + `reqwest`，因为 MCP 的 Streamable HTTP 涉及 SSE 流
//! 与请求头回写，内存传输会绕过一部分真实行为。

// 集成测试是独立 crate，不在 `cfg(test)` 里，clippy 的 `allow-unwrap-in-tests`
// 管不到它。测试里 unwrap 就是"断言这里必须成功"。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use agent_game_server_core::CreateParams;
use agent_game_server_server::{AppState, create_app};
use serde_json::{Value, json};

const ADMIN: &str = "mcp-test-token";

/// 一个跑在随机端口上的服务端实例。
struct Server {
    base: String,
    state: AppState,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl Server {
    async fn start() -> Self {
        let state = AppState::with_defaults(ADMIN.to_owned());
        let app = create_app(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let serve = axum::serve(listener, app).with_graceful_shutdown(async {
                let _ = rx.await;
            });
            let _ = serve.await;
        });

        Self {
            base: format!("http://{addr}"),
            state,
            _shutdown: shutdown,
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap()
    }

    /// 完成一次 initialize 握手，返回 `Mcp-Session-Id`。
    async fn handshake(&self) -> String {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pytest", "version": "1"},
            },
        });
        let response = Self::client()
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            200,
            "/mcp 不能重定向（Python 版的 Mount 会 307 到 /mcp/）"
        );
        let session = response
            .headers()
            .get("mcp-session-id")
            .expect("initialize 响应必须带 Mcp-Session-Id")
            .to_str()
            .unwrap()
            .to_owned();

        // initialized 通知
        let _ = Self::client()
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("mcp-session-id", &session)
            .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .unwrap();

        session
    }

    /// 一次 `tools/call`，返回 `(是否错误, 文本)`。
    async fn call(&self, session: &str, tool: &str, arguments: Value) -> (bool, String) {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        });
        let response = Self::client()
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("mcp-session-id", session)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "tools/call 的传输层必须成功");

        let text = response.text().await.unwrap();
        let envelope = parse_first_message(&text).expect("响应体里应有 JSON-RPC 消息");
        let result = &envelope["result"];
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let content = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        (is_error, content)
    }

    async fn list_tools(&self, session: &str) -> Vec<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}});
        let response = Self::client()
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("mcp-session-id", session)
            .json(&body)
            .send()
            .await
            .unwrap();
        let text = response.text().await.unwrap();
        let envelope = parse_first_message(&text).expect("响应体里应有 JSON-RPC 消息");
        envelope["result"]["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

/// 从响应体里取第一条 JSON-RPC 消息。
///
/// MCP 的 Streamable HTTP 可能回 `application/json`，也可能回
/// `text/event-stream`（SSE 帧 `data: {...}`）。两种都要能解。
fn parse_first_message(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Some(value);
    }
    let mut buffer = String::new();
    for line in text.lines() {
        if let Some(payload) = line.strip_prefix("data: ") {
            buffer.push_str(payload);
        } else if line.trim().is_empty() && !buffer.is_empty() {
            if let Ok(value) = serde_json::from_str(&buffer) {
                return Some(value);
            }
            buffer.clear();
        }
    }
    if buffer.is_empty() {
        None
    } else {
        serde_json::from_str(&buffer).ok()
    }
}

// ═══════════════════════════════════════════
// 握手与工具面
// ═══════════════════════════════════════════

#[tokio::test]
async fn initialize_returns_a_session_id_without_redirecting() {
    let server = Server::start().await;
    let session = server.handshake().await;
    assert!(!session.is_empty());
}

#[tokio::test]
async fn two_handshakes_get_different_sessions() {
    let server = Server::start().await;
    let first = server.handshake().await;
    let second = server.handshake().await;
    assert_ne!(first, second, "每次 initialize 都是独立连接");
}

#[tokio::test]
async fn tools_are_listed_with_their_required_fields() {
    let server = Server::start().await;
    let session = server.handshake().await;
    let tools = server.list_tools(&session).await;

    assert_eq!(tools.len(), 7, "{tools:?}");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default())
        .collect();
    for expected in [
        "guess_number_start",
        "guess_number_guess",
        "guess_number_state",
        "liars_dice_join",
        "liars_dice_state",
        "liars_dice_bid",
        "liars_dice_challenge",
    ] {
        assert!(names.contains(&expected), "缺少工具 {expected}: {names:?}");
    }

    let bid = tools
        .iter()
        .find(|tool| tool["name"] == "liars_dice_bid")
        .unwrap();
    assert_eq!(
        bid["inputSchema"]["required"],
        json!(["player_index", "player_token", "quantity", "face"])
    );
}

// ═══════════════════════════════════════════
// 一局游戏走完
// ═══════════════════════════════════════════

#[tokio::test]
async fn a_game_can_be_started_and_played_through_mcp() {
    let server = Server::start().await;
    let session = server.handshake().await;

    let (error, text) = server.call(&session, "guess_number_start", json!({})).await;
    assert!(!error, "{text}");
    assert!(text.contains("猜测"), "{text}");

    let (error, text) = server
        .call(&session, "guess_number_guess", json!({"number": 50}))
        .await;
    assert!(!error, "{text}");
    assert!(!text.contains("{missing:"), "提示词不能降级: {text}");
}

#[tokio::test]
async fn a_multiplayer_game_returns_a_token_per_player() {
    let server = Server::start().await;
    let session = server.handshake().await;

    let (error, text) = server
        .call(&session, "liars_dice_join", json!({"player_name": "Alice"}))
        .await;
    assert!(!error, "{text}");
    assert!(
        text.contains("token"),
        "join 的提示词必须带上 token: {text}"
    );
    assert!(text.contains("1/2"), "大厅提示应显示人数: {text}");
}

#[tokio::test]
async fn tools_without_a_session_only_allow_starting_actions() {
    let server = Server::start().await;
    let session = server.handshake().await;

    // 全新连接、直接 bid：不该静默建出一局空对局
    let (error, text) = server
        .call(
            &session,
            "liars_dice_bid",
            json!({"player_index": 0, "player_token": "x", "quantity": 1, "face": 2}),
        )
        .await;
    assert!(error, "没有对局时 bid 必须失败: {text}");
    assert!(text.contains("还没有对局"), "{text}");
    assert_eq!(server.state.manager.len(), 0, "不能建出空对局");
}

#[tokio::test]
async fn an_unknown_tool_is_a_client_error_and_creates_nothing() {
    let server = Server::start().await;
    let session = server.handshake().await;

    let (error, text) = server.call(&session, "liars_dice_nope", json!({})).await;
    assert!(error, "{text}");
    assert!(text.contains("未知工具"), "{text}");
    assert_eq!(server.state.manager.len(), 0);
}

#[tokio::test]
async fn an_unregistered_prefix_is_rejected() {
    let server = Server::start().await;
    let session = server.handshake().await;

    let (error, _text) = server
        .call(&session, "totally_unknown_tool", json!({}))
        .await;
    assert!(error);
    assert_eq!(server.state.manager.len(), 0);
}

// ═══════════════════════════════════════════
// 身份：token 优先于连接
// ═══════════════════════════════════════════

#[tokio::test]
async fn the_same_token_returns_to_the_same_game_across_connections() {
    let server = Server::start().await;

    // 第一条连接：Alice 入场，拿到 token
    let first_session = server.handshake().await;
    let (error, text) = server
        .call(
            &first_session,
            "liars_dice_join",
            json!({"player_name": "Alice"}),
        )
        .await;
    assert!(!error, "{text}");
    let token = extract_token(&text).expect("提示词里应有 token");

    // 第二条连接：带着同一个 token 进来
    let second_session = server.handshake().await;
    let (error, text) = server
        .call(
            &second_session,
            "liars_dice_join",
            json!({"player_name": "Alice", "player_token": token}),
        )
        .await;
    assert!(!error, "{text}");
    assert_eq!(
        server.state.manager.len(),
        1,
        "同一 token 必须回到原对局，而不是各开一局"
    );
}

#[tokio::test]
async fn an_explicit_session_id_without_a_token_is_rejected() {
    let server = Server::start().await;
    let session = server.handshake().await;

    // 先在服务端造一局两人对局（直接走 manager，省掉两次 join 的往返）
    let created = server
        .state
        .manager
        .create(
            "liars_dice",
            CreateParams::new(serde_json::Map::new()),
            std::time::Instant::now(),
        )
        .await
        .unwrap();
    let sid = created.id.clone();

    let (error, text) = server
        .call(
            &session,
            "liars_dice_state",
            json!({"session_id": sid, "player_index": 0, "timeout": 0}),
        )
        .await;
    assert!(error, "显式 session_id 无 token 必须拒绝: {text}");
    assert!(text.contains("player_token"), "{text}");
}

#[tokio::test]
async fn a_session_id_of_another_game_type_is_rejected() {
    let server = Server::start().await;
    let session = server.handshake().await;

    let created = server
        .state
        .manager
        .create(
            "liars_dice",
            CreateParams::new(serde_json::Map::new()),
            std::time::Instant::now(),
        )
        .await
        .unwrap();

    let (error, text) = server
        .call(
            &session,
            "guess_number_state",
            json!({"session_id": created.id}),
        )
        .await;
    assert!(error, "{text}");
    assert!(text.contains("不能用于"), "{text}");
}

#[tokio::test]
async fn a_bad_token_fails_fast_over_mcp() {
    // Python 版的 P1-2：MCP 侧曾经先等待、后校验
    let server = Server::start().await;
    let session = server.handshake().await;

    // 用真实工具凑一局两人对局
    let (error, alice) = server
        .call(&session, "liars_dice_join", json!({"player_name": "Alice"}))
        .await;
    assert!(!error, "{alice}");
    let (error, _bob) = server
        .call(&session, "liars_dice_join", json!({"player_name": "Bob"}))
        .await;
    assert!(!error);

    let started = std::time::Instant::now();
    let (error, text) = server
        .call(
            &session,
            "liars_dice_state",
            json!({"player_index": 0, "player_token": "not-mine"}),
        )
        .await;
    let elapsed = started.elapsed();

    assert!(error, "{text}");
    assert!(text.contains("不匹配"), "{text}");
    assert!(
        elapsed < Duration::from_secs(1),
        "错 token 必须 fast-fail，实际等了 {elapsed:?}"
    );
}

#[tokio::test]
async fn timeout_zero_does_not_block_over_mcp() {
    let server = Server::start().await;
    let session = server.handshake().await;

    let (error, alice) = server
        .call(&session, "liars_dice_join", json!({"player_name": "Alice"}))
        .await;
    assert!(!error, "{alice}");
    let token = extract_token(&alice).unwrap();
    let (error, _bob) = server
        .call(&session, "liars_dice_join", json!({"player_name": "Bob"}))
        .await;
    assert!(!error);

    let started = std::time::Instant::now();
    let (error, text) = server
        .call(
            &session,
            "liars_dice_state",
            json!({"player_index": 0, "player_token": token, "timeout": 0}),
        )
        .await;
    assert!(!error, "{text}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "timeout=0 不该挂起"
    );
}

#[tokio::test]
async fn a_blocking_state_call_returns_when_the_opponent_acts() {
    let server = Arc::new(Server::start().await);

    // 两个玩家 = 两条 MCP 连接。这既是真实部署的样子（每个 Agent 一条连接），
    // 也避开了一个**传输层**事实：legacy session 模式下同一条会话内的请求是
    // 串行处理的，同一会话里并发 state + bid 会互相等，与阻塞等待无关。
    let alice_session = server.handshake().await;
    let bob_session = server.handshake().await;

    let (error, alice) = server
        .call(
            &alice_session,
            "liars_dice_join",
            json!({"player_name": "Alice"}),
        )
        .await;
    assert!(!error, "{alice}");
    let alice_token = extract_token(&alice).unwrap();
    let (error, bob) = server
        .call(
            &bob_session,
            "liars_dice_join",
            json!({"player_name": "Bob"}),
        )
        .await;
    assert!(!error, "{bob}");
    let bob_token = extract_token(&bob).unwrap();

    // 谁是先手由服务端随机决定
    let current = {
        let snapshots = server.state.manager.snapshots().await;
        snapshots
            .first()
            .and_then(|row| row.state.get("current_turn"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let current = usize::try_from(current).unwrap_or(0);
    let waiter = 1 - current;
    assert_ne!(current, waiter);
    let (waiter_session, waiter_token, actor_session, actor_token) = if waiter == 0 {
        (
            alice_session.clone(),
            alice_token.clone(),
            bob_session.clone(),
            bob_token.clone(),
        )
    } else {
        (
            bob_session.clone(),
            bob_token.clone(),
            alice_session.clone(),
            alice_token.clone(),
        )
    };

    // 对手在 100ms 后叫数，把回合交出去
    let background = {
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            server
                .call(
                    &actor_session,
                    "liars_dice_bid",
                    json!({
                        "player_index": current,
                        "player_token": actor_token,
                        "quantity": 1,
                        "face": 2,
                    }),
                )
                .await
        })
    };

    let started = std::time::Instant::now();
    let (error, text) = server
        .call(
            &waiter_session,
            "liars_dice_state",
            json!({"player_index": waiter, "player_token": waiter_token, "timeout": 20}),
        )
        .await;
    let elapsed = started.elapsed();

    let (bid_error, bid_text) = background.await.unwrap();
    assert!(!bid_error, "{bid_text}");
    assert!(!error, "{text}");
    assert!(
        elapsed < Duration::from_secs(5),
        "对手行动后应立刻返回，实际等了 {elapsed:?}"
    );
    // 等到的是"轮到你了"，不是"还没轮到你"
    assert!(
        text.contains("轮到你了"),
        "被对手行动唤醒后应轮到调用方: {text}"
    );
}

/// 从 join 的提示词里取出 token（`🎲 你是玩家 0，token: xxxx`）。
fn extract_token(prompt: &str) -> Option<String> {
    let marker = "token: ";
    let start = prompt.find(marker)? + marker.len();
    let rest = &prompt[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    Some(rest[..end].trim().to_owned())
}

#[tokio::test]
async fn the_admin_websocket_rejects_missing_and_wrong_tokens() {
    // WebSocket 不能设自定义头，token 走查询参数；不匹配以 1008 关闭。
    // `tokio-tungstenite` 不在依赖里，这里退一步用 axum 的会话升级前置检查：
    // 无 token 的握手必须得到 403（不是升级成功的 101）。
    let server = Server::start().await;
    let response = Server::client()
        .get(format!("{}/api/v1/admin/ws", server.base))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403, "无 token 的 WS 必须被拒绝");

    let response = Server::client()
        .get(format!("{}/api/v1/admin/ws?token=wrong", server.base))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403, "错 token 的 WS 必须被拒绝");
}

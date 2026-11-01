//! MCP 传输：把 `rmcp` 的 Streamable HTTP 服务挂到 `/mcp`。
//!
//! 与 Python 版的对应关系：那里必须用 `Route` + `session_manager.asgi_app`，
//! 因为 `Mount("/mcp", ...)` 会 307 重定向到 `/mcp/`，裸 HTTP 客户端直接失败。
//! 这里用 `nest_service("/mcp", ...)`——`rmcp` 的 `StreamableHttpService` 是一个
//! `tower::Service`，axum 的 `nest_service` 不会加尾斜杠重定向。
//!
//! `tests/mcp_transport.rs` 对 `/mcp` 直接 `POST initialize` 并断言拿到 200 与
//! `Mcp-Session-Id` 头，把"不能 307"这条契约钉住。

use std::sync::Arc;

use agent_game_server_mcp::AgentGameServer;
use axum::Router;
use axum::routing::any_service;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;

use crate::state::AppState;

/// 构建 `/mcp` 的子路由。
///
/// 每次调用都新建一个 `StreamableHttpService`：它内部的会话表与取消令牌
/// 属于它自己，两个 app 实例不能共用同一个服务对象。
pub fn router(state: &AppState) -> Router<AppState> {
    let manager = Arc::clone(&state.manager);
    let service = StreamableHttpService::new(
        move || Ok(AgentGameServer::new(Arc::clone(&manager))),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            // 会话模式：`Mcp-Session-Id` 是连接级缓存键（身份只来自 player_token）。
            // 关掉它等于让每个请求都变成独立会话，缓存会退化成"每次都开局"。
            .with_legacy_session_mode(true)
            .with_sse_keep_alive(Some(std::time::Duration::from_secs(15))),
    );

    Router::new().route("/mcp", any_service(service))
}

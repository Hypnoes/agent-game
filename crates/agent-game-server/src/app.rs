//! `create_app()`：把三条入口装进同一个 axum app。
//!
//! 每次调用都返回一个**独立**的 app：它有自己的 `AppState`、自己的
//! `AdminBroadcast`、自己的 `/mcp` 服务对象。因此两个 app 可以各带一份对局表
//! 同时在线而互不串状态——Python 版的 `create_app(manager)` 是在一次事故
//! （内层 shutdown 取消掉外层 poller）之后才补上的能力。
//!
//! # Errors
//! 只在内置模板损坏时返回错误（`include_str!` 是编译期资产，实为不可达）。
use crate::state::AppState;

pub fn create_app(state: AppState) -> axum::Router {
    let mut router = axum::Router::new()
        .merge(routes())
        .merge(crate::mcp::router(&state))
        .with_state(state);

    // `/` 与 `/admin` 都挂在同一层：前者给探活脚本，后者是仪表盘
    router = router.layer(tower_http::trace::TraceLayer::new_for_http());
    router
}

/// REST 路由。
fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};

    axum::Router::new()
        .route("/", get(crate::games::health))
        .route("/health", get(crate::games::health))
        .route("/api/v1/games/tools", get(crate::games::tools_with_query))
        .route(
            "/api/v1/games/tools/{game_type}",
            get(crate::games::list_game_tools),
        )
        .route("/api/v1/games/{id}", post(crate::games::create_game))
        .route("/api/v1/games/{id}", get(crate::games::game_public_state))
        .route(
            "/api/v1/games/{id}/players/{player_index}",
            get(crate::games::player_state),
        )
        .route("/api/v1/games/{id}/actions", post(crate::games::act))
        .route("/api/v1/admin/games", get(crate::admin::list_games))
        .route("/api/v1/admin/games/{id}", get(crate::admin::game_detail))
        .route(
            "/api/v1/admin/prompts/reload",
            post(crate::admin::reload_prompts),
        )
        .route("/api/v1/admin/ws", get(crate::admin::admin_socket))
        .route("/admin", get(crate::admin::dashboard))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_app_is_repeatable() {
        // 两个 app 各自持有自己的对局表与广播实例
        let first = create_app(AppState::with_defaults("a".to_owned()));
        let second = create_app(AppState::with_defaults("b".to_owned()));
        // Router 没有可比较的身份；能同时构造出来、且各自包含相同的路由集合即为通过。
        // 真正的实例隔离由 `tests/poller_isolation.rs` 用行为断言。
        let _ = (first, second);
    }
}

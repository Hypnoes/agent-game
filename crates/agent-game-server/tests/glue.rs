//! 服务端的胶水层：鉴权、查询过滤、超时结算。
//!
//! 这一组补的是 server crate 变异测试里剩下的那些位置——它们的共同点是
//! **"只在一个更宽的路径里被间接覆盖"**：`dashboard` 的 token 注入被
//! `http_contract.rs` 测到"匿名不注入"，但没测"带错的 token 也不注入"；
//! `tools_with_query` 的过滤条件没有直接用例；`settle_timeouts` 的返回值
//! 没有被断言过内容。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_game_server_core::games::LiarsDiceEngine;
use agent_game_server_core::{
    ActionParams, Config, CreateParams, GameEngine, GameMeta, PlayerMode, Registry,
};
use agent_game_server_server::AppState;
use agent_game_server_server::create_app;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;

const ADMIN: &str = "startup-admin-token";

fn state() -> AppState {
    AppState::with_defaults(ADMIN.to_owned())
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// ═══════════════════════════════════════════
// /admin 的 token 注入条件
// ═══════════════════════════════════════════

#[tokio::test]
async fn the_dashboard_injects_the_token_only_when_it_was_correct() {
    // 三个分支都要覆盖：
    // - 没带 → 不注入
    // - 带错 → 不注入
    // - 带对 → 注入
    //
    // `if !presented.is_empty() && state.token_matches(&presented)` 的两个
    // 逻辑算子（`!` 与 `&&`）任一写错，都会把 token 交给不该拿到它的人。
    let app = create_app(state());

    let (status, body) = get(app.clone(), "/admin").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(ADMIN), "匿名访问不该看到 token");

    let (status, body) = get(app.clone(), "/admin?token=wrong").await;
    assert_eq!(status, StatusCode::OK, "带错 token 仍返回页面骨架");
    assert!(!body.contains(ADMIN), "错 token 不该被注入");

    let (status, body) = get(app, &format!("/admin?token={ADMIN}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(ADMIN), "正确 token 必须被注入");
    assert!(body.contains("window.ADMIN_TOKEN"), "注入的是那个全局变量");
}

#[tokio::test]
async fn an_empty_token_query_does_not_count_as_presented() {
    // `?token=` 是空串。它既不该被当成"已出示"，也不该让页面 500。
    let app = create_app(state());
    let (status, body) = get(app, "/admin?token=").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(ADMIN));
}

#[tokio::test]
async fn the_dashboard_works_through_the_header_too() {
    // 运维用 curl 时更可能带 `X-Admin-Token` 头而不是查询参数。
    let app = create_app(state());
    let request = Request::builder()
        .method("GET")
        .uri("/admin")
        .header("x-admin-token", ADMIN)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes);
    assert!(body.contains(ADMIN), "头里带对 token 也要注入");
}

// ═══════════════════════════════════════════
// /api/v1/games/tools 的过滤
// ═══════════════════════════════════════════

#[tokio::test]
async fn the_tools_listing_can_be_filtered_by_game_type() {
    // `query.game_type.as_deref().is_none_or(|wanted| wanted == *game_type)`
    // 的 `==` 改成 `!=` 会让过滤结果正好反过来。
    let app = create_app(state());

    let (status, body) = get(app.clone(), "/api/v1/games/tools?game_type=liars_dice").await;
    assert_eq!(status, StatusCode::OK);
    let games: Value = serde_json::from_str(&body).unwrap();
    let games = games["games"].as_array().unwrap();

    assert_eq!(games.len(), 1, "过滤后只剩一款游戏");
    assert_eq!(games[0]["game_type"], "liars_dice");

    // 不带过滤时两款都在
    let (_, body) = get(app, "/api/v1/games/tools").await;
    let games: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(games["games"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn an_unknown_game_type_filter_yields_nothing() {
    let app = create_app(state());
    let (status, body) = get(app, "/api/v1/games/tools?game_type=nope").await;
    assert_eq!(status, StatusCode::OK);
    let games: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        games["games"].as_array().unwrap(),
        &Vec::<serde_json::Value>::new()
    );
}

// ═══════════════════════════════════════════
// settle_timeouts 的返回值
// ═══════════════════════════════════════════

#[tokio::test]
async fn settling_without_any_timeout_returns_an_empty_list() {
    // 变异成 `vec![Default::default()]` 会让每轮都"结算出一个事件"，
    // 于是 poller 每 500ms 就广播一次，admin 页面永远在闪。
    let state = state();
    let events = agent_game_server_server::poller::settle_timeouts(&state.manager).await;
    assert!(events.is_empty(), "没有对局时不该产生事件: {events:?}");
}

#[tokio::test]
async fn settling_a_fresh_session_produces_no_event() {
    let state = state();
    state
        .manager
        .create("guess_number", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    let events = agent_game_server_server::poller::settle_timeouts(&state.manager).await;
    assert!(events.is_empty(), "刚建的对局不该超时: {events:?}");
}

#[tokio::test]
async fn settling_a_timed_out_session_produces_the_event_as_json() {
    // 在工厂里把引擎设为"已有一名玩家且大厅已超时"，这样 settle_timeouts
    // 本身必须派发事件，而不是依赖此前直接调用 check_timeout。
    let mut registry = Registry::new();
    registry.register(GameMeta::new(
        "liars_dice",
        PlayerMode::Multi,
        |params, config| {
            let mut engine = LiarsDiceEngine::new(params, config)?;
            let mut join_params = serde_json::Map::new();
            join_params.insert("player_name".to_owned(), Value::from("Solo"));
            engine
                .handle_action("join", ActionParams::new(join_params))
                .map_err(|error| error.to_string())?;
            engine.set_last_action(
                Instant::now()
                    .checked_sub(Duration::from_secs(200))
                    .unwrap(),
            );
            Ok(Box::new(engine) as agent_game_server_core::BoxedEngine)
        },
    ));
    let state = AppState::new(registry, Config::default(), ADMIN.to_owned());
    let session = state
        .manager
        .create("liars_dice", CreateParams::default(), Instant::now())
        .await
        .unwrap();
    let events = agent_game_server_server::poller::settle_timeouts(&state.manager).await;
    assert_eq!(events.len(), 1, "超时结算必须派发一个事件: {events:?}");
    let event = &events[0];
    assert!(event.is_object(), "事件必须是 JSON 对象: {event:?}");
    assert_eq!(event["session_id"], session.id);
    assert_eq!(event["type"], "lobby_timeout");
}

// ═══════════════════════════════════════════
// 广播实例
// ═══════════════════════════════════════════

#[test]
fn broadcast_of_returns_the_states_own_instance() {
    // 变异成 `Box::leak` 一个新实例会让每个调用拿到不同的广播，
    // 于是 WS 永远收不到 poller 推的消息。
    let state = state();
    let extracted = agent_game_server_server::admin::broadcast_of(&state);
    assert!(Arc::ptr_eq(extracted, &state.broadcast), "必须是同一份广播");
    assert_eq!(
        agent_game_server_server::admin::manager_of(&state).len(),
        state.manager.len()
    );
}

// ═══════════════════════════════════════════
// 两套服务端配置的隔离
// ═══════════════════════════════════════════

#[tokio::test]
async fn two_apps_with_different_configs_do_not_share_budgets() {
    // `FromRef` 取到的必须是**自己那份** `Config`：变异成默认值会让
    // 第二个 app 的等待预算变回 55 秒。
    use agent_game_server_core::Config;

    let strict = AppState::new(
        agent_game_server_server::build_registry(),
        Config {
            block_budget: std::time::Duration::from_secs(1),
            ..Config::default()
        },
        "a".to_owned(),
    );
    let loose = AppState::new(
        agent_game_server_server::build_registry(),
        Config::default(),
        "b".to_owned(),
    );

    assert_eq!(
        strict.config.block_budget,
        std::time::Duration::from_secs(1)
    );
    assert_eq!(
        loose.config.block_budget,
        std::time::Duration::from_secs(55)
    );

    // 两个 app 能同时构造，各带自己的配置
    let _first = create_app(strict);
    let _second = create_app(loose);
}

// ═══════════════════════════════════════════
// health 的信封
// ═══════════════════════════════════════════

#[tokio::test]
async fn health_reports_the_session_count() {
    let state = state();
    let expected_games = state.manager.registry().game_types();
    state
        .manager
        .create("guess_number", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    let app = create_app(state);
    let (status, body) = get(app, "/health").await;
    assert_eq!(status, StatusCode::OK);

    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["sessions"], 1);
    assert_eq!(body["ok"], true);
    // 不能只断言 `is_array()`：空数组也过。这里注册了两个游戏，断言内容。
    let games = body["games"]
        .as_array()
        .unwrap_or_else(|| panic!("/health 的 games 必须是数组: {body}"));
    assert!(
        !games.is_empty(),
        "注册表里有两个游戏，/health 的 games 不该为空: {body}"
    );
    let names: Vec<&str> = games.iter().filter_map(|g| g.as_str()).collect();
    assert_eq!(
        names.len(),
        games.len(),
        "games 里的每一项都必须是游戏名（字符串）: {body}"
    );
    assert_eq!(names, expected_games, "games 必须准确列出注册表中的游戏");
}

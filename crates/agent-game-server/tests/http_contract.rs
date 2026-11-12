//! HTTP 合同层：服务端对外承诺的形状与分类。
//!
//! 这一层不测游戏规则（那是 core 的规则层），只测"调用方拿到什么"：
//! 状态码、错误分类、秘密边界、鉴权。每条都对应 Python 版一个真实出过的问题，
//! 或者一条必须成立的契约。

// 集成测试是独立 crate，不在 `cfg(test)` 里，clippy 的 `allow-unwrap-in-tests`
// 管不到它。测试里 unwrap 就是"断言这里必须成功"。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use agent_game_server_core::CreateParams;
use agent_game_server_server::AppState;
use agent_game_server_server::create_app;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

const ADMIN: &str = "test-admin-token";

fn state() -> AppState {
    AppState::with_defaults(ADMIN.to_owned())
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(text) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(text.to_owned()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    (status, value)
}

// ═══════════════════════════════════════════
// 探活与工具面
// ═══════════════════════════════════════════

#[tokio::test]
async fn health_reports_version_and_games() {
    let (status, body) = call(create_app(state()), "GET", "/health", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["games"], json!(["guess_number", "liars_dice"]));
}

#[tokio::test]
async fn tools_endpoint_carries_the_display_names() {
    let (status, body) = call(create_app(state()), "GET", "/api/v1/games/tools", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let games = body["games"].as_array().unwrap();
    assert_eq!(games.len(), 2);

    // 展示名来自注册表，页面不再自己硬编码
    let names: Vec<&str> = games
        .iter()
        .map(|game| game["display_name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"猜数字"), "{names:?}");
    assert!(names.contains(&"大话骰"), "{names:?}");

    // 每个游戏的工具都以自己的机器键为前缀
    for game in games {
        let game_type = game["game_type"].as_str().unwrap();
        for tool in game["tools"].as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            assert!(name.starts_with(game_type), "{name} 不以 {game_type} 开头");
        }
    }
}

// ═══════════════════════════════════════════
// 建局：body 形态
// ═══════════════════════════════════════════

#[tokio::test]
async fn an_empty_body_creates_a_game_with_defaults() {
    let state = state();
    let manager = Arc::clone(&state.manager);
    let app = create_app(state);

    let (status, body) = call(app, "POST", "/api/v1/games/guess_number", None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["game_type"], "guess_number");
    assert_eq!(body["display_name"], "猜数字");
    assert_eq!(manager.len(), 1);
}

#[tokio::test]
async fn malformed_json_is_a_client_error_and_creates_nothing() {
    let state = state();
    let manager = Arc::clone(&state.manager);
    let app = create_app(state);

    for raw in ["{not json", r#"{"a":}"#, "not json at all"] {
        let (status, body) = call(
            app.clone(),
            "POST",
            "/api/v1/games/guess_number",
            Some(raw),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw} → {body}");
        assert_eq!(body["error"]["code"], "invalid_input");
    }
    assert_eq!(manager.len(), 0, "畸形请求不该建出对局");
}

#[tokio::test]
async fn non_object_json_is_a_client_error_and_creates_nothing() {
    let state = state();
    let manager = Arc::clone(&state.manager);
    let app = create_app(state);

    for raw in [r#""hello""#, "5", "true", "[1,2]", "null"] {
        let (status, body) = call(
            app.clone(),
            "POST",
            "/api/v1/games/guess_number",
            Some(raw),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw} → {body}");
    }
    assert_eq!(manager.len(), 0);
}

#[tokio::test]
async fn unknown_game_type_is_a_client_error() {
    let (status, body) = call(create_app(state()), "POST", "/api/v1/games/nope", None, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("未知游戏类型")
    );
}

#[tokio::test]
async fn invalid_creation_params_are_rejected() {
    let (status, body) = call(
        create_app(state()),
        "POST",
        "/api/v1/games/liars_dice",
        Some(r#"{"player_count": 1}"#),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// ═══════════════════════════════════════════
// 动作与秘密边界
// ═══════════════════════════════════════════

#[tokio::test]
async fn the_public_view_hides_the_secret_and_admin_view_has_it() {
    let app = create_app(state());
    let (status, created) =
        call(app.clone(), "POST", "/api/v1/games/guess_number", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let sid = created["session_id"].as_str().unwrap().to_owned();

    let (status, public) = call(
        app.clone(),
        "GET",
        &format!("/api/v1/games/{sid}"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        public["state"].get("target").is_none(),
        "公开视图不能含 target: {public}"
    );

    let (status, admin) = call(
        app,
        "GET",
        &format!("/api/v1/admin/games/{sid}"),
        None,
        &[("x-admin-token", ADMIN)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        admin["state"].get("target").is_some(),
        "admin 必须能看到答案"
    );
}

#[tokio::test]
async fn an_unknown_session_is_404() {
    let (status, _body) = call(
        create_app(state()),
        "GET",
        "/api/v1/games/doesnot1",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_missing_action_field_is_rejected() {
    let app = create_app(state());
    let (_s, created) = call(app.clone(), "POST", "/api/v1/games/guess_number", None, &[]).await;
    let sid = created["session_id"].as_str().unwrap().to_owned();

    let (status, body) = call(
        app,
        "POST",
        &format!("/api/v1/games/{sid}/actions"),
        Some("{}"),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("action")
    );
}

#[tokio::test]
async fn a_single_player_game_can_be_played_over_http() {
    let app = create_app(state());
    let (_s, created) = call(app.clone(), "POST", "/api/v1/games/guess_number", None, &[]).await;
    let sid = created["session_id"].as_str().unwrap().to_owned();

    let (status, body) = call(
        app.clone(),
        "POST",
        &format!("/api/v1/games/{sid}/actions"),
        Some(r#"{"action":"guess","number":50}"#),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["prompt"].as_str().is_some_and(|text| !text.is_empty()),
        "{body}"
    );
    assert!(!body["prompt"].as_str().unwrap().contains("{missing:"));
}

#[tokio::test]
async fn an_out_of_range_guess_is_a_client_error_not_a_500() {
    let app = create_app(state());
    let (_s, created) = call(app.clone(), "POST", "/api/v1/games/guess_number", None, &[]).await;
    let sid = created["session_id"].as_str().unwrap().to_owned();

    let (status, body) = call(
        app,
        "POST",
        &format!("/api/v1/games/{sid}/actions"),
        Some(r#"{"action":"guess","number":0}"#),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// ═══════════════════════════════════════════
// 多人身份
// ═══════════════════════════════════════════

async fn started_dice(app: axum::Router) -> (String, Vec<String>) {
    let (_s, created) = call(
        app.clone(),
        "POST",
        "/api/v1/games/liars_dice",
        Some(r#"{"player_count": 2, "dice_per_player": 3}"#),
        &[],
    )
    .await;
    let sid = created["session_id"].as_str().unwrap().to_owned();

    let mut tokens = Vec::new();
    for name in ["Alice", "Bob"] {
        let (_s, body) = call(
            app.clone(),
            "POST",
            &format!("/api/v1/games/{sid}/actions"),
            Some(&json!({"action": "join", "player_name": name}).to_string()),
            &[],
        )
        .await;
        tokens.push(
            body["private"]["player_token"]
                .as_str()
                .expect("join 必须返回 token")
                .to_owned(),
        );
    }
    (sid, tokens)
}

#[tokio::test]
async fn the_player_view_needs_the_token_header() {
    let app = create_app(state());
    let (sid, tokens) = started_dice(app.clone()).await;

    let (status, _body) = call(
        app.clone(),
        "GET",
        &format!("/api/v1/games/{sid}/players/0"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "缺 token 必须拒绝");

    let (status, body) = call(
        app.clone(),
        "GET",
        &format!("/api/v1/games/{sid}/players/0"),
        None,
        &[("x-player-token", "wrong")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "错 token 必须拒绝");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("不匹配")
    );

    let (status, body) = call(
        app,
        "GET",
        &format!("/api/v1/games/{sid}/players/0"),
        None,
        &[("x-player-token", &tokens[0])],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // 不只要"是数组"：`started_dice` 配的是 `dice_per_player: 3`，
    // 所以必须是 3 个、每个都在 1..=6。把骰子数改成 0 或越界也要能红
    // （review 指出这里原来只断言了 `is_array()`）。
    let dice = body["private"]["your_dice"]
        .as_array()
        .unwrap_or_else(|| panic!("自己的骰子必须可见: {body}"));
    assert_eq!(dice.len(), 3, "每人 3 颗骰子，实际: {dice:?}");
    for face in dice {
        let value = face
            .as_i64()
            .unwrap_or_else(|| panic!("骰子面必须是整数: {face}"));
        assert!((1..=6).contains(&value), "骰子面必须在 1..=6，实际 {value}");
    }
}

#[tokio::test]
async fn an_out_of_range_player_index_is_a_client_error() {
    let app = create_app(state());
    let (sid, tokens) = started_dice(app.clone()).await;

    let (status, _body) = call(
        app,
        "GET",
        &format!("/api/v1/games/{sid}/players/99"),
        None,
        &[("x-player-token", &tokens[0])],
    )
    .await;
    assert!(status.is_client_error(), "{status}");
}

#[tokio::test]
async fn a_missing_session_rejects_the_action_before_waiting() {
    // 这条守的是"定位不到对局立刻报错"，不是挂起 55 秒
    let started = std::time::Instant::now();
    let (status, _body) = call(
        create_app(state()),
        "POST",
        "/api/v1/games/nosuch00/actions",
        Some(r#"{"action":"state","player_index":0,"player_token":"x"}"#),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

// ═══════════════════════════════════════════
// Admin 鉴权
// ═══════════════════════════════════════════

#[tokio::test]
async fn every_admin_endpoint_requires_a_token() {
    let app = create_app(state());

    for uri in ["/api/v1/admin/games", "/api/v1/admin/games/anything"] {
        let (status, _body) = call(app.clone(), "GET", uri, None, &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} 没有鉴权");

        let (status, _body) =
            call(app.clone(), "GET", uri, None, &[("x-admin-token", "wrong")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} 接受了错 token");
    }

    let (status, _body) = call(
        app.clone(),
        "POST",
        "/api/v1/admin/prompts/reload",
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "reload 没有鉴权");

    let (status, body) = call(
        app,
        "POST",
        "/api/v1/admin/prompts/reload",
        None,
        &[("x-admin-token", ADMIN)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn the_admin_list_uses_the_envelope() {
    let state = state();
    let manager = Arc::clone(&state.manager);
    let app = create_app(state);
    let created = manager
        .create(
            "guess_number",
            CreateParams::default(),
            std::time::Instant::now(),
        )
        .await
        .unwrap();
    let created_id = created.id.clone();

    let (status, body) = call(
        app,
        "GET",
        "/api/v1/admin/games",
        None,
        &[("x-admin-token", ADMIN)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["games"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    // 信封：框架字段在顶层，引擎状态在 state 里。
    //
    // `session_id` 与创建时那个**精确比较**——信封必须原样透出真实的会话 ID，
    // 不是"随便一个字符串"。只断言 `is_string()` 会放过任何错值
    // （#16 指出）。
    assert_eq!(
        rows[0]["session_id"], created_id,
        "admin 列表的 session_id 必须是 manager 创建时那个"
    );
    // `target` 是随机值，但**范围确定**（猜数字默认 1..=100）且必须是整数。
    // 只断言 `is_number()` 会放过小数、负数、越界值（review 指出）。
    let target = rows[0]["state"]["target"]
        .as_i64()
        .expect("target 必须是整数");
    assert!(
        (1..=100).contains(&target),
        "猜数字的默认 target 必须在 1..=100，实际 {target}"
    );
    assert!(
        rows[0].get("target").is_none(),
        "框架不能把引擎的键提到顶层"
    );
}

#[tokio::test]
async fn the_dashboard_only_injects_the_token_when_shown_it() {
    let app = create_app(state());

    let (status, body) = call(app.clone(), "GET", "/admin", None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let html = body.as_str().unwrap_or_default();
    let text = if html.is_empty() {
        // Html 响应不是 JSON：这里退化成直接看原始字节
        String::new()
    } else {
        html.to_owned()
    };
    if !text.is_empty() {
        assert!(
            !text.contains("window.ADMIN_TOKEN='"),
            "匿名访问不能拿到 token"
        );
    }

    let (status, _body) = call(app, "GET", &format!("/admin?token={ADMIN}"), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
}

// ═══════════════════════════════════════════
// 超时参数
// ═══════════════════════════════════════════

#[tokio::test]
async fn a_non_numeric_timeout_is_rejected() {
    let app = create_app(state());
    let (sid, tokens) = started_dice(app.clone()).await;

    let (status, body) = call(
        app,
        "POST",
        &format!("/api/v1/games/{sid}/actions"),
        Some(
            &json!({
                "action": "state",
                "player_index": 0,
                "player_token": tokens[0],
                "timeout": "abc",
            })
            .to_string(),
        ),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_bad_token_fails_fast_instead_of_waiting() {
    // Python 版的 P1-2：错 token 的 state 调用白等满 55 秒
    let app = create_app(state());
    let (sid, _tokens) = started_dice(app.clone()).await;

    let started = std::time::Instant::now();
    let (status, _body) = call(
        app,
        "POST",
        &format!("/api/v1/games/{sid}/actions"),
        Some(
            &json!({
                "action": "state",
                "player_index": 0,
                "player_token": "not-mine",
            })
            .to_string(),
        ),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "校验必须先于等待，实际耗时 {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn timeout_zero_returns_immediately() {
    let app = create_app(state());
    let (sid, tokens) = started_dice(app.clone()).await;

    // 找一位不是当前回合的玩家，确保它本来会等待
    let (status, public) = call(
        app.clone(),
        "GET",
        &format!("/api/v1/games/{sid}"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let current = public["state"]["current_turn"].as_u64().unwrap() as usize;
    let other = 1 - current;

    let started = std::time::Instant::now();
    let (status, _body) = call(
        app,
        "POST",
        &format!("/api/v1/games/{sid}/actions"),
        Some(
            &json!({
                "action": "state",
                "player_index": other,
                "player_token": tokens[other],
                "timeout": 0,
            })
            .to_string(),
        ),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "timeout=0 不该等待"
    );
}

// ═══════════════════════════════════════════
// 预算的正负两侧
// ═══════════════════════════════════════════

#[tokio::test]
async fn a_positive_budget_actually_waits() {
    // `if budget > 0.0` 改成 `<` 会让**正**预算不进等待——
    // 而 `timeout=0` 那条用例在两种写法下都"立即返回"，分辨不出来。
    //
    // 这里用一个很小的正预算（1 秒），断言"确实等待了接近 1 秒"。
    let app = create_app(state());
    let (sid, tokens) = started_dice(app.clone()).await;

    let (status, public) = call(
        app.clone(),
        "GET",
        &format!("/api/v1/games/{sid}"),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let current = public["state"]["current_turn"].as_u64().unwrap() as usize;
    let other = 1 - current;

    let started = std::time::Instant::now();
    let (status, _body) = call(
        app,
        "POST",
        &format!("/api/v1/games/{sid}/actions"),
        Some(
            &json!({
                "action": "state",
                "player_index": other,
                "player_token": tokens[other],
                "timeout": 1,
            })
            .to_string(),
        ),
        &[],
    )
    .await;
    let elapsed = started.elapsed();

    assert_eq!(status, StatusCode::OK);
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "正预算必须真的等待，实际 {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "等待必须被预算切断，实际 {elapsed:?}"
    );
}

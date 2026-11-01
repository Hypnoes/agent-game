//! 游戏相关的 REST 端点。
//!
//! 调用顺序（对 `state` 至关重要）：解析入参 → 校验身份 → 才进入等待。
//! 任何一步挪到"等对局变化"之后，非法输入都会先白等满整个预算（55 秒），
//! 调用方还以为对手在思考。

use std::sync::Arc;
use std::time::Instant;

use agent_game_server_core::budget::resolve_budget;
use agent_game_server_core::identity::check_player_identity;
use agent_game_server_core::wait::wait_for;
use agent_game_server_core::{
    ActionParams, CreateParams, GameManager, PlayerView, Session, SessionNotFound,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::AppState;

/// `X-Player-Token`：玩家身份的唯一边界。
///
/// 走请求头而不是查询参数：查询参数会进访问日志与 `Referer`，
/// 而 token 就是这局游戏的私有信息凭证。
pub const PLAYER_TOKEN_HEADER: &str = "x-player-token";

/// 把请求体解析成 JSON 对象。
///
/// 空 body 视为"没带参数"；非空但不是合法 JSON、或不是对象，一律 400。
/// Python 版把这两种情况都静默当成 `{}`，于是一个拼错的请求建出了全默认参数
/// 的真实对局并占住匹配池；它还有一条 `body.pop(...)` 直接把非对象 body
/// 打成 500 的路径。
fn parse_object(body: &[u8]) -> Result<serde_json::Map<String, Value>, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::Map::new());
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| ApiError::invalid(format!("请求体不是合法 JSON: {error}")))?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(ApiError::invalid(format!(
            "请求体必须是 JSON 对象，收到 {}",
            type_name(&other)
        ))),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `GET /health`
pub async fn health(State(state): State<AppState>) -> Json<Value> {
    let manager: Arc<GameManager> = Arc::clone(&state.manager);
    Json(json!({
        "ok": true,
        "version": state.version(),
        "games": manager.registry().game_types(),
        "sessions": manager.len(),
    }))
}

/// `GET /api/v1/games/tools`
///
/// 返回每个游戏的机器键、展示名、玩家模式与它的 MCP 工具定义。
pub fn list_tools(State(state): State<AppState>) -> Json<Value> {
    let registry = state.manager.registry();
    let games: Vec<Value> = registry
        .iter()
        .map(|(game_type, meta)| {
            json!({
                "game_type": game_type,
                "display_name": meta.display_name,
                "player_mode": if meta.player_mode.is_multi() { "multi" } else { "single" },
                "tools": agent_game_server_mcp::tools::all_tools_for(registry, game_type),
            })
        })
        .collect();
    Json(json!({ "games": games }))
}

/// `POST /api/v1/games/{game_type}`
pub async fn create_game(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let params = parse_object(&body)?;
    let session = state
        .manager
        .create(&id, CreateParams::new(params), Instant::now())
        .await
        .map_err(ApiError::invalid)?;

    Ok(Json(json!({
        "session_id": session.id,
        "game_type": session.game_type,
        "display_name": session.display_name,
        "phase": session.engine.lock().await.phase(),
    })))
}

/// `GET /api/v1/games/{session_id}` —— 公开视图（不含秘密字段）。
pub async fn game_public_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = state
        .manager
        .get(&id)
        .ok_or_else(|| ApiError::not_found(SessionNotFound(id.clone()).to_string()))?;

    let engine = session.engine.lock().await;
    let state_view = engine.public_view();
    drop(engine);

    Ok(Json(json!({
        "session_id": session.id,
        "game_type": session.game_type,
        "display_name": session.display_name,
        "state": state_view,
    })))
}

/// `GET /api/v1/games/{session_id}/players/{player_index}`
///
/// 玩家视角，含私有信息。多人引擎要求 `X-Player-Token`。**立即返回，不等待。**
/// 单人引擎没有玩家列表，保持开放（与人身份校验的共享实现一致）。
pub async fn player_state(
    State(state): State<AppState>,
    Path((id, player_index)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let session = state
        .manager
        .get(&id)
        .ok_or_else(|| ApiError::not_found(SessionNotFound(id.clone()).to_string()))?;
    let token = header_text(&headers, PLAYER_TOKEN_HEADER);

    let engine = session.engine.lock().await;
    if let Some(tokens) = engine.player_tokens() {
        check_player_identity(Some(&tokens), Some(player_index), token.as_deref())?;
    }
    let view = engine.player_view(Some(player_index))?;
    drop(engine);
    Ok(Json(view.to_json()))
}

/// `POST /api/v1/games/{session_id}/actions`
pub async fn act(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let session = state
        .manager
        .get(&id)
        .ok_or_else(|| ApiError::not_found(SessionNotFound(id.clone()).to_string()))?;

    let mut params = parse_object(&body)?;
    let Some(action) = params
        .remove("action")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
    else {
        return Err(ApiError::invalid("缺少 action 字段"));
    };

    // 引擎崩过的对局不再接受动作：那之后它的内部状态不可信，继续跑只会
    // 让调用方看到一个逻辑上已经错乱的棋盘。
    if session.is_poisoned() {
        return Err(ApiError::poisoned());
    }

    if action == "state" {
        return state_action(&state, &session, params).await;
    }

    // 非 state 动作：timeout 只是传输层的等待预算，不传给引擎
    params.remove("timeout");

    let view = state
        .manager
        .act(&session, &action, ActionParams::new(params))
        .await?;
    Ok(Json(response_body(&session, &view)))
}

/// `action = "state"`：先校验身份，再挂起等待。
async fn state_action(
    state: &AppState,
    session: &Arc<Session>,
    mut params: serde_json::Map<String, Value>,
) -> Result<Json<Value>, ApiError> {
    if session.is_poisoned() {
        return Err(ApiError::poisoned());
    }
    let raw_timeout = params.remove("timeout");
    let args = ActionParams::new(params);
    let player_index = args.player_index()?;
    let player_token = args.player_token()?.map(ToOwned::to_owned);

    let budget = resolve_budget(
        raw_timeout.as_ref(),
        None,
        state.manager.config().block_budget_secs(),
    )
    .map_err(|error| ApiError::invalid(error.to_string()))?;

    // 校验必须在等待之前：错 token 的 state 调用不能白挂满 55 秒
    let (signal, ready_for) = {
        let engine = session.engine.lock().await;
        if let Some(tokens) = engine.player_tokens() {
            check_player_identity(Some(&tokens), player_index, player_token.as_deref())?;
        }
        (engine.signal(), player_index.unwrap_or(0))
    };

    if budget > 0.0 {
        wait_for(&signal, std::time::Duration::from_secs_f64(budget), || {
            let session = Arc::clone(session);
            async move { session.engine.lock().await.is_ready_for(ready_for) }
        })
        .await;
    }

    // 等待期间可能有对局因超时结束，立即结算，不必等后台轮询
    state.manager.check_timeouts(Instant::now()).await;

    let engine = session.engine.lock().await;
    let view = engine.player_view(player_index)?;
    drop(engine);
    Ok(Json(response_body(session, &view)))
}

/// `state` 动作的响应：把等待结果与提示词一起给调用方。
fn response_body(session: &Arc<Session>, view: &PlayerView) -> Value {
    json!({
        "session_id": session.id,
        "game_type": session.game_type,
        "display_name": session.display_name,
        "public": view.public,
        "private": view.private,
        "prompt": view.prompt,
    })
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .filter(|value| !value.is_empty())
}

/// `/api/v1/games/tools` 的查询参数（目前只有可选的 `game_type` 过滤）。
#[derive(Debug, Default, Deserialize)]
pub struct ToolsQuery {
    pub game_type: Option<String>,
}

/// `GET /api/v1/games/tools/{game_type}` —— 单个游戏的工具定义。
pub async fn list_game_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let registry = state.manager.registry();
    let Some(meta) = registry.get(&id) else {
        return Err(ApiError::not_found(format!("未知游戏类型: {id}")));
    };
    Ok(Json(json!({
        "game_type": id,
        "display_name": meta.display_name,
        "player_mode": if meta.player_mode.is_multi() { "multi" } else { "single" },
        "tools": agent_game_server_mcp::tools::all_tools_for(registry, &id),
    })))
}

/// 让 `Query` 参数在不需要时也能编译。
pub async fn tools_with_query(
    State(state): State<AppState>,
    Query(query): Query<ToolsQuery>,
) -> Json<Value> {
    let registry = state.manager.registry();
    let games: Vec<Value> = registry
        .iter()
        .filter(|(game_type, _)| {
            query
                .game_type
                .as_deref()
                .is_none_or(|wanted| wanted == *game_type)
        })
        .map(|(game_type, meta)| {
            json!({
                "game_type": game_type,
                "display_name": meta.display_name,
                "player_mode": if meta.player_mode.is_multi() { "multi" } else { "single" },
                "tools": agent_game_server_mcp::tools::all_tools_for(registry, game_type),
            })
        })
        .collect();
    Json(json!({ "games": games }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn empty_body_is_an_empty_object() {
        assert!(parse_object(b"").unwrap().is_empty());
        assert!(parse_object(b"  \n ").unwrap().is_empty());
    }

    #[test]
    fn malformed_json_is_rejected() {
        let error = parse_object(b"{not json").unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("合法 JSON"), "{}", error.message);
    }

    #[test]
    fn non_object_json_is_rejected_with_its_type() {
        for (raw, expected) in [
            (b"\"hello\"".as_slice(), "string"),
            (b"5".as_slice(), "number"),
            (b"true".as_slice(), "bool"),
            (b"[1,2]".as_slice(), "array"),
            (b"null".as_slice(), "null"),
        ] {
            let error = parse_object(raw).unwrap_err();
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert!(error.message.contains(expected), "{}", error.message);
        }
    }

    #[test]
    fn an_object_body_is_accepted() {
        let map = parse_object(br#"{"a": 1}"#).unwrap();
        assert_eq!(map.get("a"), Some(&Value::from(1)));
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_skips_empty() {
        let mut headers = HeaderMap::new();
        headers.insert("x-player-token", "abc".parse().unwrap());
        assert_eq!(
            header_text(&headers, PLAYER_TOKEN_HEADER).as_deref(),
            Some("abc")
        );
        headers.insert("x-player-token", "".parse().unwrap());
        assert_eq!(header_text(&headers, PLAYER_TOKEN_HEADER), None);
        assert_eq!(header_text(&headers, "x-missing"), None);
    }

    #[test]
    fn type_names_cover_every_json_kind() {
        assert_eq!(type_name(&Value::Null), "null");
        assert_eq!(type_name(&Value::from(true)), "bool");
        assert_eq!(type_name(&Value::from(1)), "number");
        assert_eq!(type_name(&Value::from("s")), "string");
        assert_eq!(type_name(&Value::from(vec![1])), "array");
        assert_eq!(type_name(&json!({})), "object");
    }
}

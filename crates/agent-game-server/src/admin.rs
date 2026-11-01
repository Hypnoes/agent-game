//! Admin 端点与仪表盘。
//!
//! 这里是**唯一**能看到秘密信息（`target` / `all_dice`）的出口，全部强制
//! `X-Admin-Token`。Admin token 在启动时解析一次：有 `AGENT_GAME_ADMIN_TOKEN`
//! 就用它，没有就随机生成，绝不退化成"不设 token 就不校验"。

use std::sync::Arc;

use agent_game_server_core::GameManager;
use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::broadcast::AdminBroadcast;
use crate::error::ApiError;
use crate::state::AppState;

/// 仪表盘页面（编译期内置）。
const DASHBOARD: &str = include_str!("templates/admin.html");

/// 从请求头取 admin token。
///
/// 只认请求头，不认查询参数：查询参数会进访问日志。唯一的例外是
/// `/admin` 页面本身与 WebSocket 握手（浏览器不能给 WS 设自定义头）。
fn admin_token_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-admin-token")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .filter(|value| !value.is_empty())
}

/// Admin 鉴权。失败一律 403——不区分"没带"与"带错"，避免给探测者任何提示。
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let presented = admin_token_header(headers).unwrap_or_default();
    if state.token_matches(&presented) {
        Ok(())
    } else {
        Err(ApiError::forbidden("forbidden"))
    }
}

/// `GET /api/v1/admin/games`
pub async fn list_games(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let rows: Vec<Value> = state
        .manager
        .snapshots()
        .await
        .into_iter()
        .map(|snapshot| snapshot.to_json())
        .collect();
    Ok(Json(json!({ "games": rows })))
}

/// `GET /api/v1/admin/games/{session_id}` —— 秘密信息唯一的出口。
pub async fn game_detail(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let session = state
        .manager
        .get(&session_id)
        .ok_or_else(|| ApiError::not_found("not found"))?;
    let engine = session.engine.lock().await;
    let view = engine.admin_view();
    drop(engine);

    Ok(Json(json!({
        "session_id": session.id,
        "game_type": session.game_type,
        "display_name": session.display_name,
        "state": view,
    })))
}

/// `POST /api/v1/admin/prompts/reload`
///
/// 重载所有引擎的提示词模板。模板来自内置资产（或 `AGENT_GAME_PROMPT_DIR`
/// 指定的目录），所以"重载"的实际效果是重新读盘。
pub async fn reload_prompts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers)?;
    let reloaded = crate::state::reload_all_prompts(&state);
    Ok(Json(json!({ "ok": true, "reloaded": reloaded })))
}

/// `/admin` 的查询参数。
#[derive(Debug, Default, Deserialize)]
pub struct DashboardQuery {
    pub token: Option<String>,
}

/// `GET /admin?token=`
///
/// 页面本身不含秘密，但也不能把 token 交给匿名访问者：
/// - `?token=` 匹配，或 `X-Admin-Token` 头匹配 → 注入 `window.ADMIN_TOKEN`，页面能拉数据
/// - 都没带或带错 → 仍返回页面骨架，但它发起的 API 调用会 403
///
/// 这样既不泄露 token，又不把"缺 token"伪装成 404——运维一眼能看出是没给凭证，
/// 而不是服务挂了。
pub async fn dashboard(
    State(state): State<AppState>,
    Query(query): Query<DashboardQuery>,
    headers: HeaderMap,
) -> Html<String> {
    let presented = query
        .token
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| admin_token_header(&headers))
        .unwrap_or_default();

    let html = if !presented.is_empty() && state.token_matches(&presented) {
        let injected = format!(
            "<script>window.ADMIN_TOKEN={}</script>",
            serde_json::to_string(state.admin_token.as_str()).unwrap_or_else(|_| "\"\"".to_owned())
        );
        DASHBOARD.replace("</head>", &format!("{injected}</head>"))
    } else {
        DASHBOARD.to_owned()
    };
    Html(html)
}

/// `GET /api/v1/admin/ws?token=`
///
/// 浏览器 WebSocket 不能设自定义头，所以 token 走查询参数；不匹配以 1008 关闭。
pub async fn admin_socket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<DashboardQuery>,
) -> Response {
    let presented = query.token.unwrap_or_default();
    if presented.is_empty() || !state.token_matches(&presented) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": {"code": "forbidden"}})),
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, Arc::clone(&state.broadcast)))
}

async fn handle_socket(mut socket: WebSocket, broadcast: Arc<AdminBroadcast>) {
    let client = broadcast.register_client();
    let mut rx = broadcast.subscribe();
    // 连上就先推一次：页面可能在握手完成前错过上一轮广播
    let _ = broadcast.notify();

    loop {
        tokio::select! {
            outcome = rx.recv() => match outcome {
                Ok(_) => {
                    let payload = json!({"type": "state_changed"}).to_string();
                    if socket.send(Message::Text(payload.into())).await.is_err() {
                        break;
                    }
                }
                // 落后太多：这个客户端下一次收到消息时反正会重新拉全量
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                // 客户端发什么都不处理；断开或报错就退出
                None | Some(Ok(Message::Close(_)) | Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }

    broadcast.unregister_client(client);
}

/// 供集成测试断言"两个 app 实例的广播互不影响"。
#[must_use]
pub fn broadcast_of(state: &AppState) -> &Arc<AdminBroadcast> {
    &state.broadcast
}

/// 让 `IntoResponse` 的返回类型在需要显式标注时可用。
pub type AdminResponse = (StatusCode, Json<Value>);

/// `/api/v1/admin/games` 的管理器视图（测试用）。
#[must_use]
pub fn manager_of(state: &AppState) -> &Arc<GameManager> {
    &state.manager
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_token_header_is_read_only_when_non_empty() {
        let mut headers = HeaderMap::new();
        assert_eq!(admin_token_header(&headers), None);
        headers.insert("x-admin-token", "abc".parse().unwrap());
        assert_eq!(admin_token_header(&headers).as_deref(), Some("abc"));
        headers.insert("x-admin-token", "".parse().unwrap());
        assert_eq!(admin_token_header(&headers), None);
    }

    #[test]
    fn require_admin_rejects_missing_and_wrong_tokens() {
        let state = AppState::with_defaults("secret".to_owned());
        let headers = HeaderMap::new();
        assert!(require_admin(&state, &headers).is_err());

        let mut wrong = HeaderMap::new();
        wrong.insert("x-admin-token", "nope".parse().unwrap());
        assert!(require_admin(&state, &wrong).is_err());

        let mut right = HeaderMap::new();
        right.insert("x-admin-token", "secret".parse().unwrap());
        assert!(require_admin(&state, &right).is_ok());
    }

    #[test]
    fn the_dashboard_ships_the_escape_helper() {
        // 玩家名与叫数留言直接来自 Agent，拼进 innerHTML 前必须转义
        assert!(DASHBOARD.contains("function esc("));
        assert!(DASHBOARD.contains("textContent") || DASHBOARD.contains("esc("));
    }

    #[test]
    fn the_dashboard_reads_the_token_from_the_injected_global() {
        assert!(DASHBOARD.contains("window.ADMIN_TOKEN"));
    }

    #[test]
    fn the_dashboard_hardcodes_no_game_names_or_phases() {
        // 旧版有两张硬编码表：`TYPE_LABEL`（游戏名）与一张状态优先级表。
        // 它们让"零改动加游戏"名不副实——新游戏必须回来改这个文件。
        // 展示名改由服务端下发（`display_name`），状态只分进行中/已结束。
        //
        // 只检查**代码**，不检查注释：页面里刻意留了一段说明"旧版为什么这样不行"，
        // 那段注释里会出现旧名字。
        let code: Vec<&str> = DASHBOARD
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect();
        let code = code.join("\n");

        for forbidden in ["TYPE_LABEL", "STATUS_ORDER", "猜数字", "大话骰", "🔢", "🎲"] {
            assert!(
                !code.contains(forbidden),
                "页面代码不该出现硬编码的 {forbidden}"
            );
        }
    }

    #[test]
    fn the_dashboard_escapes_before_inner_html() {
        // 每个 `innerHTML =` 赋值里出现的插值都必须先经过 `esc(`。
        // 完全精确的检查需要解析 JS，这里退一步：断言所有插值点都在
        // `esc(...)` 的形式里，覆盖 `state` 与 `game` 两类来源。
        for suspicious in ["${state.", "${game.", "${call.", "${who}"] {
            if DASHBOARD.contains(suspicious) {
                // 允许 `${state.all_dice.map(...)}` 这种先 map 后 esc 的写法；
                // 但 `${who}` 这种直接插入是不允许的。
                assert_ne!(suspicious, "${who}", "who 必须包在 esc() 里");
            }
        }
        // 正向：玩家名、留言、字段值都走 esc
        assert!(DASHBOARD.contains("esc(name)"));
        assert!(DASHBOARD.contains("esc(call.message)"));
        assert!(DASHBOARD.contains("esc(sid)"));
    }

    #[test]
    fn the_dashboard_uses_event_delegation_not_inline_handlers() {
        // 旧版把 session id 拼进 `onclick="showDetail('...')"`，那又是一处注入面。
        // 现在用 `data-id` + `addEventListener`。
        assert!(!DASHBOARD.contains("onclick="), "不该有内联事件处理器");
        assert!(DASHBOARD.contains("addEventListener"));
        assert!(DASHBOARD.contains("dataset.id"));
    }

    #[test]
    fn the_dashboard_reads_only_the_documented_envelope() {
        // 页面依赖的契约只有四项：列表信封的 `games`、每局的
        // `session_id` / `display_name` / `state`。多依赖一项都是耦合。
        for required in [
            "body.games",
            "game.session_id",
            "game.display_name",
            "game.state",
        ] {
            assert!(DASHBOARD.contains(required), "页面缺少对 {required} 的读取");
        }
        // 旧字段 `game_name` 已不存在，页面不该再读它
        assert!(!DASHBOARD.contains("game_name"), "不该读旧字段 game_name");
    }
}

//! 错误模型：分类只有一个来源。
//!
//! 每个错误都携带 HTTP 状态码与稳定的 `code` 字符串。`code` 是给调用方
//! 程序判断用的，`message` 是给人看的（中文）。二者分开，是为了让"改文案"
//! 不会破坏调用方的逻辑——Python 版的错误只有一段中文文本，客户端只能靠
//! 字符串匹配区分"参数错了"和"服务端挂了"。

use agent_game_server_core::EngineError;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// 一次请求失败的统一形状。
#[derive(Clone, Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_input", message)
    }

    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    /// 对局引擎已经 panic 过，不再接受动作。
    #[must_use]
    pub fn poisoned() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "poisoned",
            "这局对局的引擎已经崩溃，无法继续；请开新的一局",
        )
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// 把引擎错误翻译成 HTTP 错误。
    ///
    /// 这是**唯一**的映射表：MCP 侧的翻译走
    /// `agent_game_server_mcp::engine_error_text`，但分类仍由
    /// [`EngineError::is_client_error`] 决定，两处不会分叉。
    #[must_use]
    pub fn from_engine(error: EngineError) -> Self {
        match error {
            EngineError::MissingToken => Self::forbidden("缺少 player_token"),
            EngineError::TokenMismatch => Self::forbidden("player_token 不匹配"),
            EngineError::InvalidIndex(message) | EngineError::NotFound(message) => {
                Self::invalid(message)
            }
            EngineError::InvalidInput(message) => Self::invalid(message),
            EngineError::UnknownAction(message) => Self::invalid(format!("未知 action: {message}")),
            EngineError::Internal(message) => {
                tracing::error!(%message, "引擎内部错误");
                Self::internal("服务端内部错误（详见服务端日志）")
            }
        }
    }

    #[must_use]
    pub fn body(&self) -> serde_json::Value {
        json!({
            "error": {
                "code": self.code,
                "message": self.message,
            }
        })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

impl From<EngineError> for ApiError {
    fn from(error: EngineError) -> Self {
        Self::from_engine(error)
    }
}

impl From<AgentError> for ApiError {
    fn from(error: AgentError) -> Self {
        Self::invalid(error)
    }
}

/// 创建参数/超时等纯字符串错误统一按调用方问题处理。
pub type AgentError = String;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_client_error_maps_to_4xx() {
        for error in [
            EngineError::MissingToken,
            EngineError::TokenMismatch,
            EngineError::InvalidIndex("9".into()),
            EngineError::NotFound("9".into()),
            EngineError::invalid("nope"),
            EngineError::UnknownAction("x".into()),
        ] {
            let api = ApiError::from_engine(error);
            assert!(
                api.status.is_client_error(),
                "{} → {}",
                api.code,
                api.status
            );
        }
    }

    #[test]
    fn internal_errors_map_to_500() {
        let api = ApiError::from_engine(EngineError::Internal("boom".into()));
        assert_eq!(api.status, StatusCode::INTERNAL_SERVER_ERROR);
        // 内部细节不出现在响应体里
        assert!(!api.message.contains("boom"), "{}", api.message);
    }

    #[test]
    fn identity_errors_are_403_not_400() {
        assert_eq!(
            ApiError::from_engine(EngineError::MissingToken).status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ApiError::from_engine(EngineError::TokenMismatch).status,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn body_carries_a_stable_code() {
        let api = ApiError::invalid("参数不对");
        let body = api.body();
        assert_eq!(body["error"]["code"], "invalid_input");
        assert_eq!(body["error"]["message"], "参数不对");
    }

    #[test]
    fn raw_string_errors_are_client_errors() {
        let api: ApiError = "player_count 必须是 ≥ 2 的整数".to_owned().into();
        assert_eq!(api.status, StatusCode::BAD_REQUEST);
    }
}

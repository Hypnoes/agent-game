//! 引擎与传输共用的错误类型。
//!
//! 分类的意义在边界上：**调用方输入问题**（缺参数、token 不对、索引越界）必须
//! 映射成 4xx / MCP 的"调用方错误"，**服务端问题**必须映射成 5xx 并留堆栈。
//! Python 版曾经把 `KeyError`（缺必填参数）归进"非预期异常"，于是同一个漏传
//! 参数在 REST 上是 400、在 MCP 上是"服务端内部错误: KeyError"。
//!
//! 这里让错误类型本身携带分类，两条传输各写一个翻译函数，分类只有一份。

use thiserror::Error;

/// 引擎层面的错误。全部属于"调用方问题"，除了 [`EngineError::Internal`]。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EngineError {
    /// 玩家编号越界或不是整数。
    #[error("无效的玩家编号: {0}")]
    InvalidIndex(String),
    /// 编号合法但该位置没有玩家。
    #[error("无效的玩家编号: {0}")]
    NotFound(String),
    /// 多人动作缺少 `player_token`。
    #[error("缺少 player_token")]
    MissingToken,
    /// 提供的 `player_token` 与目标玩家不一致。
    #[error("player_token 不匹配")]
    TokenMismatch,
    /// 参数本身不合法（越界叫数、非法猜测值、阶段不允许等）。
    #[error("{0}")]
    InvalidInput(String),
    /// 未知 action。
    #[error("未知 action: {0}")]
    UnknownAction(String),
    /// 引擎内部故障（只在 `check_timeout` 被隔离时出现）。
    #[error("引擎内部错误: {0}")]
    Internal(String),
}

impl EngineError {
    /// 是否是调用方问题。`Internal` 之外都是。
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        !matches!(self, Self::Internal(_))
    }

    /// 构造函数：非法输入。省得到处写 `EngineError::InvalidInput(format!(...))`。
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }
}

/// 会话定位失败。单独一个类型而不是 `EngineError` 的分支：它在 REST 上是 404、
/// 在 MCP 上是"会话不存在"的提示，两种语义都不该和"参数不合法"混在一起。
#[derive(Debug, Error, PartialEq, Eq)]
#[error("会话不存在: {0}")]
pub struct SessionNotFound(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_errors_are_classified() {
        assert!(EngineError::InvalidIndex("99".into()).is_client_error());
        assert!(EngineError::MissingToken.is_client_error());
        assert!(EngineError::TokenMismatch.is_client_error());
        assert!(EngineError::invalid("数量必须 >= 1").is_client_error());
    }

    #[test]
    fn internal_errors_are_not_client_errors() {
        assert!(!EngineError::Internal("clock skewed".into()).is_client_error());
    }

    #[test]
    fn messages_are_stable_for_the_prompts() {
        // 提示词与测试都依赖这两句原文，改动会同时影响 Agent 的观感。
        assert_eq!(EngineError::MissingToken.to_string(), "缺少 player_token");
        assert_eq!(
            EngineError::TokenMismatch.to_string(),
            "player_token 不匹配"
        );
    }
}

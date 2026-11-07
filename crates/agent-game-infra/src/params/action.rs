//! 动作参数：`timeout` 已在传输层剥掉，这里只剩引擎要解释的键。

use serde_json::Value;

use crate::error::EngineError;

/// 动作参数。`timeout` 在这里已经被剥掉——它只属于传输层的等待预算。
#[derive(Clone, Debug, Default)]
pub struct ActionParams {
    pub raw: serde_json::Map<String, Value>,
}

impl ActionParams {
    #[must_use]
    pub fn new(raw: serde_json::Map<String, Value>) -> Self {
        Self { raw }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    /// 取必填整数。
    ///
    /// # Errors
    /// 缺失、或不是整数。
    pub fn require_i64(&self, key: &str) -> Result<i64, EngineError> {
        match self.raw.get(key) {
            None => Err(EngineError::invalid(format!("缺少必填参数: {key}"))),
            Some(Value::Number(number)) => number
                .as_i64()
                .ok_or_else(|| EngineError::invalid(format!("{key} 必须是整数，收到 {number}"))),
            Some(other) => Err(EngineError::invalid(format!(
                "{key} 必须是整数，收到 {other}"
            ))),
        }
    }

    /// 取可选整数。
    ///
    /// # Errors
    /// 存在但不是整数。
    pub fn optional_i64(&self, key: &str) -> Result<Option<i64>, EngineError> {
        match self.raw.get(key) {
            None => Ok(None),
            Some(Value::Number(number)) => Ok(number.as_i64()),
            Some(other) => Err(EngineError::invalid(format!(
                "{key} 必须是整数，收到 {other}"
            ))),
        }
    }

    /// 取必填字符串。
    ///
    /// # Errors
    /// 缺失、或不是字符串。
    pub fn require_str(&self, key: &str) -> Result<&str, EngineError> {
        match self.raw.get(key) {
            None => Err(EngineError::invalid(format!("缺少必填参数: {key}"))),
            Some(Value::String(text)) => Ok(text),
            Some(other) => Err(EngineError::invalid(format!(
                "{key} 必须是字符串，收到 {other}"
            ))),
        }
    }

    /// 取可选字符串（缺失与 `null` 都当没有）。
    ///
    /// # Errors
    /// 存在但不是字符串。
    pub fn optional_str(&self, key: &str) -> Result<Option<&str>, EngineError> {
        match self.raw.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(other) => Err(EngineError::invalid(format!(
                "{key} 必须是字符串，收到 {other}"
            ))),
        }
    }

    /// 取玩家编号。多人动作必填，单人动作可省略。
    ///
    /// # Errors
    /// 存在但不是非负整数。
    pub fn player_index(&self) -> Result<Option<usize>, EngineError> {
        match self.raw.get("player_index") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => {
                let Some(value) = number.as_i64() else {
                    return Err(EngineError::InvalidIndex(number.to_string()));
                };
                if value < 0 {
                    // 负索引会绕回最后一名玩家，等于凭空拿到对手的骰子
                    return Err(EngineError::InvalidIndex(value.to_string()));
                }
                Ok(Some(usize::try_from(value).unwrap_or(usize::MAX)))
            }
            Some(other) => Err(EngineError::InvalidIndex(other.to_string())),
        }
    }

    /// 取 `player_token`。
    ///
    /// # Errors
    /// 存在但不是字符串。
    pub fn player_token(&self) -> Result<Option<&str>, EngineError> {
        self.optional_str("player_token")
    }
}

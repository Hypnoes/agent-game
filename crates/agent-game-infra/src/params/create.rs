//! 创建参数：请求体里的任意 JSON，由各引擎自己解释。

use serde_json::Value;

/// 创建参数：请求体里的任意 JSON，由各引擎自己解释。
#[derive(Clone, Debug, Default)]
pub struct CreateParams {
    pub raw: serde_json::Map<String, Value>,
}

impl CreateParams {
    #[must_use]
    pub fn new(raw: serde_json::Map<String, Value>) -> Self {
        Self { raw }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    /// 取一个整数参数。
    ///
    /// # Errors
    /// 键存在但不是整数（`bool` 不被当作整数）。
    pub fn get_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.raw.get(key) {
            None => Ok(None),
            Some(Value::Number(number)) => number
                .as_i64()
                .map(Some)
                .ok_or_else(|| format!("{key} 必须是整数，收到 {number}")),
            Some(other) => Err(format!("{key} 必须是整数，收到 {other}")),
        }
    }

    /// 取一个字符串参数。
    ///
    /// # Errors
    /// 键存在但不是字符串。
    pub fn get_str(&self, key: &str) -> Result<Option<&str>, String> {
        match self.raw.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.as_str())),
            Some(other) => Err(format!("{key} 必须是字符串，收到 {other}")),
        }
    }

    /// 去掉某些键（`player_token` 不参与配置比较）。
    #[must_use]
    pub fn without(&self, key: &str) -> Self {
        let mut raw = self.raw.clone();
        raw.remove(key);
        Self { raw }
    }
}

impl From<serde_json::Map<String, Value>> for CreateParams {
    fn from(raw: serde_json::Map<String, Value>) -> Self {
        Self { raw }
    }
}

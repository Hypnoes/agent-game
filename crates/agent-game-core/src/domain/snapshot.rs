//! 对外快照：把框架字段装在信封里，不塞进引擎自己的 JSON。

use serde_json::Value;

use crate::domain::game::TimeoutEvent;
use crate::domain::session::SessionId;

/// 一次超时结算的结果。
#[derive(Clone, Debug)]
pub struct TimeoutRecord {
    pub session_id: SessionId,
    pub game_type: &'static str,
    pub event: TimeoutEvent,
}

impl TimeoutRecord {
    /// 转成事件 JSON：`{session_id, game_type, type, expired?, loser?}`。
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = match self.event.to_json() {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        object.insert(
            "session_id".to_owned(),
            Value::String(self.session_id.clone()),
        );
        object.insert(
            "game_type".to_owned(),
            Value::String(self.game_type.to_owned()),
        );
        Value::Object(object)
    }
}

/// admin 列表里的一行。
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub game_type: &'static str,
    pub display_name: &'static str,
    pub created_at_ms: u128,
    /// 引擎的 admin 视图（含秘密）。公开出口不会用它。
    pub state: Value,
}

impl SessionSnapshot {
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "session_id": self.session_id,
            "game_type": self.game_type,
            "display_name": self.display_name,
            "created_at": self.created_at_ms,
            "state": self.state,
        })
    }
}

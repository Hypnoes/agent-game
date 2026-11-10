//! 大厅：把一个请求匹配到已有对局，或新建一桌。
//!
//! 复用顺序（与 Python 版一致）：
//! 1. 带 `player_token` 且命中某个**同类型、未结束**对局里的玩家 → 回到那一局。
//! 2. 多人游戏里有一个 `waiting` 且创建参数兼容的对局 → 加入那一桌。
//! 3. 否则新建。
//!
//! 这段逻辑住在独立文件里，是因为"匹配"与"超时结算"各有独立不变量
//! （AGENTS.md 第 3 条 vs 第 9/10 条），混在一个 impl 里时改一处必须读另一处。

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::session::{Session, SessionId};
use crate::supervisor::GameManager;
use agent_game_server_infra::params::CreateParams;

impl GameManager {
    /// 创建或复用一个 session。
    ///
    /// 复用顺序（与 Python 版一致）：
    /// 1. 带 `player_token` 且命中某个**同类型、未结束**对局里的玩家 → 回到那一局。
    /// 2. 多人游戏里有一个 `waiting` 且创建参数兼容的对局 → 加入那一桌。
    /// 3. 否则新建。
    ///
    /// # Errors
    /// 未知游戏类型，或构造函数拒绝了参数。
    pub async fn create(
        &self,
        game_type: &str,
        params: CreateParams,
        now: Instant,
    ) -> Result<Arc<Session>, String> {
        if !self.registry.contains(game_type) {
            let known = self.registry.game_types();
            return Err(format!("未知游戏类型: {game_type}。已注册: {known:?}"));
        }

        let mut setup = params.clone();
        let player_token = setup
            .raw
            .get("player_token")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        setup.raw.remove("player_token");

        // `player_name` 是**动作参数**，不是建桌配置。
        //
        // 留在 `setup` 里会让"两个人想坐同一桌"永远匹配失败：他们的名字不同，
        // `compatible` 一比较就判为不兼容，于是各开一桌。这个坑在传输层只透传
        // `player_token` 的时期被掩盖了——那时 `player_name` 根本到不了这里；
        // 把创建参数改成整体透传之后它立刻现形。
        setup.raw.remove("player_name");

        let existing = self.session_list();

        // 1. 断线重连：只认同类型、且未结束的对局
        if let Some(token) = player_token.as_deref() {
            for session in &existing {
                if session.game_type != game_type || self.is_finished(session).await {
                    continue;
                }
                let engine = session.engine.lock().await;
                if engine.find_player_by_token(token).is_some() {
                    drop(engine);
                    return Ok(Arc::clone(session));
                }
            }
        }

        // 2. 多人游戏：加入等待中的、参数兼容的桌子
        let multi = self
            .registry
            .get(game_type)
            .is_some_and(|meta| meta.player_mode.is_multi());
        if multi {
            for session in &existing {
                if session.game_type != game_type {
                    continue;
                }
                let engine = session.engine.lock().await;
                let waiting = engine.phase() == engine.waiting_phase() && !engine.is_finished();
                drop(engine);
                if waiting && compatible(&session.setup, &setup) {
                    return Ok(Arc::clone(session));
                }
            }
        }

        // 3. 新建
        let engine = self.registry.create(game_type, &setup, &self.config)?;
        let Some(meta) = self.registry.get(game_type) else {
            return Err(format!("未知游戏类型: {game_type}"));
        };
        let session = Arc::new(Session::new(new_session_id(), meta, engine, setup, now));
        if let Ok(mut map) = self.sessions.write() {
            map.insert(session.id.clone(), Arc::clone(&session));
        }
        Ok(session)
    }
}

/// 参数兼容：请求里出现的每个键，只要已存在的 session 也给了同名键就必须相等。
///
/// 引擎没声明的键（比如它不认识的调参）忽略——与 Python 版一致，
/// 这样新增请求参数不会把老引擎挤出匹配池。
pub(crate) fn compatible(existing: &CreateParams, wanted: &CreateParams) -> bool {
    for (key, value) in &wanted.raw {
        if let Some(current) = existing.raw.get(key)
            && current != value
        {
            return false;
        }
    }
    true
}

pub(crate) fn new_session_id() -> SessionId {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

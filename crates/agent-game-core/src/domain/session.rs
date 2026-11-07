//! 一个进行中的对局：引擎加锁、结束时刻、毒化标记。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::sync::Mutex as AsyncMutex;

use crate::domain::game::BoxedEngine;
use crate::domain::registry::GameMeta;
use agent_game_server_infra::params::CreateParams;

/// session 标识：UUID v4 的前 8 个字符。
pub type SessionId = String;

/// 一个进行中的对局。
#[derive(Debug)]
pub struct Session {
    pub id: SessionId,
    pub game_type: &'static str,
    pub display_name: &'static str,
    pub created_at: Instant,
    /// 引擎。所有访问都必须先拿这把锁，且**不要跨 await 持有**。
    pub engine: AsyncMutex<BoxedEngine>,
    /// 创建参数（不含 `player_token`），用于"同一张桌子"的匹配判断。
    pub setup: CreateParams,
    finished_at: Mutex<Option<Instant>>,
    poisoned: AtomicBool,
}

impl Session {
    pub(crate) fn new(
        id: SessionId,
        meta: &GameMeta,
        engine: BoxedEngine,
        setup: CreateParams,
        now: Instant,
    ) -> Self {
        Self {
            id,
            game_type: meta.game_type,
            display_name: meta.display_name,
            created_at: now,
            engine: AsyncMutex::new(engine),
            setup,
            finished_at: Mutex::new(None),
            poisoned: AtomicBool::new(false),
        }
    }

    /// 引擎是否已经 panic 过。毒化后的对局不再接受动作。
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }

    /// 标记引擎已毒化。
    pub fn poison(&self) {
        self.poisoned.store(true, Ordering::SeqCst);
    }

    pub(crate) fn finished_at(&self) -> Option<Instant> {
        self.finished_at.lock().ok().and_then(|guard| *guard)
    }

    pub(crate) fn set_finished_at(&self, value: Option<Instant>) {
        if let Ok(mut guard) = self.finished_at.lock() {
            *guard = value;
        }
    }
}

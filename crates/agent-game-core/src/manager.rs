//! Session 路由与生命周期。
//!
//! 职责：查注册表、创建 session（多人游戏自动匹配）、按 token 找到原对局、
//! 超时结算、已结束对局的回收。
//!
//! 三处与 Python 版刻意不同的地方：
//!
//! 1. **引擎装在 `tokio::sync::Mutex` 里，持锁不跨 `.await`**。等待路径见
//!    [`crate::wait`]：取条件、放锁、再等。Python 版是单线程 asyncio，一条
//!    "取状态 → 渲染 → 返回"中间没有 await，天然不会有这种问题；Rust 里必须
//!    以锁的边界显式表达。
//! 2. **`check_timeouts` 逐 session 隔离错误**。一个坏引擎返回 `Err` 只影响它
//!    自己——那条"坏引擎不能杀死整个服务"的约束从 `except Exception` 变成类型上的
//!    `Result`。
//! 3. **快照是信封**：`{session_id, game_type, display_name, created_at, state}`。
//!    框架字段不再塞进引擎的 JSON，污染在结构上不可能发生（Python 版 P1-4
//!    就是直接往 `get_public_state()` 的返回值里写 `session_id`）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::engine::{BoxedEngine, CreateParams, TimeoutEvent};
use crate::error::SessionNotFound;
use crate::registry::{GameMeta, Registry};

/// session 标识：UUID v4 的前 8 个字符。
pub type SessionId = String;

/// 已上报过的超时事件的缓存上限。
const MAX_SEEN_EXPIRED: usize = 512;

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
    fn new(
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

    fn finished_at(&self) -> Option<Instant> {
        self.finished_at.lock().ok().and_then(|guard| *guard)
    }

    fn set_finished_at(&self, value: Option<Instant>) {
        if let Ok(mut guard) = self.finished_at.lock() {
            *guard = value;
        }
    }
}

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

/// Session 路由与生命周期。
#[derive(Debug)]
pub struct GameManager {
    registry: Registry,
    config: Config,
    sessions: RwLock<BTreeMap<SessionId, Arc<Session>>>,
    /// `(session_id, 事件类型)` —— 同一个超时事件不跨多次轮询重复上报。
    seen_expired: Mutex<Vec<(SessionId, &'static str)>>,
    /// 起始时刻，`created_at_ms` 由它与 session 的 `Instant` 差值算出。
    started: Instant,
}

impl GameManager {
    #[must_use]
    pub fn new(registry: Registry, config: Config) -> Self {
        Self {
            registry,
            config,
            sessions: RwLock::new(BTreeMap::new()),
            seen_expired: Mutex::new(Vec::new()),
            started: Instant::now(),
        }
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// 当前对局数量（测试与 `/health` 用）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.read().map_or(0, |map| map.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 所有 session 的 id（诊断用）。
    #[must_use]
    pub fn session_ids(&self) -> Vec<SessionId> {
        self.sessions
            .read()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions
            .read()
            .ok()
            .and_then(|map| map.get(session_id).cloned())
    }

    /// 移除一个 session。
    pub fn remove(&self, session_id: &str) {
        if let Ok(mut map) = self.sessions.write() {
            map.remove(session_id);
        }
    }

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
                let waiting = engine.phase() == "waiting" && !engine.is_finished();
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

    async fn is_finished(&self, session: &Arc<Session>) -> bool {
        let engine = session.engine.lock().await;
        engine.is_finished()
    }

    /// 取 session 快照列表（Arc 的克隆），供调用方在锁外逐个处理。
    fn session_list(&self) -> Vec<Arc<Session>> {
        self.sessions
            .read()
            .map(|map| map.values().cloned().collect())
            .unwrap_or_default()
    }

    /// 全部 session 的 admin 快照。
    pub async fn snapshots(&self) -> Vec<SessionSnapshot> {
        let sessions = self.session_list();
        let mut rows = Vec::with_capacity(sessions.len());
        for session in sessions {
            let engine = session.engine.lock().await;
            let state = engine.admin_view();
            drop(engine);
            rows.push(SessionSnapshot {
                session_id: session.id.clone(),
                game_type: session.game_type,
                display_name: session.display_name,
                created_at_ms: (session.created_at - self.started).as_millis(),
                state,
            });
        }
        rows
    }

    /// 全部 session 的指纹：序列化后用于"状态真的变了吗"的判断。
    ///
    /// `Value` 序列化失败会返回 `None`（比如引擎塞了会 panic 的 `Debug` 实现），
    /// 由调用方决定强制广播——一次序列化失败不能把轮询循环打死。
    pub async fn state_signature(&self) -> Option<String> {
        let rows: Vec<Value> = self
            .snapshots()
            .await
            .into_iter()
            .map(|row| row.to_json())
            .collect();
        serde_json::to_string(&rows).ok()
    }

    /// 结算全部 session 的超时，并回收超过保留期的已结束对局。
    ///
    /// 单个引擎的错误就地隔离：记日志后继续下一个。这是**唯一**结算超时的地方，
    /// 一个坏引擎不能让其余对局永远不再超时。
    pub async fn check_timeouts(&self, now: Instant) -> Vec<TimeoutRecord> {
        let mut events = Vec::new();
        for session in self.session_list() {
            let mut engine = session.engine.lock().await;

            // 已结束：第一次看到时记下结束时刻，超过保留期就回收
            if engine.is_finished() {
                drop(engine);
                let finished_at = session.finished_at().unwrap_or_else(|| {
                    session.set_finished_at(Some(now));
                    now
                });
                if now.saturating_duration_since(finished_at) > self.config.finished_retention {
                    self.remove(&session.id);
                }
                continue;
            }

            // 重新开局（同一 session 又 start 了一局）：旧一轮的结束时刻必须清掉，
            // 否则下一轮结束会被立刻当成过期回收。
            session.set_finished_at(None);

            let outcome = engine.check_timeout(now);
            let finished_now = engine.is_finished();
            drop(engine);

            match outcome {
                Ok(Some(event)) => {
                    if event.expired {
                        // 大厅/单人对局超时：只报一次，随后立刻回收
                        let first_time = self.mark_expired(&session.id, event.kind);
                        self.remove(&session.id);
                        if first_time {
                            events.push(TimeoutRecord {
                                session_id: session.id.clone(),
                                game_type: session.game_type,
                                event,
                            });
                        }
                    } else {
                        if finished_now {
                            session.set_finished_at(Some(now));
                        }
                        events.push(TimeoutRecord {
                            session_id: session.id.clone(),
                            game_type: session.game_type,
                            event,
                        });
                    }
                }
                Ok(None) => {
                    if finished_now {
                        session.set_finished_at(Some(now));
                    }
                }
                Err(error) => {
                    // 坏引擎只影响自己：记日志，继续下一个
                    tracing::warn!(
                        session_id = %session.id,
                        game_type = session.game_type,
                        %error,
                        "check_timeouts 跳过异常 session"
                    );
                }
            }
        }
        events
    }

    /// 登记一个已上报的超时事件。
    ///
    /// 返回 `true` 表示这是第一次上报。顺带把缓存限制在有界范围内，
    /// 避免长期运行后无限增长（FIFO 淘汰）。
    fn mark_expired(&self, session_id: &str, kind: &'static str) -> bool {
        let Ok(mut seen) = self.seen_expired.lock() else {
            return true;
        };
        if seen
            .iter()
            .any(|(id, existing)| id == session_id && *existing == kind)
        {
            return false;
        }
        seen.push((session_id.to_owned(), kind));
        while seen.len() > MAX_SEEN_EXPIRED {
            seen.remove(0);
        }
        true
    }

    /// 定位一个对局。
    ///
    /// # Errors
    /// session 不存在。
    pub fn require(&self, session_id: &str) -> Result<Arc<Session>, SessionNotFound> {
        self.get(session_id)
            .ok_or_else(|| SessionNotFound(session_id.to_owned()))
    }

    /// 在一个会话上执行动作，并把"引擎 panic"翻译成"毒化这个对局"。
    ///
    /// 引擎是外部代码（新游戏由别人写），它的内部 bug 不该让整个进程去死。
    /// `catch_unwind` 需要 `UnwindSafe`，而 `Box<dyn GameEngine>` 不保证它，
    /// 所以调用方用 `AssertUnwindSafe` 显式声明"我接受 panic 之后状态可能不一致"
    /// ——这正是要把对局毒化掉的原因：它确实可能不一致了。
    ///
    /// 这个包装是**唯一**把 panic 变成毒化状态的地方，传输层只需要读
    /// [`Session::is_poisoned`]。
    ///
    /// # Errors
    /// 引擎返回的错误原样透传；panic 转成 [`EngineError::Internal`]。
    pub async fn act(
        &self,
        session: &Arc<Session>,
        action: &str,
        params: crate::engine::ActionParams,
    ) -> Result<crate::engine::PlayerView, crate::error::EngineError> {
        let mut engine = session.engine.lock().await;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine.handle_action(action, params)
        }));

        match outcome {
            Ok(result) => result,
            Err(_payload) => {
                session.poison();
                tracing::error!(
                    session_id = %session.id,
                    game_type = session.game_type,
                    action,
                    "引擎在 handle_action 中 panic，对局标记为 poisoned"
                );
                Err(crate::error::EngineError::Internal(
                    "引擎在处理动作时崩溃".to_owned(),
                ))
            }
        }
    }
}

/// 参数兼容：请求里出现的每个键，只要已存在的 session 也给了同名键就必须相等。
///
/// 引擎没声明的键（比如它不认识的调参）忽略——与 Python 版一致，
/// 这样新增请求参数不会把老引擎挤出匹配池。
fn compatible(existing: &CreateParams, wanted: &CreateParams) -> bool {
    for (key, value) in &wanted.raw {
        if let Some(current) = existing.raw.get(key)
            && current != value
        {
            return false;
        }
    }
    true
}

fn new_session_id() -> SessionId {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as TestArc;

    use crate::engine::{ActionParams, GameEngine, PlayerMode};
    use crate::error::EngineError;
    use crate::signal::ChangeSignal;
    use crate::testing::DummyEngine;

    fn meta(game_type: &'static str, mode: PlayerMode, phase: &'static str) -> GameMeta {
        GameMeta::new(game_type, mode, move |_params, _config| {
            Ok(Box::new(
                DummyEngine::new(game_type)
                    .with_player_mode(mode)
                    .with_phase(phase),
            ) as BoxedEngine)
        })
    }

    fn manager_with(entries: Vec<GameMeta>) -> GameManager {
        let mut registry = Registry::new();
        for entry in entries {
            registry.register(entry);
        }
        GameManager::new(registry, Config::default())
    }

    #[tokio::test]
    async fn create_and_lookup_a_single_player_session() {
        let manager = manager_with(vec![meta(
            "guess_number",
            PlayerMode::Single,
            "in_progress",
        )]);
        let session = manager
            .create("guess_number", CreateParams::default(), Instant::now())
            .await
            .unwrap();

        assert_eq!(session.game_type, "guess_number");
        assert_eq!(session.id.len(), 8);
        assert_eq!(manager.len(), 1);
        assert!(manager.get(&session.id).is_some());
    }

    #[tokio::test]
    async fn unknown_game_type_is_rejected() {
        let manager = manager_with(vec![]);
        let err = manager
            .create("nope", CreateParams::default(), Instant::now())
            .await
            .unwrap_err();
        assert!(err.contains("未知游戏类型"), "{err}");
    }

    #[tokio::test]
    async fn single_player_games_never_share_a_session() {
        let manager = manager_with(vec![meta(
            "guess_number",
            PlayerMode::Single,
            "in_progress",
        )]);
        let first = manager
            .create("guess_number", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        let second = manager
            .create("guess_number", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(manager.len(), 2);
    }

    #[tokio::test]
    async fn multi_player_requests_join_a_waiting_table() {
        let manager = manager_with(vec![meta("liars_dice", PlayerMode::Multi, "waiting")]);
        let first = manager
            .create("liars_dice", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        let second = manager
            .create("liars_dice", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        assert_eq!(first.id, second.id, "等待中的桌子应当被复用");
        assert_eq!(manager.len(), 1);
    }

    #[tokio::test]
    async fn a_started_table_is_not_reused_for_matching() {
        let manager = manager_with(vec![meta("liars_dice", PlayerMode::Multi, "bidding")]);
        let first = manager
            .create("liars_dice", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        let second = manager
            .create("liars_dice", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        assert_ne!(first.id, second.id, "已开局的桌子不该再被匹配");
    }

    #[tokio::test]
    async fn waiting_tables_with_different_setup_are_not_merged() {
        let manager = manager_with(vec![meta("liars_dice", PlayerMode::Multi, "waiting")]);
        let mut two = CreateParams::default();
        two.raw.insert("player_count".to_owned(), Value::from(2));
        let two_session = manager
            .create("liars_dice", two, Instant::now())
            .await
            .unwrap();

        let mut six = CreateParams::default();
        six.raw.insert("player_count".to_owned(), Value::from(6));
        let six_session = manager
            .create("liars_dice", six, Instant::now())
            .await
            .unwrap();

        assert_ne!(two_session.id, six_session.id);
        assert_eq!(manager.len(), 2);
    }

    #[tokio::test]
    async fn finished_sessions_are_reclaimed_after_retention() {
        let config = Config::default();
        let retention = config.finished_retention;
        let mut registry = Registry::new();
        registry.register(meta("guess_number", PlayerMode::Single, "in_progress"));
        let manager = GameManager::new(registry, config);

        let start = Instant::now();
        let session = manager
            .create("guess_number", CreateParams::default(), start)
            .await
            .unwrap();
        {
            let mut engine = session.engine.lock().await;
            engine
                .handle_action("finish", ActionParams::default())
                .unwrap();
        }

        // 第一次结算：只记下结束时刻，不回收
        manager.check_timeouts(start).await;
        assert_eq!(manager.len(), 1);

        // 超过保留期
        manager.check_timeouts(start + retention * 2).await;
        assert_eq!(manager.len(), 0);
    }

    #[tokio::test]
    async fn snapshots_carry_the_envelope_not_a_merged_map() {
        let manager = manager_with(vec![meta(
            "guess_number",
            PlayerMode::Single,
            "in_progress",
        )]);
        let session = manager
            .create("guess_number", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        let rows = manager.snapshots().await;

        assert_eq!(rows.len(), 1);
        let json = rows[0].to_json();
        assert_eq!(json["session_id"], session.id);
        assert_eq!(json["game_type"], "guess_number");
        assert_eq!(json["display_name"], "guess_number");
        // admin 视图里的秘密字段待在 state 里，不会被塞进顶层
        assert_eq!(json["state"]["secret"], "admin-only");
        assert!(json.get("secret").is_none());
    }

    #[tokio::test]
    async fn a_broken_engine_does_not_stop_the_others() {
        let mut registry = Registry::new();
        registry.register(meta("guess_number", PlayerMode::Single, "in_progress"));
        registry.register(GameMeta::new("exploding", PlayerMode::Single, |_p, _c| {
            Ok(Box::new(DummyEngine::new("exploding").exploding()) as BoxedEngine)
        }));
        let manager = GameManager::new(registry, Config::default());

        let good = manager
            .create("guess_number", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        manager
            .create("exploding", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        {
            let mut engine = good.engine.lock().await;
            engine
                .handle_action("finish", ActionParams::default())
                .unwrap();
        }

        // 坏引擎抛错，但好引擎仍然被结算（表现为不 panic、两个 session 都还在）
        let events = manager.check_timeouts(Instant::now()).await;
        assert!(events.is_empty(), "没有超时事件时不该产生事件");
        assert_eq!(manager.len(), 2);
    }

    #[tokio::test]
    async fn timeout_events_are_reported_once_then_the_session_is_removed() {
        #[derive(Debug)]
        struct LobbyTimeout;
        impl GameEngine for LobbyTimeout {
            fn game_type(&self) -> &'static str {
                "lobby"
            }
            fn player_mode(&self) -> PlayerMode {
                PlayerMode::Multi
            }
            fn phase(&self) -> &'static str {
                "waiting"
            }
            fn admin_view(&self) -> Value {
                Value::Null
            }
            fn public_view(&self) -> Value {
                Value::Null
            }
            fn player_view(&self, _i: Option<usize>) -> Result<crate::PlayerView, EngineError> {
                Ok(crate::PlayerView::default())
            }
            fn handle_action(
                &mut self,
                _a: &str,
                _p: ActionParams,
            ) -> Result<crate::PlayerView, EngineError> {
                Ok(crate::PlayerView::default())
            }
            fn check_timeout(
                &mut self,
                _now: Instant,
            ) -> Result<Option<TimeoutEvent>, EngineError> {
                Ok(Some(TimeoutEvent::lobby_timeout()))
            }
            fn signal(&self) -> TestArc<ChangeSignal> {
                static SIGNAL: std::sync::OnceLock<ChangeSignal> = std::sync::OnceLock::new();
                let signal = SIGNAL.get_or_init(ChangeSignal::new);
                let _ = signal;
                TestArc::new(ChangeSignal::new())
            }
            fn prompts(&self) -> &crate::PromptStore {
                static PROMPTS: std::sync::OnceLock<crate::PromptStore> =
                    std::sync::OnceLock::new();
                PROMPTS.get_or_init(|| crate::PromptStore::new("[templates]\n"))
            }
            fn setup_params(&self) -> CreateParams {
                CreateParams::default()
            }
        }

        let mut registry = Registry::new();
        registry.register(GameMeta::new("lobby", PlayerMode::Multi, |_p, _c| {
            Ok(Box::new(LobbyTimeout) as BoxedEngine)
        }));
        let manager = GameManager::new(registry, Config::default());
        manager
            .create("lobby", CreateParams::default(), Instant::now())
            .await
            .unwrap();

        let events = manager.check_timeouts(Instant::now()).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.kind, "lobby_timeout");
        assert_eq!(manager.len(), 0, "大厅超时后立即回收");
    }

    #[tokio::test]
    async fn an_engine_panic_poisons_the_session_instead_of_killing_the_process() {
        // 引擎是新游戏作者写的代码，它的 bug 不该让整个进程去死。
        // panic 之后那个对局的内部状态可能已经不一致，所以标记 poisoned：
        // 后续动作一律拒绝，而不是让调用方看到一个逻辑上错乱的棋盘。
        #[derive(Debug)]
        struct Panicky;
        #[allow(clippy::panic)] // 这个用例的全部意义就是让引擎 panic
        impl GameEngine for Panicky {
            fn game_type(&self) -> &'static str {
                "panicky"
            }
            fn player_mode(&self) -> PlayerMode {
                PlayerMode::Single
            }
            fn phase(&self) -> &'static str {
                "in_progress"
            }
            fn admin_view(&self) -> Value {
                Value::Null
            }
            fn public_view(&self) -> Value {
                Value::Null
            }
            fn player_view(&self, _i: Option<usize>) -> Result<crate::PlayerView, EngineError> {
                Ok(crate::PlayerView::default())
            }
            fn handle_action(
                &mut self,
                _action: &str,
                _params: ActionParams,
            ) -> Result<crate::PlayerView, EngineError> {
                panic!("模拟引擎内部 bug")
            }
            fn check_timeout(
                &mut self,
                _now: Instant,
            ) -> Result<Option<TimeoutEvent>, EngineError> {
                Ok(None)
            }
            fn signal(&self) -> TestArc<ChangeSignal> {
                TestArc::new(ChangeSignal::new())
            }
            fn prompts(&self) -> &crate::PromptStore {
                static PROMPTS: std::sync::OnceLock<crate::PromptStore> =
                    std::sync::OnceLock::new();
                PROMPTS.get_or_init(|| crate::PromptStore::new("[templates]\n"))
            }
            fn setup_params(&self) -> CreateParams {
                CreateParams::default()
            }
        }

        let mut registry = Registry::new();
        registry.register(GameMeta::new("panicky", PlayerMode::Single, |_p, _c| {
            Ok(Box::new(Panicky) as BoxedEngine)
        }));
        let manager = GameManager::new(registry, Config::default());
        let session = manager
            .create("panicky", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        assert!(!session.is_poisoned());

        let result = manager
            .act(&session, "anything", ActionParams::default())
            .await;

        assert!(result.is_err(), "panic 必须转成错误");
        assert!(session.is_poisoned(), "对局必须被标记为 poisoned");
        // 其它对局不受影响
        assert_eq!(manager.len(), 1);
    }

    #[tokio::test]
    async fn a_finished_engine_does_not_emit_timeout_events() {
        let manager = manager_with(vec![meta(
            "guess_number",
            PlayerMode::Single,
            "in_progress",
        )]);
        let session = manager
            .create("guess_number", CreateParams::default(), Instant::now())
            .await
            .unwrap();
        {
            let mut engine = session.engine.lock().await;
            engine
                .handle_action("finish", ActionParams::default())
                .unwrap();
        }
        let events = manager.check_timeouts(Instant::now()).await;
        assert!(events.is_empty());
        assert_eq!(manager.len(), 1, "刚结束的对局还在保留期内");
    }

    #[test]
    fn compatible_ignores_keys_the_existing_session_does_not_know() {
        let mut existing = serde_json::Map::new();
        existing.insert("player_count".to_owned(), Value::from(2));
        let mut wanted = serde_json::Map::new();
        wanted.insert("player_count".to_owned(), Value::from(2));
        wanted.insert("unknown".to_owned(), Value::from(1));

        assert!(compatible(
            &CreateParams::new(existing.clone()),
            &CreateParams::new(wanted)
        ));

        let mut mismatched = serde_json::Map::new();
        mismatched.insert("player_count".to_owned(), Value::from(6));
        assert!(!compatible(
            &CreateParams::new(existing),
            &CreateParams::new(mismatched)
        ));
    }

    #[test]
    fn session_ids_are_eight_lowercase_hex_chars() {
        let id = new_session_id();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}

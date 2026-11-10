//! 测试替身与共享夹具。
//!
//! 只在 `cfg(test)` 与 `#[cfg(any(test, feature = "testing"))]` 下编译：
//! 三个 crate 的测试都用它当引擎替身（注册表的前缀解析、manager 的匹配、
//! poller 的坏引擎隔离），放在这里而不是各自 `tests/` 下各写一份。

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::Value;

use crate::game::{DEFAULT_WAITING_PHASE, GameEngine, PlayerMode, PlayerView, TimeoutEvent};
use agent_game_server_infra::error::EngineError;
use agent_game_server_infra::params::ActionParams;
use agent_game_server_infra::prompt::PromptStore;
use agent_game_server_infra::signal::ChangeSignal;

fn shared_prompts() -> &'static PromptStore {
    static PROMPTS: OnceLock<PromptStore> = OnceLock::new();
    PROMPTS.get_or_init(|| PromptStore::new("[templates]\ndummy = \"dummy\"\n"))
}

/// 一个可配置的最小引擎。
#[derive(Debug)]
pub struct DummyEngine {
    game_type: &'static str,
    player_mode: PlayerMode,
    phase: &'static str,
    signal: Arc<ChangeSignal>,
    /// 每次 `check_timeout` 是否返回错误。
    explode: bool,
    /// 是否在第一次 `check_timeout` 后进入终态。
    finish_on_timeout: bool,
    /// 自报的大厅态名。`None` = **不覆盖**，用 trait 的默认实现。
    ///
    /// 用 `Option` 而不是直接给个 `"waiting"` 字符串：那样 `DummyEngine`
    /// 永远覆盖 trait 默认值，**默认实现就再也测不到了**——改坏默认值
    /// （比如把 `"waiting"` 改成 `"finished"`）不会有任何用例变红。
    waiting_phase: Option<&'static str>,
    /// 自报的终结态名。默认 `["finished"]`。
    finished_states: &'static [&'static str],
}

impl DummyEngine {
    #[must_use]
    pub fn new(game_type: &'static str) -> Self {
        Self {
            game_type,
            player_mode: PlayerMode::Single,
            phase: "in_progress",
            signal: Arc::new(ChangeSignal::new()),
            explode: false,
            finish_on_timeout: false,
            waiting_phase: None,
            finished_states: &["finished"],
        }
    }

    #[must_use]
    pub fn with_phase(mut self, phase: &'static str) -> Self {
        self.phase = phase;
        self
    }

    #[must_use]
    pub fn with_player_mode(mut self, mode: PlayerMode) -> Self {
        self.player_mode = mode;
        self
    }

    /// 自报一个不同的大厅态名。
    ///
    /// 用来证明 `lobby.rs` 真的按**引擎自报的名字**判断大厅态，而不是写死
    /// `"waiting"`——后者会让"零改动加一个新游戏"不成立。
    #[must_use]
    pub fn with_waiting_phase(mut self, name: &'static str) -> Self {
        self.waiting_phase = Some(name);
        self
    }

    /// 自报一组终结态名。
    ///
    /// 用来构造"阶段名恰好等于大厅名、但其实已经结束"这种刁钻场景，
    /// 验证 `lobby.rs` 的 `&& !is_finished()` 真的在起作用。
    #[must_use]
    pub fn finished_states_are(mut self, states: &'static [&'static str]) -> Self {
        self.finished_states = states;
        self
    }

    /// `check_timeout` 每次都返回内部错误，用于验证"坏引擎只影响自己"。
    #[must_use]
    pub fn exploding(mut self) -> Self {
        self.explode = true;
        self
    }

    /// 一次 `check_timeout` 之后进入 `finished`，用于验证保留期回收。
    #[must_use]
    pub fn finishing_on_timeout(mut self) -> Self {
        self.finish_on_timeout = true;
        self
    }
}

impl GameEngine for DummyEngine {
    fn game_type(&self) -> &'static str {
        self.game_type
    }

    fn player_mode(&self) -> PlayerMode {
        self.player_mode
    }

    fn finished_states(&self) -> &'static [&'static str] {
        self.finished_states
    }

    fn phase(&self) -> &'static str {
        self.phase
    }

    fn waiting_phase(&self) -> &'static str {
        // 没有覆盖时返回**默认常量**。
        //
        // 不能写 `GameEngine::waiting_phase(self)`——`Self` 是 `DummyEngine`，
        // 那调回本方法，无限递归（实测 `STATUS_STACK_OVERFLOW`，把整个
        // `routing` 测试二进制打崩）。Rust 没有"调用被覆盖的 trait 默认实现"
        // 的语法，所以默认值必须抽成常量（`DEFAULT_WAITING_PHASE`）。
        self.waiting_phase.unwrap_or(DEFAULT_WAITING_PHASE)
    }

    fn admin_view(&self) -> Value {
        serde_json::json!({"phase": self.phase, "secret": "admin-only"})
    }

    fn public_view(&self) -> Value {
        serde_json::json!({"phase": self.phase})
    }

    fn player_view(&self, _player_index: Option<usize>) -> Result<PlayerView, EngineError> {
        Ok(PlayerView {
            public: self.public_view(),
            private: serde_json::json!({}),
            prompt: format!("{}:{}", self.game_type, self.phase),
        })
    }

    fn handle_action(
        &mut self,
        action: &str,
        _params: ActionParams,
    ) -> Result<PlayerView, EngineError> {
        match action {
            "finish" => {
                self.phase = "finished";
                self.signal.notify();
                self.player_view(None)
            }
            "state" => self.player_view(None),
            other => Err(EngineError::UnknownAction(other.to_owned())),
        }
    }

    fn check_timeout(&mut self, _now: Instant) -> Result<Option<TimeoutEvent>, EngineError> {
        if self.explode {
            return Err(EngineError::Internal("boom".to_owned()));
        }
        if self.finish_on_timeout && self.phase != "finished" {
            self.phase = "finished";
            self.signal.notify();
            return Ok(Some(TimeoutEvent::turn_timeout(0)));
        }
        Ok(None)
    }

    fn signal(&self) -> Arc<ChangeSignal> {
        Arc::clone(&self.signal)
    }

    fn prompts(&self) -> &PromptStore {
        shared_prompts()
    }
}

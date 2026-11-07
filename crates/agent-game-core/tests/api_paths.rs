//! 公开 API 看门：重排目录不许悄悄破坏导入路径。
//!
//! 目录重排是**纯内部**的改动，但它极容易顺手打断调用方：把 `engine.rs` 拆成
//! 两个文件、把 `manager.rs` 拆成四个，编译期只在**调用方**那里才报错，
//! 而 core 自己的测试可能全部绿——因为它们早就跟着改了。
//!
//! 这个文件只做一件事：把重排前 `lib.rs` 公开过的**每一个模块路径**都
//! import 一遍。少一个就编译失败。它不测行为，测的是"路径还在"。
//!
//! 清单来源：`git show python-v0.3.0..HEAD` 之前的 `crates/agent-game-core/src/lib.rs`，
//! 共 12 个 `pub mod`。

#![allow(unused_imports, clippy::unwrap_used, clippy::expect_used)]

// ── 12 个公开模块路径，逐个必须有 ──
use agent_game_server_core::budget::resolve_budget;
use agent_game_server_core::config::{Config, DEFAULT_BLOCK_BUDGET_SECS};
use agent_game_server_core::engine::{
    ActionParams, BoxedEngine, CreateParams, GameEngine, PlayerMode, PlayerView, ToolSpec,
};
use agent_game_server_core::error::{EngineError, SessionNotFound};
use agent_game_server_core::games::{GuessNumberEngine, LiarsDiceEngine};
use agent_game_server_core::identity::check_player_identity;
use agent_game_server_core::manager::{
    GameManager, Session, SessionId, SessionSnapshot, TimeoutRecord,
};
use agent_game_server_core::prompt::{Ctx, PromptStore};
use agent_game_server_core::registry::{GameMeta, Registry};
use agent_game_server_core::signal::{ChangeSignal, WaitOutcome};
use agent_game_server_core::testing::DummyEngine;
use agent_game_server_core::wait::wait_for;

/// 路径能被解析就说明兼容层在。这条用例的存在意义就是**通过编译**。
#[test]
fn every_public_module_path_still_resolves() {
    // 逐个把类型过一遍，防止 `use` 被优化成未使用而静默失效。
    let _: fn() -> PlayerMode = || PlayerMode::Single;
    let _: Option<BoxedEngine> = None;
    let _: Option<ActionParams> = None;
    let _: Option<CreateParams> = None;
    let _: Option<PlayerView> = None;
    let _: Option<ToolSpec> = None;
    let _: Option<EngineError> = None;
    let _: Option<SessionNotFound> = None;
    let _: Option<GameManager> = None;
    let _: Option<Session> = None;
    let _: Option<SessionId> = None;
    let _: Option<SessionSnapshot> = None;
    let _: Option<TimeoutRecord> = None;
    let _: Option<Ctx> = None;
    let _: Option<PromptStore> = None;
    let _: Option<GameMeta> = None;
    let _: Option<Registry> = None;
    let _: Option<ChangeSignal> = None;
    let _: Option<WaitOutcome> = None;
    let _: Option<DummyEngine> = None;
    let _: Option<Config> = None;
    let _: Option<f64> = Some(DEFAULT_BLOCK_BUDGET_SECS);

    // 函数与泛型入口
    let _ = resolve_budget;
    let _ = check_player_identity::<&str>;
    // `wait_for` 是泛型 async fn；在函数体内命名它即证路径可解析
    #[allow(dead_code)]
    async fn _proves_wait_for(signal: &ChangeSignal, budget: std::time::Duration) -> WaitOutcome {
        wait_for(signal, budget, || async { true }).await
    }
    let _: Option<GuessNumberEngine> = None;
    let _: Option<LiarsDiceEngine> = None;

    // trait 必须仍可作为边界使用
    fn _assert_trait_object_safe(_engine: &dyn GameEngine) {}
}

/// 顶层再导出也必须仍在——有些调用方直接 `use agent_game_server_core::GameManager`。
#[test]
fn top_level_reexports_still_resolve() {
    use agent_game_server_core::{
        ActionParams, BoxedEngine, ChangeSignal, Config, CreateParams, Ctx, EngineError,
        GameEngine, GameManager, GameMeta, PlayerMode, PlayerView, PromptStore, Registry, Session,
        SessionId, SessionNotFound, SessionSnapshot, TimeoutEvent, TimeoutRecord, ToolSpec,
        VERSION, WaitOutcome,
    };

    let _: Option<ActionParams> = None;
    let _: Option<BoxedEngine> = None;
    let _: Option<ChangeSignal> = None;
    let _: Option<Config> = None;
    let _: Option<CreateParams> = None;
    let _: Option<Ctx> = None;
    let _: Option<EngineError> = None;
    let _: Option<GameManager> = None;
    let _: Option<GameMeta> = None;
    let _: Option<PlayerMode> = None;
    let _: Option<PlayerView> = None;
    let _: Option<PromptStore> = None;
    let _: Option<Registry> = None;
    let _: Option<Session> = None;
    let _: Option<SessionId> = None;
    let _: Option<SessionNotFound> = None;
    let _: Option<SessionSnapshot> = None;
    let _: Option<TimeoutEvent> = None;
    let _: Option<TimeoutRecord> = None;
    let _: Option<ToolSpec> = None;
    let _: Option<WaitOutcome> = None;
    let _: Option<&str> = Some(VERSION);

    fn _assert_engine(_: &dyn GameEngine) {}
}

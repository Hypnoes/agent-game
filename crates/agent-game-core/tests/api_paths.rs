//! 公开 API 看门：重排目录不许悄悄破坏导入路径。
//!
//! 目录重排是**纯内部**的改动，但它极容易顺手打断调用方：把 `engine.rs` 拆成
//! 两个文件、把 `manager.rs` 拆成四个，编译期只在**调用方**那里才报错，
//! 而 core 自己的测试可能全部绿——因为它们早就跟着改了。
//!
//! 这个文件只做一件事：把重排前 `lib.rs` 公开过的**每一个符号**都 import 一遍。
//! 少一个就编译失败。它不测行为，测的是"路径还在"。
//!
//! 清单来源：重排前的 `crates/agent-game-core/src/lib.rs`——12 个 `pub mod`，
//! 以及各模块内**曾经 pub 过**的符号。后者容易被忽略：模块路径还在，但里面的
//! 某个 `pub const` / `pub fn` 被搬走或私有化了，调用方照样编译失败。
//!
//! 这份清单是被 review 逐轮补充的。缺哪个符号就补哪个，不要删。
//!
//! # 数量说明
//!
//! 重排前有 51 个公开符号，这里探 50 个。差的那一个是
//! `games::state_machine::Transitions`——它整个 workspace 零调用者，是死代码，
//! 已在 006（#8）里删除。**它不再是"刻意例外"，是已经不存在了。**
//! 同样，`GameEngine::setup_params()` 也在 006 里移除（生产路径零调用，
//! `Session.setup` 才是大厅匹配真正消费的那份配置）。

#![allow(unused_imports, clippy::unwrap_used, clippy::expect_used)]

// ── 12 个公开模块路径，逐个必须有 ──
use agent_game_server_core::budget::resolve_budget;
use agent_game_server_core::config::{
    Config, DEFAULT_BLOCK_BUDGET_SECS, DEFAULT_FINISHED_RETENTION_SECS, DEFAULT_POLL_INTERVAL_MS,
    DEFAULT_SESSION_CACHE_CAP, DEFAULT_TURN_TIMEOUT_SECS,
};
use agent_game_server_core::engine::{
    ActionParams, BoxedEngine, CreateParams, DEFAULT_FINISHED_STATES, DEFAULT_SENSITIVE_KEYS,
    GameEngine, PlayerMode, PlayerView, TimeoutEvent, ToolSpec,
};
use agent_game_server_core::error::{EngineError, SessionNotFound};
use agent_game_server_core::games::{GuessNumberEngine, LiarsDiceEngine};
use agent_game_server_core::identity::check_player_identity;
use agent_game_server_core::manager::{
    GameManager, Session, SessionId, SessionSnapshot, TimeoutRecord,
};
use agent_game_server_core::prompt::{
    BROKEN_PREFIX, Ctx, MISSING_PREFIX, PromptStore, Templates, Value, parse_toml, render_template,
};
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
    // 这两个常量曾公开在 `core::engine` 下。`pub use domain::game::*` 会把它们
    // 一起带出来（它们是 `pub const`，不是 `use` 进来的名字），所以兼容模块
    // 至今能导出它们——但没有探针就没人知道哪天会掉。
    let _: &[&str] = DEFAULT_FINISHED_STATES;
    let _: &[&str] = DEFAULT_SENSITIVE_KEYS;

    // 函数与泛型入口
    let _ = resolve_budget;
    let _ = check_player_identity::<&str>;
    let _ = parse_toml;
    let _ = render_template;
    // `wait_for` 是泛型 async fn；在函数体内命名它即证路径可解析
    #[allow(dead_code)]
    async fn _proves_wait_for(signal: &ChangeSignal, budget: std::time::Duration) -> WaitOutcome {
        wait_for(signal, budget, || async { true }).await
    }
    let _: Option<GuessNumberEngine> = None;
    let _: Option<LiarsDiceEngine> = None;

    // trait 必须仍可作为边界使用
    fn _assert_trait_object_safe(_engine: &dyn GameEngine) {}

    // `config` 模块的四个历史常量 + 一个换算函数
    let _: u64 = DEFAULT_TURN_TIMEOUT_SECS;
    let _: u64 = DEFAULT_FINISHED_RETENTION_SECS;
    let _: usize = DEFAULT_SESSION_CACHE_CAP;
    let _: u64 = DEFAULT_POLL_INTERVAL_MS;
    let _ = agent_game_server_core::config::secs_f64;

    // `prompt` 模块的两个前缀常量、值枚举、模板表
    let _: &str = MISSING_PREFIX;
    let _: &str = BROKEN_PREFIX;
    let _: fn(i64) -> Value = Value::Int;
    let _: Option<Templates> = None;
}

/// 模块内**嵌套路径**上的公开符号。
///
/// 上面那个用例只把符号从模块根 import 一遍。这一步补的是另一类回归：
/// 符号本身还在，但它所在的**子模块路径**变了（比如 `budget::BudgetError`
/// 被挪到 `budget::error::BudgetError`）——调用方照样编译失败。
#[test]
fn nested_public_symbols_still_resolve() {
    use agent_game_server_core::budget::BudgetError;
    use agent_game_server_core::games::guess_number::{
        Guess, GuessResult, default_finished_states,
    };
    use agent_game_server_core::games::liars_dice::{Call, Player};
    use agent_game_server_core::identity::PlayerIdentity;
    use agent_game_server_core::signal::Subscription;

    let _: Option<BudgetError> = None;
    let _: Option<Guess> = None;
    let _: Option<GuessResult> = None;
    let _: Option<Call> = None;
    let _: Option<Player> = None;
    let _: Option<Subscription> = None;
    let _: fn() -> &'static [&'static str] = default_finished_states;

    // trait 必须仍可作为约束使用
    fn _assert_identity<T: PlayerIdentity>(_value: T) {}
}

/// 从 `lib.rs` 顶层直接导出的常量。
///
/// `VERSION` 的路径是 `agent_game_server_core::VERSION`，不在任何模块里，
/// 容易被上面几条漏掉。
#[test]
fn version_is_still_exported() {
    let _: &str = agent_game_server_core::VERSION;
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

//! agent-game 的纯逻辑内核。
//!
//! 这个 crate 不依赖 axum、不碰网络：只有规则、预算、身份判断、等待原语、
//! 提示词渲染、游戏注册表与 session 路由。
//!
//! # 三层结构
//!
//! ```text
//! games/    GameImpl：具体规则（大话骰、猜数字）
//!    ↓     可以用下面两层
//! domain/   GameAbstraction 与 Lobby：引擎协议、匹配、对局生命周期
//!    ↓     可以用 infra
//! infra/    不依赖上面两层：预算、配置、错误、信号、等待、提示词、注册表、参数
//! ```
//!
//! 依赖方向是**单向**的：`infra` 不认识 `domain`，`domain` 不认识 `games`。
//! 这条约束由 `tests/layering.rs` 解析源码断言，不是靠注释自觉。

pub mod domain;
pub mod games;
pub mod infra;

// ── 兼容路径 ──
// 外部 crate 与集成测试按这些模块路径引用（`core::budget::resolve_budget`、
// `core::identity::check_player_identity`、`core::wait::wait_for`、
// `core::engine::BoxedEngine` 等）。重排目录时保留它们，
// 让"内部结构变了"不变成"调用方必须跟着改"。
//
// 重排前 `lib.rs` 有 12 个 `pub mod`，每一个都必须仍可导入。缺一个就是
// 一次公开 API 破坏，而编译期只在**调用方**那里才报错——所以
// `tests/api_paths.rs` 会逐个把它们 import 一遍。
pub use infra::{budget, config, error, identity, prompt, signal, wait};

/// 旧路径 `core::engine` 的别名。引擎协议现在住在 `domain::game`。
pub mod engine {
    pub use crate::domain::game::*;
    // `ActionParams` / `CreateParams` 曾被公开在 `core::engine` 下，但它们是
    // `infra::params` 的类型，不是 `domain::game` 的。通配再导出带不出它们
    // （`use` 进来的名字不是 `pub`），必须显式转发。
    pub use crate::infra::params::{ActionParams, CreateParams};
}

/// 旧路径 `core::testing` 的别名。测试替身现在住在 `domain::testing`。
pub mod testing {
    pub use crate::domain::testing::*;
}

/// 旧路径 `core::registry` 的别名。注册表现在住在 `domain::registry`。
pub mod registry {
    pub use crate::domain::registry::*;
}

/// 旧路径 `core::manager` 的别名。现在拆成 `domain::supervisor` 与
/// `domain::session` / `domain::snapshot`。
pub mod manager {
    pub use crate::domain::session::{Session, SessionId};
    pub use crate::domain::snapshot::{SessionSnapshot, TimeoutRecord};
    pub use crate::domain::supervisor::GameManager;
}

// ── 顶层再导出 ──
pub use domain::game::{BoxedEngine, GameEngine, PlayerMode, PlayerView, TimeoutEvent, ToolSpec};
pub use domain::registry::{GameMeta, Registry};
pub use domain::session::{Session, SessionId};
pub use domain::snapshot::{SessionSnapshot, TimeoutRecord};
pub use domain::supervisor::GameManager;
pub use infra::config::Config;
pub use infra::error::{EngineError, SessionNotFound};
pub use infra::params::{ActionParams, CreateParams};
pub use infra::prompt::{Ctx, PromptStore};
pub use infra::signal::{ChangeSignal, WaitOutcome};

/// 版本号的权威来源：`Cargo.toml` 一个地方写，运行期读它。
///
/// 旧版在 `config.py` 与 `pyproject.toml` 各写一份，还专门写了一条测试
/// 断言两者一致——Rust 用 `CARGO_PKG_VERSION` 让那条测试失去存在意义。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

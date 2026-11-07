//! agent-game 的纯逻辑内核。
//!
//! 这个 crate 不依赖 axum、不碰网络：只有规则、游戏注册表、大厅匹配、
//! 对局生命周期与引擎协议。
//!
//! # 分层
//!
//! ```text
//! agent-game-server-core（本 crate）
//!   games/    GameImpl：具体规则（大话骰、猜数字）
//!      ↓     可以用 domain
//!   domain/   GameAbstraction 与 Lobby：引擎协议、匹配、对局生命周期
//!      ↓     可以用 infra
//! agent-game-server-infra（独立 crate）
//!   budget / config / error / identity / params / prompt / signal / wait
//! ```
//!
//! # 为什么 infra 是独立 crate
//!
//! 依赖方向**由编译器强制**，不是靠源码扫描。`infra` 不依赖本 crate，
//! 任何反向依赖都会让 cargo 在循环依赖检查处直接失败（`cyclic package
//! dependency`），不存在"某种写法漏检"的余地。
//!
//! 这条约束最初由 `tests/layering.rs` 读源码文本来守，但 review 连续三轮各找到
//! 新的绕过写法（相对路径 `super::super::`、token 间空格 `crate :: domain`、
//! 非 `use` 的表达式路径），每次都合法可编译却漏检。字符串匹配无法穷尽一种
//! 语言的模块引用语法，所以改用 crate 边界。
//!
//! `domain` 与 `games` 留在同一 crate 内（`domain → games` 也禁止），那条方向
//! 仍由 `tests/layering.rs` 守——为它单独抽一个 crate 收益不抵成本。

pub mod domain;
pub mod games;

// ── 兼容路径 ──
// 重排前 `lib.rs` 有 12 个 `pub mod`，每一个都必须仍可导入。缺一个就是一次
// 公开 API 破坏，而编译期只在**调用方**那里才报错——所以 `tests/api_paths.rs`
// 会逐个把它们 import 一遍。
pub use agent_game_server_infra::{budget, config, error, identity, prompt, signal, wait};

/// 旧路径 `core::engine` 的别名。引擎协议现在住在 `domain::game`。
pub mod engine {
    pub use crate::domain::game::*;
    // `ActionParams` / `CreateParams` 曾被公开在 `core::engine` 下，但它们是
    // `infra` crate 的类型，不是 `domain::game` 的。通配再导出带不出它们
    // （`use` 进来的名字不是 `pub`），必须显式转发。
    pub use agent_game_server_infra::params::{ActionParams, CreateParams};
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
pub use agent_game_server_infra::config::Config;
pub use agent_game_server_infra::error::{EngineError, SessionNotFound};
pub use agent_game_server_infra::params::{ActionParams, CreateParams};
pub use agent_game_server_infra::prompt::{Ctx, PromptStore};
pub use agent_game_server_infra::signal::{ChangeSignal, WaitOutcome};
pub use domain::game::{BoxedEngine, GameEngine, PlayerMode, PlayerView, TimeoutEvent, ToolSpec};
pub use domain::registry::{GameMeta, Registry};
pub use domain::session::{Session, SessionId};
pub use domain::snapshot::{SessionSnapshot, TimeoutRecord};
pub use domain::supervisor::GameManager;

/// 版本号的权威来源：`Cargo.toml` 一个地方写，运行期读它。
///
/// 旧版在 `config.py` 与 `pyproject.toml` 各写一份，还专门写了一条测试
/// 断言两者一致——Rust 用 `CARGO_PKG_VERSION` 让那条测试失去存在意义。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

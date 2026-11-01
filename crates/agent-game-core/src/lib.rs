//! agent-game 的纯逻辑内核。
//!
//! 这个 crate 不依赖 axum、不碰网络：只有规则、预算、身份判断、等待原语、
//! 提示词渲染、游戏注册表与 session 路由。传输层（HTTP 与 MCP）各自一个 crate，
//! 共用这里的同一份判断。

pub mod budget;
pub mod config;
pub mod engine;
pub mod error;
pub mod games;
pub mod identity;
pub mod manager;
pub mod prompt;
pub mod registry;
pub mod signal;
pub mod testing;
pub mod wait;

pub use config::Config;
pub use engine::{
    ActionParams, BoxedEngine, CreateParams, GameEngine, PlayerMode, PlayerView, TimeoutEvent,
    ToolSpec,
};
pub use error::{EngineError, SessionNotFound};
pub use manager::{GameManager, Session, SessionId, SessionSnapshot, TimeoutRecord};
pub use prompt::{Ctx, PromptStore};
pub use registry::{GameMeta, Registry};
pub use signal::{ChangeSignal, WaitOutcome};

/// 版本号的权威来源：`Cargo.toml` 一个地方写，运行期读它。
///
/// Python 版在 `config.py` 与 `pyproject.toml` 各写一份，还专门写了一条测试
/// 断言两者一致——Rust 用 `CARGO_PKG_VERSION` 让那条测试失去存在意义。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

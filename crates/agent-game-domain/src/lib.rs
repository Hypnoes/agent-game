//! agent-game 的领域层：游戏抽象、大厅匹配、会话生命周期、注册表与监督者。
//!
//! # 分层
//!
//! ```text
//! agent-game-server-core
//!   games/    GameImpl：具体规则（大话骰、猜数字）
//!      ↓
//! agent-game-server-domain（本 crate）
//!   game / lobby / session / snapshot / supervisor / registry / testing
//!      ↓
//! agent-game-server-infra
//!   budget / config / error / identity / params / prompt / signal / wait
//! ```
//!
//! # 为什么这一层也是独立 crate
//!
//! `domain` 不许依赖 `games`（具体游戏）。这条一度由 `core/tests/layering.rs`
//! 读源码文本来守，但 review 连续找到绕过写法，最后一个尤其说明问题：
//!
//! ```ignore
//! use crate as root;
//! use root::games::Engine;      // 扫描器只看 `crate::games`，这个绕过去了
//! ```
//!
//! 字符串匹配无法穷尽 Rust 的模块引用语法。抽成独立 crate 后，
//! 任何反向依赖都在 cargo 的循环依赖检查处失败，与写法无关。
//!
//! 这个 crate **不依赖** `agent-game-server-core`，`core` 依赖它。

pub mod game;
pub mod lobby;
pub mod registry;
pub mod session;
pub mod snapshot;
pub mod supervisor;
pub mod testing;

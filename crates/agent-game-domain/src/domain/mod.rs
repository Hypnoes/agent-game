//! 领域层：游戏抽象、大厅匹配、对局生命周期与监督。
//!
//! 依赖方向：`domain` 可以使用 `infra`，但 `infra` 不认识 `domain`。

pub mod game;
pub mod lobby;
pub mod registry;
pub mod session;
pub mod snapshot;
pub mod supervisor;
pub mod testing;

//! 基础设施层：被上面两层使用，自己不使用上面两层。
//!
//! 这一层的依赖方向由 `tests/layering.rs` 守着：这里出现
//! `use crate::domain` 或 `use crate::games` 就算违规。

pub mod budget;
pub mod config;
pub mod error;
pub mod identity;
pub mod params;
pub mod prompt;
pub mod signal;
pub mod wait;

//! agent-game 的基础设施层：被上面两层使用，自己不使用上面两层。
//!
//! # 分层为什么由 crate 边界强制，而不是靠源码扫描
//!
//! 之前这一层是 `agent-game-core` 里的一个目录，依赖方向靠 `tests/layering.rs`
//! 读源码文本、匹配 `use crate::domain` 之类的前缀来守。那条路走不通：
//! review 连续三轮各找到新的绕过写法，每次都合法可编译却漏检——
//!
//! ```text
//! use super::super::domain::game::GameEngine;   // 相对路径
//! use crate :: domain :: game::GameEngine;      // token 间空格
//! fn f(_: crate::domain::game::PlayerMode) {}   // 非 use 的路径
//! ```
//!
//! 字符串扫描无法穷尽一种语言的模块引用语法。所以这一层被抽成**独立 crate**：
//! 它不依赖 `agent-game-server-core`，任何反向依赖都会让 cargo 在循环依赖检查
//! 处直接失败（`cyclic package dependency`）。这样约束由编译器执行，
//! 不存在"漏检某种写法"的余地。
//!
//! `tests/layering.rs` 仍然保留，但它现在守的是**同一 crate 内部**的方向
//! （`domain → games`），那种情况抽 crate 收益不抵成本。

pub mod budget;
pub mod config;
pub mod error;
pub mod identity;
pub mod params;
pub mod prompt;
pub mod signal;
pub mod wait;

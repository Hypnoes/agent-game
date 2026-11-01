//! agent-game 服务端：MCP 端点 + REST + Admin 跑在同一个 axum app 上。
//!
//! 三个入口共用同一份 [`AppState`]（注册表、manager、配置、admin token），
//! 因此"谁是谁"的判断不会在三条路径上分叉。
//!
//! ```text
//! GET  /health
//! GET  /api/v1/games/tools
//! POST /api/v1/games/{game_type}
//! POST /api/v1/games/{session_id}/actions
//! GET  /api/v1/games/{session_id}
//! GET  /api/v1/games/{session_id}/players/{player_index}
//! GET  /api/v1/admin/games
//! GET  /api/v1/admin/games/{session_id}
//! POST /api/v1/admin/prompts/reload
//! GET  /admin?token=
//! GET  /api/v1/admin/ws?token=
//! ANY  /mcp
//! ```

pub mod admin;
pub mod app;
pub mod broadcast;
pub mod error;
pub mod games;
pub mod mcp;
pub mod poller;
pub mod state;

pub use app::create_app;
pub use error::ApiError;
pub use state::{AppState, build_registry};

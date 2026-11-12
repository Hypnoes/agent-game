//! 应用状态：一次构造、按实例共享。
//!
//! Python 版的 `manager` 是模块全局，`create_app()` 只是把同一份全局包进
//! FastAPI；于是测试只能用三种不同的 monkeypatch 手法隔离（其中一套只清了
//! 三个字典，`_finished_at` 与 `_seen_expired` 从来没被还原）。
//!
//! 这里 `AppState` 是一个值：造两个就是两套独立世界，不需要任何全局状态。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use agent_game_server_core::games::{GuessNumberEngine, LiarsDiceEngine};
use agent_game_server_core::{Config, GameManager, GameMeta, PlayerMode, Registry, VERSION};
use axum::extract::FromRef;
use serde_json::Value;

use crate::broadcast::AdminBroadcast;

/// 两个内置引擎。
pub fn build_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(
        GameMeta::new("guess_number", PlayerMode::Single, |params, config| {
            GuessNumberEngine::new(params, config)
                .map(|engine| Box::new(engine) as agent_game_server_core::BoxedEngine)
        })
        .with_display_name("猜数字")
        .with_tools(agent_game_server_core::games::guess_number::tools()),
    );
    registry.register(
        GameMeta::new("liars_dice", PlayerMode::Multi, |params, config| {
            LiarsDiceEngine::new(params, config)
                .map(|engine| Box::new(engine) as agent_game_server_core::BoxedEngine)
        })
        .with_display_name("大话骰")
        .with_tools(agent_game_server_core::games::liars_dice::tools()),
    );
    registry
}

/// 服务端共享状态。
#[derive(Clone, Debug)]
pub struct AppState {
    pub manager: Arc<GameManager>,
    pub config: Config,
    pub admin_token: Arc<String>,
    pub broadcast: Arc<AdminBroadcast>,
    pub prompt_dir: Option<Arc<PathBuf>>,
    pub started_at: Instant,
}

impl AppState {
    /// 用给定的注册表与配置构造。
    #[must_use]
    pub fn new(registry: Registry, config: Config, admin_token: String) -> Self {
        let manager = Arc::new(GameManager::new(registry, config.clone()));
        Self {
            manager,
            config,
            admin_token: Arc::new(admin_token),
            broadcast: Arc::new(AdminBroadcast::new()),
            prompt_dir: None,
            started_at: Instant::now(),
        }
    }

    /// 用两个内置引擎与默认配置构造。
    #[must_use]
    pub fn with_defaults(admin_token: String) -> Self {
        Self::new(build_registry(), Config::default(), admin_token)
    }

    /// 版本号。
    #[must_use]
    pub fn version(&self) -> &'static str {
        VERSION
    }

    /// 恒定时间比较 admin token。
    ///
    /// 按字节比较并先做长度检查：`subtle` 的 `ct_eq` 对不等长的切片返回
    /// "不相等"而不 panic。Python 版为此专门处理过非 ASCII 的 `TypeError`
    /// ——这里根本不存在编码问题，token 就是字节。
    #[must_use]
    pub fn token_matches(&self, presented: &str) -> bool {
        use subtle::ConstantTimeEq;
        let expected = self.admin_token.as_bytes();
        let actual = presented.as_bytes();
        if expected.len() != actual.len() {
            return false;
        }
        expected.ct_eq(actual).into()
    }

    /// 当前 session 数量。
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.manager.len()
    }
}

/// 指定提示词目录，并**立即应用到所有引擎的模板仓库**。
///
/// 只把路径存进 `AppState` 是空操作——引擎读的是自己那份进程级共享的
/// `PromptStore`，它不会去看 `AppState`。这里必须真的把它们连起来，
/// 否则 `--prompt-dir` 这个启动参数在行为上等于不存在。
pub fn set_prompt_dir(state: &mut AppState, dir: PathBuf) {
    state.prompt_dir = Some(Arc::new(dir));
    apply_prompt_dir(state);
}

/// 把 `AppState` 上的提示词目录同步到每个引擎的 `PromptStore`。
///
/// 引擎的仓库是 `OnceLock` 共享的（每个引擎类型一份），所以这项设置对
/// 已经在跑的对局同样生效。
fn apply_prompt_dir(state: &AppState) {
    let dir: Option<PathBuf> = state.prompt_dir.as_deref().cloned();
    for (_game_type, meta) in state.manager.registry().iter() {
        if let Ok(engine) = (meta.factory)(
            &agent_game_server_core::CreateParams::default(),
            &state.config,
        ) {
            engine.prompts().set_dir(dir.clone());
        }
    }
}

/// 重载所有引擎的提示词模板，返回被重载的游戏类型。
///
/// 引擎的 `PromptStore` 是进程级共享（每个引擎类型一份），所以这里只需按注册表
/// 逐个提醒一次。带 `display_name` 的引擎都能被重载——与 Python 版"只重载自己
/// 声明了 `get_prompts` 的引擎"同义，区别是 Rust 里所有引擎都必须提供
/// `prompts()`，不存在"探测不到"的分支。
pub fn reload_all_prompts(state: &AppState) -> Vec<&'static str> {
    let registry = state.manager.registry();
    let mut reloaded = Vec::new();
    for (game_type, meta) in registry.iter() {
        // 造一个临时实例读它的提示词仓库；模板本身是 `OnceLock` 共享的，
        // 重载对已经在跑的对局同样生效。
        if let Ok(engine) = (meta.factory)(
            &agent_game_server_core::CreateParams::default(),
            &state.config,
        ) {
            engine.prompts().reload();
            reloaded.push(game_type);
        }
    }
    reloaded
}

/// 让各个 handler 能从 `State<AppState>` 里只取自己要的那一份。
impl FromRef<AppState> for Arc<GameManager> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.manager)
    }
}

impl FromRef<AppState> for Config {
    fn from_ref(state: &AppState) -> Self {
        state.config.clone()
    }
}

impl FromRef<AppState> for Arc<AdminBroadcast> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.broadcast)
    }
}

/// 供 admin 列表与工具定义共用的展示名清单。
#[must_use]
pub fn display_names(manager: &GameManager) -> Value {
    serde_json::json!(
        manager
            .registry()
            .display_names()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_has_both_games_with_display_names() {
        let registry = build_registry();
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.get("guess_number").unwrap().display_name, "猜数字");
        assert_eq!(registry.get("liars_dice").unwrap().display_name, "大话骰");
    }

    #[test]
    fn registered_player_modes_match_the_engines() {
        let registry = build_registry();
        assert_eq!(
            registry.get("guess_number").unwrap().player_mode,
            PlayerMode::Single
        );
        assert_eq!(
            registry.get("liars_dice").unwrap().player_mode,
            PlayerMode::Multi
        );
    }

    #[test]
    fn token_comparison_is_exact() {
        let state = AppState::with_defaults("secret-token".to_owned());
        assert!(state.token_matches("secret-token"));
        assert!(!state.token_matches("secret-toke"));
        assert!(!state.token_matches("secret-token "));
        assert!(!state.token_matches(""));
        // 非 ASCII 不 panic（Python 版的 str 版 compare_digest 会抛 TypeError）
        assert!(!state.token_matches("令牌"));
    }

    #[test]
    fn version_comes_from_cargo() {
        let state = AppState::with_defaults("x".to_owned());
        assert_eq!(state.version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn two_states_do_not_share_sessions() {
        let first = AppState::with_defaults("a".to_owned());
        let second = AppState::with_defaults("a".to_owned());
        assert_eq!(first.session_count(), 0);
        assert_eq!(second.session_count(), 0);
        assert!(!std::sync::Arc::ptr_eq(&first.manager, &second.manager));
    }

    #[test]
    fn reload_reports_both_engines() {
        let state = AppState::with_defaults("x".to_owned());
        let reloaded = reload_all_prompts(&state);
        assert_eq!(reloaded, vec!["guess_number", "liars_dice"]);
    }

    #[test]
    fn display_names_come_from_the_registry() {
        let state = AppState::with_defaults("x".to_owned());
        let names = display_names(&state.manager);
        assert_eq!(names["guess_number"], "猜数字");
        assert_eq!(names["liars_dice"], "大话骰");
    }
}

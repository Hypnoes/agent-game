//! 游戏注册表：一份机器键 + 一份展示名。
//!
//! Python 版有两个字符串：`game_type`（注册表键、URL 路径、工具名前缀、匹配过滤、
//! 缓存键）与 `game_name`（人类可读）。后者被写进 JSON 却**没有任何消费者**——
//! admin 页面自己按 `game_type` 硬编码了一张 `TYPE_LABEL`。同一份展示名知识
//! 存了两份，且真正生效的那份在框架里，新游戏必须回来改页面。
//!
//! 这里合成一处：`game_type` 是机器键，`display_name` 是展示名，二者都从
//! [`GameMeta`] 出，`display_name` 随 `/api/v1/games/tools` 与 admin 列表下发。
//!
//! `player_mode` 也住在 `GameMeta` 里，而不是每次创建时造一个探针实例去问引擎：
//! 后者会让"多人游戏该不该匹配到这张桌子"依赖构造函数能否被空参数调用。
//! 漂移风险由 `tests/registry_parity.rs` 兜住——它对每个注册项造一个默认实例，
//! 断言引擎自报的 `player_mode` 与注册表声明的一致。

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::game::{BoxedEngine, PlayerMode};
use agent_game_server_infra::config::Config;
use agent_game_server_infra::params::CreateParams;

/// 一个已经注册的游戏。
#[derive(Clone)]
pub struct GameMeta {
    /// 机器键：URL 路径、MCP 工具名前缀、匹配过滤键、连接缓存键。
    pub game_type: &'static str,
    /// 展示名：只用于人看的界面，缺省等于 `game_type`。
    pub display_name: &'static str,
    /// 单人还是多人。决定"新请求能不能并进等待中的桌子"。
    pub player_mode: PlayerMode,
    /// 构造函数。参数来自创建请求，配置来自服务端。
    pub factory: Arc<dyn Fn(&CreateParams, &Config) -> Result<BoxedEngine, String> + Send + Sync>,
}

impl std::fmt::Debug for GameMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameMeta")
            .field("game_type", &self.game_type)
            .field("display_name", &self.display_name)
            .field("player_mode", &self.player_mode)
            .finish_non_exhaustive()
    }
}

impl GameMeta {
    /// 用一个构造函数注册游戏。展示名缺省时用 `game_type`。
    pub fn new<F>(game_type: &'static str, player_mode: PlayerMode, factory: F) -> Self
    where
        F: Fn(&CreateParams, &Config) -> Result<BoxedEngine, String> + Send + Sync + 'static,
    {
        Self {
            game_type,
            display_name: game_type,
            player_mode,
            factory: Arc::new(factory),
        }
    }

    /// 显式指定展示名。
    #[must_use]
    pub fn with_display_name(mut self, display_name: &'static str) -> Self {
        self.display_name = display_name;
        self
    }
}

/// 游戏注册表。
#[derive(Debug, Default, Clone)]
pub struct Registry {
    games: BTreeMap<&'static str, GameMeta>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个游戏。同名重复注册会覆盖——与"同名即同一款游戏"的语义一致。
    pub fn register(&mut self, meta: GameMeta) {
        self.games.insert(meta.game_type, meta);
    }

    #[must_use]
    pub fn get(&self, game_type: &str) -> Option<&GameMeta> {
        self.games.get(game_type)
    }

    #[must_use]
    pub fn contains(&self, game_type: &str) -> bool {
        self.games.contains_key(game_type)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &GameMeta)> {
        self.games.iter().map(|(key, meta)| (*key, meta))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.games.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.games.is_empty()
    }

    /// 所有机器键（`/health` 用）。
    #[must_use]
    pub fn game_types(&self) -> Vec<&'static str> {
        self.games.keys().copied().collect()
    }

    /// 按已注册的 `game_type` 前缀最长优先剥离，得到 `(game_type, action)`。
    ///
    /// 最短优先会在同时注册 `guess` 与 `guess_number` 时把 `guess_number_state`
    /// 判给 `guess`。未注册的前缀一律返回 `None`，不拿拼错的工具名去建对局。
    #[must_use]
    pub fn split_tool_name<'a>(&self, tool_name: &'a str) -> Option<(&'static str, &'a str)> {
        let mut best: Option<(&'static str, &str)> = None;
        for game_type in self.games.keys() {
            let prefix_len = game_type.len() + 1;
            if tool_name.len() <= prefix_len || !tool_name.starts_with(game_type) {
                continue;
            }
            if tool_name.as_bytes().get(game_type.len()) != Some(&b'_') {
                continue;
            }
            let action = &tool_name[prefix_len..];
            let better = match best {
                None => true,
                Some((current, _)) => game_type.len() > current.len(),
            };
            if better {
                best = Some((game_type, action));
            }
        }
        best
    }

    /// 造一个引擎实例。
    ///
    /// # Errors
    /// 未知游戏类型，或构造函数拒绝了参数。
    pub fn create(
        &self,
        game_type: &str,
        params: &CreateParams,
        config: &Config,
    ) -> Result<BoxedEngine, String> {
        let Some(meta) = self.games.get(game_type) else {
            let known = self.game_types();
            return Err(format!("未知游戏类型: {game_type}。已注册: {known:?}"));
        };
        (meta.factory)(params, config)
    }

    /// 每个游戏的展示名清单（admin 列表与工具定义用）。
    #[must_use]
    pub fn display_names(&self) -> BTreeMap<&'static str, &'static str> {
        self.games
            .iter()
            .map(|(key, meta)| (*key, meta.display_name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyEngine;

    fn dummy_meta(game_type: &'static str) -> GameMeta {
        GameMeta::new(game_type, PlayerMode::Single, move |_params, _config| {
            Ok(Box::new(DummyEngine::new(game_type)) as BoxedEngine)
        })
    }

    #[test]
    fn register_and_lookup() {
        let mut registry = Registry::new();
        registry.register(dummy_meta("guess_number"));
        assert!(registry.contains("guess_number"));
        assert_eq!(registry.len(), 1);
        assert!(!registry.contains("liars_dice"));
        assert_eq!(registry.game_types(), vec!["guess_number"]);
    }

    #[test]
    fn display_name_defaults_to_the_machine_key() {
        let mut registry = Registry::new();
        registry.register(dummy_meta("guess_number"));
        assert_eq!(
            registry.get("guess_number").unwrap().display_name,
            "guess_number"
        );
    }

    #[test]
    fn display_name_can_be_declared_once() {
        let mut registry = Registry::new();
        registry.register(dummy_meta("guess_number").with_display_name("猜数字"));
        assert_eq!(registry.get("guess_number").unwrap().display_name, "猜数字");
        assert_eq!(
            registry.display_names().get("guess_number"),
            Some(&"猜数字")
        );
    }

    #[test]
    fn unknown_game_type_lists_what_is_registered() {
        let registry = Registry::new();
        let err = registry
            .create("nope", &CreateParams::default(), &Config::default())
            .unwrap_err();
        assert!(err.contains("未知游戏类型"), "{err}");
    }

    #[test]
    fn tool_name_splitting_prefers_the_longest_prefix() {
        let mut registry = Registry::new();
        registry.register(dummy_meta("guess"));
        registry.register(dummy_meta("guess_number"));

        assert_eq!(
            registry.split_tool_name("guess_number_state"),
            Some(("guess_number", "state"))
        );
        assert_eq!(
            registry.split_tool_name("guess_state"),
            Some(("guess", "state"))
        );
    }

    #[test]
    fn tool_name_without_a_prefix_is_rejected() {
        let mut registry = Registry::new();
        registry.register(dummy_meta("liars_dice"));
        assert_eq!(registry.split_tool_name("other_dice_state"), None);
        // 只有前缀、没有 action 不算命中
        assert_eq!(registry.split_tool_name("liars_dice_"), None);
        // 前缀后面不是下划线不算命中
        assert_eq!(registry.split_tool_name("liars_dicexstate"), None);
    }

    #[test]
    fn multiword_game_types_work() {
        let mut registry = Registry::new();
        registry.register(dummy_meta("mega_liars_dice"));
        assert_eq!(
            registry.split_tool_name("mega_liars_dice_bid"),
            Some(("mega_liars_dice", "bid"))
        );
    }

    #[test]
    fn non_ascii_names_do_not_panic_on_a_byte_boundary() {
        let mut registry = Registry::new();
        registry.register(dummy_meta("猜数字"));
        assert_eq!(
            registry.split_tool_name("猜数字_开始"),
            Some(("猜数字", "开始"))
        );
        assert_eq!(registry.split_tool_name("x"), None);
    }

    #[test]
    fn the_default_registry_is_empty() {
        let registry = Registry::new();
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.display_names().len(), 0);
    }

    #[test]
    fn registered_game_can_be_instantiated() {
        let mut registry = Registry::new();
        registry.register(dummy_meta("guess_number").with_display_name("猜数字"));
        let engine = registry
            .create("guess_number", &CreateParams::default(), &Config::default())
            .unwrap();
        assert_eq!(engine.game_type(), "guess_number");
        assert_eq!(engine.player_mode(), PlayerMode::Single);
    }
}

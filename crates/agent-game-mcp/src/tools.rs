//! 把引擎声明的工具翻译成 MCP 的 `Tool`。
//!
//! **工具表不写在这里。** 每款游戏通过 `GameEngine::describe_tools()` 自己声明，
//! 这个模块只做两件事：
//!
//! 1. 遍历注册表，把每个引擎的声明翻译成 `rmcp::model::Tool`。
//! 2. 判断"某个工具名是否真的被声明过"——前缀能剥出 action，不等于这个 action
//!    存在，`liars_dice_nope` 也能剥成 `("liars_dice", "nope")`。
//!
//! 曾经这里硬编码了两款游戏的完整 schema，于是"零改动加游戏"名不副实：
//! 新游戏的工具永远返回"未知工具"。改成从注册表派生之后，新增游戏只需实现
//! `describe_tools()` 并注册 `GameMeta`。

use agent_game_server_core::{PlayerMode, Registry, ToolSpec};
use rmcp::model::Tool;

/// 注册表里所有游戏声明的工具。
#[must_use]
pub fn all_tools(registry: &Registry) -> Vec<Tool> {
    let mut tools = Vec::new();
    for (game_type, _meta) in registry.iter() {
        // 造一个实例读它的声明。工具的 schema 与实例状态无关，所以用默认参数
        // 构造就够了；构造失败的游戏（参数校验过严）会返回空表——那是一个
        // 真实的信号：它的 `describe_tools` 永远拿不到，值得在测试里断言。
        if let Some(specs) = specs_of(registry, game_type) {
            tools.extend(specs.into_iter().map(to_tool));
        }
    }
    tools
}

/// 某个游戏的工具声明。未注册或构造失败时返回 `None`。
///
/// 公开是为了让 `/api/v1/games/tools` 不必经过 MCP 的 `Tool` 类型。
#[must_use]
pub fn specs_of(registry: &Registry, game_type: &str) -> Option<Vec<ToolSpec>> {
    let meta = registry.get(game_type)?;
    let engine = (meta.factory)(
        &agent_game_server_core::CreateParams::default(),
        &agent_game_server_core::Config::default(),
    )
    .ok()?;
    Some(engine.describe_tools())
}

/// 某个游戏的工具定义。未注册或该引擎不声明工具时返回空表。
#[must_use]
pub fn all_tools_for(registry: &Registry, game_type: &str) -> Vec<Tool> {
    specs_of(registry, game_type)
        .unwrap_or_default()
        .into_iter()
        .map(to_tool)
        .collect()
}

/// 某个游戏的工具数量（测试用）。
#[must_use]
pub fn tool_count(registry: &Registry, game_type: &str) -> usize {
    specs_of(registry, game_type).map_or(0, |specs| specs.len())
}

/// `tool_name` 是否真的由该引擎**声明**过。
///
/// 只按前缀放行会让拼错的工具名走到引擎里再以"未知 action"失败，错误信息
/// 指向错的地方；更要紧的是它可能先建出一局空对局。
#[must_use]
pub fn declares(registry: &Registry, game_type: &str, tool_name: &str) -> bool {
    specs_of(registry, game_type)
        .is_some_and(|specs| specs.iter().any(|spec| spec.name == tool_name))
}

/// 把一份声明翻译成协议类型。
fn to_tool(spec: ToolSpec) -> Tool {
    let schema = match spec.input_schema {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    // 名字与描述在协议里是 `Cow<'static, str>`：`ToolSpec` 持有 `String`，
    // 泄漏成 `'static` 太重，所以走 `new_with_raw` 之外的那条路——
    // `Tool::new` 接受 `Into<Cow<'static, str>>`，传 `String` 会 clone 一次。
    Tool::new(spec.name, spec.description, schema)
}

/// 注册表里每个游戏的 `(game_type, display_name, player_mode, 工具数)`。
///
/// `/api/v1/games/tools` 与测试都用它，避免两处各自遍历一次注册表。
#[must_use]
pub fn describe_all(registry: &Registry) -> Vec<(&'static str, &'static str, PlayerMode, usize)> {
    registry
        .iter()
        .map(|(game_type, meta)| {
            (
                game_type,
                meta.display_name,
                meta.player_mode,
                tool_count(registry, game_type),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_game_server_core::{BoxedEngine, GameMeta};

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.register(GameMeta::new(
            "guess_number",
            PlayerMode::Single,
            |params, config| {
                agent_game_server_core::games::GuessNumberEngine::new(params, config)
                    .map(|engine| Box::new(engine) as BoxedEngine)
            },
        ));
        registry.register(GameMeta::new(
            "liars_dice",
            PlayerMode::Multi,
            |params, config| {
                agent_game_server_core::games::LiarsDiceEngine::new(params, config)
                    .map(|engine| Box::new(engine) as BoxedEngine)
            },
        ));
        registry
    }

    #[test]
    fn every_registered_game_contributes_tools() {
        let registry = registry();
        assert_eq!(all_tools(&registry).len(), 7);
        assert_eq!(tool_count(&registry, "guess_number"), 3);
        assert_eq!(tool_count(&registry, "liars_dice"), 4);
    }

    #[test]
    fn tool_names_are_prefixed_with_the_game_type() {
        let registry = registry();
        for tool in all_tools(&registry) {
            let name = tool.name.as_ref();
            assert!(
                name.starts_with("guess_number_") || name.starts_with("liars_dice_"),
                "{name}"
            );
        }
    }

    #[test]
    fn the_join_tool_declares_the_table_sizing_parameters() {
        // `player_count` 与 `dice_per_player` 决定了"谁能坐同一桌"。
        // 不在 schema 里声明，Agent 就不知道可以传——那正是"三人局各开一桌"
        // 那个缺陷的表现之一。
        let registry = registry();
        let join = all_tools_for(&registry, "liars_dice")
            .into_iter()
            .find(|tool| tool.name == "liars_dice_join")
            .expect("必须有 join 工具");

        let properties = join
            .input_schema
            .get("properties")
            .expect("schema 有 properties");
        assert!(properties.get("player_count").is_some(), "{properties}");
        assert!(properties.get("dice_per_player").is_some());
    }

    #[test]
    fn required_lists_match_the_documented_fields() {
        let registry = registry();
        let tools = all_tools_for(&registry, "liars_dice");
        let bid = tools
            .iter()
            .find(|tool| tool.name == "liars_dice_bid")
            .unwrap();
        assert_eq!(
            bid.input_schema.get("required").unwrap(),
            &serde_json::json!(["player_index", "player_token", "quantity", "face"])
        );

        let join = tools
            .iter()
            .find(|tool| tool.name == "liars_dice_join")
            .unwrap();
        assert_eq!(
            join.input_schema.get("required").unwrap(),
            &serde_json::json!(["player_name"])
        );
    }

    #[test]
    fn the_timeout_parameter_documents_the_cap() {
        let registry = registry();
        let tools = all_tools_for(&registry, "liars_dice");
        let state = tools
            .iter()
            .find(|tool| tool.name == "liars_dice_state")
            .unwrap();
        let timeout = &state.input_schema.get("properties").unwrap()["timeout"];
        assert_eq!(timeout["maximum"], 55);
        assert_eq!(timeout["default"], 55);
    }

    #[test]
    fn single_player_tools_need_no_token() {
        let registry = registry();
        for tool in all_tools_for(&registry, "guess_number") {
            let required = tool.input_schema.get("required").unwrap();
            if let Some(items) = required.as_array() {
                assert!(
                    !items.iter().any(|item| item == "player_token"),
                    "{}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn unregistered_games_contribute_no_tools() {
        let registry = Registry::new();
        assert_eq!(all_tools(&registry), Vec::new(), "空注册表不该产出工具");
        assert_eq!(tool_count(&registry, "nope"), 0);
        assert_eq!(
            all_tools_for(&registry, "nope"),
            Vec::new(),
            "未注册的游戏不该有工具"
        );
    }

    #[test]
    fn declares_rejects_a_name_that_only_shares_the_prefix() {
        let registry = registry();
        assert!(declares(&registry, "liars_dice", "liars_dice_bid"));
        assert!(!declares(&registry, "liars_dice", "liars_dice_nope"));
        assert!(!declares(&registry, "liars_dice", "guess_number_start"));
    }

    #[test]
    fn an_engine_without_a_tool_surface_contributes_nothing() {
        // `describe_tools` 有默认空实现：纯内部用途的引擎不该让工具表出错。
        let mut registry = Registry::new();
        registry.register(GameMeta::new("silent", PlayerMode::Single, |_p, _c| {
            Ok(
                Box::new(agent_game_server_core::testing::DummyEngine::new("silent"))
                    as BoxedEngine,
            )
        }));
        assert_eq!(all_tools(&registry), Vec::new(), "空注册表不该产出工具");
        assert_eq!(tool_count(&registry, "silent"), 0);
    }

    #[test]
    fn describe_all_reports_the_mode_and_the_tool_count() {
        let registry = registry();
        let rows = describe_all(&registry);
        assert_eq!(rows.len(), 2);
        let dice = rows
            .iter()
            .find(|(name, ..)| *name == "liars_dice")
            .unwrap();
        assert_eq!(dice.2, PlayerMode::Multi);
        assert_eq!(dice.3, 4);
    }
}

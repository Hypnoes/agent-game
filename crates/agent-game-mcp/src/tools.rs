//! 把注册元数据里的工具声明翻译成 MCP 的 `Tool`。
//!
//! **工具表不写在这里，也不在引擎里。** 每款游戏通过
//! `GameMeta::with_tools()` 在**注册时**声明一次，这个模块只做两件事：
//!
//! 1. 遍历注册表，把每份声明翻译成 `rmcp::model::Tool`。
//! 2. 判断"某个工具名是否真的被声明过"——前缀能剥出 action，不等于这个 action
//!    存在，`liars_dice_nope` 也能剥成 `("liars_dice", "nope")`。
//!
//! # 两代历史
//!
//! 第一代硬编码了两款游戏的完整 schema，于是"零改动加游戏"名不副实：新游戏的
//! 工具永远返回"未知工具"。
//!
//! 第二代替换成 `GameEngine::describe_tools()`——工具表住在引擎里。那带来了
//! 两个问题：引擎的 `impl` 块被 100 多行 schema 撑大；取工具表要先**构造一个
//! 引擎**（本模块的 `specs_of` 曾经这么干，而它被 HTTP 端点调用）。
//!
//! 现在是第三代：工具表是**注册元数据**（`GameMeta::tools`），读它不需要引擎。
//!
//! # 没有工具表会怎样
//!
//! `GameMeta::new()` 默认空表——**忘了 `.with_tools()` 不会报错，只会静默地
//! 没有工具**。`crates/agent-game-server/tests/invariants.rs` 里有一条遍历注册
//! 表的用例盯着这件事。

use agent_game_server_core::{PlayerMode, Registry, ToolSpec};
use rmcp::model::Tool;

/// 注册表里所有游戏声明的工具。
#[must_use]
pub fn all_tools(registry: &Registry) -> Vec<Tool> {
    let mut tools = Vec::new();
    for (game_type, meta) in registry.iter() {
        // 直接读注册元数据，**不构造引擎**。
        if !meta.tools.is_empty() {
            let specs = specs_of(registry, game_type).unwrap_or_default();
            tools.extend(specs.into_iter().map(to_tool));
        }
    }
    tools
}

/// 某个游戏的工具声明。未注册时返回 `None`；注册了但没声明工具时返回空表。
///
/// 公开是为了让 `/api/v1/games/tools` 不必经过 MCP 的 `Tool` 类型。
#[must_use]
pub fn specs_of(registry: &Registry, game_type: &str) -> Option<Vec<ToolSpec>> {
    // 工具表在注册元数据里，**不需要构造引擎**。
    //
    // 改前这里 `(meta.factory)(...)` 造一个用完即弃的引擎只为读一串静态
    // JSON Schema，而本函数被三个 HTTP 端点调用——等于每个请求配一个引擎。
    registry.get(game_type).map(|meta| meta.tools.clone())
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
        registry.register(
            GameMeta::new("guess_number", PlayerMode::Single, |params, config| {
                agent_game_server_core::games::GuessNumberEngine::new(params, config)
                    .map(|engine| Box::new(engine) as BoxedEngine)
            })
            // 工具面由 `GameMeta` 声明，不是引擎的方法——直接用生产函数，
            // 免得测试里的 schema 与生产漂移。
            .with_tools(agent_game_server_core::games::guess_number::tools()),
        );
        registry.register(
            GameMeta::new("liars_dice", PlayerMode::Multi, |params, config| {
                agent_game_server_core::games::LiarsDiceEngine::new(params, config)
                    .map(|engine| Box::new(engine) as BoxedEngine)
            })
            .with_tools(agent_game_server_core::games::liars_dice::tools()),
        );
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
        // 没有 `.with_tools()` 就是空表：纯内部用途的游戏不该让工具表出错。
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

    /// **读工具表不该构造引擎。**
    ///
    /// 这是 #19 的核心收益：`specs_of()` 改前是 `(meta.factory)(...)` 造一个
    /// 用完即弃的引擎只为读一串静态 JSON，而它被三个 HTTP 端点调用——等于每个
    /// 请求配一个引擎。
    ///
    /// 用一个**会记录调用次数**的 factory 来钉住这件事：只要有人把
    /// `specs_of` 改回造引擎，计数就不再是 0，这条会红。
    #[test]
    fn reading_the_tools_never_constructs_an_engine() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // 用 `'static` 的计数器：factory 要求 `Send + Sync + 'static`
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        CALLS.store(0, Ordering::SeqCst);

        let mut registry = Registry::new();
        registry.register(
            GameMeta::new("counter", PlayerMode::Single, |_p, _c| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                // 构造故意失败：就算有人真的调了，也拿不到可用引擎
                Err("这条测试不该构造引擎".to_owned())
            })
            .with_tools(vec![ToolSpec::new(
                "counter_state",
                "看状态",
                serde_json::json!({"type": "object", "properties": {}, "required": []}),
            )]),
        );

        // 读工具表：三条路径都不能构造引擎
        let specs = specs_of(&registry, "counter");
        let tools = all_tools(&registry);
        let count = tool_count(&registry, "counter");

        assert_eq!(
            specs.map(|s| s.len()),
            Some(1),
            "即使 factory 会失败，工具表也要读得出来"
        );
        assert_eq!(tools.len(), 1);
        assert_eq!(count, 1);
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            0,
            "读工具表不该构造引擎——改前 specs_of 会调一次 factory"
        );
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

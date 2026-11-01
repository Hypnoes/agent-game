//! MCP 工具定义。
//!
//! 与 Python 版逐字对齐：工具名、描述、`required` 字段、`timeout` 的默认值与上限。
//! 这些直接决定 Agent 怎么调用，改一个字都是玩法契约的变化。
//!
//! schema 手写而不是用 `schemars` 从类型派生：派生会引入宏与 rmcp 版本耦合，
//! 而"7 个工具、字段固定"这件事用 `serde_json::json!` 表达更直接，
//! `tests/tool_schemas.rs` 用结构断言钉住 `required` 与描述的完整性。

use agent_game_server_core::{PlayerMode, Registry};
use rmcp::model::Tool;
use serde_json::{Value, json};

/// 所有工具定义。
#[must_use]
pub fn all_tools(registry: &Registry) -> Vec<Tool> {
    let mut tools = Vec::new();
    for (game_type, _meta) in registry.iter() {
        match game_type {
            "guess_number" => tools.extend(guess_number_tools()),
            "liars_dice" => tools.extend(liars_dice_tools()),
            _ => {}
        }
    }
    tools
}

/// 某个游戏的工具定义（`/api/v1/games/tools` 用；未注册的游戏返回空列表）。
#[must_use]
pub fn all_tools_for(game_type: &str) -> Vec<Tool> {
    match game_type {
        "guess_number" => guess_number_tools(),
        "liars_dice" => liars_dice_tools(),
        _ => Vec::new(),
    }
}

/// 某个引擎的工具数量（测试用）。
#[must_use]
pub fn tool_count(_registry: &Registry, game_type: &str) -> usize {
    match game_type {
        "guess_number" => guess_number_tools().len(),
        "liars_dice" => liars_dice_tools().len(),
        _ => 0,
    }
}

fn tool(name: &'static str, description: &'static str, schema: Value) -> Tool {
    let schema = match schema {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    Tool::new(name, description, schema)
}

/// 某个 `tool_name` 是否真的由该引擎**声明**过。
///
/// 工具名前缀能剥出 action，不等于这个 action 存在：`liars_dice_nope` 也能剥成
/// `("liars_dice", "nope")`。只按前缀放行会让拼错的工具名走到引擎里再以
/// "未知 action"失败，错误信息指向错的地方；更要紧的是它可能先建出一局空对局。
#[must_use]
pub fn declares(_registry: &Registry, game_type: &str, tool_name: &str) -> bool {
    all_tools_for(game_type)
        .iter()
        .any(|tool| tool.name.as_ref() == tool_name)
}

#[must_use]
pub fn guess_number_tools() -> Vec<Tool> {
    vec![
        tool(
            "guess_number_start",
            "开始一局新的猜数字游戏（1-100）。Agent 调用此工具创建对局并开始猜测。",
            json!({"type": "object", "properties": {}, "required": []}),
        ),
        tool(
            "guess_number_guess",
            "提交一个 1-100 之间的猜测数字。返回 too_high/too_low/correct 和剩余次数。\
             游戏未结束时返回值引导你继续猜。",
            json!({
                "type": "object",
                "properties": {
                    "number": {
                        "type": "integer",
                        "description": "猜测数字（1-100）",
                        "minimum": 1,
                        "maximum": 100,
                    }
                },
                "required": ["number"],
            }),
        ),
        tool(
            "guess_number_state",
            "查看当前对局状态，包括所有历史猜测。不需要参数。",
            json!({"type": "object", "properties": {}, "required": []}),
        ),
    ]
}

#[must_use]
pub fn liars_dice_tools() -> Vec<Tool> {
    vec![
        tool(
            "liars_dice_join",
            "加入大话骰（吹牛/撒谎骰子）匹配队列。只需提供名字，服务器自动匹配对手。\
             如果之前断线，传入 player_token 可重连回原有对局。每人5颗骰子，1是百搭。\
             返回玩家编号和游戏状态。",
            json!({
                "type": "object",
                "properties": {
                    "player_name": {"type": "string", "description": "你的名字"},
                    "player_token": {
                        "type": "string",
                        "description": "断线重连时传入之前的 token",
                    },
                    // 建桌参数：同桌的所有玩家必须传一致的值，否则会各开一桌。
                    "player_count": {
                        "type": "integer",
                        "description": "这一桌几个人（默认 2，至少 2）。同桌的所有玩家必须传同一个值。",
                        "minimum": 2,
                        "default": 2,
                    },
                    "dice_per_player": {
                        "type": "integer",
                        "description": "每人几颗骰子（默认 5，至少 1）。同桌的所有玩家必须传同一个值。",
                        "minimum": 1,
                        "default": 5,
                    },
                },
                "required": ["player_name"],
            }),
        ),
        tool(
            "liars_dice_state",
            "查看大话骰对局当前状态。**注意：如果还没轮到你，这个调用会阻塞等待\
             （最长 55 秒）直到对手行动、游戏结束或超时。返回慢是正常的——不是出错，\
             是服务器在等你对面的玩家决策。等满 55 秒仍未轮到你时，返回的提示会告诉你\
             继续等还是就此打住。** 必须带上你的 player_index 和 player_token。",
            json!({
                "type": "object",
                "properties": {
                    "player_index": {"type": "integer", "description": "你的玩家编号"},
                    "player_token": {
                        "type": "string",
                        "description": "你的身份 token（必填）",
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "本次最多阻塞多少秒，缺省 55，上限 55",
                        "default": 55,
                        "maximum": 55,
                    },
                },
                "required": ["player_index", "player_token"],
            }),
        ),
        tool(
            "liars_dice_bid",
            "在大话骰中叫数。宣称全场至少有多少颗某个点数的骰子。数量不能超过场上骰子\
             总数，面值 1-6，且必须比上家大。可选附带一句话 bluff 或误导对手。",
            json!({
                "type": "object",
                "properties": {
                    "player_index": {"type": "integer", "description": "你的玩家编号"},
                    "player_token": {
                        "type": "string",
                        "description": "你的身份 token（必填）",
                    },
                    "quantity": {
                        "type": "integer",
                        "description": "数量（≥1，且不超过场上骰子总数）",
                        "minimum": 1,
                    },
                    "face": {
                        "type": "integer",
                        "description": "面值 1-6",
                        "minimum": 1,
                        "maximum": 6,
                    },
                    "message": {
                        "type": "string",
                        "description": "可选: bluff/误导信息",
                    },
                },
                "required": ["player_index", "player_token", "quantity", "face"],
            }),
        ),
        tool(
            "liars_dice_challenge",
            "在大话骰中开盅（挑战上家的叫数）。所有人揭骰验证。不能开自己的盅。\
             必须带上你的 player_index 和 player_token。",
            json!({
                "type": "object",
                "properties": {
                    "player_index": {"type": "integer", "description": "你的玩家编号"},
                    "player_token": {
                        "type": "string",
                        "description": "你的身份 token（必填）",
                    },
                },
                "required": ["player_index", "player_token"],
            }),
        ),
    ]
}

/// 引擎应有的 `player_mode`，用于测试断言工具面与引擎一致。
#[must_use]
pub fn expected_player_mode(game_type: &str) -> Option<PlayerMode> {
    match game_type {
        "guess_number" => Some(PlayerMode::Single),
        "liars_dice" => Some(PlayerMode::Multi),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_game_server_core::GameMeta;
    use serde_json::json;

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.register(GameMeta::new(
            "guess_number",
            PlayerMode::Single,
            |_p, _c| Err("not used".to_owned()),
        ));
        registry.register(GameMeta::new("liars_dice", PlayerMode::Multi, |_p, _c| {
            Err("not used".to_owned())
        }));
        registry
    }

    #[test]
    fn every_registered_game_has_tools() {
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
    fn required_lists_match_the_documented_fields() {
        let tools = liars_dice_tools();
        let bid = tools
            .iter()
            .find(|tool| tool.name == "liars_dice_bid")
            .unwrap();
        assert_eq!(
            bid.input_schema.get("required").unwrap(),
            &json!(["player_index", "player_token", "quantity", "face"])
        );

        let join = tools
            .iter()
            .find(|tool| tool.name == "liars_dice_join")
            .unwrap();
        assert_eq!(
            join.input_schema.get("required").unwrap(),
            &json!(["player_name"])
        );
    }

    #[test]
    fn the_timeout_parameter_documents_the_cap() {
        let tools = liars_dice_tools();
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
        let tools = guess_number_tools();
        for tool in tools {
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
        let mut registry = Registry::new();
        registry.register(GameMeta::new("other", PlayerMode::Single, |_p, _c| {
            Err("x".to_owned())
        }));
        assert!(all_tools(&registry).is_empty());
        assert_eq!(tool_count(&registry, "other"), 0);
    }
}

//! 参数访问器与协议默认值的契约。
//!
//! 这些方法看起来"太简单，不用测"，于是变异测试立刻抓到了它们：把
//! `CreateParams::get` 改成永远返回 `None`、把 `optional_i64` 改成永远返回
//! `Some(0)`，整套测试依然全绿。
//!
//! 它们简单，但每一条都是一个**契约**：JSON 里 `true` 不是数字、缺键与 `null`
//! 是两件不同的事、`without` 必须真的去掉键。写坏了不会崩，只会静默错。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_game_server_core::engine::{
    DEFAULT_FINISHED_STATES, DEFAULT_SENSITIVE_KEYS, PlayerMode, PlayerView, TimeoutEvent,
};
use agent_game_server_core::{
    ActionParams, CreateParams, EngineError, GameEngine, GameMeta, PromptStore, Registry,
};
use serde_json::{Value, json};

fn params(value: Value) -> ActionParams {
    match value {
        Value::Object(map) => ActionParams::new(map),
        _ => ActionParams::new(serde_json::Map::new()),
    }
}

fn create(value: Value) -> CreateParams {
    match value {
        Value::Object(map) => CreateParams::new(map),
        _ => CreateParams::new(serde_json::Map::new()),
    }
}

// ═══════════════════════════════════════════
// CreateParams
// ═══════════════════════════════════════════

#[test]
fn get_returns_the_value_that_was_stored() {
    let params = create(json!({"player_count": 3}));
    assert_eq!(params.get("player_count"), Some(&Value::from(3)));
    assert_eq!(params.get("missing"), None);
}

#[test]
fn get_i64_reads_integers_and_rejects_everything_else() {
    let params = create(json!({"n": 7}));
    assert_eq!(params.get_i64("n"), Ok(Some(7)));
    assert_eq!(params.get_i64("missing"), Ok(None));

    // 浮点不是整数：静默截断会让 6.5 变成 6
    assert!(create(json!({"n": 7.5})).get_i64("n").is_err());
    // bool 更不是：Python 里它是 int 的子类，会静默变成 1/0
    assert!(create(json!({"n": true})).get_i64("n").is_err());
    assert!(create(json!({"n": "7"})).get_i64("n").is_err());
}

#[test]
fn get_str_reads_strings_and_rejects_everything_else() {
    let params = create(json!({"name": "Alice"}));
    assert_eq!(params.get_str("name"), Ok(Some("Alice")));
    assert_eq!(params.get_str("missing"), Ok(None));
    assert!(create(json!({"name": 5})).get_str("name").is_err());
}

#[test]
fn without_actually_removes_the_key() {
    let params = create(json!({"player_count": 2, "player_token": "tok"}));
    let stripped = params.without("player_token");

    assert!(stripped.get("player_token").is_none(), "键必须被移除");
    assert_eq!(stripped.get("player_count"), Some(&Value::from(2)));
    // 原对象不受影响
    assert!(params.get("player_token").is_some(), "不能改动原对象");
}

#[test]
fn from_a_json_map_preserves_every_key() {
    let mut raw = serde_json::Map::new();
    raw.insert("a".to_owned(), Value::from(1));
    raw.insert("b".to_owned(), Value::from("x"));
    let params = CreateParams::from(raw);
    assert_eq!(params.get("a"), Some(&Value::from(1)));
    assert_eq!(params.get("b"), Some(&Value::from("x")));
}

#[test]
fn default_create_params_are_empty() {
    let params = CreateParams::default();
    assert!(params.get("anything").is_none());
    assert_eq!(params.get_i64("anything"), Ok(None));
}

// ═══════════════════════════════════════════
// ActionParams
// ═══════════════════════════════════════════

#[test]
fn require_i64_demands_the_key() {
    let action = params(json!({"quantity": 3}));
    assert_eq!(action.require_i64("quantity").unwrap(), 3);

    let error = action.require_i64("face").unwrap_err();
    assert!(error.to_string().contains("face"), "{error}");
    assert!(
        error.is_client_error(),
        "缺参数是调用方问题，不是服务端 bug"
    );
}

#[test]
fn require_i64_rejects_non_integers() {
    assert!(params(json!({"n": 1.5})).require_i64("n").is_err());
    assert!(params(json!({"n": true})).require_i64("n").is_err());
    assert!(params(json!({"n": "1"})).require_i64("n").is_err());
    assert!(params(json!({"n": null})).require_i64("n").is_err());
}

#[test]
fn optional_i64_distinguishes_absent_from_zero() {
    // 这条是变异测试抓到的：把 `optional_i64` 改成恒返回 `Some(0)`
    // 会让"没给"与"给了 0"混为一谈。
    assert_eq!(params(json!({})).optional_i64("n").unwrap(), None);
    assert_eq!(params(json!({"n": 0})).optional_i64("n").unwrap(), Some(0));
    assert_eq!(params(json!({"n": 5})).optional_i64("n").unwrap(), Some(5));
    assert!(params(json!({"n": "5"})).optional_i64("n").is_err());
}

#[test]
fn require_str_demands_the_key() {
    let action = params(json!({"player_name": "Alice"}));
    assert_eq!(action.require_str("player_name").unwrap(), "Alice");
    assert!(action.require_str("message").is_err());
    assert!(params(json!({"x": 1})).require_str("x").is_err());
}

#[test]
fn optional_str_treats_absent_and_null_alike() {
    assert_eq!(params(json!({})).optional_str("m").unwrap(), None);
    assert_eq!(params(json!({"m": null})).optional_str("m").unwrap(), None);
    assert_eq!(
        params(json!({"m": "hi"})).optional_str("m").unwrap(),
        Some("hi")
    );
    assert!(params(json!({"m": 5})).optional_str("m").is_err());
}

#[test]
fn player_index_rejects_negative_values() {
    // 负索引会绕回最后一名玩家，等于凭空拿到对手的骰子
    let error = params(json!({"player_index": -1}))
        .player_index()
        .unwrap_err();
    assert!(matches!(error, EngineError::InvalidIndex(_)), "{error:?}");
    assert_eq!(
        params(json!({"player_index": 1})).player_index().unwrap(),
        Some(1)
    );
}

#[test]
fn player_index_rejects_non_integers_and_absent_is_none() {
    assert_eq!(params(json!({})).player_index().unwrap(), None);
    assert_eq!(
        params(json!({"player_index": null}))
            .player_index()
            .unwrap(),
        None
    );
    for bad in [json!("0"), json!(true), json!(1.5)] {
        assert!(
            params(json!({"player_index": bad})).player_index().is_err(),
            "{bad} 不该被当成合法索引"
        );
    }
}

#[test]
fn player_token_is_read_from_the_params() {
    assert_eq!(
        params(json!({"player_token": "tok"}))
            .player_token()
            .unwrap(),
        Some("tok")
    );
    assert_eq!(params(json!({})).player_token().unwrap(), None);
    assert!(params(json!({"player_token": 1})).player_token().is_err());
}

// ═══════════════════════════════════════════
// 协议的默认值
// ═══════════════════════════════════════════

/// 一个什么都不声明的引擎：全部可选能力都取默认值。
#[derive(Debug)]
struct Bare {
    prompts: PromptStore,
    signal: std::sync::Arc<agent_game_server_core::ChangeSignal>,
}

impl Bare {
    fn new() -> Self {
        Self {
            prompts: PromptStore::new("[templates]\n"),
            signal: std::sync::Arc::new(agent_game_server_core::ChangeSignal::new()),
        }
    }
}

impl GameEngine for Bare {
    fn game_type(&self) -> &'static str {
        "bare"
    }
    fn player_mode(&self) -> PlayerMode {
        PlayerMode::Single
    }
    fn phase(&self) -> &'static str {
        "in_progress"
    }
    fn admin_view(&self) -> Value {
        json!({"phase": self.phase()})
    }
    fn public_view(&self) -> Value {
        json!({"phase": self.phase()})
    }
    fn player_view(&self, _player_index: Option<usize>) -> Result<PlayerView, EngineError> {
        Ok(PlayerView::default())
    }
    fn handle_action(
        &mut self,
        _action: &str,
        _params: ActionParams,
    ) -> Result<PlayerView, EngineError> {
        Ok(PlayerView::default())
    }
    fn check_timeout(
        &mut self,
        _now: std::time::Instant,
    ) -> Result<Option<TimeoutEvent>, EngineError> {
        Ok(None)
    }
    fn signal(&self) -> std::sync::Arc<agent_game_server_core::ChangeSignal> {
        std::sync::Arc::clone(&self.signal)
    }
    fn prompts(&self) -> &PromptStore {
        &self.prompts
    }
}

#[test]
fn the_default_finished_states_cover_the_three_common_names() {
    let engine = Bare::new();
    assert_eq!(engine.finished_states(), DEFAULT_FINISHED_STATES);
    assert!(DEFAULT_FINISHED_STATES.contains(&"finished"));
    assert!(DEFAULT_FINISHED_STATES.contains(&"won"));
    assert!(DEFAULT_FINISHED_STATES.contains(&"lost"));
}

#[test]
fn the_default_sensitive_keys_cover_the_two_builtin_secrets() {
    let engine = Bare::new();
    assert_eq!(engine.sensitive_keys(), DEFAULT_SENSITIVE_KEYS);
    assert!(DEFAULT_SENSITIVE_KEYS.contains(&"target"));
    assert!(DEFAULT_SENSITIVE_KEYS.contains(&"all_dice"));
}

#[test]
fn a_bare_engine_is_finished_only_in_a_terminal_phase() {
    let engine = Bare::new();
    assert!(!engine.is_finished(), "in_progress 不是终态");
}

#[test]
fn the_default_waiting_condition_never_waits() {
    // 单人即时游戏的默认实现：state 调用不该挂起
    let engine = Bare::new();
    assert!(engine.is_ready_for(0));
    assert!(engine.is_ready_for(99));
}

#[test]
fn a_bare_engine_has_no_multiplayer_identity() {
    let engine = Bare::new();
    assert_eq!(engine.find_player_by_token("anything"), None);
    assert_eq!(engine.player_tokens(), None);
}

#[test]
fn is_multi_is_false_for_single_player_modes() {
    // 变异成恒 `true` 会让单人游戏也要求 token
    assert!(!PlayerMode::Single.is_multi());
    assert!(PlayerMode::Multi.is_multi());
}

// ═══════════════════════════════════════════
// 序列化出口
// ═══════════════════════════════════════════

#[test]
fn player_view_to_json_keeps_all_three_parts() {
    let view = PlayerView {
        public: json!({"phase": "bidding"}),
        private: json!({"your_dice": [1, 2]}),
        prompt: "轮到你".to_owned(),
    };
    let json = view.to_json();

    assert_eq!(json["public"]["phase"], "bidding");
    assert_eq!(json["private"]["your_dice"], json!([1, 2]));
    assert_eq!(json["prompt"], "轮到你");
}

#[test]
fn timeout_event_to_json_carries_the_type_and_the_optional_fields() {
    let turn = TimeoutEvent::turn_timeout(2).to_json();
    assert_eq!(turn["type"], "turn_timeout");
    assert_eq!(turn["loser"], 2);
    assert!(
        turn.get("expired").is_none(),
        "非回收型事件不该带 expired: {turn}"
    );

    let lobby = TimeoutEvent::lobby_timeout().to_json();
    assert_eq!(lobby["type"], "lobby_timeout");
    assert_eq!(lobby["expired"], true);
    assert!(lobby.get("loser").is_none());

    let session = TimeoutEvent::session_timeout().to_json();
    assert_eq!(session["type"], "session_timeout");
    assert_eq!(session["expired"], true);
}

// ═══════════════════════════════════════════
// 注册表的展示名与工厂
// ═══════════════════════════════════════════

#[test]
fn a_registered_game_carries_its_display_name_and_mode() {
    let mut registry = Registry::new();
    registry.register(
        GameMeta::new("bare", PlayerMode::Multi, |_params, _config| {
            Ok(Box::new(Bare::new()) as agent_game_server_core::engine::BoxedEngine)
        })
        .with_display_name("无人游戏"),
    );

    let meta = registry.get("bare").expect("注册过就查得到");
    assert_eq!(meta.display_name, "无人游戏");
    assert_eq!(meta.player_mode, PlayerMode::Multi);
    assert_eq!(registry.display_names().get("bare"), Some(&"无人游戏"));
}

#[test]
fn a_factory_error_is_reported_verbatim() {
    let mut registry = Registry::new();
    registry.register(GameMeta::new("picky", PlayerMode::Single, |_p, _c| {
        Err("player_count 必须是 ≥ 2 的整数".to_owned())
    }));

    let error = registry
        .create(
            "picky",
            &CreateParams::default(),
            &agent_game_server_core::Config::default(),
        )
        .unwrap_err();
    assert!(error.contains("player_count"), "{error}");
}

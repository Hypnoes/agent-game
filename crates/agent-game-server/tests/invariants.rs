//! 不变量层：必须永远成立的规则，而不是事故清单。
//!
//! 每个用例上方标注它守的是哪条不变量，以及删掉对应代码会发生什么。
//! 这些是"从没出过 bug 所以从没被写过"的那一类——它们坏掉时同样致命。

// 集成测试是独立 crate，不在 `cfg(test)` 里，clippy 的 `allow-unwrap-in-tests`
// 管不到它。测试里 unwrap 就是"断言这里必须成功"。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Instant;

use agent_game_server_core::games::{GuessNumberEngine, LiarsDiceEngine};
use agent_game_server_core::{
    ActionParams, ChangeSignal, Config, CreateParams, EngineError, GameEngine, GameMeta,
    PlayerMode, PlayerView, Registry, Session,
};
use agent_game_server_server::AppState;
use serde_json::{Value, json};

// ═══════════════════════════════════════════
// 不变量 1：1 是百搭
// ═══════════════════════════════════════════

fn started(player_count: usize, dice_per_player: usize) -> (LiarsDiceEngine, Vec<String>) {
    let mut raw = serde_json::Map::new();
    raw.insert("player_count".to_owned(), Value::from(player_count));
    raw.insert("dice_per_player".to_owned(), Value::from(dice_per_player));
    let mut engine = LiarsDiceEngine::new(&CreateParams::new(raw), &Config::default()).unwrap();
    for index in 0..player_count {
        let mut params = serde_json::Map::new();
        params.insert("player_name".to_owned(), Value::String(format!("P{index}")));
        engine
            .handle_action("join", ActionParams::new(params))
            .unwrap();
    }
    let tokens = engine
        .players
        .iter()
        .map(|player| player.token.clone())
        .collect();
    (engine, tokens)
}

fn script(engine: &mut LiarsDiceEngine, faces: Vec<Vec<i64>>) {
    assert_eq!(faces.len(), engine.players.len());
    for (player, dice) in engine.players.iter_mut().zip(faces) {
        player.dice = dice;
    }
}

fn bid(
    engine: &mut LiarsDiceEngine,
    player: usize,
    token: &str,
    quantity: i64,
    face: i64,
) -> Result<PlayerView, EngineError> {
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(player));
    raw.insert("player_token".to_owned(), Value::String(token.to_owned()));
    raw.insert("quantity".to_owned(), Value::from(quantity));
    raw.insert("face".to_owned(), Value::from(face));
    engine.handle_action("bid", ActionParams::new(raw))
}

fn challenge(
    engine: &mut LiarsDiceEngine,
    player: usize,
    token: &str,
) -> Result<PlayerView, EngineError> {
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(player));
    raw.insert("player_token".to_owned(), Value::String(token.to_owned()));
    engine.handle_action("challenge", ActionParams::new(raw))
}

#[test]
fn ones_count_as_any_face() {
    // 删掉 `count_face` 里的 `die == 1` 分支，这条立刻红。
    let (mut engine, _tokens) = started(2, 3);
    script(&mut engine, vec![vec![1, 3, 3], vec![2, 3, 3]]);
    assert_eq!(engine.count_face(3), 5, "3 的个数 = 明面 4 + 百搭 1");
    assert_eq!(engine.count_face(2), 2, "2 的个数 = 明面 1 + 百搭 1");
}

#[test]
fn ones_only_count_as_ones_when_bidding_ones() {
    // 与上一条互为反面。只断言 count_face(1) 还不够：若实现写成
    // `d == face || d == 1`，叫 1 时会把非 1 的点数也算进来。
    let (mut engine, _tokens) = started(2, 3);
    script(&mut engine, vec![vec![1, 3, 3], vec![2, 3, 3]]);
    assert_eq!(engine.count_face(1), 1);
    assert_ne!(engine.count_face(1), engine.total_dice());
}

#[test]
fn bidding_ones_with_only_one_face_loses() {
    // 让"叫 1 时 1 不百搭"这一条真的影响胜负，而不只是某个函数返回值。
    let (mut engine, tokens) = started(2, 3);
    script(&mut engine, vec![vec![1, 3, 3], vec![2, 3, 3]]);
    engine.set_current_turn(0);

    bid(&mut engine, 0, &tokens[0], 2, 1).unwrap();
    challenge(&mut engine, 1, &tokens[1]).unwrap();
    assert_eq!(
        engine.loser(),
        Some(0),
        "全场只有 1 颗 1，2×1 不成立，叫的人该输"
    );
}

#[test]
fn the_wildcard_changes_the_actual_count_not_just_the_display() {
    // 走完整路径（叫数 → 开盅 → 判定输家）：计数错了但没人用它判定，规则仍然是好的；
    // 反过来，计数对但判定没用它，百搭就形同虚设。
    let (mut engine, tokens) = started(2, 3);
    script(&mut engine, vec![vec![1, 3, 3], vec![2, 3, 3]]);
    engine.set_current_turn(0);

    bid(&mut engine, 0, &tokens[0], 5, 3).unwrap();
    challenge(&mut engine, 1, &tokens[1]).unwrap();
    assert_eq!(engine.loser(), Some(1), "含百搭的 5×3 成立，开盅的人输");
}

// ═══════════════════════════════════════════
// 不变量 2：开局后不许再加人
// ═══════════════════════════════════════════

#[test]
fn join_after_start_is_refused() {
    // 否则新玩家会触发 `start_game()`，把进行中的对局推倒重来。
    let (mut engine, _tokens) = started(2, 3);
    let before: Vec<Vec<i64>> = engine.players.iter().map(|p| p.dice.clone()).collect();

    let mut params = serde_json::Map::new();
    params.insert("player_name".to_owned(), Value::from("Late"));
    let result = engine.handle_action("join", ActionParams::new(params));

    assert!(result.is_err(), "开局后加入必须被拒绝");
    assert_eq!(engine.players.len(), 2, "人数不变");
    let after: Vec<Vec<i64>> = engine.players.iter().map(|p| p.dice.clone()).collect();
    assert_eq!(before, after, "骰子不能被重摇");
}

// ═══════════════════════════════════════════
// 不变量 3：每一条状态变更路径都要 notify
// ═══════════════════════════════════════════

#[test]
fn every_state_changing_path_notifies() {
    // 用"订阅 → 动作 → 检查"三步：动作前订阅，动作后 has_changed()。
    // 少一条 notify，对应的那一段立刻红。

    // bid
    let (mut engine, tokens) = started(3, 3);
    let signal = engine.signal();
    let sub = signal.subscribe();
    engine.set_current_turn(0);
    bid(&mut engine, 0, &tokens[0], 1, 2).unwrap();
    assert!(sub.has_changed(), "bid 必须通知");

    // challenge
    let signal = engine.signal();
    let sub = signal.subscribe();
    challenge(&mut engine, 1, &tokens[1]).unwrap();
    assert!(sub.has_changed(), "challenge 必须通知");
    drop(sub);

    // 回合超时判负
    let (mut engine, _tokens) = started(2, 3);
    let signal = engine.signal();
    let sub = signal.subscribe();
    let _ = engine
        .check_timeout(Instant::now() + engine.turn_timeout() * 2)
        .unwrap();
    assert!(sub.has_changed(), "超时判负必须通知");
    drop(sub);

    // 大厅超时
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let mut params = serde_json::Map::new();
    params.insert("player_name".to_owned(), Value::from("Solo"));
    engine
        .handle_action("join", ActionParams::new(params))
        .unwrap();
    let signal = engine.signal();
    let sub = signal.subscribe();
    let _ = engine
        .check_timeout(Instant::now() + std::time::Duration::from_secs(300))
        .unwrap();
    assert!(sub.has_changed(), "大厅超时必须通知等待者");
    drop(sub);

    // 入座（未满员）
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let signal = engine.signal();
    let sub = signal.subscribe();
    let mut params = serde_json::Map::new();
    params.insert("player_name".to_owned(), Value::from("A"));
    engine
        .handle_action("join", ActionParams::new(params))
        .unwrap();
    assert!(sub.has_changed(), "入座必须通知大厅里的等待者");
    drop(sub);

    // 开局（满员）
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let mut params = serde_json::Map::new();
    params.insert("player_name".to_owned(), Value::from("A"));
    engine
        .handle_action("join", ActionParams::new(params))
        .unwrap();
    let signal = engine.signal();
    let sub = signal.subscribe();
    let mut params = serde_json::Map::new();
    params.insert("player_name".to_owned(), Value::from("B"));
    engine
        .handle_action("join", ActionParams::new(params))
        .unwrap();
    assert!(sub.has_changed(), "开局必须通知等待者");
}

// ═══════════════════════════════════════════
// 不变量 4：超时与开盅对"输掉本轮"的处理必须一致
// ═══════════════════════════════════════════

#[test]
fn a_timeout_clears_the_round_bid_like_a_challenge_does() {
    // 超时分支只掉骰、不清叫数时，下家面对一个无法再加码的僵尸叫数，
    // 只能替别人开盅——胜负由别人的超时决定。
    let (mut engine, tokens) = started(3, 2);
    engine.set_current_turn(0);
    bid(&mut engine, 0, &tokens[0], 6, 3).unwrap();
    assert!(engine.current_call().is_some());

    let now = Instant::now() + engine.turn_timeout() * 2;
    let _ = engine.check_timeout(now).unwrap();

    assert_eq!(engine.current_call(), None, "超时必须清零本轮叫数");
    // 下家现在能正常加码，而不是被"超过总数"锁死
    engine.set_current_turn(2);
    assert!(bid(&mut engine, 2, &tokens[2], 1, 3).is_ok());
}

#[test]
fn a_timeout_records_the_round_as_void() {
    let (mut engine, tokens) = started(3, 2);
    engine.set_current_turn(0);
    bid(&mut engine, 0, &tokens[0], 2, 3).unwrap();
    let before = engine.call_history().len();

    let now = Instant::now() + engine.turn_timeout() * 2;
    let _ = engine.check_timeout(now).unwrap();

    assert_eq!(engine.call_history().len(), before + 1);
    let last = engine.call_history().last().unwrap();
    assert_eq!(last.quantity, 0, "quantity=0 表示这不是一次叫数");
    assert!(last.message.contains("超时判负"), "{}", last.message);
}

#[test]
fn a_timeout_without_a_bid_adds_no_history() {
    let (mut engine, _tokens) = started(3, 2);
    let before = engine.call_history().len();
    let now = Instant::now() + engine.turn_timeout() * 2;
    let _ = engine.check_timeout(now).unwrap();
    assert_eq!(
        engine.call_history().len(),
        before,
        "没有叫数就没有作废记录"
    );
}

// ═══════════════════════════════════════════
// 不变量 5：两条时间轴的关系
// ═══════════════════════════════════════════

#[test]
fn the_turn_timeout_is_longer_than_the_wait_budget() {
    // 判负必须晚于等待预算，否则每个 state 都会先等到判负。
    let engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    assert!(
        engine.turn_timeout() > Config::default().block_budget,
        "turn_timeout {:?} 必须大于 block_budget {:?}",
        engine.turn_timeout(),
        Config::default().block_budget
    );
}

// ═══════════════════════════════════════════
// 不变量 6：每个注册引擎的秘密边界
// ═══════════════════════════════════════════

/// 遍历**每一个**注册的引擎，断言它的公开视图不含敏感字段、admin 视图含。
///
/// 这是"秘密信息靠构造而不是靠剥离"的看门人：新增一个引擎若忘了写
/// `public_view`（或把它写成 `admin_view` 的别名），这条立刻红。
#[tokio::test]
async fn every_registered_engine_keeps_its_secrets_out_of_the_public_view() {
    let state = AppState::with_defaults("x".to_owned());
    let registry: Registry = state.manager.registry().clone();

    for (game_type, meta) in registry.iter() {
        let engine = (meta.factory)(&CreateParams::default(), &state.config)
            .unwrap_or_else(|error| panic!("{game_type} 造不出来: {error}"));

        let public = engine.public_view();
        let admin = engine.admin_view();
        let sensitive = engine.sensitive_keys();
        assert!(
            !sensitive.is_empty(),
            "{game_type} 没有声明任何 sensitive_keys"
        );

        for key in sensitive {
            assert!(
                public.get(key).is_none(),
                "{game_type} 的公开视图泄露了 {key}: {public}"
            );
            assert!(
                admin.get(key).is_some(),
                "{game_type} 的 admin 视图缺少 {key}（那它就不该被声明为秘密）"
            );
        }
    }
}

#[tokio::test]
async fn a_players_view_keeps_opponent_dice_private() {
    let state = AppState::with_defaults("x".to_owned());
    let registry = state.manager.registry().clone();
    let engine =
        (registry.get("liars_dice").unwrap().factory)(&CreateParams::default(), &state.config)
            .unwrap();

    // 造一局两人对局
    let mut session = engine;
    for name in ["A", "B"] {
        let mut params = serde_json::Map::new();
        params.insert("player_name".to_owned(), Value::String(name.to_owned()));
        session
            .handle_action("join", ActionParams::new(params))
            .unwrap();
    }

    let view = session.player_view(Some(0)).unwrap();
    let own = view.private["your_dice"].clone();
    assert!(own.is_array());

    // 公开部分里不能有任何骰子数组
    let public = view.public.to_string();
    assert!(!public.contains("all_dice"), "{public}");
    // 私有部分只有自己的骰子
    assert!(view.private.get("all_dice").is_none(), "{}", view.private);
}

#[tokio::test]
async fn an_engine_can_declare_its_own_finished_states() {
    // 声明终态叫 `draw`、藏 `solution` 的引擎，不碰框架代码就能生效。
    #[derive(Debug)]
    struct DrawEngine {
        phase: &'static str,
        signal: ChangeSignal,
        prompts: agent_game_server_core::PromptStore,
    }

    impl GameEngine for DrawEngine {
        fn game_type(&self) -> &'static str {
            "draw_game"
        }
        fn player_mode(&self) -> PlayerMode {
            PlayerMode::Single
        }
        fn finished_states(&self) -> &'static [&'static str] {
            &["draw"]
        }
        fn sensitive_keys(&self) -> &'static [&'static str] {
            &["solution"]
        }
        fn phase(&self) -> &'static str {
            self.phase
        }
        fn admin_view(&self) -> Value {
            json!({"phase": self.phase, "solution": 42})
        }
        fn public_view(&self) -> Value {
            json!({"phase": self.phase})
        }
        fn player_view(&self, _index: Option<usize>) -> Result<PlayerView, EngineError> {
            Ok(PlayerView {
                public: self.public_view(),
                private: json!({}),
                prompt: "draw".to_owned(),
            })
        }
        fn handle_action(
            &mut self,
            action: &str,
            _params: ActionParams,
        ) -> Result<PlayerView, EngineError> {
            if action == "draw" {
                self.phase = "draw";
                self.signal.notify();
            }
            self.player_view(None)
        }
        fn check_timeout(
            &mut self,
            _now: Instant,
        ) -> Result<Option<agent_game_server_core::TimeoutEvent>, EngineError> {
            Ok(None)
        }
        fn signal(&self) -> Arc<ChangeSignal> {
            Arc::new(ChangeSignal::new())
        }
        fn prompts(&self) -> &agent_game_server_core::PromptStore {
            &self.prompts
        }
    }

    let mut registry = Registry::new();
    registry.register(GameMeta::new(
        "draw_game",
        PlayerMode::Single,
        |_params, _config| {
            Ok(Box::new(DrawEngine {
                phase: "in_progress",
                signal: ChangeSignal::new(),
                prompts: agent_game_server_core::PromptStore::new("[templates]\n"),
            }) as Box<dyn GameEngine>)
        },
    ));
    let state = AppState::new(registry, Config::default(), "x".to_owned());

    let session: Arc<Session> = state
        .manager
        .create("draw_game", CreateParams::default(), Instant::now())
        .await
        .unwrap();
    assert!(!session.engine.lock().await.is_finished(), "还没结束");

    session
        .engine
        .lock()
        .await
        .handle_action("draw", ActionParams::default())
        .unwrap();
    assert!(
        session.engine.lock().await.is_finished(),
        "引擎声明的 draw 必须被当成终态"
    );

    // 结束 300 秒后被回收（用默认保留期）
    let retention = state.config.finished_retention;
    state.manager.check_timeouts(Instant::now()).await;
    assert_eq!(state.manager.len(), 1, "保留期内不回收");
    state
        .manager
        .check_timeouts(Instant::now() + retention * 2)
        .await;
    assert_eq!(state.manager.len(), 0, "超过保留期必须回收");
}

// ═══════════════════════════════════════════
// 不变量 7：猜数字的边界
// ═══════════════════════════════════════════

#[test]
fn guess_number_accepts_integral_floats_and_rejects_fractional_ones() {
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let target = engine.target();

    let mut params = serde_json::Map::new();
    params.insert(
        "number".to_owned(),
        serde_json::json!(f64::from(i32::try_from(target).unwrap())),
    );
    assert!(
        engine
            .handle_action("guess", ActionParams::new(params))
            .is_ok(),
        "整数形式的浮点必须接受（MCP 客户端常见）"
    );

    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let mut params = serde_json::Map::new();
    params.insert("number".to_owned(), serde_json::json!(3.5));
    assert!(
        engine
            .handle_action("guess", ActionParams::new(params))
            .is_err(),
        "非整数必须拒绝"
    );
}

#[test]
fn guess_number_rejects_bool_as_a_number() {
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let mut params = serde_json::Map::new();
    params.insert("number".to_owned(), Value::Bool(true));
    let result = engine.handle_action("guess", ActionParams::new(params));
    assert!(result.is_err(), "True 不是有意义的猜测");
    assert_eq!(engine.guess_count(), 0, "非法猜测不该计数");
}

//! 边界层：差值一个单位的规则。
//!
//! 变异测试把 `>` 改成 `>=`、把 `*` 改成 `+`、把 `<` 改成 `<=`，整套测试
//! 依然全绿——因为既有用例只测"能跑"，没测"差一个单位时结果应该不同"。
//!
//! 这个文件的每一条站位都在一个比较符的两侧：左边必须成立、右边必须不成立。
//! 它不测玩法，只测**边界是不是精确的**。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Instant;

use agent_game_server_core::games::LiarsDiceEngine;
use agent_game_server_core::{
    ActionParams, Config, CreateParams, EngineError, GameEngine, GameManager, GameMeta, PlayerMode,
    PlayerView, Registry,
};
use serde_json::Value;

/// 一个注册了两个内置游戏的 manager。
///
/// 这批用例要验 `Session.setup`——那是大厅匹配真正消费的那份配置，
/// 只有经由 `GameManager::create()` 才会产生 `Session`。
fn manager() -> GameManager {
    let mut registry = Registry::new();
    registry.register(GameMeta::new(
        "liars_dice",
        PlayerMode::Multi,
        |params, config| Ok(Box::new(LiarsDiceEngine::new(params, config)?) as _),
    ));
    registry.register(GameMeta::new(
        "guess_number",
        PlayerMode::Single,
        |params, config| {
            Ok(
                Box::new(agent_game_server_core::games::GuessNumberEngine::new(
                    params, config,
                )?) as _,
            )
        },
    ));
    GameManager::new(registry, Config::default())
}

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

// ═══════════════════════════════════════════
// 配置：读回来的值就是传进去的值
// ═══════════════════════════════════════════

#[test]
fn the_engine_reports_the_configuration_it_was_built_with() {
    let (engine, _) = started(4, 3);
    assert_eq!(engine.player_count(), 4);
    assert_eq!(engine.dice_per_player(), 3);
}

#[test]
fn the_default_configuration_is_two_players_with_five_dice() {
    let engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    assert_eq!(engine.player_count(), 2);
    assert_eq!(engine.dice_per_player(), 5);
}

// ═══════════════════════════════════════════
// 测试辅助：越界时不能改状态
// ═══════════════════════════════════════════

#[test]
fn set_dice_ignores_a_shape_that_does_not_match_the_players() {
    // 集成测试靠它摆骰子。形状对不上时静默忽略，而不是 panic 或只改一半——
    // 一个改了一半的棋盘会让后续断言给出错误的结论。
    let (mut engine, _) = started(2, 3);
    let before: Vec<Vec<i64>> = engine.players.iter().map(|p| p.dice.clone()).collect();

    engine.set_dice(vec![vec![1, 1, 1]]);
    let after: Vec<Vec<i64>> = engine.players.iter().map(|p| p.dice.clone()).collect();
    assert_eq!(before, after, "形状不匹配时不该改动任何玩家");
}

#[test]
fn set_dice_applies_a_matching_shape() {
    let (mut engine, _) = started(2, 3);
    engine.set_dice(vec![vec![1, 2, 3], vec![4, 5, 6]]);
    assert_eq!(engine.players[0].dice, vec![1, 2, 3]);
    assert_eq!(engine.players[1].dice, vec![4, 5, 6]);
}

#[test]
fn set_current_turn_accepts_only_valid_indices() {
    let (mut engine, _) = started(3, 2);

    engine.set_current_turn(2);
    assert_eq!(engine.current_turn(), 2);

    // 越界索引必须被忽略，而不是静默改成 0 或 panic
    engine.set_current_turn(3);
    assert_eq!(engine.current_turn(), 2, "越界不该改动");

    engine.set_current_turn(0);
    assert_eq!(engine.current_turn(), 0, "边界内的合法值仍然生效");
}

// ═══════════════════════════════════════════
// 叫数比较：跨 1 的两条换算
// ═══════════════════════════════════════════

#[test]
fn switching_to_ones_needs_half_plus_one_exactly() {
    // 上一手 4×3 → 叫 1 需要 quantity >= 4/2+1 = 3
    let (mut engine, tokens) = started(3, 4);
    engine.set_current_turn(0);
    bid(&mut engine, 0, &tokens[0], 4, 3).unwrap();

    // 2 不够，3 刚好够
    assert!(bid(&mut engine, 1, &tokens[1], 2, 1).is_err(), "2 应该被拒");
    assert!(bid(&mut engine, 1, &tokens[1], 3, 1).is_ok(), "3 是下界");
}

#[test]
fn switching_away_from_ones_needs_exactly_double() {
    // 上一手 2×1 → 叫非 1 需要 quantity >= 2*2 = 4
    let (mut engine, tokens) = started(3, 4);
    engine.set_current_turn(0);
    bid(&mut engine, 0, &tokens[0], 2, 1).unwrap();

    assert!(bid(&mut engine, 1, &tokens[1], 3, 3).is_err(), "3 应该被拒");
    assert!(bid(&mut engine, 1, &tokens[1], 4, 3).is_ok(), "4 是下界");
}

#[test]
fn the_same_quantity_with_a_higher_face_is_the_boundary() {
    let (mut engine, tokens) = started(3, 4);
    engine.set_current_turn(0);
    bid(&mut engine, 0, &tokens[0], 2, 3).unwrap();

    // 同数量、同面值：不成立
    assert!(bid(&mut engine, 1, &tokens[1], 2, 3).is_err());
    // 同数量、高一面：成立（差一即边界）
    assert!(bid(&mut engine, 1, &tokens[1], 2, 4).is_ok());
}

#[test]
fn a_higher_quantity_is_always_valid_regardless_of_face() {
    // 上一手 2×6（最大面值）时"抬数量"仍然成立——这是唯一还能叫的方向。
    let (mut engine, tokens) = started(3, 4);
    engine.set_current_turn(0);
    bid(&mut engine, 0, &tokens[0], 2, 6).unwrap();

    // 同数量但面值已经到顶：只能抬数量
    assert!(bid(&mut engine, 1, &tokens[1], 2, 6).is_err(), "同叫数不行");
    assert!(bid(&mut engine, 1, &tokens[1], 3, 1).is_ok(), "抬数量成立");
}

#[test]
fn quantity_must_not_exceed_the_total_dice() {
    let (mut engine, tokens) = started(2, 3);
    assert_eq!(engine.total_dice(), 6);
    engine.set_current_turn(0);

    assert!(
        bid(&mut engine, 0, &tokens[0], 7, 3).is_err(),
        "7 > 6 应该被拒"
    );
    assert!(
        bid(&mut engine, 0, &tokens[0], 6, 3).is_ok(),
        "6 == 6 是上界，合法"
    );
}

// ═══════════════════════════════════════════
// 超时：恰好越线才算超时
// ═══════════════════════════════════════════

#[test]
fn a_turn_times_out_strictly_after_the_limit() {
    // 语义是 `>`：最后时刻仍在时限内，越过一个单位才算超时。
    // 注意不能用 `Instant::now() + turn_timeout` 做"恰好相等"的测试——
    // 构造这个 `Instant` 时已经消耗了几微秒，它实际上已经越线了。
    let (mut engine, _) = started(3, 3);
    let budget = engine.turn_timeout();

    // 明确落在时限之内
    let inside = Instant::now() + budget.saturating_sub(std::time::Duration::from_millis(100));
    assert_eq!(
        engine.check_timeout(inside).unwrap(),
        None,
        "时限内不该判负"
    );

    // 明确越过
    let outside = Instant::now() + budget + std::time::Duration::from_millis(100);
    assert!(
        engine.check_timeout(outside).unwrap().is_some(),
        "越过必须判负"
    );
}

#[test]
fn a_lobby_times_out_strictly_after_two_minutes() {
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let mut params = serde_json::Map::new();
    params.insert("player_name".to_owned(), Value::from("Solo"));
    engine
        .handle_action("join", ActionParams::new(params))
        .unwrap();

    let inside = Instant::now() + std::time::Duration::from_secs(119);
    assert_eq!(
        engine.check_timeout(inside).unwrap(),
        None,
        "119 秒不该超时"
    );

    let outside = Instant::now() + std::time::Duration::from_secs(121);
    assert!(engine.check_timeout(outside).unwrap().is_some());
}

#[test]
fn a_bidding_game_does_not_time_out_in_the_lobby_window() {
    // 阶段判断错位会让进行中的对局在大厅逻辑里被判超时
    let (mut engine, _) = started(2, 3);
    let far = Instant::now() + std::time::Duration::from_secs(130);
    let outcome = engine.check_timeout(far).unwrap();
    // 130 秒超过了 60 秒回合时限，应该得到"回合超时"而不是"大厅超时"
    let event = outcome.expect("应该有事件");
    assert_eq!(event.kind, "turn_timeout");
    assert!(!event.expired);
}

// ═══════════════════════════════════════════
// 信号与配置：访问器必须返回真实的那份
// ═══════════════════════════════════════════

#[test]
fn the_signal_is_the_engines_own() {
    // 变异成 `Arc::new(Default::default())` 会让每次取到的都是新信号，
    // 于是等待者永远收不到通知。
    let (mut engine, _) = started(2, 3);
    let signal = engine.signal();

    let sub = signal.subscribe();
    for index in 0..2 {
        engine.set_current_turn(index);
    }
    engine
        .check_timeout(Instant::now() + engine.turn_timeout() * 2)
        .unwrap();
    assert!(sub.has_changed(), "超时通知必须发到同一份信号上");
}

/// 创建配置必须原样留在 `Session.setup` 里。
///
/// 这曾经测的是 `GameEngine::setup_params()`，那个方法在生产路径上零调用——
/// `Session` 已经持有同一份 `CreateParams`，大厅匹配用的就是它
/// （`lobby.rs` 的 `compatible(&session.setup, &setup)`）。方法删掉后，
/// 断言改为直接验那份真正被消费的数据。
#[tokio::test]
async fn the_session_keeps_the_creation_configuration() {
    // 变异成 `Default::default()` 会让"同配置才复用桌子"的匹配失效：
    // 所有等待中的桌子看起来参数都一样。
    let mut raw = serde_json::Map::new();
    raw.insert("player_count".to_owned(), Value::from(6));
    raw.insert("dice_per_player".to_owned(), Value::from(2));

    let manager = manager();
    let session = manager
        .create("liars_dice", CreateParams::new(raw), Instant::now())
        .await
        .unwrap();

    assert_eq!(session.setup.get("player_count"), Some(&Value::from(6)));
    assert_eq!(session.setup.get("dice_per_player"), Some(&Value::from(2)));
}

#[tokio::test]
async fn the_session_setup_never_includes_the_player_token() {
    // token 不是配置项：把它算进匹配键会让"两个人等同一张桌"永远匹配不上。
    let mut raw = serde_json::Map::new();
    raw.insert("player_count".to_owned(), Value::from(2));
    raw.insert("player_token".to_owned(), Value::from("secret"));

    let manager = manager();
    let session = manager
        .create("liars_dice", CreateParams::new(raw), Instant::now())
        .await
        .unwrap();

    assert!(
        session.setup.get("player_token").is_none(),
        "token 不能进配置指纹"
    );
}

// ═══════════════════════════════════════════
// 视图：结束与未结束必须给不同的东西
// ═══════════════════════════════════════════

#[test]
fn a_running_game_hides_the_dice_a_finished_one_reveals_them() {
    let (mut engine, tokens) = started(2, 2);

    // 进行中：玩家视角只有自己的骰子
    let running = engine.player_view(Some(0)).unwrap();
    assert!(running.private.get("all_dice").is_none());

    // 打一局到结束
    engine.set_current_turn(0);
    bid(&mut engine, 0, &tokens[0], 2, 1).unwrap();
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(1));
    raw.insert("player_token".to_owned(), Value::String(tokens[1].clone()));
    engine
        .handle_action("challenge", ActionParams::new(raw))
        .unwrap();
    assert_eq!(engine.phase(), "finished");

    let done = engine.player_view(Some(0)).unwrap();
    assert!(
        done.private.get("all_dice").is_some(),
        "结束后必须亮出全场骰子"
    );
    assert!(done.private.get("winner").is_some());
}

// ═══════════════════════════════════════════
// 猜数字的边界
// ═══════════════════════════════════════════

mod guess_number {
    use super::*;
    use agent_game_server_core::games::GuessNumberEngine;

    fn engine() -> GuessNumberEngine {
        GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap()
    }

    fn guess(engine: &mut GuessNumberEngine, number: i64) -> Result<PlayerView, EngineError> {
        let mut raw = serde_json::Map::new();
        raw.insert("number".to_owned(), Value::from(number));
        engine.handle_action("guess", ActionParams::new(raw))
    }

    #[test]
    fn the_range_boundaries_are_inclusive() {
        // min/max 本身必须可猜：`<` 写成 `<=` 会让边界值永远猜不中
        let mut engine = engine();
        let min = engine.min();
        let max = engine.max();
        assert!(guess(&mut engine, min).is_ok(), "下界必须可猜");
        assert!(guess(&mut engine, max).is_ok(), "上界必须可猜");

        assert!(guess(&mut engine, min - 1).is_err(), "下界外必须被拒");
        assert!(guess(&mut engine, max + 1).is_err(), "上界外必须被拒");
    }

    #[test]
    fn the_last_allowed_attempt_loses_and_the_one_before_does_not() {
        let mut engine = engine();
        let target = engine.target();
        // 挑一个必然猜错的数
        let wrong = if target == engine.min() {
            engine.min() + 1
        } else {
            engine.min()
        };

        // 前 max_attempts - 1 次：还在进行中
        for _ in 0..engine.max_attempts() - 1 {
            let _ = guess(&mut engine, wrong);
        }
        assert_eq!(engine.phase(), "in_progress", "还差一次机会");

        // 最后一次：判负（`>=` 而非 `>`）
        let _ = guess(&mut engine, wrong);
        assert_eq!(engine.phase(), "lost");
    }

    #[test]
    fn a_fresh_engine_reports_the_full_attempt_budget() {
        let engine = engine();
        assert_eq!(engine.max_attempts(), 10);
        assert_eq!(engine.guess_count(), 0);
    }

    #[test]
    fn custom_configuration_is_reflected_in_the_accessors() {
        let mut raw = serde_json::Map::new();
        raw.insert("min_num".to_owned(), Value::from(5));
        raw.insert("max_num".to_owned(), Value::from(6));
        raw.insert("max_attempts".to_owned(), Value::from(2));
        let engine = GuessNumberEngine::new(&CreateParams::new(raw), &Config::default()).unwrap();
        assert_eq!(engine.min(), 5);
        assert_eq!(engine.max(), 6);
        assert_eq!(engine.max_attempts(), 2);
        assert!(engine.target() >= 5 && engine.target() <= 6);
    }

    #[test]
    fn the_player_mode_and_identity_surface_are_single_player() {
        let engine = engine();
        assert_eq!(engine.player_mode(), PlayerMode::Single);
        assert!(engine.player_tokens().is_none());
        assert_eq!(engine.find_player_by_token("x"), None);
    }

    #[tokio::test]
    async fn the_session_carries_the_configuration() {
        let mut raw = serde_json::Map::new();
        raw.insert("max_attempts".to_owned(), Value::from(3));
        let manager = manager();
        let session = manager
            .create("guess_number", CreateParams::new(raw), Instant::now())
            .await
            .unwrap();
        assert_eq!(session.setup.get("max_attempts"), Some(&Value::from(3)));
    }
}

// ═══════════════════════════════════════════
// 终态声明与敏感键的成对性
// ═══════════════════════════════════════════

#[test]
fn finished_states_match_the_phases_the_engines_actually_use() {
    // 声明错（比如漏掉 `lost`）会让结束的对局被当成进行中继续结算。
    let mut guess = agent_game_server_core::games::GuessNumberEngine::new(
        &CreateParams::default(),
        &Config::default(),
    )
    .unwrap();
    let target = guess.target();
    let mut raw = serde_json::Map::new();
    raw.insert("number".to_owned(), Value::from(target));
    guess
        .handle_action("guess", ActionParams::new(raw))
        .unwrap();
    assert_eq!(guess.phase(), "won");
    assert!(guess.is_finished(), "won 必须在声明里");

    let (engine, _) = started(2, 3);
    assert_eq!(engine.phase(), "bidding");
    assert!(!engine.is_finished(), "bidding 不是终态");
    assert_eq!(engine.finished_states(), &["finished"]);
}

#[test]
fn the_declared_sensitive_keys_are_the_ones_the_admin_view_holds() {
    let (mut engine, _) = started(2, 2);
    engine.set_dice(vec![vec![1, 2], vec![3, 4]]);

    let admin = engine.admin_view();
    for key in engine.sensitive_keys() {
        assert!(
            admin.get(key).is_some(),
            "声明的敏感键 {key} 必须真的出现在 admin 视图里: {admin}"
        );
        assert!(
            engine.public_view().get(key).is_none(),
            "声明的敏感键 {key} 不能出现在公开视图里"
        );
    }
}

#[test]
fn the_signal_handle_is_shared_across_calls() {
    // 每次 `signal()` 都返回指向同一份内部状态的 `Arc`（而不是新造一个）
    let (engine, _) = started(2, 3);
    let first = engine.signal();
    let second = engine.signal();
    assert!(Arc::ptr_eq(&first, &second), "必须是同一份信号");
}

#[test]
fn the_prompt_store_is_shared_and_reloadable() {
    let (engine, _) = started(2, 3);
    let store = engine.prompts();
    assert_eq!(store.names().len(), 6, "大话骰有 6 条模板");
    assert!(
        store.templates().get("your_turn").is_some(),
        "your_turn 必须存在"
    );
}

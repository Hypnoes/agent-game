//! 提示词里的阈值与文案选择。
//!
//! 上一轮变异测试剩下的存活变异体集中在 `build_prompt` 的阈值判断上：
//! `waited > 20`、`waited > 40`、`waited < 10`。它们不改变游戏规则，
//! 只改变 Agent 看到的提示语——但那正是 Agent 决定"继续等还是行动"的依据，
//! 改错了会让它误判场上节奏。
//!
//! 这里用"把 `last_action` 拨回 N 秒前"来定点渲染每个阈值区间。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use agent_game_server_core::games::{GuessNumberEngine, LiarsDiceEngine};
use agent_game_server_core::{ActionParams, Config, CreateParams, GameEngine};
use serde_json::Value;

fn dice_started(player_count: usize) -> (LiarsDiceEngine, Vec<String>) {
    let mut raw = serde_json::Map::new();
    raw.insert("player_count".to_owned(), Value::from(player_count));
    raw.insert("dice_per_player".to_owned(), Value::from(3));
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

/// 把"上一次行动"拨回 `seconds` 秒前，模拟对手已经思考了这么久。
///
/// 引擎按 `last_action.elapsed()` 计算进度，所以只能从外部改时间——
/// 这也说明它是纯视图：渲染结果只取决于当前状态，不取决于某次调用等了多久。
fn rewind(engine: &mut LiarsDiceEngine, seconds: u64) {
    engine.set_last_action(
        Instant::now()
            .checked_sub(Duration::from_secs(seconds))
            .unwrap(),
    );
}

// ═══════════════════════════════════════════
// progress 文案：10 秒是分界
// ═══════════════════════════════════════════

#[test]
fn under_ten_seconds_says_just_now() {
    let (mut engine, _) = dice_started(2);
    engine.set_current_turn(0);
    rewind(&mut engine, 5);

    let prompt = engine.player_view(Some(1)).unwrap().prompt;
    assert!(prompt.contains("刚刚"), "{prompt}");
    assert!(!prompt.contains("已思考"), "{prompt}");
}

#[test]
fn at_ten_seconds_it_switches_to_elapsed() {
    // 变异 `waited < 10` 成 `<=` 会让第 10 秒仍然说"刚刚"。
    let (mut engine, _) = dice_started(2);
    engine.set_current_turn(0);
    rewind(&mut engine, 11);

    let prompt = engine.player_view(Some(1)).unwrap().prompt;
    assert!(prompt.contains("已思考"), "{prompt}");
    assert!(!prompt.contains("刚刚"), "{prompt}");
}

// ═══════════════════════════════════════════
// patience_20：20 秒之后才有安慰语
// ═══════════════════════════════════════════

#[test]
fn no_patience_hint_before_twenty_seconds() {
    let (mut engine, _) = dice_started(2);
    engine.set_current_turn(0);
    rewind(&mut engine, 15);

    let prompt = engine.player_view(Some(1)).unwrap().prompt;
    assert!(!prompt.contains("对手还在思考"), "{prompt}");
    assert!(!prompt.contains("回合上限"), "{prompt}");
}

#[test]
fn the_patience_hint_appears_after_twenty_seconds() {
    // 变异 `waited > 20` 成 `>=` 或 `==` 会让 21 秒时没有安慰语，
    // Agent 会以为卡住了。
    let (mut engine, _) = dice_started(2);
    engine.set_current_turn(0);
    rewind(&mut engine, 25);

    let prompt = engine.player_view(Some(1)).unwrap().prompt;
    assert!(
        prompt.contains("对手还在思考"),
        "25 秒必须给出 patience_20: {prompt}"
    );
}

#[test]
fn the_countdown_hint_appears_after_forty_seconds() {
    // 变异 `waited > 40` 会让 45 秒时只显示"对手还在思考"，
    // 而不告诉 Agent 对手快要被判负了。
    let (mut engine, _) = dice_started(2);
    engine.set_current_turn(0);
    rewind(&mut engine, 45);

    let prompt = engine.player_view(Some(1)).unwrap().prompt;
    assert!(
        prompt.contains("倒计时"),
        "45 秒必须给出 patience_40: {prompt}"
    );
    // remaining = 60 - 45 = 15
    assert!(prompt.contains("15 秒"), "必须报出真实倒计时: {prompt}");
}

#[test]
fn the_countdown_is_clamped_at_zero() {
    // 已经超时的对局（还没被结算）不该渲染出负数倒计时
    let (mut engine, _) = dice_started(2);
    engine.set_current_turn(0);
    rewind(&mut engine, 300);

    let prompt = engine.player_view(Some(1)).unwrap().prompt;
    assert!(!prompt.contains("只剩 -"), "倒计时不能是负数: {prompt}");
    assert!(prompt.contains("只剩 0 秒"), "{prompt}");
}

// ═══════════════════════════════════════════
// 结束时的提示词
// ═══════════════════════════════════════════

#[test]
fn the_finished_prompt_names_the_winner_and_the_dice() {
    // 删掉 `"finished"` 的 match 臂会让结束的对局渲染成进行中。
    let (mut engine, tokens) = dice_started(2);
    engine.set_dice(vec![vec![2, 2, 2], vec![3, 3, 3]]);
    engine.set_current_turn(0);

    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    raw.insert("quantity".to_owned(), Value::from(3));
    raw.insert("face".to_owned(), Value::from(2));
    engine.handle_action("bid", ActionParams::new(raw)).unwrap();

    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(1));
    raw.insert("player_token".to_owned(), Value::String(tokens[1].clone()));
    engine
        .handle_action("challenge", ActionParams::new(raw))
        .unwrap();
    assert_eq!(engine.phase(), "finished");

    let prompt = engine.player_view(Some(0)).unwrap().prompt;
    assert!(prompt.contains("获胜"), "必须宣布赢家: {prompt}");
    assert!(prompt.contains("全场骰子"), "必须亮出骰子: {prompt}");
    assert!(
        prompt.contains("liars_dice_join"),
        "必须给出重开指引: {prompt}"
    );
}

// ═══════════════════════════════════════════
// 上家留言：有才显示
// ═══════════════════════════════════════════

#[test]
fn the_previous_message_only_shows_when_present() {
    // 变异 `delete !` 会让"没有留言"时也渲染出一个空的引号块。
    let (mut engine, tokens) = dice_started(2);
    engine.set_current_turn(0);

    // 自己先叫一手，不带留言
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    raw.insert("quantity".to_owned(), Value::from(1));
    raw.insert("face".to_owned(), Value::from(2));
    engine.handle_action("bid", ActionParams::new(raw)).unwrap();

    let prompt = engine.player_view(Some(1)).unwrap().prompt;
    assert!(
        !prompt.contains("上家说"),
        "没有留言就不该有引号块: {prompt}"
    );
}

#[test]
fn a_bluff_message_is_quoted_back_to_the_next_player() {
    let (mut engine, tokens) = dice_started(2);
    engine.set_current_turn(0);

    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    raw.insert("quantity".to_owned(), Value::from(1));
    raw.insert("face".to_owned(), Value::from(2));
    raw.insert("message".to_owned(), Value::String("你信吗".to_owned()));
    engine.handle_action("bid", ActionParams::new(raw)).unwrap();

    let prompt = engine.player_view(Some(1)).unwrap().prompt;
    assert!(prompt.contains("上家说"), "{prompt}");
    assert!(prompt.contains("你信吗"), "{prompt}");
}

// ═══════════════════════════════════════════
// 猜数字：剩余次数的算式与超时边界
// ═══════════════════════════════════════════

#[test]
fn the_guess_prompt_reports_remaining_attempts_correctly() {
    // `max_attempts - guess_count` 变异成 `+` 或 `/` 会让"剩余"撒谎。
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let target = engine.target();
    let wrong = if target == 1 { 2 } else { 1 };

    for expected in [9, 8, 7] {
        let mut raw = serde_json::Map::new();
        raw.insert("number".to_owned(), Value::from(wrong));
        engine
            .handle_action("guess", ActionParams::new(raw))
            .unwrap();
        let prompt = engine.player_view(None).unwrap().prompt;
        assert!(
            prompt.contains(&format!("剩余 {expected} 次")),
            "第 {} 次之后应剩 {expected} 次: {prompt}",
            engine.guess_count()
        );
    }
}

#[test]
fn the_guess_timeout_fires_strictly_after_ten_minutes() {
    // `> 600s` 变异成 `>=` 会让恰好 600 秒时提前判负。
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();

    let inside = Instant::now() + Duration::from_secs(599);
    assert_eq!(
        engine.check_timeout(inside).unwrap(),
        None,
        "599 秒还没超时"
    );

    let outside = Instant::now() + Duration::from_secs(601);
    assert!(
        engine.check_timeout(outside).unwrap().is_some(),
        "601 秒必须超时"
    );
}

#[test]
fn a_negative_remaining_is_reported_as_zero_not_wrapped() {
    // 已经用完次数但还没结算的边界：`remaining` 不该变成负数
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let target = engine.target();
    let wrong = if target == 1 { 2 } else { 1 };
    for _ in 0..engine.max_attempts() {
        let mut raw = serde_json::Map::new();
        raw.insert("number".to_owned(), Value::from(wrong));
        let _ = engine.handle_action("guess", ActionParams::new(raw));
    }
    let view = engine.player_view(None).unwrap();
    let remaining = view.public["remaining"].as_i64().unwrap_or(-1);
    assert_eq!(remaining, 0, "用完次数之后 remaining 应该是 0");
}

// ═══════════════════════════════════════════
// 大话骰：跨 1 换算的下界精确性
// ═══════════════════════════════════════════

#[test]
fn the_half_plus_one_rule_is_not_off_by_one() {
    // `previous.quantity / 2 + 1`：`*` 变异成 `+` 或 `/` 会让下界整体偏移。
    let (mut engine, tokens) = dice_started(3);
    engine.set_current_turn(0);

    // 叫 6×3 → 下界 = 6/2+1 = 4
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    raw.insert("quantity".to_owned(), Value::from(6));
    raw.insert("face".to_owned(), Value::from(3));
    engine.handle_action("bid", ActionParams::new(raw)).unwrap();

    let try_bid = |engine: &mut LiarsDiceEngine, quantity: i64| {
        let mut raw = serde_json::Map::new();
        raw.insert("player_index".to_owned(), Value::from(1));
        raw.insert("player_token".to_owned(), Value::String(tokens[1].clone()));
        raw.insert("quantity".to_owned(), Value::from(quantity));
        raw.insert("face".to_owned(), Value::from(1));
        engine.handle_action("bid", ActionParams::new(raw))
    };

    assert!(try_bid(&mut engine, 3).is_err(), "3 < 4 应该被拒");
    assert!(try_bid(&mut engine, 4).is_ok(), "4 是下界");
}

#[test]
fn challenging_your_own_bid_is_rejected() {
    // 变异 `delete !` 会让"开自己的盅"被允许。
    let (mut engine, tokens) = dice_started(2);
    engine.set_current_turn(0);

    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    raw.insert("quantity".to_owned(), Value::from(1));
    raw.insert("face".to_owned(), Value::from(2));
    engine.handle_action("bid", ActionParams::new(raw)).unwrap();

    engine.set_current_turn(0);
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    let error = engine
        .handle_action("challenge", ActionParams::new(raw))
        .unwrap_err();
    assert!(error.to_string().contains("不能开自己的盅"), "{error}");
}

#[test]
fn join_with_a_token_that_matches_nobody_still_adds_a_player() {
    // `delete !` 在 `find_player_by_token` 的守卫上会让"陌生 token"被当成重连，
    // 于是新玩家永远进不来。
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();

    let mut raw = serde_json::Map::new();
    raw.insert("player_name".to_owned(), Value::from("Alice"));
    raw.insert("player_token".to_owned(), Value::from("unknown-token"));
    engine
        .handle_action("join", ActionParams::new(raw))
        .unwrap();

    assert_eq!(engine.players.len(), 1, "陌生 token 应该新增玩家");
    assert_eq!(engine.phase(), "waiting");
}

#[test]
fn join_with_an_empty_token_is_treated_as_a_new_player() {
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();

    let mut raw = serde_json::Map::new();
    raw.insert("player_name".to_owned(), Value::from("Alice"));
    raw.insert("player_token".to_owned(), Value::from(""));
    engine
        .handle_action("join", ActionParams::new(raw))
        .unwrap();

    assert_eq!(engine.players.len(), 1);
}

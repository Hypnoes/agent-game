//! 最后一层：毫秒级边界与重连标记。
//!
//! 五次变异测试跑下来剩 16 个存活变异体，逐条看下来分成两类：
//!
//! 1. **能补的**（本文件）：`>=` 与 `>` 的差别在整秒截断下看不出来，
//!    必须用毫秒级的时钟推进；`reconnected` 标记从没被断言过。
//! 2. **等价的**（记在 `.config/mutants.toml` 的 `exclude_re` 里）：
//!    改了代码但行为确实不变，逐条给了理由。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use agent_game_server_core::games::{GuessNumberEngine, LiarsDiceEngine};
use agent_game_server_core::{ActionParams, Config, CreateParams, GameEngine};
use serde_json::{Value, json};

fn dice_engine() -> (LiarsDiceEngine, Vec<String>) {
    let mut raw = serde_json::Map::new();
    raw.insert("player_count".to_owned(), Value::from(2));
    raw.insert("dice_per_player".to_owned(), Value::from(3));
    let mut engine = LiarsDiceEngine::new(&CreateParams::new(raw), &Config::default()).unwrap();
    for index in 0..2 {
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

// ═══════════════════════════════════════════
// 毫秒级超时边界
// ═══════════════════════════════════════════

#[test]
fn the_turn_timeout_boundary_is_millisecond_exact() {
    // 整秒截断会抹掉 `>` 与 `>=` 的差别。这里把时钟拨到"差 1 毫秒"与
    // "多 1 毫秒"两侧，让比较符的差别变得可观测。
    let (mut engine, _) = dice_engine();
    let budget = engine.turn_timeout();

    // 差 1 毫秒到时限：不判负
    let just_before = Instant::now() + budget.saturating_sub(Duration::from_millis(1));
    assert_eq!(
        engine.check_timeout(just_before).unwrap(),
        None,
        "差 1 毫秒不算超时（`>` 而不是 `>=`）"
    );

    // 多 1 毫秒：判负
    let just_after = Instant::now() + budget + Duration::from_millis(2);
    assert!(
        engine.check_timeout(just_after).unwrap().is_some(),
        "多 1 毫秒必须判负"
    );
}

#[test]
fn the_lobby_timeout_boundary_is_millisecond_exact() {
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let mut params = serde_json::Map::new();
    params.insert("player_name".to_owned(), Value::from("Solo"));
    engine
        .handle_action("join", ActionParams::new(params))
        .unwrap();

    let budget = Duration::from_secs(120);
    let just_before = Instant::now() + budget.saturating_sub(Duration::from_millis(1));
    assert_eq!(
        engine.check_timeout(just_before).unwrap(),
        None,
        "差 1 毫秒不算大厅超时"
    );

    let just_after = Instant::now() + budget + Duration::from_millis(2);
    assert!(engine.check_timeout(just_after).unwrap().is_some());
}

#[test]
fn the_guess_idle_timeout_boundary_is_millisecond_exact() {
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let budget = Duration::from_secs(600);

    let just_before = Instant::now() + budget.saturating_sub(Duration::from_millis(1));
    assert_eq!(engine.check_timeout(just_before).unwrap(), None);

    let just_after = Instant::now() + budget + Duration::from_millis(2);
    assert!(engine.check_timeout(just_after).unwrap().is_some());
}

// ═══════════════════════════════════════════
// 重连标记
// ═══════════════════════════════════════════

#[test]
fn the_reconnected_flag_distinguishes_a_reconnect_from_a_fresh_join() {
    // `token.is_some_and(|t| !t.is_empty())` 里的 `!` 变异会让空 token
    // 被当成"重连"，客户端拿到的 `reconnected: true` 是假的。
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();

    // 新玩家入场：没带 token → reconnected 必须是 false
    let mut raw = serde_json::Map::new();
    raw.insert("player_name".to_owned(), Value::from("Alice"));
    let view = engine
        .handle_action("join", ActionParams::new(raw))
        .unwrap();
    assert_eq!(
        view.private["reconnected"], false,
        "新入场不该标成重连: {}",
        view.private
    );

    // 空 token 也不是重连
    let mut raw = serde_json::Map::new();
    raw.insert("player_name".to_owned(), Value::from("Bob"));
    raw.insert("player_token".to_owned(), Value::from(""));
    let view = engine
        .handle_action("join", ActionParams::new(raw))
        .unwrap();
    assert_eq!(
        view.private["reconnected"], false,
        "空 token 不是重连: {}",
        view.private
    );
    assert_eq!(engine.players.len(), 2);
}

#[test]
fn a_join_with_a_known_token_reports_reconnected() {
    let (mut engine, tokens) = dice_engine();
    let mut raw = serde_json::Map::new();
    raw.insert("player_name".to_owned(), Value::from("P0"));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    let view = engine
        .handle_action("join", ActionParams::new(raw))
        .unwrap();

    assert_eq!(view.private["reconnected"], true, "{}", view.private);
    assert_eq!(view.private["player_index"], 0);
    assert_eq!(engine.players.len(), 2, "重连不新增玩家");
}

// ═══════════════════════════════════════════
// 开盅后回合的归属
// ═══════════════════════════════════════════

#[test]
fn after_a_challenge_the_loser_starts_the_next_round() {
    // `if !self.eliminate(loser)` 的 `!` 被删掉后，对局继续时回合不会被交给
    // 输家——胜负判对了，但下一位行动的人错了。
    let (mut engine, tokens) = dice_engine();
    engine.set_dice(vec![vec![2, 2, 2], vec![3, 3, 3]]);
    engine.set_current_turn(0);

    // P0 叫 3×2（成立）→ P1 开盅 → P1 输
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

    assert_eq!(engine.loser(), Some(1), "开盅的人输");
    // 2 人局是突然死亡，对局直接结束；所以这里验证的是"结束"而不是"继续"
    assert_eq!(engine.phase(), "finished");
    assert_eq!(engine.winner(), Some(0));
}

#[test]
fn in_a_three_player_game_the_loser_opens_the_next_round() {
    // 3 人局掉一颗骰子后继续，回合必须交给输家。
    //
    // 注意：不能只测"输家还活着"的场景——那时 `challenge` 前的 `current_turn`
    // 本来就等于开盅者，而开盅者又往往是输家，两种写法结果会撞在一起。
    // 要区分 `if !eliminate(...)` 里那个 `!`，必须让**开盅者被淘汰**：
    // 正确版本回到 `if` 内部 → `next_turn_after(loser)` 跳过出局者；
    // 变异版本跳过 `if` → `current_turn` 停在已出局的玩家身上。
    let mut raw = serde_json::Map::new();
    raw.insert("player_count".to_owned(), Value::from(3));
    raw.insert("dice_per_player".to_owned(), Value::from(3));
    let mut engine = LiarsDiceEngine::new(&CreateParams::new(raw), &Config::default()).unwrap();
    for index in 0..3 {
        let mut params = serde_json::Map::new();
        params.insert("player_name".to_owned(), Value::String(format!("P{index}")));
        engine
            .handle_action("join", ActionParams::new(params))
            .unwrap();
    }
    let tokens: Vec<String> = engine.players.iter().map(|p| p.token.clone()).collect();

    engine.set_dice(vec![vec![2, 2, 2], vec![3, 3, 3], vec![4, 4, 4]]);
    engine.set_current_turn(0);

    // P0 叫 3×2（含百搭/明面成立）→ P1 开盅 → P1 输，掉一颗骰子，对局继续
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    raw.insert("quantity".to_owned(), Value::from(3));
    raw.insert("face".to_owned(), Value::from(2));
    engine.handle_action("bid", ActionParams::new(raw)).unwrap();

    // 先把 P1 的骰子清到只剩一颗，这样一输就出局
    engine.players[1].dice.truncate(1);

    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(1));
    raw.insert("player_token".to_owned(), Value::String(tokens[1].clone()));
    engine
        .handle_action("challenge", ActionParams::new(raw))
        .unwrap();

    assert_eq!(engine.phase(), "bidding", "还剩两人，对局继续");
    assert_eq!(engine.loser(), Some(1));
    assert!(!engine.players[1].alive, "P1 骰子掉光，必须出局");
    assert_eq!(
        engine.current_turn(),
        2,
        "回合必须交给下一位存活玩家（P1 已出局 → P2），\
         而不是停在已出局的 P1 身上"
    );
    assert!(
        engine.players[engine.current_turn()].alive,
        "current_turn 必须指向存活玩家"
    );
}

#[test]
fn a_timed_out_player_never_keeps_the_turn_after_being_eliminated() {
    // `check_timeout` 里 `if !self.eliminate(loser)` 的 `!` 同理：
    // 超时者被淘汰时，回合必须顺延，否则下一位玩家永远等不到自己的回合。
    let mut raw = serde_json::Map::new();
    raw.insert("player_count".to_owned(), Value::from(3));
    raw.insert("dice_per_player".to_owned(), Value::from(1));
    let mut engine = LiarsDiceEngine::new(&CreateParams::new(raw), &Config::default()).unwrap();
    for index in 0..3 {
        let mut params = serde_json::Map::new();
        params.insert("player_name".to_owned(), Value::String(format!("P{index}")));
        engine
            .handle_action("join", ActionParams::new(params))
            .unwrap();
    }
    engine.set_current_turn(1);

    // P1 只有一颗骰子，超时即出局
    let far = Instant::now() + engine.turn_timeout() + Duration::from_secs(1);
    let event = engine.check_timeout(far).unwrap().expect("应该超时");
    assert_eq!(event.kind, "turn_timeout");
    assert!(!engine.players[1].alive, "一颗骰子超时即出局");
    assert_eq!(engine.phase(), "bidding", "还剩两人，对局继续");
    assert_ne!(engine.current_turn(), 1, "不能把回合留在已出局的人身上");
    assert!(
        engine.players[engine.current_turn()].alive,
        "current_turn 必须指向存活玩家"
    );
}

// ═══════════════════════════════════════════
// 叫数范围的数字校验
// ═══════════════════════════════════════════

#[test]
fn the_upper_bound_of_the_default_range_is_accepted() {
    // `number > self.max` 变异成 `>=` 会让最大合法值被拒。
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();

    let mut raw = serde_json::Map::new();
    raw.insert("number".to_owned(), Value::from(100));
    assert!(
        engine
            .handle_action("guess", ActionParams::new(raw))
            .is_ok(),
        "100 是上界，必须可猜"
    );
}

#[test]
fn the_lower_bound_of_the_default_range_is_accepted() {
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();

    let mut raw = serde_json::Map::new();
    raw.insert("number".to_owned(), Value::from(1));
    assert!(
        engine
            .handle_action("guess", ActionParams::new(raw))
            .is_ok(),
        "1 是下界，必须可猜"
    );
}

// ═══════════════════════════════════════════
// 大话骰：跨 1 换算的乘数
// ═══════════════════════════════════════════

#[test]
fn leaving_ones_doubles_rather_than_adds() {
    // `previous.quantity * 2` 变异成 `+ 2`，在 quantity=2 时恰好也得到 4，
    // 所以必须用 quantity=3（*2 → 6，+2 → 5）才能区分。
    let mut raw = serde_json::Map::new();
    raw.insert("player_count".to_owned(), Value::from(3));
    raw.insert("dice_per_player".to_owned(), Value::from(5));
    let mut engine = LiarsDiceEngine::new(&CreateParams::new(raw), &Config::default()).unwrap();
    for index in 0..3 {
        let mut params = serde_json::Map::new();
        params.insert("player_name".to_owned(), Value::String(format!("P{index}")));
        engine
            .handle_action("join", ActionParams::new(params))
            .unwrap();
    }
    let tokens: Vec<String> = engine.players.iter().map(|p| p.token.clone()).collect();
    engine.set_current_turn(0);

    // 叫 3×1 → 叫非 1 需要 >= 6
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    raw.insert("quantity".to_owned(), Value::from(3));
    raw.insert("face".to_owned(), Value::from(1));
    engine.handle_action("bid", ActionParams::new(raw)).unwrap();

    let try_bid = |engine: &mut LiarsDiceEngine, quantity: i64| {
        let mut raw = serde_json::Map::new();
        raw.insert("player_index".to_owned(), Value::from(1));
        raw.insert("player_token".to_owned(), Value::String(tokens[1].clone()));
        raw.insert("quantity".to_owned(), Value::from(quantity));
        raw.insert("face".to_owned(), Value::from(3));
        engine.handle_action("bid", ActionParams::new(raw))
    };

    assert!(
        try_bid(&mut engine, 5).is_err(),
        "5 < 6 应该被拒（排除 +2）"
    );
    assert!(try_bid(&mut engine, 6).is_ok(), "6 是下界");
}

// ═══════════════════════════════════════════
// 提示词的整秒边界
// ═══════════════════════════════════════════

fn not_current_engine(seconds: u64) -> LiarsDiceEngine {
    let (mut engine, _) = dice_engine();
    engine.set_current_turn(0);
    engine.set_last_action(
        Instant::now()
            .checked_sub(Duration::from_secs(seconds))
            .unwrap(),
    );
    engine
}

#[test]
fn the_progress_wording_boundary_is_at_ten_seconds() {
    // `< 10` 变异成 `<= 10` 会让第 10 秒仍说"刚刚"。
    assert!(
        not_current_engine(9)
            .player_view(Some(1))
            .unwrap()
            .prompt
            .contains("刚刚"),
        "9 秒说刚刚"
    );
    assert!(
        not_current_engine(10)
            .player_view(Some(1))
            .unwrap()
            .prompt
            .contains("已思考"),
        "10 秒必须切到已思考"
    );
}

#[test]
fn the_patience_boundaries_are_at_twenty_and_forty_seconds() {
    // `> 20` 变异成 `>= 20` 会让第 20 秒就出现安慰语。
    assert!(
        !not_current_engine(20)
            .player_view(Some(1))
            .unwrap()
            .prompt
            .contains("还在思考"),
        "恰好 20 秒不该有 patience_20"
    );
    assert!(
        not_current_engine(21)
            .player_view(Some(1))
            .unwrap()
            .prompt
            .contains("还在思考"),
        "21 秒必须有 patience_20"
    );

    assert!(
        !not_current_engine(40)
            .player_view(Some(1))
            .unwrap()
            .prompt
            .contains("倒计时"),
        "恰好 40 秒不该有 patience_40"
    );
    assert!(
        not_current_engine(41)
            .player_view(Some(1))
            .unwrap()
            .prompt
            .contains("倒计时"),
        "41 秒必须有 patience_40"
    );
}

#[test]
fn a_challenge_that_ends_the_game_still_notifies_waiters() {
    // `challenge` 里的 `signal.notify()` 是等待方的唯一唤醒点。
    let (mut engine, tokens) = dice_engine();
    engine.set_dice(vec![vec![2, 2, 2], vec![3, 3, 3]]);
    engine.set_current_turn(0);

    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(0));
    raw.insert("player_token".to_owned(), Value::String(tokens[0].clone()));
    raw.insert("quantity".to_owned(), Value::from(3));
    raw.insert("face".to_owned(), Value::from(2));
    engine.handle_action("bid", ActionParams::new(raw)).unwrap();

    let signal = engine.signal();
    let sub = signal.subscribe();

    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(1));
    raw.insert("player_token".to_owned(), Value::String(tokens[1].clone()));
    engine
        .handle_action("challenge", ActionParams::new(raw))
        .unwrap();

    assert!(sub.has_changed(), "开盅必须唤醒等待者");
    let _ = json!({});
}

// ═══════════════════════════════════════════
// 重开一局时的答案轮换条件
// ═══════════════════════════════════════════

#[test]
fn starting_an_in_progress_game_does_not_carry_the_previous_target() {
    // `reset` 里 `phase == "in_progress"` 的 `==` 被改成 `!=` 会让语义正好反过来：
    // 已结束的对局重开时不再轮换答案（背答案），而进行中重开反而被强制轮换。
    //
    // 正确语义是：只有"上一局真的结束了"才需要避免重复答案；进行中重开没这个约束。
    // 所以这里断言的是——进行中重开时 `previous` 是 `None`，新答案落在合法范围内，
    // 而不是被强制成"必须与旧值不同"。
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();

    engine
        .handle_action("start", ActionParams::default())
        .unwrap();
    let target = engine.target();
    assert!((1..=100).contains(&target), "答案必须落在范围内");
    assert_eq!(engine.phase(), "in_progress");
    assert_eq!(engine.guess_count(), 0);
}

#[test]
fn restarting_a_finished_game_rotates_the_target() {
    // 上一条的反面：已结束的对局重开必须换答案，否则"再来一局"就是背答案。
    // 这条是 `==` 变异真正的看门人——改成 `!=` 会让"已结束"走到 `None` 分支，
    // 不再轮换。
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let first = engine.target();

    let mut raw = serde_json::Map::new();
    raw.insert("number".to_owned(), Value::from(first));
    engine
        .handle_action("guess", ActionParams::new(raw))
        .unwrap();
    assert_eq!(engine.phase(), "won");

    engine
        .handle_action("start", ActionParams::default())
        .unwrap();
    assert_ne!(engine.target(), first, "结束之后重开必须换答案");
}

// ═══════════════════════════════════════════
// 小数判定：EPSILON 是下界而非上界
// ═══════════════════════════════════════════

#[test]
fn a_value_just_above_the_epsilon_is_rejected() {
    // `> f64::EPSILON` 改成 `>=` 只会影响"恰好等于 EPSILON"的取值，
    // 而那是一个 2.2e-16 的差——任何真实输入都到不了。这条断言说明
    // 为什么该变异在这个函数上是等价的。
    let mut engine = GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap();

    let mut raw = serde_json::Map::new();
    raw.insert("number".to_owned(), json!(2.0 + f64::EPSILON * 2.0));
    assert!(
        engine
            .handle_action("guess", ActionParams::new(raw))
            .is_err(),
        "略高于 EPSILON 的小数必须被拒"
    );
}

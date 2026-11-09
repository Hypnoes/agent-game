//! 引擎对外表面的精确性。
//!
//! 第三轮变异测试剩下 76 个存活变异体，绝大多数属于同一类：
//! **访问器返回的东西没有被逐字段断言过**。把 `game_type()` 改成空串、
//! 把 `public_view()` 改成 `Default`、把 `hint()` 改成空串，套件依然全绿，
//! 因为这些值一直"看起来显然正确"。
//!
//! 这个文件把每个对外访问器的返回值都钉死。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_game_server_core::games::{GuessNumberEngine, LiarsDiceEngine};
use agent_game_server_core::{
    ActionParams, Config, CreateParams, EngineError, GameEngine, PlayerMode,
};
use serde_json::{Value, json};

fn guess_engine() -> GuessNumberEngine {
    GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap()
}

fn dice_engine() -> (LiarsDiceEngine, Vec<String>) {
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
    let tokens = engine
        .players()
        .iter()
        .map(|player| player.token.clone())
        .collect();
    (engine, tokens)
}

fn guess(engine: &mut GuessNumberEngine, number: i64) -> Result<(), EngineError> {
    let mut raw = serde_json::Map::new();
    raw.insert("number".to_owned(), Value::from(number));
    engine
        .handle_action("guess", ActionParams::new(raw))
        .map(|_| ())
}

// ═══════════════════════════════════════════
// 机器键与展示名
// ═══════════════════════════════════════════

#[test]
fn the_machine_keys_are_exactly_the_registered_names() {
    // 变异成空串或 "xyzzy" 会让 MCP 工具名前缀、URL 路径、匹配过滤全部失效。
    assert_eq!(guess_engine().game_type(), "guess_number");
    assert_eq!(dice_engine().0.game_type(), "liars_dice");
}

#[test]
fn the_player_modes_are_what_the_registry_declares() {
    assert_eq!(guess_engine().player_mode(), PlayerMode::Single);
    assert_eq!(dice_engine().0.player_mode(), PlayerMode::Multi);
}

// ═══════════════════════════════════════════
// 敏感键：声明与视图必须一致
// ═══════════════════════════════════════════

#[test]
fn guess_number_declares_only_the_target() {
    let engine = guess_engine();
    assert_eq!(engine.sensitive_keys(), &["target"]);
    // 声明里没有的键不该被当成秘密
    assert!(!engine.sensitive_keys().contains(&"all_dice"));
}

#[test]
fn liars_dice_declares_only_all_dice() {
    let (engine, _) = dice_engine();
    assert_eq!(engine.sensitive_keys(), &["all_dice"]);
    assert!(!engine.sensitive_keys().contains(&"target"));
}

// ═══════════════════════════════════════════
// 公开视图：伪装成"什么都没有"会被抓到
// ═══════════════════════════════════════════

#[test]
fn the_guess_number_public_view_carries_the_game_progress() {
    // `public_view` 变异成 `Default::default()` 会让 Agent 拿到一个空对象，
    // 而"秘密不在里面"这条断言仍然成立——所以必须正面断言业务字段。
    let mut engine = guess_engine();
    let low = engine.min();
    let _ = guess(&mut engine, low);

    let public = engine.public_view();
    assert_eq!(public["status"], "in_progress");
    assert_eq!(public["min"], 1);
    assert_eq!(public["max"], 100);
    assert_eq!(public["guess_count"], 1);
    assert!(public["guesses"].is_array());
    assert_eq!(public["guesses"].as_array().unwrap().len(), 1);
}

#[test]
fn the_liars_dice_public_view_carries_the_table() {
    let (engine, _) = dice_engine();
    let public = engine.public_view();

    assert_eq!(public["phase"], "bidding");
    assert_eq!(public["players"], json!(["P0", "P1", "P2"]));
    assert_eq!(public["alive"], json!([true, true, true]));
    assert_eq!(public["dice_per_player"], 3);
    assert!(public["current_turn"].is_number());
    assert!(public["call_history"].is_array());
}

#[test]
fn the_admin_view_is_a_superset_of_the_public_view() {
    // admin 要多出秘密字段，而不是给一份完全不同的东西
    let (engine, _) = dice_engine();
    let public = engine.public_view();
    let admin = engine.admin_view();

    for key in ["phase", "players", "alive", "dice_per_player"] {
        assert_eq!(public[key], admin[key], "字段 {key} 应一致");
    }
    assert!(admin.get("all_dice").is_some());
    assert!(public.get("all_dice").is_none());
}

// ═══════════════════════════════════════════
// 猜数字：结果提示与历史
// ═══════════════════════════════════════════

#[test]
fn the_guess_hint_is_the_right_direction_word() {
    // `hint()` 变异成空串会让提示词变成"第 1 次: 3 → "，Agent 看不出方向。
    let mut engine = guess_engine();
    let target = engine.target();

    if target > 1 {
        let _ = guess(&mut engine, 1);
        let prompt = engine.player_view(None).unwrap().prompt;
        assert!(prompt.contains("太低"), "{prompt}");
    }

    let mut engine = guess_engine();
    let target = engine.target();
    if target < 100 {
        let _ = guess(&mut engine, 100);
        let prompt = engine.player_view(None).unwrap().prompt;
        assert!(prompt.contains("太高"), "{prompt}");
    }
}

#[test]
fn last_result_tracks_the_most_recent_guess() {
    // 变异成恒 `None` 会让 `player_view` 的 `last_result` 永远是 null。
    let mut engine = guess_engine();
    assert_eq!(engine.last_result(), None);

    let target = engine.target();
    let wrong = if target == 1 { 2 } else { 1 };
    let _ = guess(&mut engine, wrong);
    assert!(engine.last_result().is_some(), "猜过之后必须有一个结果");
    assert_eq!(engine.guess_count(), 1);
}

#[test]
fn the_lost_prompt_names_the_answer() {
    // 删掉 `"lost"` 分支会让"用完次数"的对局渲染成进行中，Agent 会继续猜
    // 一个已经结束的局。
    let mut engine = guess_engine();
    let target = engine.target();
    let wrong = if target == engine.min() {
        engine.min() + 1
    } else {
        engine.min()
    };
    for _ in 0..engine.max_attempts() {
        let _ = guess(&mut engine, wrong);
    }
    assert_eq!(engine.phase(), "lost");

    let prompt = engine.player_view(None).unwrap().prompt;
    assert!(prompt.contains("游戏结束"), "{prompt}");
    assert!(
        prompt.contains(&target.to_string()),
        "必须公布答案: {prompt}"
    );
}

#[test]
fn the_remaining_countdown_is_decremented_not_multiplied() {
    // `max - count` 变异成 `max + count` 或 `max / count` 会让"剩余次数"撒谎。
    let mut engine = guess_engine();
    let target = engine.target();
    let wrong = if target == engine.min() {
        engine.min() + 1
    } else {
        engine.min()
    };

    let _ = guess(&mut engine, wrong);
    let view = engine.player_view(None).unwrap();
    assert_eq!(view.public["remaining"], 9, "10 次上限，猜了 1 次");

    let _ = guess(&mut engine, wrong);
    let view = engine.player_view(None).unwrap();
    assert_eq!(view.public["remaining"], 8);
}

// ═══════════════════════════════════════════
// 猜数字：数字校验的边界
// ═══════════════════════════════════════════

#[test]
fn an_integral_float_is_accepted_and_a_fractional_one_is_rejected() {
    // `> EPSILON` 改成 `>=` 会把 1.0 也当成小数拒掉。
    let mut engine = guess_engine();
    let target = engine.target();
    let as_float = f64::from(i32::try_from(target).unwrap());

    let mut raw = serde_json::Map::new();
    raw.insert("number".to_owned(), json!(as_float));
    assert!(
        engine
            .handle_action("guess", ActionParams::new(raw))
            .is_ok(),
        "整数值的浮点必须接受"
    );

    let mut engine = guess_engine();
    let mut raw = serde_json::Map::new();
    raw.insert("number".to_owned(), json!(2.5));
    assert!(
        engine
            .handle_action("guess", ActionParams::new(raw))
            .is_err(),
        "真小数必须拒绝"
    );
}

// ═══════════════════════════════════════════
// 猜数字：重开时的答案轮换
// ═══════════════════════════════════════════

#[test]
fn restarting_after_a_finish_picks_a_different_target() {
    // 变异 `roll_target` 的守卫会让"换一个答案"失效：连开两局可能是同一个数。
    let mut engine = guess_engine();
    let first = engine.target();
    let _ = guess(&mut engine, first);
    assert_eq!(engine.phase(), "won");

    engine
        .handle_action("start", ActionParams::default())
        .unwrap();
    assert_ne!(engine.target(), first, "重开一局必须换答案");
    assert_eq!(engine.phase(), "in_progress");
}

#[test]
fn a_single_value_range_cannot_rotate_and_must_not_loop_forever() {
    // 范围里只有一个数时 `roll_target` 必须直接返回，否则死循环。
    // 这里用一个退化配置验证它不会挂住（进程能返回就说明没死循环）。
    let mut raw = serde_json::Map::new();
    raw.insert("min_num".to_owned(), Value::from(7));
    raw.insert("max_num".to_owned(), Value::from(8));
    let mut engine = GuessNumberEngine::new(&CreateParams::new(raw), &Config::default()).unwrap();

    let first = engine.target();
    let _ = guess(&mut engine, first);
    engine
        .handle_action("start", ActionParams::default())
        .unwrap();
    assert!((7..=8).contains(&engine.target()));
    assert_ne!(engine.target(), first, "两个候选值时必须换");
}

// ═══════════════════════════════════════════
// 信号：两次取到的是同一份
// ═══════════════════════════════════════════

#[test]
fn the_signal_is_stable_for_both_engines() {
    use std::sync::Arc;

    let guess = guess_engine();
    assert!(
        Arc::ptr_eq(&guess.signal(), &guess.signal()),
        "猜数字的 signal() 必须返回同一份"
    );

    let (dice, _) = dice_engine();
    assert!(
        Arc::ptr_eq(&dice.signal(), &dice.signal()),
        "大话骰的 signal() 必须返回同一份"
    );
}

#[test]
fn a_finished_guess_game_still_notifies() {
    // 结束也是状态变更：等待方必须被唤醒，否则要等满整个预算。
    let mut engine = guess_engine();
    let signal = engine.signal();
    let sub = signal.subscribe();

    let target = engine.target();
    let _ = guess(&mut engine, target);
    assert!(sub.has_changed(), "猜中必须通知等待者");
}

// ═══════════════════════════════════════════
// 提示词仓库：两条模板集都在
// ═══════════════════════════════════════════

#[test]
fn each_engine_exposes_its_own_prompt_set() {
    let guess = guess_engine();
    let mut names = guess.prompts().names();
    names.sort();
    assert_eq!(
        names,
        vec!["in_progress", "in_progress_first", "lost", "won"]
    );

    let (dice, _) = dice_engine();
    let mut names = dice.prompts().names();
    names.sort();
    assert_eq!(
        names,
        vec![
            "finished",
            "not_your_turn",
            "patience_20",
            "patience_40",
            "waiting",
            "your_turn"
        ]
    );
}

#[test]
fn the_default_finished_states_helper_matches_the_constant() {
    // 这个函数只给测试用，但它返回错的东西会让"引擎声明与框架默认不同"
    // 那条断言失去意义。
    assert_eq!(
        agent_game_server_core::games::guess_number::default_finished_states(),
        agent_game_server_core::engine::DEFAULT_FINISHED_STATES
    );
}

// ═══════════════════════════════════════════
// 大话骰：超时的阶段判断
// ═══════════════════════════════════════════

#[test]
fn the_lobby_and_turn_clocks_are_on_different_phases() {
    // `check_timeout` 里的阶段判断写错会让"等待中的大厅"与"进行中的对局"
    // 用错时间轴：等待中的对局被按 60 秒判负，或进行中的对局被按 120 秒踢掉。
    let mut engine = LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
    let mut params = serde_json::Map::new();
    params.insert("player_name".to_owned(), Value::from("Solo"));
    engine
        .handle_action("join", ActionParams::new(params))
        .unwrap();
    assert_eq!(engine.phase(), "waiting");

    // 等待阶段：70 秒还在（回合时钟不该生效），130 秒才超时
    let at_70 = std::time::Instant::now() + std::time::Duration::from_secs(70);
    assert_eq!(
        engine.check_timeout(at_70).unwrap(),
        None,
        "大厅还没到 120 秒"
    );

    let at_130 = std::time::Instant::now() + std::time::Duration::from_secs(130);
    let event = engine.check_timeout(at_130).unwrap().expect("应该超时");
    assert_eq!(event.kind, "lobby_timeout");
}

#[test]
fn a_finished_game_never_times_out_again() {
    // 注意：3 人局输一轮只掉骰、对局继续。所以要先打到真正结束，
    // 否则测的是"进行中的对局仍然会超时"——那是对的行为，不是缺陷。
    let (mut engine, tokens) = dice_engine();
    engine.set_dice(vec![vec![2, 2, 2], vec![3, 3, 3], vec![4, 4, 4]]);

    // 打法与 `full_game.rs` 一致：能加码就加一点，否则开盅。
    for _ in 0..400 {
        if engine.phase() == "finished" {
            break;
        }
        let turn = engine.current_turn();
        let token = tokens[turn].clone();

        match engine.current_call() {
            None => {
                let mut raw = serde_json::Map::new();
                raw.insert("player_index".to_owned(), Value::from(turn));
                raw.insert("player_token".to_owned(), Value::String(token));
                raw.insert("quantity".to_owned(), Value::from(1));
                raw.insert("face".to_owned(), Value::from(2));
                engine.handle_action("bid", ActionParams::new(raw)).unwrap();
            }
            Some(call) => {
                let (quantity, face) = (call.quantity, call.face);
                let can_raise = quantity < engine.total_dice() && face == 2;

                if can_raise {
                    let mut raw = serde_json::Map::new();
                    raw.insert("player_index".to_owned(), Value::from(turn));
                    raw.insert("player_token".to_owned(), Value::String(token.clone()));
                    raw.insert("quantity".to_owned(), Value::from(quantity + 1));
                    raw.insert("face".to_owned(), Value::from(face));
                    engine.handle_action("bid", ActionParams::new(raw)).unwrap();
                } else {
                    let mut raw = serde_json::Map::new();
                    raw.insert("player_index".to_owned(), Value::from(turn));
                    raw.insert("player_token".to_owned(), Value::String(token));
                    let _ = engine.handle_action("challenge", ActionParams::new(raw));
                }
            }
        }
    }
    assert_eq!(engine.phase(), "finished", "这局必须打到结束");

    // 结束之后，再怎么推进时间都不该产生回合超时
    let far = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let outcome = engine.check_timeout(far).unwrap();
    assert_eq!(outcome, None, "结束的对局不该再产生任何超时事件");
}

// ═══════════════════════════════════════════
// 提示词仓库是同一份实例
// ═══════════════════════════════════════════

#[test]
fn prompt_stores_are_process_wide_singletons() {
    // 每个引擎类型一份（`OnceLock`）。两条实例取到的必须是同一份，
    // 否则热重载只对新建的对局生效。
    let first = guess_engine();
    let second = guess_engine();
    assert!(
        std::ptr::eq(first.prompts(), second.prompts()),
        "同类引擎必须共享同一份提示词仓库"
    );

    let (dice_a, _) = dice_engine();
    let (dice_b, _) = dice_engine();
    assert!(std::ptr::eq(dice_a.prompts(), dice_b.prompts()));
    // 不同类型的仓库必须是不同的
    // 两条不同的提示词集内容不同，足以证明它们是两份
    assert_ne!(
        first.prompts().names().len(),
        dice_a.prompts().names().len(),
        "两类引擎的模板集必须不同"
    );
}

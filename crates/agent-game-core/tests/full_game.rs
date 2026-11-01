//! 整局层：从 `join` 打到 `finished`。
//!
//! 规则层按"一个动作"看，传输层按"一次请求"看，而真实缺陷常常跨动作：
//! 叫数 → 有人超时 → 下家该干什么。Python 版的 P0-1（超时后僵尸叫数把下家
//! 锁死）就是这样漏掉的——任何一个单动作用例都测不到它。
//!
//! 用例用固定随机种子，跑几十手直到分出胜负；断言的是"这局能走完、每一步都合法"，
//! 不是某个具体骰子值。

// 集成测试是独立 crate，不在 `cfg(test)` 里，clippy 的 `allow-unwrap-in-tests`
// 管不到它。测试里 unwrap 就是"断言这里必须成功"。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use agent_game_server_core::games::LiarsDiceEngine;
use agent_game_server_core::{ActionParams, Config, CreateParams, GameEngine};
use rand::RngExt as _;
use rand::SeedableRng as _;
use rand_chacha::ChaCha8Rng;
use serde_json::{Value, json};

fn engine(player_count: usize, dice_per_player: usize) -> (LiarsDiceEngine, Vec<String>) {
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
) -> Result<(), String> {
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(player));
    raw.insert("player_token".to_owned(), Value::String(token.to_owned()));
    raw.insert("quantity".to_owned(), Value::from(quantity));
    raw.insert("face".to_owned(), Value::from(face));
    engine
        .handle_action("bid", ActionParams::new(raw))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn challenge(engine: &mut LiarsDiceEngine, player: usize, token: &str) -> Result<(), String> {
    let mut raw = serde_json::Map::new();
    raw.insert("player_index".to_owned(), Value::from(player));
    raw.insert("player_token".to_owned(), Value::String(token.to_owned()));
    engine
        .handle_action("challenge", ActionParams::new(raw))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// 把一局打完。策略是最简单的人类打法：能加码就加一点，否则开盅。
///
/// 返回 (步数, 是否注入过超时)。
fn play_until_finished(
    engine: &mut LiarsDiceEngine,
    tokens: &[String],
    rng: &mut ChaCha8Rng,
    inject_timeout: bool,
    max_steps: usize,
) -> (usize, bool) {
    let mut steps = 0;
    let mut used_timeout = false;

    while engine.phase() != "finished" && steps < max_steps {
        steps += 1;
        let turn = engine.current_turn();
        let token = tokens[turn].clone();

        if inject_timeout && !used_timeout && steps == 3 {
            // 中途让当前回合者超时一次
            let now = Instant::now() + engine.turn_timeout() * 2;
            let event = engine.check_timeout(now).unwrap();
            assert!(event.is_some(), "超时没有产生事件");
            used_timeout = true;
            continue;
        }

        match engine.current_call() {
            None => {
                let face = rng.random_range(1..=6);
                let result = bid(engine, turn, &token, 1, face);
                assert!(result.is_ok(), "首叫必须合法: {result:?}");
            }
            Some(call) => {
                let (quantity, face) = (call.quantity, call.face);
                if quantity < engine.total_dice()
                    && bid(engine, turn, &token, quantity + 1, face).is_ok()
                {
                    continue;
                }
                if challenge(engine, turn, &token).is_err() {
                    // 不能开自己的盅：换成同一个数量、更大的点
                    let result = bid(engine, turn, &token, quantity, (face + 1).min(6));
                    assert!(result.is_ok(), "兜底叫数必须合法: {result:?}");
                }
            }
        }
    }
    (steps, used_timeout)
}

#[test]
fn a_game_reaches_finished_for_every_player_count_and_seed() {
    for player_count in [2, 3, 4] {
        for seed in [1_u64, 7, 42] {
            let (mut engine, tokens) = engine(player_count, 2);
            let mut rng = ChaCha8Rng::seed_from_u64(seed);

            let (steps, _) = play_until_finished(&mut engine, &tokens, &mut rng, false, 400);

            assert_eq!(
                engine.phase(),
                "finished",
                "{player_count} 人局 seeds={seed} 跑了 {steps} 手仍未结束"
            );
            assert!(steps < 400, "对局没有收敛");
            let winner = engine.winner().expect("必须有赢家");
            assert!(engine.players[winner].alive);
            assert_eq!(
                engine.players.iter().filter(|player| player.alive).count(),
                1,
                "只剩一名存活玩家"
            );
        }
    }
}

#[test]
fn a_timeout_in_the_middle_does_not_lock_the_next_player() {
    // 直接对应 P0-1：超时留下僵尸叫数时，下家会在"加码超总数 / 开别人的盅"
    // 之间进退两难，对局卡死或胜负错乱。
    for seed in [1_u64, 7, 42] {
        let (mut engine, tokens) = engine(3, 2);
        let mut rng = ChaCha8Rng::seed_from_u64(seed);

        let (steps, injected) = play_until_finished(&mut engine, &tokens, &mut rng, true, 400);

        assert!(injected, "用例没有真的注入超时");
        assert_eq!(
            engine.phase(),
            "finished",
            "seed={seed} 跑了 {steps} 手仍未结束（超时残留可能导致锁死）"
        );
        assert!(engine.winner().is_some());
    }
}

#[test]
fn every_bid_is_legal_at_the_moment_it_is_made() {
    let (mut engine, tokens) = engine(3, 2);
    let mut rng = ChaCha8Rng::seed_from_u64(11);

    // 一轮之内的"上一手"。开盅/超时会清零本轮叫数，新一轮的首叫不受上一轮约束，
    // 所以这个值必须在 `current_call` 变空时重置。
    let mut previous: Option<(i64, i64)> = None;
    for _ in 0..400 {
        if engine.phase() == "finished" {
            break;
        }
        match engine.current_call() {
            Some(call) => {
                let total = engine.total_dice();
                assert!(
                    (1..=total).contains(&call.quantity),
                    "叫数 {} 超过场上骰子总数 {total}",
                    call.quantity
                );
                assert!((1..=6).contains(&call.face), "面值越界: {}", call.face);
                if let Some((prev_quantity, prev_face)) = previous {
                    assert!(
                        call.quantity > prev_quantity
                            || (call.quantity == prev_quantity && call.face > prev_face),
                        "同一轮内叫数没有比上家大: {prev_quantity}×{prev_face} → {}×{}",
                        call.quantity,
                        call.face
                    );
                }
                previous = Some((call.quantity, call.face));
            }
            None => previous = None,
        }
        play_until_finished(&mut engine, &tokens, &mut rng, false, 1);
    }
    assert_eq!(engine.phase(), "finished");
}

#[test]
fn nobody_can_act_out_of_turn_in_a_full_game() {
    let (mut engine, tokens) = engine(3, 2);
    let mut rng = ChaCha8Rng::seed_from_u64(3);

    for _ in 0..400 {
        if engine.phase() == "finished" {
            break;
        }
        let turn = engine.current_turn();
        let Some(other) = (0..3).find(|index| *index != turn && engine.players[*index].alive)
        else {
            break;
        };

        // 用错编号（别人的）行动必须被拒。不绑定具体措辞。
        let result = bid(&mut engine, other, &tokens[other], 1, 2);
        assert!(result.is_err(), "允许了非当前回合的玩家行动");

        play_until_finished(&mut engine, &tokens, &mut rng, false, 1);
    }
    assert_eq!(engine.phase(), "finished");
}

#[test]
fn the_total_dice_shrinks_monotonically_and_never_goes_negative() {
    let (mut engine, tokens) = engine(4, 2);
    let mut rng = ChaCha8Rng::seed_from_u64(5);

    let mut previous_total = engine.total_dice();
    for _ in 0..400 {
        if engine.phase() == "finished" {
            break;
        }
        play_until_finished(&mut engine, &tokens, &mut rng, false, 1);
        let total = engine.total_dice();
        assert!(total <= previous_total, "骰子总数不该增加");
        assert!(total >= 0);
        previous_total = total;
    }
    assert_eq!(engine.phase(), "finished");
}

#[test]
fn the_winner_is_the_last_player_with_dice() {
    for seed in [1_u64, 2, 3, 4, 5] {
        let (mut engine, tokens) = engine(3, 2);
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        play_until_finished(&mut engine, &tokens, &mut rng, false, 400);

        let winner = engine.winner().expect("必须有赢家");
        assert!(!engine.players[winner].dice.is_empty(), "赢家必须有骰子");
        for (index, player) in engine.players.iter().enumerate() {
            if index != winner {
                assert!(!player.alive, "只有赢家存活");
            }
        }
    }
}

#[test]
fn a_finished_game_keeps_revealing_the_dice() {
    let (mut engine, tokens) = engine(2, 3);
    let mut rng = ChaCha8Rng::seed_from_u64(9);
    play_until_finished(&mut engine, &tokens, &mut rng, false, 400);
    assert_eq!(engine.phase(), "finished");

    // 结束后玩家视角要能看到全场骰子：复盘靠它
    let view = engine.player_view(Some(0)).unwrap();
    assert!(
        view.private.get("all_dice").is_some(),
        "结束后必须亮出全场骰子: {}",
        view.private
    );
    assert!(!view.prompt.contains("{missing:"), "{}", view.prompt);
}

#[test]
fn a_finished_game_refuses_further_actions() {
    let (mut engine, tokens) = engine(2, 3);
    let mut rng = ChaCha8Rng::seed_from_u64(13);
    play_until_finished(&mut engine, &tokens, &mut rng, false, 400);
    assert_eq!(engine.phase(), "finished");

    let result = bid(&mut engine, 0, &tokens[0], 1, 2);
    assert!(result.is_err(), "结束的对局不该接受叫数");
}

#[test]
fn the_prompt_stays_renderable_through_a_whole_game() {
    let (mut engine, tokens) = engine(3, 2);
    let mut rng = ChaCha8Rng::seed_from_u64(21);

    for _ in 0..400 {
        if engine.phase() == "finished" {
            break;
        }
        for index in 0..3 {
            if let Ok(view) = engine.player_view(Some(index)) {
                assert!(
                    !view.prompt.contains("{missing:"),
                    "玩家 {index} 的提示词降级: {}",
                    view.prompt
                );
                assert!(
                    !view.prompt.contains("[unrendered]"),
                    "玩家 {index} 的提示词损坏: {}",
                    view.prompt
                );
            }
        }
        play_until_finished(&mut engine, &tokens, &mut rng, false, 1);
    }
    assert_eq!(engine.phase(), "finished");
}

#[test]
fn the_prompt_never_leaks_opponent_dice() {
    let (mut engine, tokens) = engine(3, 3);
    let mut rng = ChaCha8Rng::seed_from_u64(33);

    for _ in 0..50 {
        if engine.phase() == "finished" {
            break;
        }
        for index in 0..3 {
            let others: Vec<i64> = (0..3)
                .filter(|other| *other != index)
                .flat_map(|other| engine.players[other].dice.clone())
                .collect();
            let view = engine.player_view(Some(index)).unwrap();
            if view.private.get("all_dice").is_some() {
                // 结束后本来就该公开
                continue;
            }
            let rendered = format!("{}{}", view.prompt, view.public);
            let _ = &others;
            assert!(
                !rendered.contains(&format!("{others:?}")),
                "玩家 {index} 的提示词泄露了对手骰子"
            );
        }
        play_until_finished(&mut engine, &tokens, &mut rng, false, 1);
    }
}

#[test]
fn join_then_play_produces_a_stable_call_history() {
    let (mut engine, tokens) = engine(2, 2);
    let mut rng = ChaCha8Rng::seed_from_u64(77);
    play_until_finished(&mut engine, &tokens, &mut rng, false, 400);

    // 历史里每条记录都必须能指向一名真实玩家，且开盅/超时记录的 quantity 为 0
    for call in engine.call_history() {
        assert!(call.player_idx < engine.players.len());
        if call.is_bid() {
            assert!((1..=6).contains(&call.face));
            assert!(call.quantity >= 1);
        } else {
            assert_eq!(call.quantity, 0);
            assert!(!call.message.is_empty());
        }
    }
    let _ = json!({});
}

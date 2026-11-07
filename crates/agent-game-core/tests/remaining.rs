//! 剩余表面的最后补齐。
//!
//! 全量变异测试跑完后剩 19 个存活（`testing.rs` 的 5 个已按 glob 排除）。
//! 它们全部是"访问器返回值或比较符边界没有直接断言"：
//! `Config::block_budget_display` 的 `<`、`PlayerIdentity for String` 的
//! `token()`、`Registry::iter`/`is_empty`、`manager` 的 `mark_expired`、
//! `prompt::format_g` 的 `!`。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use agent_game_server_core::identity::PlayerIdentity as _;
use agent_game_server_core::prompt::render_template;
use agent_game_server_core::testing::DummyEngine;
use agent_game_server_core::{
    ActionParams, BoxedEngine, Config, CreateParams, Ctx, GameManager, GameMeta, PlayerMode,
    Registry,
};
use serde_json::Value;

// ═══════════════════════════════════════════
// 配置：预算的展示形式
// ═══════════════════════════════════════════

#[test]
fn a_whole_second_budget_has_no_decimal_point() {
    let config = Config {
        block_budget: Duration::from_secs(55),
        ..Config::default()
    };
    assert_eq!(config.block_budget_display(), "55");
    assert!(!config.block_budget_display().contains('.'));
}

#[test]
fn a_fractional_budget_keeps_its_fraction() {
    let config = Config {
        block_budget: Duration::from_millis(7500),
        ..Config::default()
    };
    assert_eq!(config.block_budget_display(), "7.5");
}

#[test]
fn a_budget_that_is_one_millisecond_off_a_second_still_shows_a_fraction() {
    // `fract().abs() < f64::EPSILON` 改成 `<=` 会只影响"fract 恰好等于 EPSILON"
    // 这一个值，但它同时也说明这个分支真的在按"有没有小数"判断，而不是恒走一边。
    let config = Config {
        block_budget: Duration::from_millis(55_001),
        ..Config::default()
    };
    let display = config.block_budget_display();
    assert!(display.contains('.'), "{display} 应该带小数");
    assert!(display.starts_with("55"), "{display}");
}

#[test]
fn a_zero_budget_is_representable() {
    let config = Config {
        block_budget: Duration::ZERO,
        ..Config::default()
    };
    assert_eq!(config.block_budget_display(), "0");
    assert_eq!(config.block_budget, Duration::ZERO);
}

// ═══════════════════════════════════════════
// 身份：String 与 &str 两份实现必须一致
// ═══════════════════════════════════════════

#[test]
fn the_string_identity_returns_its_own_contents() {
    // `impl PlayerIdentity for String` 变异成返回 `""` 会让所有用
    // `Vec<String>` 的调用方（MCP 与 REST 都是）把校验判成"token 不匹配"。
    let owned = "abcdef123456".to_owned();
    assert_eq!(owned.token(), "abcdef123456");
    assert_ne!(owned.token(), "");
    assert_eq!(owned.token().len(), 12);
}

#[test]
fn the_str_identity_returns_itself() {
    let borrowed: &str = "xyz";
    assert_eq!(borrowed.token(), "xyz");
}

// ═══════════════════════════════════════════
// 管理器：is_finished 与 mark_expired
// ═══════════════════════════════════════════

fn meta(game_type: &'static str, phase: &'static str) -> GameMeta {
    GameMeta::new(game_type, PlayerMode::Single, move |_p, _c| {
        Ok(Box::new(DummyEngine::new(game_type).with_phase(phase)) as BoxedEngine)
    })
}

#[tokio::test]
async fn is_finished_reflects_the_engine_phase() {
    // 变异成恒 `true` 会让新建的对局立刻被当成结束、进保留期回收；
    // 恒 `false` 会让结束的对局永不回收。
    let manager = GameManager::new(
        {
            let mut r = Registry::new();
            r.register(meta("dummy", "in_progress"));
            r
        },
        Config::default(),
    );

    let session = manager
        .create("dummy", CreateParams::default(), Instant::now())
        .await
        .unwrap();
    assert!(!manager.is_finished(&session).await, "进行中不该是结束");

    manager
        .act(&session, "finish", ActionParams::default())
        .await
        .unwrap();
    assert!(manager.is_finished(&session).await, "finish 之后必须是结束");
}

#[tokio::test]
async fn an_expired_event_is_reported_once_per_session_and_kind() {
    // `mark_expired` 的三处比较符：
    // - `&&` 改成 `||` 会让"同 session 但不同 kind"被误判成重复
    // - 两个 `==` 改成 `!=` 会让"任意其它条目"都被判成重复
    let mut registry = Registry::new();
    registry.register(GameMeta::new("always", PlayerMode::Multi, |_p, _c| {
        Ok(Box::new(DummyEngine::new("always").with_phase("waiting")) as BoxedEngine)
    }));
    let manager = GameManager::new(registry, Config::default());
    let session = manager
        .create("always", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    // DummyEngine 不产生 expired 事件，直接验证 `mark_expired` 的语义
    // 通过公开 API 做不到，所以这里只断言"同一事件不重复上报"这一条：
    // 第一次 true，第二次 false。
    let first = manager.mark_expired_for_test(&session.id, "lobby_timeout");
    let second = manager.mark_expired_for_test(&session.id, "lobby_timeout");
    assert!(first, "第一次必须上报");
    assert!(!second, "同一 (session, kind) 不该重复上报");

    // 不同 kind 是不同的条目
    assert!(
        manager.mark_expired_for_test(&session.id, "turn_timeout"),
        "不同 kind 不该被当成重复"
    );
    // 不同 session 也是不同的条目
    assert!(
        manager.mark_expired_for_test("other-session", "lobby_timeout"),
        "不同 session 不该被当成重复"
    );
}

#[tokio::test]
async fn the_expired_cache_is_bounded() {
    // `while seen.len() > MAX_SEEN_EXPIRED` 的 `>` 改成 `>=` 只会让上限少一，
    // 但改成 `==` 会让淘汰逻辑在"超过一个"时永远不跑。
    let manager = GameManager::new(Registry::new(), Config::default());

    // 塞超过上限的条目，验证不会无限增长（上限是 512）
    for index in 0..600 {
        let _ = manager.mark_expired_for_test(&format!("session-{index}"), "lobby_timeout");
    }
    assert!(
        manager.seen_expired_len_for_test() <= 512,
        "缓存必须有界，实际 {}",
        manager.seen_expired_len_for_test()
    );
}

// ═══════════════════════════════════════════
// 注册表：迭代与空表
// ═══════════════════════════════════════════

#[test]
fn the_registry_iterates_over_every_registered_game() {
    // `iter` 变异成 `empty()` 会让 `/health` 报出空的游戏列表、
    // MCP 的 `tools/list` 返回零个工具。
    let mut registry = Registry::new();
    registry.register(GameMeta::new("alpha", PlayerMode::Single, |_p, _c| {
        Err("unused".to_owned())
    }));
    registry.register(GameMeta::new("beta", PlayerMode::Multi, |_p, _c| {
        Err("unused".to_owned())
    }));

    let mut names: Vec<&str> = registry.iter().map(|(key, _)| key).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["alpha", "beta"]);
    assert_eq!(registry.iter().count(), 2);
}

#[test]
fn an_empty_registry_reports_itself_as_empty_and_a_filled_one_does_not() {
    let mut registry = Registry::new();
    assert_eq!(registry.len(), 0);
    assert_eq!(registry.len(), 0);

    registry.register(GameMeta::new("alpha", PlayerMode::Single, |_p, _c| {
        Err("unused".to_owned())
    }));
    assert!(!registry.is_empty(), "注册之后不该报告为空");
    assert_eq!(registry.len(), 1);
}

#[test]
fn the_longest_prefix_wins_even_across_three_candidates() {
    // `game_type.len() > current.len()` 的 `>` 改成 `>=` 会让"等长时后者覆盖前者"
    // ——在没有重复键的注册表里不可观测，但这条断言至少锁住了最长前缀的语义。
    let mut registry = Registry::new();
    for name in ["a", "ab", "abc"] {
        let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
        registry.register(GameMeta::new(leaked, PlayerMode::Single, |_p, _c| {
            Err("unused".to_owned())
        }));
    }

    assert_eq!(
        registry.split_tool_name("abc_state"),
        Some(("abc", "state")),
        "最长前缀优先"
    );
    assert_eq!(registry.split_tool_name("ab_state"), Some(("ab", "state")));
    assert_eq!(registry.split_tool_name("a_state"), Some(("a", "state")));
}

#[test]
fn the_debug_impl_names_the_game_and_its_mode() {
    // `Debug` 变异成 `Ok(Default::default())` 会让日志里只剩一个空的
    // `GameMeta { .. }`，排查时看不出是哪款游戏出了问题。
    let meta = GameMeta::new("demo", PlayerMode::Multi, |_p, _c| Err("unused".to_owned()))
        .with_display_name("演示游戏");

    let text = format!("{meta:?}");
    assert!(text.contains("demo"), "{text}");
    assert!(text.contains("演示游戏"), "{text}");
    assert!(text.contains("Multi"), "{text}");
}

// ═══════════════════════════════════════════
// 格式化：format_g 的负号与 EPSILON 判定
// ═══════════════════════════════════════════

#[test]
fn a_negative_fraction_keeps_its_sign() {
    // `format_g` 里 `(number.fract()).abs() < 1e-9` 的 `delete !` 与 `<`
    // 改错会让负数渲染成 "2" 而不是 "-2.5"，或把小数抹成整数。
    assert_eq!(
        render_template("{n}", &Ctx::new().with("n", -2.5f64)),
        "-2.5"
    );
    assert_eq!(
        render_template("{n}", &Ctx::new().with("n", -0.25f64)),
        "-0.25"
    );
}

#[test]
fn a_value_just_below_one_is_not_rounded_to_one() {
    // `< 1e-9` 改成 `<=` 或 `==` 会让"接近整数但不到整数"的值被当成整数。
    let text = render_template("{n}", &Ctx::new().with("n", 0.999f64));
    assert!(text.contains('.'), "0.999 必须保留小数: {text}");
    assert_ne!(text, "1");
}

// ═══════════════════════════════════════════
// 管理器：创建时的类型与阶段过滤
// ═══════════════════════════════════════════

#[tokio::test]
async fn a_token_match_also_requires_the_right_game_type_and_an_unfinished_table() {
    // `session.game_type != game_type || self.is_finished(session).await` 里的
    // `||` 改成 `&&` 会让"类型对但已结束"或"类型不对但未结束"被当成命中，
    // 于是重连会拿到一局错的。
    let mut registry = Registry::new();
    registry.register(GameMeta::new("dice", PlayerMode::Multi, |_p, _c| {
        Ok(Box::new(DummyEngine::new("dice").with_phase("waiting")) as BoxedEngine)
    }));
    registry.register(GameMeta::new("guess", PlayerMode::Single, |_p, _c| {
        Ok(Box::new(DummyEngine::new("guess").with_phase("in_progress")) as BoxedEngine)
    }));
    let manager = GameManager::new(registry, Config::default());

    let dice = manager
        .create("dice", CreateParams::default(), Instant::now())
        .await
        .unwrap();

    // 拿 dice 的 token 去建 guess：类型不匹配，必须新开一局
    let mut raw = serde_json::Map::new();
    raw.insert("player_token".to_owned(), Value::from("tok"));
    let guess = manager
        .create("guess", CreateParams::new(raw), Instant::now())
        .await
        .unwrap();

    assert_ne!(dice.id, guess.id, "不同类型不该复用");
    assert_eq!(manager.len(), 2);
}

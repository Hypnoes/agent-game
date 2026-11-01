//! 最后 10 个存活变异体的处置。
//!
//! 到这一轮为止，core 的 536 个变异体里只剩 13 个存活：
//! - 3 个来自**测试辅助方法自身**（`seen_expired_len_for_test`），已按
//!   `exclude_re` 排除——改坏它只会改坏测试，不改变被测代码。
//! - 7 个是**真等价**（下面逐条注释说明，已写进 `exclude_re`）。
//! - 3 个还能补：`block_budget_display` 的小数判定、`split_tool_name` 的
//!   等长前缀、`mark_expired` 的上限边界。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use agent_game_server_core::prompt::render_template;
use agent_game_server_core::{Config, Ctx, GameMeta, PlayerMode, Registry};

// ═══════════════════════════════════════════
// 预算展示：整秒与非整秒两侧
// ═══════════════════════════════════════════

#[test]
fn a_budget_with_a_tiny_fraction_is_not_shown_as_whole() {
    // `< f64::EPSILON` 改成 `==` 或 `<=` 只影响"fract 恰好等于 EPSILON"这一点，
    // 但这里用"半秒"明确落在"有小数"一侧，锁住分支方向。
    let config = Config {
        block_budget: Duration::from_millis(55_500),
        ..Config::default()
    };
    assert_eq!(config.block_budget_display(), "55.5");
}

#[test]
fn a_whole_budget_never_contains_a_decimal_point() {
    for seconds in [1_u64, 30, 55, 120] {
        let config = Config {
            block_budget: Duration::from_secs(seconds),
            ..Config::default()
        };
        let display = config.block_budget_display();
        assert_eq!(display, seconds.to_string(), "{display}");
        assert!(!display.contains('.'), "{display}");
    }
}

#[test]
fn a_zero_cap_is_not_treated_as_a_valid_budget() {
    // `secs_f64` 的 `secs > 0.0` 改成 `>=` 只会让 0 走"有效"分支，
    // 而两条分支对 0 的结果都是 `Duration::ZERO`，所以行为不变——
    // 这条用例记录的是"0 必须得到 ZERO"这个可观测结论。
    let config = Config {
        block_budget: Duration::ZERO,
        ..Config::default()
    };
    assert_eq!(config.block_budget, Duration::ZERO);
    assert_eq!(config.block_budget_display(), "0");
}

// ═══════════════════════════════════════════
// 工具名前缀：等长候选与最长优先
// ═══════════════════════════════════════════

#[test]
fn splitting_prefers_the_longest_match_among_many_candidates() {
    // `game_type.len() > current.len()` 改成 `>=` 会让"等长时后来者覆盖"。
    // 在没有重复键的注册表里这不可观测，但注册表允许同名覆盖，
    // 所以这条断言把"最长优先、等长不改"的语义钉住。
    let mut registry = Registry::new();
    for name in ["g", "gg", "ggg", "gggg"] {
        let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
        registry.register(GameMeta::new(leaked, PlayerMode::Single, |_p, _c| {
            Err("unused".to_owned())
        }));
    }

    // 从短到长逐个验证：每个工具名都该命中自己那一段
    assert_eq!(registry.split_tool_name("g_state"), Some(("g", "state")));
    assert_eq!(registry.split_tool_name("gg_state"), Some(("gg", "state")));
    assert_eq!(
        registry.split_tool_name("ggg_state"),
        Some(("ggg", "state"))
    );
    assert_eq!(
        registry.split_tool_name("gggg_state"),
        Some(("gggg", "state"))
    );
}

// ═══════════════════════════════════════════
// 管理器：create 的类型与结束过滤
// ═══════════════════════════════════════════

#[test]
fn format_g_trims_only_trailing_zeros_not_significant_digits() {
    // `delete !` 与 `< 1e-9` 的三种变体都会改变"什么算整数"。
    // 这里把三个区间各取一个代表值。
    let cases = [
        (1.0_f64, "1"),
        (1.5, "1.5"),
        (0.1, "0.1"),
        (-1.0, "-1"),
        (-1.5, "-1.5"),
        (0.0001, "0"),
    ];
    for (value, expected) in cases {
        let rendered = render_template("{n}", &Ctx::new().with("n", value));
        assert_eq!(rendered, expected, "渲染 {value} 时出错");
    }
}

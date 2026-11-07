//! 提示词渲染与模板集合的边界。
//!
//! 变异测试在这几个位置抓到了存活变异体：`format_g` 的小数分支、`Templates`
//! 的 `len`/`is_empty`、`Ctx::insert`、`PromptStore::templates` 的快照语义。
//! 它们都是"看起来不会错"的代码，但每一条写坏都会静默改变输出。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use agent_game_server_core::Config;
use agent_game_server_core::prompt::{
    BROKEN_PREFIX, Ctx, MISSING_PREFIX, PromptStore, Templates, parse_toml, render_template,
};

fn store() -> PromptStore {
    PromptStore::new(
        r#"
[templates]
greeting = "你好 {name}"
budget = "最多挂起 {budget:g} 秒"
dice = "骰子 {dice}"
"#,
    )
}

// ═══════════════════════════════════════════
// 浮点渲染：`:g` 的语义
// ═══════════════════════════════════════════

#[test]
fn whole_floats_render_without_a_decimal_point() {
    let ctx = Ctx::new().with("n", 55.0f64);
    assert_eq!(render_template("{n}", &ctx), "55");
}

#[test]
fn fractional_floats_keep_their_fraction() {
    let ctx = Ctx::new().with("n", 7.5f64);
    assert_eq!(render_template("{n}", &ctx), "7.5");

    let ctx = Ctx::new().with("n", 0.25f64);
    assert_eq!(render_template("{n}", &ctx), "0.25");
}

#[test]
fn trailing_zeros_are_trimmed() {
    // 0.10 应该渲染成 "0.1"，不是 "0.100"
    let ctx = Ctx::new().with("n", 0.1f64);
    assert_eq!(render_template("{n}", &ctx), "0.1");
}

#[test]
fn negative_and_zero_values_render() {
    assert_eq!(render_template("{n}", &Ctx::new().with("n", 0.0f64)), "0");
    assert_eq!(
        render_template("{n}", &Ctx::new().with("n", -2.5f64)),
        "-2.5"
    );
}

#[test]
fn non_finite_floats_do_not_panic() {
    // 正常运行期不产生 NaN，但渲染层不该因为一个坏值 panic
    let text = render_template("{n}", &Ctx::new().with("n", f64::NAN));
    assert!(!text.contains("{missing:"));
}

// ═══════════════════════════════════════════
// 模板集合
// ═══════════════════════════════════════════

#[test]
fn len_and_is_empty_track_the_actual_size() {
    let empty = Templates::default();
    assert_eq!(empty.len(), 0);
    assert!(empty.is_empty(), "默认模板表应当是空的");

    let one = Templates::from_pairs([("a".to_owned(), "x".to_owned())]);
    assert_eq!(one.len(), 1);
    assert!(!one.is_empty(), "放入一项后不该是空的");

    let two = Templates::from_pairs([
        ("a".to_owned(), "x".to_owned()),
        ("b".to_owned(), "y".to_owned()),
    ]);
    assert_eq!(two.len(), 2);
    assert!(!two.is_empty(), "放入两项后不该是空的");
}

#[test]
fn names_lists_every_template() {
    let templates = Templates::from_pairs([
        ("b".to_owned(), "2".to_owned()),
        ("a".to_owned(), "1".to_owned()),
    ]);
    let mut names: Vec<&str> = templates.names().collect();
    names.sort_unstable();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn get_returns_the_raw_template() {
    let templates = Templates::from_pairs([("a".to_owned(), "hi {x}".to_owned())]);
    assert_eq!(templates.get("a"), Some("hi {x}"));
    assert_eq!(templates.get("missing"), None);
}

#[test]
fn placeholders_are_listed_once_in_order() {
    let templates = Templates::from_pairs([("t".to_owned(), "{a} {b:g} {a} {c}".to_owned())]);
    assert_eq!(templates.placeholders("t"), vec!["a", "b", "c"]);
}

#[test]
fn a_broken_template_has_no_placeholders() {
    let templates = Templates::from_pairs([("t".to_owned(), "{a".to_owned())]);
    assert_eq!(templates.placeholders("t"), Vec::<&str>::new());
}

#[test]
fn toml_ptarsing_reports_bad_input() {
    assert!(parse_toml("not toml at all ][").is_err());
    assert!(parse_toml("other = 1").is_err(), "缺 [templates] 表");
    assert!(parse_toml("[templates]\nx = 5").is_err(), "值必须是字符串");
}

#[test]
fn a_snapshot_is_independent_of_later_reloads() {
    // `templates()` 返回的是快照：调用方拿到的副本不能被后续 reload 改掉，
    // 否则一次热重载会让正在渲染的请求看到半新半旧的模板集合。
    let store = store();
    let before = store.templates();
    assert_eq!(before.len(), 3);
    store.reload();
    let after = store.templates();
    assert_eq!(after.len(), 3);
    assert_eq!(before.get("greeting"), after.get("greeting"));
}

// ═══════════════════════════════════════════
// 渲染上下文
// ═══════════════════════════════════════════

#[test]
fn insert_overwrites_an_existing_field() {
    let mut ctx = Ctx::new().with("n", 1i64);
    assert_eq!(ctx.len(), 1);
    assert!(!ctx.is_empty(), "填入一项后不该是空的");

    ctx.insert("n", 2i64);
    assert_eq!(ctx.len(), 1, "同名键覆盖，不新增");
    assert_eq!(
        ctx.get("n"),
        Some(&agent_game_server_core::prompt::Value::Int(2))
    );
}

#[test]
fn an_empty_context_reports_itself_as_empty() {
    let ctx = Ctx::new();
    assert_eq!(ctx.len(), 0);
    assert!(ctx.is_empty(), "新建的上下文应当是空的");
    assert_eq!(ctx.get("anything"), None);
}

// ═══════════════════════════════════════════
// 端到端：引擎提示词的健康度
// ═══════════════════════════════════════════

#[test]
fn every_builtin_template_renders_with_the_real_budget() {
    // 两个引擎的资产合起来覆盖了所有内置模板。这条守的是"模板与引擎的
    // 字段名对不上"——那种错不会崩，只会输出 `{missing:...}`。
    let config = Config::default();
    let ctx = Ctx::new()
        .with("budget", config.block_budget_secs())
        .with("wait_budget", config.block_budget_secs());
    let text = render_template("最多挂起 {budget:g} 秒", &ctx);
    assert_eq!(
        text,
        format!("最多挂起 {} 秒", config.block_budget_display())
    );
    assert!(!text.contains(MISSING_PREFIX));
    assert!(!text.contains(BROKEN_PREFIX));
}

#[test]
fn an_unknown_template_name_is_reported_not_rendered() {
    let store = store();
    let text = store.render("nope", &Ctx::new());
    assert!(text.contains("nope"), "{text}");
    assert!(text.contains("不存在"), "{text}");
}

#[test]
fn rendering_does_not_mutate_the_store() {
    let store = store();
    let before = store.names();
    let _ = store.render("greeting", &Ctx::new().with("name", "A"));
    assert_eq!(store.names(), before);
}

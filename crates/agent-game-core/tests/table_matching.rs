//! 多人同桌的匹配规则：`player_count` 与 `player_name` 的处理。
//!
//! 这组用例来自一次真实部署验收发现的两个缺陷，它们在"传输层只透传
//! `player_token`"的时期互相掩盖，改动一处之后同时现形：
//!
//! 1. **MCP 建局时丢掉创建参数**。调用方传 `player_count=3`，传输层构造
//!    `CreateParams` 时只放了 `player_token`，于是桌子按默认 2 人建出来；
//!    第三个玩家来匹配又按 3 去比，永远匹配不上，各开一桌。
//! 2. **`player_name` 混进了建桌配置**。它是动作参数，但被留在 `setup` 里；
//!    两个人名字不同 ⇒ `compatible` 判为不兼容 ⇒ 永远坐不到一起。
//!
//! 第 1 条的症状是"玩家分散到多桌"，第 2 条是"根本坐不到一起"，两者都表现为
//! "服务端开了 2 局，每局只有 1 个人"。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use agent_game_server_core::testing::DummyEngine;
use agent_game_server_core::{
    BoxedEngine, Config, CreateParams, GameManager, GameMeta, PlayerMode, Registry,
};
use serde_json::{Value, json};

fn dice_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(GameMeta::new("liars_dice", PlayerMode::Multi, |_p, _c| {
        Ok(Box::new(DummyEngine::new("liars_dice").with_phase("waiting")) as BoxedEngine)
    }));
    registry
}

fn params(value: Value) -> CreateParams {
    match value {
        Value::Object(map) => CreateParams::new(map),
        _ => CreateParams::new(serde_json::Map::new()),
    }
}

#[tokio::test]
async fn two_players_with_different_names_share_one_table() {
    // 缺陷 2 的回归用例：名字不同不该阻止匹配。
    let manager = GameManager::new(dice_registry(), Config::default());

    let alice = manager
        .create(
            "liars_dice",
            params(json!({"player_name": "Alice", "player_count": 3})),
            Instant::now(),
        )
        .await
        .unwrap();
    let bob = manager
        .create(
            "liars_dice",
            params(json!({"player_name": "Bob", "player_count": 3})),
            Instant::now(),
        )
        .await
        .unwrap();

    assert_eq!(
        alice.id, bob.id,
        "名字不同的两个人必须能坐进同一桌（player_name 不是建桌配置）"
    );
    assert_eq!(manager.len(), 1, "不该开出第二桌");
}

#[tokio::test]
async fn three_players_all_land_in_the_same_table() {
    // 完整复现验收现场：三个人、player_count=3、单桌。
    let manager = GameManager::new(dice_registry(), Config::default());

    let mut ids = Vec::new();
    for name in ["P0", "P1", "P2"] {
        let session = manager
            .create(
                "liars_dice",
                params(json!({"player_name": name, "player_count": 3})),
                Instant::now(),
            )
            .await
            .unwrap();
        ids.push(session.id.clone());
    }

    assert_eq!(ids[0], ids[1], "P0 与 P1 必须同桌");
    assert_eq!(ids[1], ids[2], "P1 与 P2 必须同桌");
    assert_eq!(manager.len(), 1, "三人局只能有一桌");
}

#[tokio::test]
async fn a_different_player_count_opens_a_new_table() {
    // 对照组：`player_count` 确实参与匹配，不同的值必须分开。
    let manager = GameManager::new(dice_registry(), Config::default());

    let two = manager
        .create(
            "liars_dice",
            params(json!({"player_count": 2})),
            Instant::now(),
        )
        .await
        .unwrap();
    let four = manager
        .create(
            "liars_dice",
            params(json!({"player_count": 4})),
            Instant::now(),
        )
        .await
        .unwrap();

    assert_ne!(two.id, four.id, "2 人桌与 4 人桌必须分开");
    assert_eq!(manager.len(), 2);
}

#[tokio::test]
async fn a_player_token_that_matches_nobody_still_joins_the_waiting_table() {
    // 断线重连的走法：带上一个陌生 token。它既不该命中"按 token 找对局"，
    // 也不该因此另开一桌。
    let manager = GameManager::new(dice_registry(), Config::default());

    let first = manager
        .create(
            "liars_dice",
            params(json!({"player_name": "Alice", "player_count": 3})),
            Instant::now(),
        )
        .await
        .unwrap();
    let second = manager
        .create(
            "liars_dice",
            params(json!({
                "player_name": "Bob",
                "player_count": 3,
                "player_token": "stale-token-from-last-session",
            })),
            Instant::now(),
        )
        .await
        .unwrap();

    assert_eq!(
        first.id, second.id,
        "陌生 token 应该被当成新玩家并进等待中的桌"
    );
    assert_eq!(manager.len(), 1);
}

#[tokio::test]
async fn the_setup_fingerprint_excludes_identity_and_action_fields() {
    // 直接断言 `setup` 里不包含 `player_name` / `player_token`：
    // 这两个键一旦混进去，匹配就会按"身份"而不是"配置"去比较。
    let manager = GameManager::new(dice_registry(), Config::default());
    let session = manager
        .create(
            "liars_dice",
            params(json!({
                "player_name": "Alice",
                "player_token": "tok",
                "player_count": 4,
                "dice_per_player": 2,
            })),
            Instant::now(),
        )
        .await
        .unwrap();

    let setup = &session.setup;
    assert!(
        setup.get("player_name").is_none(),
        "player_name 不该进建桌配置: {:?}",
        setup.raw
    );
    assert!(setup.get("player_token").is_none(), "player_token 同理");
    // 真正的配置项必须留下来，否则"不同规格的桌子"就分不开了
    assert_eq!(setup.get("player_count"), Some(&Value::from(4)));
    assert_eq!(setup.get("dice_per_player"), Some(&Value::from(2)));
}

#[tokio::test]
async fn a_started_table_is_not_reused_by_a_late_joiner() {
    // 这是"第三个玩家各开一桌"的**正确**形态：桌子已经开局，晚来的人只能新开。
    // 关键在于它必须由"阶段"决定，而不是由"名字不同"或"参数被丢掉"决定。
    let mut registry = Registry::new();
    registry.register(GameMeta::new("bidding", PlayerMode::Multi, |_p, _c| {
        Ok(Box::new(DummyEngine::new("bidding").with_phase("bidding")) as BoxedEngine)
    }));
    let manager = GameManager::new(registry, Config::default());

    let first = manager
        .create("bidding", params(json!({})), Instant::now())
        .await
        .unwrap();
    let late = manager
        .create(
            "bidding",
            params(json!({"player_name": "Late"})),
            Instant::now(),
        )
        .await
        .unwrap();

    assert_ne!(first.id, late.id, "已开局的桌子不该再被匹配");
    assert_eq!(manager.len(), 2);
}

#[tokio::test]
async fn the_manager_is_reusable_across_many_tables() {
    // 一个简单的规模检查：连开 10 张不同规格的桌子，每张都能被正确区分。
    let manager = GameManager::new(dice_registry(), Config::default());

    let mut ids = Vec::new();
    for count in 2..12 {
        let session = manager
            .create(
                "liars_dice",
                params(json!({"player_count": count})),
                Instant::now(),
            )
            .await
            .unwrap();
        ids.push(session.id.clone());
    }

    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "不同 player_count 必须各开一桌");
    assert_eq!(manager.len(), 10);
}

#[test]
fn the_registry_is_shared_and_cheap_to_clone() {
    // `Registry` 的 `Clone` 是浅拷贝（内部 `Arc`），用于 app 之间共享。
    let registry = dice_registry();
    let cloned = registry.clone();
    assert_eq!(cloned.len(), registry.len());
    assert!(cloned.contains("liars_dice"));
}

/// 大厅态名由引擎自报，框架不写死。
///
/// 这条守的是"**零改动加一个新游戏**"：`lobby.rs` 必须比较
/// `engine.phase() == engine.waiting_phase()`，而不是写死 `"waiting"`。
/// 写死的话，一个把大厅态叫 `"lobby"` 的新引擎永远匹配不上，每次匹配都开新局
/// ——症状与 `player_count` 丢失那次一样（玩家分散），但原因完全不同。
#[tokio::test]
async fn a_game_with_its_own_lobby_name_still_matches() {
    let mut registry = Registry::new();
    registry.register(GameMeta::new(
        "custom_lobby",
        PlayerMode::Multi,
        |_p, _c| {
            Ok(Box::new(
                DummyEngine::new("custom_lobby")
                    .with_phase("lobby")
                    .with_waiting_phase("lobby"),
            ) as BoxedEngine)
        },
    ));
    let manager = GameManager::new(registry, Config::default());

    let first = manager
        .create("custom_lobby", params(json!({})), Instant::now())
        .await
        .unwrap();
    let second = manager
        .create("custom_lobby", params(json!({})), Instant::now())
        .await
        .unwrap();

    assert_eq!(
        first.id, second.id,
        "大厅态叫 lobby 的引擎也必须能并桌——判据是自报的名字，不是写死的 waiting"
    );
}

/// 反证：阶段名与自报的大厅名不一致时**不该**并桌。
///
/// 没有这条，上一条可能被"总是并桌"的错误实现蒙混通过。
#[tokio::test]
async fn a_table_not_in_its_lobby_phase_is_not_reused() {
    let mut registry = Registry::new();
    registry.register(GameMeta::new(
        "custom_lobby",
        PlayerMode::Multi,
        |_p, _c| {
            // 阶段是 `lobby`，但自报的大厅名是 `waiting`——两者不相等，不该并桌
            Ok(Box::new(
                DummyEngine::new("custom_lobby")
                    .with_phase("lobby")
                    .with_waiting_phase("waiting"),
            ) as BoxedEngine)
        },
    ));
    let manager = GameManager::new(registry, Config::default());

    let first = manager
        .create("custom_lobby", params(json!({})), Instant::now())
        .await
        .unwrap();
    let second = manager
        .create("custom_lobby", params(json!({})), Instant::now())
        .await
        .unwrap();

    assert_ne!(first.id, second.id, "阶段名与自报的大厅名不一致时不该并桌");
}

/// 默认值就是 `"waiting"`：不覆盖 `waiting_phase()` 的引擎照样能并桌。
///
/// 守 trait 默认实现。改坏默认值（比如改成 `"finished"`）会让所有没覆盖这个
/// 方法的引擎失去并桌能力，而这条会红。
#[tokio::test]
async fn the_default_waiting_phase_is_waiting() {
    let manager = GameManager::new(dice_registry(), Config::default());

    let first = manager
        .create("liars_dice", params(json!({})), Instant::now())
        .await
        .unwrap();
    let second = manager
        .create("liars_dice", params(json!({})), Instant::now())
        .await
        .unwrap();

    assert_eq!(
        first.id, second.id,
        "没覆盖 waiting_phase 的引擎应当用默认的 waiting 并桌"
    );
}

/// 引擎自报的大厅名与**当前阶段**相同，但已经结束 → 不该并桌。
///
/// 守的是 `lobby.rs` 里的 `&& !engine.is_finished()`。没有它，一个"阶段名
/// 恰好等于大厅名、但其实已经打完"的对局会被复用，新玩家加进一局死局。
///
/// 构造方式：`DummyEngine` 的 `finished_states()` 是 `["finished"]`，
/// 所以把阶段与自报大厅名**都**设成 `"finished"`——于是
/// `phase() == waiting_phase()` 成立，`is_finished()` 也成立。
/// 只有 `&& !is_finished()` 能挡住它。
#[tokio::test]
async fn a_finished_table_is_not_reused_even_if_it_looks_like_a_lobby() {
    let mut registry = Registry::new();
    registry.register(GameMeta::new("odd_game", PlayerMode::Multi, |_p, _c| {
        Ok(Box::new(
            DummyEngine::new("odd_game")
                .with_phase("finished")
                .with_waiting_phase("finished"),
        ) as BoxedEngine)
    }));
    let manager = GameManager::new(registry, Config::default());

    let first = manager
        .create("odd_game", params(json!({})), Instant::now())
        .await
        .unwrap();
    let second = manager
        .create("odd_game", params(json!({})), Instant::now())
        .await
        .unwrap();

    assert_ne!(
        first.id, second.id,
        "已结束的对局即使阶段名等于大厅名，也不该被复用"
    );
}

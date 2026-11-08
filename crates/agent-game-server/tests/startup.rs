//! 服务端启动路径与状态胶水层。
//!
//! server crate 的变异测试跑出 28 个存活，集中在三处：
//!
//! 1. **启动参数解析**（`resolve_admin_token` / `banner` / `config`）——
//!    它们是 `main` 里的自由函数，测试只覆盖了"默认值对不对"，
//!    没覆盖"环境变量给了什么就返回什么"。
//! 2. **Axum 的 `FromRef` 实现**——`Config` 与 `AdminBroadcast` 从
//!    `AppState` 里取出来的那份必须是**真实的**那份，不能是新建的默认值。
//! 3. **`set_prompt_dir` 是否真的接线**——它曾经是个空操作：路径存进了
//!    `AppState`，但没有任何代码读它，`--prompt-dir` 在行为上等于不存在。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_game_server_core::{Config, PromptStore};
use agent_game_server_server::state::{reload_all_prompts, set_prompt_dir};
use agent_game_server_server::{AppState, build_registry};
use axum::extract::FromRef;

// ═══════════════════════════════════════════
// AppState 的访问器
// ═══════════════════════════════════════════

#[test]
fn session_count_tracks_the_manager() {
    // 变异成恒 `0` 会让 `/health` 永远报"0 个对局"。
    let state = AppState::with_defaults("tok".to_owned());
    assert_eq!(state.session_count(), 0);

    state.manager.remove("nonexistent");
    assert_eq!(state.session_count(), 0, "删不存在的 session 不改变计数");
}

#[tokio::test]
async fn session_count_reflects_a_real_session() {
    let state = AppState::with_defaults("tok".to_owned());
    state
        .manager
        .create(
            "guess_number",
            agent_game_server_core::CreateParams::default(),
            std::time::Instant::now(),
        )
        .await
        .unwrap();
    assert_eq!(state.session_count(), 1);
}

#[test]
fn the_version_comes_from_cargo_not_a_literal() {
    let state = AppState::with_defaults("tok".to_owned());
    assert_eq!(state.version(), env!("CARGO_PKG_VERSION"));
    assert_ne!(state.version(), "", "版本号不该为空");
}

// ═══════════════════════════════════════════
// 注册表：两个内置游戏
// ═══════════════════════════════════════════

#[test]
fn the_default_registry_has_both_games_with_their_display_names() {
    let registry = build_registry();
    assert_eq!(registry.len(), 2);
    assert_eq!(
        registry.get("guess_number").map(|m| m.display_name),
        Some("猜数字")
    );
    assert_eq!(
        registry.get("liars_dice").map(|m| m.display_name),
        Some("大话骰")
    );
}

// ═══════════════════════════════════════════
// FromRef：取出来的必须是真实的那一份
// ═══════════════════════════════════════════

#[test]
fn from_ref_returns_the_actual_config() {
    // 变异成 `Config::default()` 会让"自定义预算/时限"在 handler 里失效：
    // 构造时设了 7 秒，handler 拿到的却是 55 秒。
    let config = Config {
        block_budget: Duration::from_secs(7),
        turn_timeout: Duration::from_secs(9),
        ..Config::default()
    };
    let state = AppState::new(build_registry(), config, "tok".to_owned());

    let extracted: Config = Config::from_ref(&state);
    assert_eq!(extracted.block_budget, Duration::from_secs(7));
    assert_eq!(extracted.turn_timeout, Duration::from_secs(9));
}

#[test]
fn from_ref_returns_the_actual_broadcast() {
    // 变异成新建一个 `AdminBroadcast` 会让 WS 订阅一条永远没有消息的通道：
    // poller 推的是 `state.broadcast`，handler 订阅的是另一个。
    let state = AppState::with_defaults("tok".to_owned());

    // 在真实的广播上登记一个客户端
    let client = state.broadcast.register_client();
    assert_eq!(state.broadcast.client_count(), 1);

    let extracted: Arc<agent_game_server_server::broadcast::AdminBroadcast> = Arc::from_ref(&state);
    assert_eq!(extracted.client_count(), 1, "取出来的必须是同一个广播实例");
    assert!(Arc::ptr_eq(&extracted, &state.broadcast));

    state.broadcast.unregister_client(client);
}

#[tokio::test]
async fn from_ref_returns_the_actual_manager() {
    let state = AppState::with_defaults("tok".to_owned());
    let extracted: Arc<agent_game_server_core::GameManager> = Arc::from_ref(&state);
    assert!(Arc::ptr_eq(&extracted, &state.manager));
}

// ═══════════════════════════════════════════
// 提示词目录：必须真的接线
// ═══════════════════════════════════════════

/// 造一个含单条模板的临时目录。
fn temp_prompt_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("agent-game-prompts-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("override.toml"),
        "[templates]\nyour_turn = \"来自磁盘的提示词\"\nwon = \"磁盘版胜利\"\n",
    )
    .unwrap();
    dir
}

#[test]
fn setting_a_prompt_dir_actually_reaches_the_engines() {
    // 这是那个"空操作"缺陷的回归用例：`set_prompt_dir` 只把路径存进
    // `AppState` 时，引擎读的还是内置模板，`--prompt-dir` 完全无效。
    with_prompt_lock(|| {
        let dir = temp_prompt_dir("wired");
        let mut state = AppState::with_defaults("tok".to_owned());

        set_prompt_dir(&mut state, dir.clone());

        // 引擎的仓库是进程级共享的，所以这里能直接读到
        with_shared_prompt_store(|store| {
            assert_eq!(
                store.dir(),
                Some(dir.clone()),
                "引擎的 PromptStore 必须拿到这个目录"
            );
            assert_eq!(
                store.render("your_turn", &agent_game_server_core::Ctx::new()),
                "来自磁盘的提示词"
            );
            // 清干净，别影响其它用例
            store.set_dir(None);
        });
        std::fs::remove_dir_all(&dir).ok();
    });
}

#[test]
fn clearing_the_prompt_dir_falls_back_to_the_builtin_templates() {
    with_prompt_lock(|| {
        let dir = temp_prompt_dir("cleared");
        with_shared_prompt_store(|store| {
            store.set_dir(Some(dir.clone()));
            assert_eq!(
                store.render("your_turn", &agent_game_server_core::Ctx::new()),
                "来自磁盘的提示词"
            );

            store.set_dir(None);
            assert_eq!(store.dir(), None);
            let text = store.render("your_turn", &agent_game_server_core::Ctx::new());
            assert!(
                text.contains("轮到你了"),
                "清掉目录后必须回落到内置模板，实际: {text}"
            );
        });
        std::fs::remove_dir_all(&dir).ok();
    });
}

#[test]
fn a_missing_directory_falls_back_instead_of_failing() {
    // 提示词层不能因为文件系统问题让服务起不来。
    with_prompt_lock(|| {
        with_shared_prompt_store(|store| {
            store.set_dir(Some(PathBuf::from("definitely/not/here")));
            let text = store.render("your_turn", &agent_game_server_core::Ctx::new());
            assert!(!text.is_empty(), "目录不存在时应当回落到内置模板");
            store.set_dir(None);
        });
    });
}

/// 在大话骰引擎共享的那份 `PromptStore` 上做一件事。
///
/// 它是 `OnceLock` 单例——这正是"设置一处、对所有对局生效"的依据。
/// 借用只能活在一个实例的生命周期里，所以调用方传闭包而不是拿引用。
/// 提示词仓库是**进程级共享**的，而 cargo test 默认多线程。
/// 凡是改它 `dir` 的用例必须串行，否则一个用例的 `set_dir(None)` 会
/// 把另一个正在断言的用例的目录清掉。
static PROMPT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 持有提示词锁并执行闭包。
fn with_prompt_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = PROMPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f()
}

/// 借用引擎的共享提示词仓库。
///
/// **两个引擎各有一份 `OnceLock`**（`liars_dice.rs` 与 `guess_number.rs` 各自
/// 一个 `static PROMPTS`）。`apply_prompt_dir` 会给**每一个**注册引擎的 store
/// 设目录，所以清理也必须两份都清——只清 `liars_dice` 那份的话，
/// `guess_number` 那份会带着上一个用例的临时目录进入下一个用例，
/// 表现为"偶尔读到不该存在的磁盘提示词"。
fn with_shared_prompt_store<R>(f: impl FnOnce(&PromptStore) -> R) -> R {
    use agent_game_server_core::GameEngine as _;
    use agent_game_server_core::games::{GuessNumberEngine, LiarsDiceEngine};

    let liars = LiarsDiceEngine::new(
        &agent_game_server_core::CreateParams::default(),
        &Config::default(),
    )
    .unwrap();
    let guess = GuessNumberEngine::new(
        &agent_game_server_core::CreateParams::default(),
        &Config::default(),
    )
    .unwrap();

    let result = f(liars.prompts());

    // 两份都复位，别把状态漏给下一个用例
    liars.prompts().set_dir(None);
    guess.prompts().set_dir(None);
    result
}

// ═══════════════════════════════════════════
// reload-prompts 端点
// ═══════════════════════════════════════════

#[test]
fn reloading_reports_every_registered_engine() {
    // 必须持锁：`reload_all_prompts` 会遍历注册表、对每个引擎的共享
    // `PromptStore` 调 `reload()`。而 `with_prompt_lock` 那三个用例正在
    // `set_dir(Some/None)`。两者并发时 `reload()` 可能读到某个临时目录
    // 刚被清掉、或另一份引擎的 store 已指向别处的中间状态。
    //
    // 这条曾经不持锁，CI 上偶发失败：同一个 commit 连跑两次，一次 success
    // 一次 failure（本地多核跑 5 次都过，runner 只有 2 核，调度不同就翻面）。
    with_prompt_lock(|| {
        let state = AppState::with_defaults("tok".to_owned());
        let reloaded = reload_all_prompts(&state);
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.contains(&"guess_number"));
        assert!(reloaded.contains(&"liars_dice"));
    });
}

// ═══════════════════════════════════════════
// 启动配置
// ═══════════════════════════════════════════

#[test]
fn the_default_config_leaves_room_for_the_client_timeout() {
    // `--block-budget` 的默认值不能超过 MCP 客户端的 70 秒。
    let config = Config::default();
    assert!(config.block_budget < Duration::from_secs(70));
    assert!(config.turn_timeout > config.block_budget);
}

#[test]
fn the_poll_interval_is_short_enough_to_feel_live() {
    // 后台轮询是唯一的周期结算点。间隔太长会让超时延迟、admin 页面卡顿；
    // 太短则空转。500ms 是那个折中。
    let config = Config::default();
    assert_eq!(config.poll_interval, Duration::from_millis(500));
}

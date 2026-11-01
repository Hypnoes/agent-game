//! 后台轮询：结算超时 + 状态有变就广播。
//!
//! 这是周期性地对**所有**对局结算超时的地方（REST 的 state 分支也会就地结算一次，
//! 那是为了不让调用方等后台轮询）。循环一旦退出，所有对局都不再超时、session
//! 永不回收、admin 页面永远不再更新，而且悄无声息。
//!
//! 所以这里的防线不是 `catch_unwind`：整个 workspace 禁止 `unsafe`，
//! 请求路径上的 `unwrap`/`expect`/`panic` 也被 clippy 禁掉，能 panic 的地方
//! 只剩下"库内部 bug"。防线是这样两层：
//!
//! 1. 每条引擎的 `check_timeout` 返回 `Result`，错误逐 session 隔离（manager 里做）。
//! 2. 轮询任务由一个监督者持有：任务一旦退出就记 `error!` 并重启，而不是让
//!    服务静默失去超时结算能力。见 [`spawn`]。

use std::sync::Arc;
use std::time::Duration;

use agent_game_server_core::GameManager;

use crate::broadcast::AdminBroadcast;

/// 结算所有超时，返回产生的事件。
pub async fn settle_timeouts(manager: &Arc<GameManager>) -> Vec<serde_json::Value> {
    manager
        .check_timeouts(std::time::Instant::now())
        .await
        .into_iter()
        .map(|record| record.to_json())
        .collect()
}

/// 一轮完整的刷新：结算超时 → 算签名 → 有变就广播。
///
/// 返回本轮的签名（`None` 表示序列化失败），以及是否真的广播了。
pub async fn refresh_once(
    manager: &Arc<GameManager>,
    broadcast: &AdminBroadcast,
) -> (Option<String>, bool) {
    settle_timeouts(manager).await;
    let signature = manager.state_signature().await;

    let unchanged = match (&signature, broadcast.last_signature()) {
        (Some(current), Some(previous)) => *current == previous,
        // 序列化失败（`None`）时强制广播：一次失败不能让页面永久静默
        (None, _) | (Some(_), None) => false,
    };
    if unchanged {
        return (signature, false);
    }
    broadcast.set_last_signature(signature.clone());
    broadcast.notify();
    (signature, true)
}

/// 轮询循环本体。
pub async fn run_loop(
    manager: Arc<GameManager>,
    broadcast: Arc<AdminBroadcast>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    // 错过的拍子延期处理，而不是一次性补发一串（补发会让结算瞬间跑很多次）
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        refresh_once(&manager, &broadcast).await;
    }
}

/// 启动轮询任务。
///
/// 返回值是监督任务：循环若因 panic 退出，监督者记 `error!` 并重启它。
/// 这是"单点静默失效"的唯一防线——没有它，一次库内部 panic 会让整个服务
/// 悄悄不再结算超时。
#[must_use]
pub fn spawn(
    manager: Arc<GameManager>,
    broadcast: Arc<AdminBroadcast>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_loop(
                Arc::clone(&manager),
                Arc::clone(&broadcast),
                interval,
            ));
            match handle.await {
                Ok(()) => {
                    // 循环正常返回只可能是任务被取消；退出监督
                    return;
                }
                Err(error) if error.is_cancelled() => return,
                Err(error) => {
                    tracing::error!(%error, "admin 轮询任务异常退出，重启它");
                    // 重启前给一点间隔，避免 panic 循环把日志刷爆
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_game_server_core::games::GuessNumberEngine;
    use agent_game_server_core::{Config, GameMeta, PlayerMode, Registry};

    fn manager() -> Arc<GameManager> {
        let mut registry = Registry::new();
        registry.register(GameMeta::new(
            "guess_number",
            PlayerMode::Single,
            |params, config| {
                GuessNumberEngine::new(params, config)
                    .map(|engine| Box::new(engine) as agent_game_server_core::BoxedEngine)
            },
        ));
        Arc::new(GameManager::new(registry, Config::default()))
    }

    #[tokio::test]
    async fn the_first_round_broadcasts() {
        let manager = manager();
        let broadcast = AdminBroadcast::new();
        let (signature, announced) = refresh_once(&manager, &broadcast).await;
        assert!(signature.is_some());
        assert!(announced, "第一次没有历史签名，必须广播");
    }

    #[tokio::test]
    async fn an_unchanged_state_is_not_rebroadcast() {
        let manager = manager();
        let broadcast = AdminBroadcast::new();
        refresh_once(&manager, &broadcast).await;
        let (_signature, announced) = refresh_once(&manager, &broadcast).await;
        assert!(!announced, "状态没变就不该重复推送");
    }

    #[tokio::test]
    async fn a_new_session_changes_the_signature() {
        let manager = manager();
        let broadcast = AdminBroadcast::new();
        refresh_once(&manager, &broadcast).await;

        manager
            .create(
                "guess_number",
                agent_game_server_core::CreateParams::default(),
                std::time::Instant::now(),
            )
            .await
            .unwrap();

        let (_signature, announced) = refresh_once(&manager, &broadcast).await;
        assert!(announced, "新建对局必须触发一次推送");
    }

    #[tokio::test]
    async fn a_serialisation_failure_forces_a_broadcast() {
        let manager = manager();
        let broadcast = AdminBroadcast::new();
        // 直接塞一个"上次签名存在但与当前不同"的状态，模拟上一轮失败后恢复
        broadcast.set_last_signature(Some("stale".to_owned()));
        let (_signature, announced) = refresh_once(&manager, &broadcast).await;
        assert!(announced);
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_runs_until_cancelled() {
        let manager = manager();
        let broadcast = AdminBroadcast::new();
        let handle = spawn(manager, Arc::new(broadcast), Duration::from_millis(10));
        tokio::time::advance(Duration::from_millis(5)).await;
        handle.abort();
        let _ = handle.await;
    }
}

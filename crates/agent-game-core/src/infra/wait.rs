//! 回合等待循环——等待语义只写这一份。
//!
//! Python 版把这段循环放在**引擎**里（`LiarsDiceEngine.wait_for_change`），代价是
//! 每个回合制游戏都要自己实现一遍"先订阅、再判条件、再等剩余预算、醒来重判"。
//! 第三个游戏作者写错一次，AGENTS 里那条"别把循环简化回任何一次唤醒就返回"
//! 就失效一次，而这套逻辑和具体玩法毫无关系。
//!
//! 这里把它上移成框架函数：引擎只回答两个问题——"现在轮到他了吗"和"变了就叫我"。

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::infra::signal::{ChangeSignal, WaitOutcome};

/// 等待"条件成立"，最多 `budget`。
///
/// `cond` 每轮重新求值；`signal` 只负责"变了"这件事。被唤醒**不等于**条件成立：
/// 3 人以上时任何一名玩家叫数都会广播，醒来后必须重新判断，否则调用方会误以为
/// 可以行动、发出一个必然被拒绝的动作。
///
/// 顺序是固定的：**先订阅、再判条件、再等剩余预算**。反过来会让"检查"与"订阅"
/// 之间发生的变更丢掉唤醒，白等满整个预算。
pub async fn wait_for<C, Fut>(signal: &ChangeSignal, budget: Duration, mut cond: C) -> WaitOutcome
where
    C: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let mut sub = signal.subscribe();
    let deadline = Instant::now() + budget;

    loop {
        // 先订阅（上面已做），再判条件：这两步之间发生的 notify 已把版本号推进，
        // 下面 `signal.wait` 一进来就会发现，不会丢。
        if cond().await {
            return WaitOutcome::Ready;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return WaitOutcome::Timeout;
        }

        match signal.wait(&mut sub, remaining).await {
            WaitOutcome::Timeout => return WaitOutcome::Timeout,
            // 被唤醒：可能只是别人行动，回到循环顶部重判，未成立就继续用剩下的预算等
            WaitOutcome::Ready => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test(start_paused = true)]
    async fn already_satisfied_returns_immediately() {
        let signal = ChangeSignal::new();
        let started = Instant::now();
        let outcome = wait_for(&signal, Duration::from_secs(5), || async { true }).await;

        assert_eq!(outcome, WaitOutcome::Ready);
        assert!(started.elapsed() < Duration::from_millis(1));
    }

    #[tokio::test(start_paused = true)]
    async fn wakes_when_the_condition_becomes_true() {
        let signal = Arc::new(ChangeSignal::new());
        let ready = Arc::new(AtomicBool::new(false));

        let setter = (Arc::clone(&signal), Arc::clone(&ready));
        let handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            setter.1.store(true, Ordering::SeqCst);
            setter.0.notify();
        });

        let outcome = wait_for(&signal, Duration::from_secs(5), || {
            let ready = Arc::clone(&ready);
            async move { ready.load(Ordering::SeqCst) }
        })
        .await;

        assert_eq!(outcome, WaitOutcome::Ready);
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_spurious_wakeup_keeps_waiting() {
        // 3 人局的核心场景：别人行动把我唤醒，但没轮到我，必须继续等，
        // 而不是返回 Ready 让调用方误以为可以行动。
        let signal = Arc::new(ChangeSignal::new());
        let ready = Arc::new(AtomicBool::new(false));

        let noise = Arc::clone(&signal);
        let handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            noise.notify(); // 假唤醒
            noise.notify(); // 再来一次
        });

        let started = Instant::now();
        let outcome = wait_for(&signal, Duration::from_millis(100), || {
            let ready = Arc::clone(&ready);
            async move { ready.load(Ordering::SeqCst) }
        })
        .await;

        assert_eq!(outcome, WaitOutcome::Timeout, "假唤醒不能当成轮到我");
        assert!(started.elapsed() >= Duration::from_millis(100));
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn false_then_true_within_the_same_wakeup() {
        // 一次广播里条件先假后真：循环重判必须看到"真"。
        let signal = Arc::new(ChangeSignal::new());
        let ready = Arc::new(AtomicBool::new(false));
        let checks = Arc::new(AtomicBool::new(false));

        let setter = (Arc::clone(&signal), Arc::clone(&ready));
        let handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            setter.1.store(true, Ordering::SeqCst);
            setter.0.notify();
        });

        let outcome = wait_for(&signal, Duration::from_secs(5), || {
            let ready = Arc::clone(&ready);
            let checks = Arc::clone(&checks);
            async move {
                // 第一次求值故意返回 false，模拟"被唤醒那一刻还没轮到"
                if !checks.swap(true, Ordering::SeqCst) {
                    return false;
                }
                ready.load(Ordering::SeqCst)
            }
        })
        .await;

        assert_eq!(outcome, WaitOutcome::Ready);
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn no_lost_wakeup_when_notified_before_the_first_check() {
        let signal = Arc::new(ChangeSignal::new());
        let ready = Arc::new(AtomicBool::new(true));

        // 订阅发生在 wait_for 内部；这里保证"进入函数前就已经 notify 过"
        signal.notify();

        let outcome = wait_for(&signal, Duration::from_secs(30), || {
            let ready = Arc::clone(&ready);
            async move { ready.load(Ordering::SeqCst) }
        })
        .await;

        assert_eq!(outcome, WaitOutcome::Ready);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_budget_only_checks_once() {
        let signal = ChangeSignal::new();
        let started = Instant::now();
        let outcome = wait_for(&signal, Duration::ZERO, || async { false }).await;

        assert_eq!(outcome, WaitOutcome::Timeout);
        assert!(started.elapsed() < Duration::from_millis(1));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_leaves_no_waiter() {
        let signal = Arc::new(ChangeSignal::new());
        let task = tokio::spawn({
            let signal = Arc::clone(&signal);
            async move { wait_for(&signal, Duration::from_secs(30), || async { false }).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(signal.waiter_count(), 1);

        task.abort();
        let _ = task.await;
        assert_eq!(signal.waiter_count(), 0);
    }
}

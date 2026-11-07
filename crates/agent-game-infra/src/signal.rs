//! `ChangeSignal` —— 状态变更信号，供阻塞式等待使用。
//!
//! 语义（与 Python 版逐条对齐）：
//! - `subscribe()` 每次拿到一个**新的** [`Subscription`]，携带当前版本号。
//!   绝不复用上一个：复用的 receiver 会把"已经发生过"的变更误报成"刚刚发生"。
//! - `notify()` 把版本号 +1，唤醒全部等待者。
//! - 调用方按"先订阅 → 再检查状态 → 再等待"的顺序使用。`notify()` 可能恰好落在
//!   检查与进入等待之间：那个变更已经把版本号推上去了，`wait` 一进来就发现
//!   "版本号已经变了"，立刻返回——不会丢掉这次唤醒、白等满一整个预算。
//! - `Subscription` 是 RAII 的：被 drop（包括任务被取消）时自动减计数，
//!   所以 `waiter_count()` 不会因为取消而泄漏。
//!
//! 用 `watch` 而不是 `Notify`：`Notify::notify_waiters` 不保存许可，竞态下会丢
//! 唤醒；`watch` 的版本号本身就记录了"变过没有"。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

/// 一次等待的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// 条件成立（轮到自己，或对局已结束）。
    Ready,
    /// 等满了预算，条件仍未成立。
    Timeout,
}

/// 一次订阅。drop 时自动从信号的等待者计数里移除。
#[derive(Debug)]
pub struct Subscription {
    rx: watch::Receiver<u64>,
    live: Arc<AtomicUsize>,
}

impl Subscription {
    /// 订阅之后、进入等待之前，版本号是否已经前进过。
    ///
    /// 调用方在"检查条件"与"进入等待"之间用这个判断有没有漏掉一次唤醒。
    #[must_use]
    pub fn has_changed(&self) -> bool {
        self.rx.has_changed().unwrap_or(false)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
pub struct ChangeSignal {
    tx: watch::Sender<u64>,
    live: Arc<AtomicUsize>,
}

impl Default for ChangeSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeSignal {
    #[must_use]
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(0u64);
        Self {
            tx,
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 订阅：拿到一个携带当前版本号、drop 时自动注销的 [`Subscription`]。
    #[must_use]
    pub fn subscribe(&self) -> Subscription {
        self.live.fetch_add(1, Ordering::SeqCst);
        Subscription {
            rx: self.tx.subscribe(),
            live: Arc::clone(&self.live),
        }
    }

    /// 唤醒全部等待者。没有等待者时调用是安全的空操作。
    pub fn notify(&self) {
        // `send_modify` 只在所有 receiver 都被 drop 时才失败；那时没有等待者，
        // 版本号也不必推进（下一个订阅者从当前版本开始，它不关心历史）。
        // 没有等待者时它是一个安全的空操作，返回的 `()` 不需要处理。
        self.tx
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// 等待一次变更，最多 `budget`。
    ///
    /// 只等"版本号相对 `sub` 已经前进"这一个条件。收到变更返回
    /// [`WaitOutcome::Ready`]，超时返回 [`WaitOutcome::Timeout`]。
    /// **不判断"是不是轮到我"** —— 那是调用方循环要重复做的事。
    pub async fn wait(&self, sub: &mut Subscription, budget: Duration) -> WaitOutcome {
        if budget.is_zero() {
            return WaitOutcome::Timeout;
        }
        // 订阅之后、进入等待之前已经发生过 notify()，这次唤醒不能丢
        if sub.has_changed() {
            return WaitOutcome::Ready;
        }
        match tokio::time::timeout(budget, sub.rx.changed()).await {
            // `changed()` 成功即表示版本推进过——它同时把"已读"标记推进了，
            // 所以这里**不能**再问 `has_changed()`（那已经是 false 了）。
            Ok(Ok(())) => WaitOutcome::Ready,
            // sender 被 drop 不可能发生（signal 与引擎同生命周期），当作没变
            Ok(Err(_)) => WaitOutcome::Timeout,
            Err(_) => WaitOutcome::Timeout,
        }
    }

    /// 等到绝对时刻 `deadline`。等价于 `wait`，但避免调用方自己算剩余时间。
    pub async fn wait_until(&self, sub: &mut Subscription, deadline: Instant) -> WaitOutcome {
        let budget = deadline.saturating_duration_since(Instant::now());
        self.wait(sub, budget).await
    }

    /// 当前在册的等待者数量（测试与诊断用）。
    #[must_use]
    pub fn waiter_count(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn notify_wakes_a_waiter() {
        let signal = Arc::new(ChangeSignal::new());
        let mut sub = signal.subscribe();

        let notifier = Arc::clone(&signal);
        let handle = tokio::spawn(async move {
            tokio::task::yield_now().await;
            notifier.notify();
        });

        let outcome = signal.wait(&mut sub, Duration::from_secs(5)).await;
        assert_eq!(outcome, WaitOutcome::Ready);
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn no_lost_wakeup_between_subscribe_and_wait() {
        let signal = ChangeSignal::new();
        let mut sub = signal.subscribe();
        signal.notify(); // 订阅之后、进入等待之前发生的变更

        // 先直接断言短路条件本身：这条是"不丢唤醒"的**唯一**依据。
        // 只断言 `wait` 返回 Ready 不够——`changed()` 那条路径在预算很大时
        // 也会立刻成功（版本号早就变了），于是短路分支写坏了也不会被发现。
        assert!(
            sub.has_changed(),
            "订阅之后发生的 notify 必须被 has_changed 看到"
        );

        let started = Instant::now();
        assert_eq!(
            signal.wait(&mut sub, Duration::from_secs(30)).await,
            WaitOutcome::Ready
        );
        assert!(started.elapsed() < Duration::from_millis(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fresh_subscription_reports_no_change() {
        // 上一条的反面：`has_changed()` 不能恒为真，否则每次等待都会立刻返回。
        let signal = ChangeSignal::new();
        let sub = signal.subscribe();
        assert!(!sub.has_changed(), "刚订阅还没发生任何变更");
    }

    #[tokio::test(start_paused = true)]
    async fn returns_timeout_at_the_budget() {
        let signal = ChangeSignal::new();
        let mut sub = signal.subscribe();

        let started = Instant::now();
        assert_eq!(
            signal.wait(&mut sub, Duration::from_millis(200)).await,
            WaitOutcome::Timeout
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test(start_paused = true)]
    async fn one_notify_wakes_every_waiter() {
        let signal = Arc::new(ChangeSignal::new());
        let mut first = signal.subscribe();
        let mut second = signal.subscribe();

        let a = tokio::spawn({
            let signal = Arc::clone(&signal);
            async move { signal.wait(&mut first, Duration::from_secs(1)).await }
        });
        let b = tokio::spawn({
            let signal = Arc::clone(&signal);
            async move { signal.wait(&mut second, Duration::from_secs(1)).await }
        });

        tokio::task::yield_now().await;
        signal.notify();

        assert_eq!(a.await.unwrap(), WaitOutcome::Ready);
        assert_eq!(b.await.unwrap(), WaitOutcome::Ready);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_subscription_does_not_replay_old_changes() {
        let signal = ChangeSignal::new();
        let mut first = signal.subscribe();
        signal.notify();
        assert_eq!(
            signal.wait(&mut first, Duration::from_secs(1)).await,
            WaitOutcome::Ready
        );

        // 新订阅者从当前版本开始，不会把"上一次变更"误当成刚刚发生
        let mut second = signal.subscribe();
        assert_eq!(
            signal.wait(&mut second, Duration::from_millis(50)).await,
            WaitOutcome::Timeout
        );
    }

    #[tokio::test(start_paused = true)]
    async fn zero_budget_does_not_wait() {
        let signal = ChangeSignal::new();
        let mut sub = signal.subscribe();
        let started = Instant::now();
        assert_eq!(
            signal.wait(&mut sub, Duration::ZERO).await,
            WaitOutcome::Timeout
        );
        assert!(started.elapsed() < Duration::from_millis(1));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_until_respects_an_absolute_deadline() {
        let signal = ChangeSignal::new();
        let mut sub = signal.subscribe();
        let deadline = Instant::now() + Duration::from_millis(30);
        assert_eq!(
            signal.wait_until(&mut sub, deadline).await,
            WaitOutcome::Timeout
        );
        assert_eq!(
            signal
                .wait_until(&mut sub, Instant::now() - Duration::from_secs(1))
                .await,
            WaitOutcome::Timeout
        );
    }

    #[test]
    fn waiter_count_is_raii() {
        let signal = ChangeSignal::new();
        assert_eq!(signal.waiter_count(), 0);
        {
            let _a = signal.subscribe();
            let _b = signal.subscribe();
            assert_eq!(signal.waiter_count(), 2);
        }
        // 两个订阅都被 drop：计数必须归零，不能泄漏
        assert_eq!(signal.waiter_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_waiter_leaves_no_trace() {
        let signal = Arc::new(ChangeSignal::new());
        let sub = signal.subscribe();

        let task = tokio::spawn({
            let signal = Arc::clone(&signal);
            async move {
                let mut sub = sub;
                signal.wait(&mut sub, Duration::from_secs(30)).await
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(signal.waiter_count(), 1);

        task.abort();
        let _ = task.await;
        assert_eq!(signal.waiter_count(), 0, "取消等待后不能留下等待者");
    }
}

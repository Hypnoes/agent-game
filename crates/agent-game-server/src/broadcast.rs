//! admin WebSocket 广播。
//!
//! 每个 app 实例持有一份 [`AdminBroadcast`]：连接集合、上次广播的签名、
//! 后台任务句柄。Python 版这三样曾经是模块级全局，被所有 app 实例共享，
//! 于是两个实例同时在线时内层 shutdown 会把外层的 poller 一起取消掉
//! （实测 poller 任务数 2 → 0）。收进结构体后这是结构上的保证，不是纪律。

use std::collections::BTreeSet;
use std::sync::Mutex;

use tokio::sync::broadcast;

/// 广播给 admin 客户端的唯一消息：只有"状态变了"，不含任何内容。
///
/// 不带内容是有意的：`/api/v1/admin/ws` 一旦推送状态本身，就等于把
/// `target` / `all_dice` 推到每条连接上，而这些连接的鉴权只在握手时做一次。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateChanged;

/// 一台 app 实例自己的 admin 推送状态。
#[derive(Debug)]
pub struct AdminBroadcast {
    tx: broadcast::Sender<StateChanged>,
    /// 已连接的客户端数量（诊断用；真实推送走 `broadcast` 通道）。
    clients: Mutex<BTreeSet<u64>>,
    next_client_id: Mutex<u64>,
    /// 上一次广播的状态指纹。
    last_signature: Mutex<Option<String>>,
}

impl AdminBroadcast {
    /// 通道容量。
    ///
    /// 广播只用于"刷新页面"这类幂等动作，慢客户端丢几条不影响正确性——
    /// 它下一次收到消息时反正会重新拉全量。
    pub const CAPACITY: usize = 32;

    #[must_use]
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(Self::CAPACITY);
        Self {
            tx,
            clients: Mutex::new(BTreeSet::new()),
            next_client_id: Mutex::new(0),
            last_signature: Mutex::new(None),
        }
    }

    /// 订阅广播。
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<StateChanged> {
        self.tx.subscribe()
    }

    /// 登记一条连接，返回它的编号（断开时要交回来）。
    pub fn register_client(&self) -> u64 {
        let Ok(mut next) = self.next_client_id.lock() else {
            return 0;
        };
        *next += 1;
        let id = *next;
        drop(next);
        if let Ok(mut clients) = self.clients.lock() {
            clients.insert(id);
        }
        id
    }

    /// 注销一条连接。
    pub fn unregister_client(&self, id: u64) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.remove(&id);
        }
    }

    /// 当前连接数。
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.clients.lock().map_or(0, |clients| clients.len())
    }

    /// 推送一条"状态变了"。没有订阅者时返回 `false`。
    pub fn notify(&self) -> bool {
        self.tx.send(StateChanged).is_ok()
    }

    /// 上一次广播的指纹。
    #[must_use]
    pub fn last_signature(&self) -> Option<String> {
        self.last_signature
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// 覆盖上一次的指纹。
    pub fn set_last_signature(&self, signature: Option<String>) {
        if let Ok(mut guard) = self.last_signature.lock() {
            *guard = signature;
        }
    }
}

impl Default for AdminBroadcast {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_are_tracked_per_broadcast() {
        let broadcast = AdminBroadcast::new();
        assert_eq!(broadcast.client_count(), 0);
        let first = broadcast.register_client();
        let second = broadcast.register_client();
        assert_eq!(broadcast.client_count(), 2);
        broadcast.unregister_client(first);
        assert_eq!(broadcast.client_count(), 1);
        broadcast.unregister_client(second);
        assert_eq!(broadcast.client_count(), 0);
    }

    #[test]
    fn two_broadcasts_do_not_share_clients() {
        let first = AdminBroadcast::new();
        let second = AdminBroadcast::new();
        first.register_client();
        assert_eq!(first.client_count(), 1);
        assert_eq!(second.client_count(), 0, "实例之间不能共享连接集合");
    }

    #[test]
    fn notify_reports_whether_anyone_listened() {
        let broadcast = AdminBroadcast::new();
        assert!(!broadcast.notify(), "没有订阅者时 send 失败");
        let _rx = broadcast.subscribe();
        assert!(broadcast.notify());
    }

    #[tokio::test]
    async fn subscribers_receive_state_changed() {
        let broadcast = AdminBroadcast::new();
        let mut rx = broadcast.subscribe();
        assert!(broadcast.notify());
        assert_eq!(rx.recv().await, Ok(StateChanged));
    }

    #[test]
    fn signature_is_per_instance() {
        let first = AdminBroadcast::new();
        let second = AdminBroadcast::new();
        first.set_last_signature(Some("abc".to_owned()));
        assert_eq!(first.last_signature().as_deref(), Some("abc"));
        assert_eq!(second.last_signature(), None);
    }
}

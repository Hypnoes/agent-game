//! 服务端的共享配置。
//!
//! Python 版把这些数值放在模块全局 `config.BLOCK_BUDGET`，靠 monkeypatch 改动；
//! 这里换成一份按值传入的不可变结构。区别不只是风格：模块全局的默认值会在
//! import 时求值一次，之后改它就有"有的人生效、有的人没生效"的隐患，
//! Python 版为此在 `parse_block_budget` 里专门用 `None` 做哨兵绕开。

use std::time::Duration;

/// 默认的等待预算：一次 `state` 调用最多在服务端挂起 55 秒。
///
/// 这个值必须小于 MCP 消费端的工具调用超时（README 里配的是 70 秒），
/// 否则客户端会在服务端返回之前先超时，Agent 看到的是传输层错误而不是对局状态。
pub const DEFAULT_BLOCK_BUDGET_SECS: f64 = 55.0;

/// 大话骰的回合时限：当前回合多久不行动判负。
///
/// 与 `block_budget` 是两条不同的时间轴，必须大于它——判负要发生在调用方
/// 拿到状态之后，否则每个 `state` 都会先等到判负。
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 60;

/// 已结束的对局在内存里保留多久，过期由超时结算回收。
pub const DEFAULT_FINISHED_RETENTION_SECS: u64 = 300;

/// MCP 连接级会话缓存的容量。
pub const DEFAULT_SESSION_CACHE_CAP: usize = 512;

/// 后台轮询间隔。
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 500;

#[derive(Clone, Debug)]
pub struct Config {
    /// 一次 `state` 调用最多挂起多久。同时也是 `timeout` 参数的上限。
    pub block_budget: Duration,
    /// 回合制游戏的判负时限。
    pub turn_timeout: Duration,
    /// 已结束对局的保留期。
    pub finished_retention: Duration,
    /// 连接级会话缓存上限。
    pub session_cache_cap: usize,
    /// 后台轮询间隔。
    pub poll_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            block_budget: secs_f64(DEFAULT_BLOCK_BUDGET_SECS),
            turn_timeout: Duration::from_secs(DEFAULT_TURN_TIMEOUT_SECS),
            finished_retention: Duration::from_secs(DEFAULT_FINISHED_RETENTION_SECS),
            session_cache_cap: DEFAULT_SESSION_CACHE_CAP,
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
        }
    }
}

impl Config {
    /// 等待预算的秒数形式。解析 `timeout` 参数时要拿它当上限。
    #[must_use]
    pub fn block_budget_secs(&self) -> f64 {
        self.block_budget.as_secs_f64()
    }

    /// 提示词渲染用的预算数值：整数就显示整数，避免出现 "55.0 秒"。
    #[must_use]
    pub fn block_budget_display(&self) -> String {
        let secs = self.block_budget_secs();
        if (secs.fract()).abs() < f64::EPSILON {
            format!("{secs:.0}")
        } else {
            // Rust 的 format! 没有 `:g`，这里手工去掉尾随零
            format!("{secs:.3}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        }
    }
}

/// 把秒数转成 `Duration`。
///
/// 负数、NaN、无穷会被夹成 0 并保留成"立即返回"的语义，而不是 panic：
/// `Duration::from_secs_f64` 对这三类输入会 panic，而配置写错不该让进程起不来。
///
/// 公开是为了让它可测：`NaN` 与 `inf` 如果走到 `from_secs_f64` 会直接崩，
/// 而调用方（`Config::default`）传的永远是常量——没有用例的话，`&&` 被改成
/// `||` 这类错误永远暴露不出来。
#[must_use]
pub fn secs_f64(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let cfg = Config::default();
        assert!((cfg.block_budget_secs() - 55.0).abs() < f64::EPSILON);
        assert_eq!(cfg.turn_timeout, Duration::from_secs(60));
        assert_eq!(cfg.finished_retention, Duration::from_secs(300));
        assert_eq!(cfg.session_cache_cap, 512);
    }

    #[test]
    fn turn_timeout_must_exceed_block_budget() {
        // 两条时间轴：判负必须晚于等待预算，否则每次 state 都会先等到判负。
        let cfg = Config::default();
        assert!(cfg.turn_timeout > cfg.block_budget);
    }

    #[test]
    fn budget_display_drops_trailing_zero() {
        let cfg = Config::default();
        assert_eq!(cfg.block_budget_display(), "55");
    }

    #[test]
    fn budget_display_keeps_fraction() {
        let cfg = Config {
            block_budget: Duration::from_millis(7500),
            ..Config::default()
        };
        assert_eq!(cfg.block_budget_display(), "7.5");
    }

    #[test]
    fn client_timeout_leaves_room_for_the_server() {
        // README 与 DSH 配置里写的是 70 秒；留 10 秒以上余量给协议往返。
        let client_timeout_secs = 70.0;
        let cfg = Config::default();
        assert!(client_timeout_secs > cfg.block_budget_secs());
        assert!(client_timeout_secs - cfg.block_budget_secs() >= 10.0);
    }

    #[test]
    fn a_positive_finite_number_becomes_a_duration() {
        assert_eq!(secs_f64(55.0), Duration::from_secs(55));
        assert_eq!(secs_f64(0.5), Duration::from_millis(500));
    }

    #[test]
    fn nan_and_infinity_are_clamped_instead_of_panicking() {
        // `&&` 改成 `||` 会让 NaN 也进入 `Duration::from_secs_f64`，那里会 panic。
        // 这条用例是那个变异体的唯一看门人。
        assert_eq!(secs_f64(f64::NAN), Duration::ZERO);
        assert_eq!(secs_f64(f64::INFINITY), Duration::ZERO);
        assert_eq!(secs_f64(f64::NEG_INFINITY), Duration::ZERO);
    }

    #[test]
    fn zero_and_negative_become_zero() {
        // `secs > 0.0` 改成 `>=` 对 0 的结果不变（两条分支都给 ZERO），
        // 但负数必须被夹住。
        assert_eq!(secs_f64(0.0), Duration::ZERO);
        assert_eq!(secs_f64(-1.0), Duration::ZERO);
        assert_eq!(secs_f64(-0.5), Duration::ZERO);
    }
}

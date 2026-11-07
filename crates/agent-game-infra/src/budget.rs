//! `timeout` 参数的唯一解析器。
//!
//! 两条传输（REST 与 MCP）都必须经由这里。Python 版曾经各写一份，语义正好相反：
//! MCP 侧 `float(args.pop("timeout", None) or BLOCK_BUDGET)` 让 `timeout=0`
//! 变成 55 秒，REST 侧 `min(float(...), BLOCK_BUDGET)` 让 0 就是 0——同一个
//! 客户端在两条通道上等到的时长不一样。
//!
//! 契约：
//! - 省略 / `null` → 用满预算（或调用方给的 `default`）
//! - 数值 → 夹到 `[0, cap]`；`0` 表示**本次不等待**
//! - 负数 → 0
//! - `bool` / 字符串 / `NaN` / `±inf` / 非数字 JSON → 错误，绝不静默降级

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BudgetError {
    #[error("timeout 必须是数字，收到 {0}")]
    NotNumeric(String),
    #[error("timeout 必须是有限数字，收到 {0}")]
    NotFinite(String),
}

/// 把调用方传入的 `timeout` 解析成一次等待的预算（秒）。
///
/// `cap` 是当前配置的等待预算；`default` 只回答"没给参数时用多少秒"，
/// 不改变上限。
///
/// # Errors
/// 传入既不是"省略"也不是有限数字时返回 [`BudgetError`]。
pub fn resolve_budget(
    raw: Option<&Value>,
    default: Option<f64>,
    cap: f64,
) -> Result<f64, BudgetError> {
    let cap = sanitize_cap(cap);
    let budget = match raw {
        None | Some(Value::Null) => default.unwrap_or(cap),
        Some(value) => to_finite(value)?,
    };
    Ok(budget.clamp(0.0, cap))
}

/// 上限本身若是负数或 NaN（配置写错了），按 0 处理，避免 `clamp` panic。
fn sanitize_cap(cap: f64) -> f64 {
    if cap.is_finite() && cap > 0.0 {
        cap
    } else {
        0.0
    }
}

fn to_finite(value: &Value) -> Result<f64, BudgetError> {
    // `as_f64` 对 bool / 字符串 / 数组 / 对象都返回 None：Python 里 bool 是 int
    // 的子类，`timeout=true` 会静默变成 1 秒；serde_json 的类型区分让它不可能发生。
    let Some(number) = value.as_f64() else {
        return Err(BudgetError::NotNumeric(describe(value)));
    };
    if !number.is_finite() {
        return Err(BudgetError::NotFinite(describe(value)));
    }
    Ok(number)
}

/// `NaN` / `±inf` 不是合法 JSON，但 `serde_json::Value` 也装不下它们；
/// 真正会走到这里的是经 `json!` 构造出来的极端值，用 Debug 形式报告即可。
fn describe(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 40 {
        format!("{}…", text.chars().take(40).collect::<String>())
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CAP: f64 = 55.0;

    #[test]
    fn absent_uses_full_budget() {
        assert_eq!(resolve_budget(None, None, CAP), Ok(55.0));
        assert_eq!(resolve_budget(Some(&Value::Null), None, CAP), Ok(55.0));
    }

    #[test]
    fn zero_means_do_not_wait() {
        assert_eq!(resolve_budget(Some(&json!(0)), None, CAP), Ok(0.0));
    }

    #[test]
    fn negative_clamps_to_zero() {
        assert_eq!(resolve_budget(Some(&json!(-5)), None, CAP), Ok(0.0));
    }

    #[test]
    fn above_cap_clamps_to_budget() {
        assert_eq!(resolve_budget(Some(&json!(999)), None, CAP), Ok(55.0));
    }

    #[test]
    fn fractional_values_are_kept() {
        assert_eq!(resolve_budget(Some(&json!(3.7)), None, CAP), Ok(3.7));
    }

    #[test]
    fn rejects_bool_and_string() {
        assert!(resolve_budget(Some(&json!(true)), None, CAP).is_err());
        assert!(resolve_budget(Some(&json!(false)), None, CAP).is_err());
        assert!(resolve_budget(Some(&json!("55")), None, CAP).is_err());
    }

    #[test]
    fn rejects_non_scalar_json() {
        assert!(resolve_budget(Some(&json!([1])), None, CAP).is_err());
        assert!(resolve_budget(Some(&json!({"a": 1})), None, CAP).is_err());
    }

    #[test]
    fn explicit_default_is_not_the_cap() {
        assert_eq!(resolve_budget(None, Some(1.0), CAP), Ok(1.0));
        assert_eq!(resolve_budget(Some(&json!(0)), Some(1.0), CAP), Ok(0.0));
        // 显式给 99 被夹到 cap 55，而不是 default 1.0
        assert_eq!(resolve_budget(Some(&json!(99)), Some(1.0), CAP), Ok(55.0));
    }

    #[test]
    fn cap_follows_the_runtime_config() {
        // 对应 Python 版的 "reads budget at call time"：上限由调用方传入，
        // 不在 import 时固化。
        assert_eq!(resolve_budget(Some(&json!(999)), None, 2.0), Ok(2.0));
    }

    #[test]
    fn broken_cap_does_not_panic() {
        assert_eq!(resolve_budget(Some(&json!(5)), None, f64::NAN), Ok(0.0));
        assert_eq!(resolve_budget(None, None, -1.0), Ok(0.0));
    }

    #[test]
    fn error_message_carries_the_offending_value() {
        let err = resolve_budget(Some(&json!("abc")), None, CAP).unwrap_err();
        assert!(err.to_string().contains("abc"), "{err}");
    }

    #[test]
    fn long_values_are_truncated_in_the_error() {
        let long = "x".repeat(100);
        let err = resolve_budget(Some(&json!(long)), None, CAP).unwrap_err();
        assert!(err.to_string().ends_with('…'), "{err}");
    }
}

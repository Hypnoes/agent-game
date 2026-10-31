//! 简单可变状态机：校验 → 更新。
//!
//! 两个引擎的状态图都很小（猜数字：`in_progress → won|lost`；大话骰：
//! `waiting → bidding → finished`），Python 版为此写了一个 ABC 基类 + 两个子类
//! + 四个钩子方法。这里的取值是："状态图是数据，不是类"——每个引擎在
//! [`Transitions`] 里写一张表，`transition_to` 校验后返回新状态。

use crate::error::EngineError;

/// 一张状态转移表。
#[derive(Debug, Clone)]
pub struct Transitions {
    edges: Vec<(&'static str, Vec<&'static str>)>,
}

impl Transitions {
    #[must_use]
    pub fn new(edges: Vec<(&'static str, Vec<&'static str>)>) -> Self {
        Self { edges }
    }

    #[must_use]
    pub fn allowed_from(&self, from: &str) -> Vec<&'static str> {
        self.edges
            .iter()
            .find(|(state, _)| *state == from)
            .map(|(_, targets)| targets.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn can_transition(&self, from: &str, to: &str) -> bool {
        self.allowed_from(from).contains(&to)
    }

    /// 校验一次转移。
    ///
    /// # Errors
    /// 目标不在 `from` 的允许列表里。
    pub fn check(&self, from: &'static str, to: &'static str) -> Result<(), EngineError> {
        if self.can_transition(from, to) {
            Ok(())
        } else {
            Err(EngineError::invalid(format!(
                "状态机不能从 '{from}' 转到 '{to}'。允许: {:?}",
                self.allowed_from(from)
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guess() -> Transitions {
        Transitions::new(vec![
            ("in_progress", vec!["won", "lost"]),
            ("won", vec![]),
            ("lost", vec![]),
        ])
    }

    #[test]
    fn allowed_targets_are_reported() {
        assert_eq!(guess().allowed_from("in_progress"), vec!["won", "lost"]);
        assert_eq!(guess().allowed_from("won"), Vec::<&str>::new());
        assert_eq!(guess().allowed_from("unknown"), Vec::<&str>::new());
    }

    #[test]
    fn valid_transition_passes() {
        assert_eq!(guess().check("in_progress", "won"), Ok(()));
    }

    #[test]
    fn invalid_transition_is_rejected_with_the_allowed_list() {
        let err = guess().check("won", "lost").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("won"), "{text}");
        assert!(text.contains("lost"), "{text}");
    }
}

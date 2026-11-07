//! 身份校验：与传输无关、与等待无关。
//!
//! 大话骰的全部玩法建立在私有信息上：能按索引取状态就等于能看对手的骰子，
//! 能按索引出手就等于能替对手行动。所以多人引擎的动作必须同时给出
//! `player_index` **与** 与之匹配的 `player_token`。
//!
//! 两条传输都必须在**进入等待之前**调用这里的函数。Python 版曾经分叉过：
//! REST 先校验再等待，MCP 先等待再校验——同一个 token 打错的 `state` 调用，
//! REST 立刻报错，MCP 白等满 55 秒。

use crate::error::EngineError;

/// 一名玩家对外可见的最小信息：身份校验只需要 token。
pub trait PlayerIdentity {
    fn token(&self) -> &str;
}

impl PlayerIdentity for &str {
    fn token(&self) -> &str {
        self
    }
}

impl PlayerIdentity for String {
    fn token(&self) -> &str {
        self
    }
}

/// 校验"调用方是不是这名玩家"。
///
/// - `players` 为 `None`：单人游戏没有玩家列表，保持开放（兼容原有调用方式）。
/// - 索引越界 / 负数：拒绝，绝不绕回最后一名玩家。
/// - 缺 token 或 token 不对：拒绝。
///
/// # Errors
/// 返回对应的 [`EngineError`] 变体，由传输层翻译成 HTTP 状态码或 MCP 文本。
pub fn check_player_identity<P: PlayerIdentity>(
    players: Option<&[P]>,
    player_index: Option<usize>,
    player_token: Option<&str>,
) -> Result<(), EngineError> {
    let Some(players) = players else {
        return Ok(());
    };

    let Some(index) = player_index else {
        // 多人游戏里省略 player_index 等价于按 0 号取——不能静默放行
        return Err(EngineError::InvalidIndex("缺少 player_index".into()));
    };

    if index >= players.len() {
        return Err(EngineError::NotFound(index.to_string()));
    }

    let Some(token) = player_token else {
        return Err(EngineError::MissingToken);
    };
    if token.is_empty() {
        return Err(EngineError::MissingToken);
    }
    if players[index].token() != token {
        return Err(EngineError::TokenMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn players() -> [&'static str; 2] {
        ["tok-a", "tok-b"]
    }

    #[test]
    fn single_player_engine_stays_open() {
        assert_eq!(check_player_identity::<&str>(None, None, None), Ok(()));
        assert_eq!(check_player_identity::<&str>(None, Some(99), None), Ok(()));
    }

    #[test]
    fn multi_player_requires_a_token() {
        let ps = players();
        assert_eq!(
            check_player_identity(Some(&ps), Some(0), None),
            Err(EngineError::MissingToken)
        );
        assert_eq!(
            check_player_identity(Some(&ps), Some(0), Some("")),
            Err(EngineError::MissingToken)
        );
    }

    #[test]
    fn multi_player_rejects_wrong_token() {
        let ps = players();
        assert_eq!(
            check_player_identity(Some(&ps), Some(0), Some("tok-b")),
            Err(EngineError::TokenMismatch)
        );
    }

    #[test]
    fn multi_player_rejects_out_of_range() {
        let ps = players();
        assert_eq!(
            check_player_identity(Some(&ps), Some(2), Some("tok-a")),
            Err(EngineError::NotFound("2".into()))
        );
    }

    #[test]
    fn multi_player_rejects_missing_index() {
        let ps = players();
        assert!(matches!(
            check_player_identity(Some(&ps), None, Some("tok-a")),
            Err(EngineError::InvalidIndex(_))
        ));
    }

    #[test]
    fn correct_token_passes() {
        let ps = players();
        assert_eq!(
            check_player_identity(Some(&ps), Some(1), Some("tok-b")),
            Ok(())
        );
    }
}

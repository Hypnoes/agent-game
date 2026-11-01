//! 猜数字：单人对局，1-100 范围内猜目标数字。
//!
//! 规则极简，但与 Python 版逐条对齐的细节不少：`start` 无条件重开一局、
//! 重开时**换一个答案**（不重复上一局）、猜测值接受整数形式的浮点、
//! `bool` 不算数字、超时只判负一次。

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rand::RngExt as _;
use serde_json::Value;

use crate::config::Config;
use crate::engine::{
    ActionParams, CreateParams, DEFAULT_FINISHED_STATES, GameEngine, PlayerMode, PlayerView,
    TimeoutEvent,
};
use crate::error::EngineError;
use crate::prompt::{Ctx, PromptStore};
use crate::signal::ChangeSignal;

const PROMPT_TOML: &str = include_str!("../../assets/guess_number_prompts.toml");

/// 单人对局多久不动就判负。
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

fn shared_prompts() -> &'static PromptStore {
    static PROMPTS: OnceLock<PromptStore> = OnceLock::new();
    PROMPTS.get_or_init(|| PromptStore::new(PROMPT_TOML))
}

/// 一次猜测的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuessResult {
    TooLow,
    TooHigh,
    Correct,
}

impl GuessResult {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLow => "too_low",
            Self::TooHigh => "too_high",
            Self::Correct => "correct",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::TooLow => "太低 ↑",
            Self::TooHigh => "太高 ↓",
            Self::Correct => "对了",
        }
    }
}

/// 一条猜测记录。
#[derive(Clone, Debug)]
pub struct Guess {
    pub number: i64,
    pub result: GuessResult,
    /// 相对对局开始的毫秒数。
    pub at_ms: u128,
}

/// 猜数字引擎。每个实例管理一局游戏。
#[derive(Debug)]
pub struct GuessNumberEngine {
    game_type: &'static str,
    min: i64,
    max: i64,
    max_attempts: i64,
    phase: &'static str,
    target: i64,
    guesses: Vec<Guess>,
    guess_count: i64,
    last_result: Option<GuessResult>,
    started_at: Instant,
    last_action: Instant,
    ended_at_ms: Option<u128>,
    signal: Arc<ChangeSignal>,
    setup: CreateParams,
    prompts: &'static PromptStore,
}

impl GuessNumberEngine {
    /// 从创建参数造一局。
    ///
    /// # Errors
    /// 参数不合法（范围反了、次数 < 1、类型不对）。
    pub fn new(params: &CreateParams, config: &Config) -> Result<Self, String> {
        let min = params.get_i64("min_num")?.unwrap_or(1);
        let max = params.get_i64("max_num")?.unwrap_or(100);
        let max_attempts = params.get_i64("max_attempts")?.unwrap_or(10);

        if min >= max {
            return Err(format!(
                "min_num 必须小于 max_num，收到 min_num={min}, max_num={max}"
            ));
        }
        if max_attempts < 1 {
            return Err(format!("max_attempts 必须 >= 1，收到 {max_attempts}"));
        }
        // 配置里的超时只用于回合制游戏；这里保留读一次的痕迹，避免 config 未用
        let _ = config;

        let now = Instant::now();
        let mut engine = Self {
            game_type: "guess_number",
            min,
            max,
            max_attempts,
            phase: "in_progress",
            target: 0,
            guesses: Vec::new(),
            guess_count: 0,
            last_result: None,
            started_at: now,
            last_action: now,
            ended_at_ms: None,
            signal: Arc::new(ChangeSignal::new()),
            setup: params.without("player_token"),
            prompts: shared_prompts(),
        };
        engine.roll_target(None);
        Ok(engine)
    }

    #[must_use]
    pub fn min(&self) -> i64 {
        self.min
    }

    #[must_use]
    pub fn max(&self) -> i64 {
        self.max
    }

    #[must_use]
    pub fn max_attempts(&self) -> i64 {
        self.max_attempts
    }

    #[must_use]
    pub fn target(&self) -> i64 {
        self.target
    }

    #[must_use]
    pub fn guess_count(&self) -> i64 {
        self.guess_count
    }

    #[must_use]
    pub fn last_result(&self) -> Option<GuessResult> {
        self.last_result
    }

    #[must_use]
    pub fn guesses(&self) -> &[Guess] {
        &self.guesses
    }

    /// 摇一个新答案。范围里还有别的数时保证换一个（不重复上一局）。
    fn roll_target(&mut self, previous: Option<i64>) {
        let mut rng = rand::rng();
        match previous {
            None if self.max <= self.min => self.target = rng.random_range(self.min..=self.max),
            None => self.target = rng.random_range(self.min..=self.max),
            Some(old) if self.max <= self.min => self.target = old,
            Some(old) => {
                // 至少有两个候选，循环必然终止
                self.target = loop {
                    let candidate = rng.random_range(self.min..=self.max);
                    if candidate != old {
                        break candidate;
                    }
                };
            }
        }
    }

    /// 重置成一局全新的游戏。配置（min/max/max_attempts）不变。
    fn reset(&mut self) {
        let previous = if self.phase == "in_progress" {
            None
        } else {
            Some(self.target)
        };
        self.phase = "in_progress";
        self.roll_target(previous);
        self.guesses.clear();
        self.guess_count = 0;
        self.last_result = None;
        self.ended_at_ms = None;
        self.started_at = Instant::now();
        self.last_action = self.started_at;
        self.signal.notify();
    }

    fn end(&mut self, phase: &'static str) {
        self.phase = phase;
        self.ended_at_ms = Some(self.started_at.elapsed().as_millis());
        self.signal.notify();
    }

    /// 把外部传进来的猜测值收敛成合法整数。
    ///
    /// `bool` 是"数字"的错觉（JSON 里它根本不是数字，REST 层已经挡住），
    /// 但 `1.0` 这种整数形式的浮点要接受——MCP 客户端经常把整数序列化成浮点。
    fn validate_number(&self, raw: &Value) -> Result<i64, EngineError> {
        let number = match raw {
            Value::Bool(value) => {
                return Err(EngineError::invalid(format!(
                    "猜测值必须是整数，收到 {value}"
                )));
            }
            Value::Number(number) => {
                if let Some(int) = number.as_i64() {
                    int
                } else if let Some(float) = number.as_f64() {
                    if float.fract().abs() > f64::EPSILON {
                        return Err(EngineError::invalid(format!(
                            "猜测值必须是整数，收到 {float}"
                        )));
                    }
                    float as i64
                } else {
                    return Err(EngineError::invalid(format!(
                        "猜测值必须是整数，收到 {number}"
                    )));
                }
            }
            other => {
                return Err(EngineError::invalid(format!(
                    "猜测值必须是 {}~{} 的整数，收到 {other}",
                    self.min, self.max
                )));
            }
        };

        if number < self.min || number > self.max {
            return Err(EngineError::invalid(format!(
                "猜测值必须在 {}~{} 之间，收到 {number}",
                self.min, self.max
            )));
        }
        Ok(number)
    }

    fn guess(&mut self, raw: &Value) -> Result<(), EngineError> {
        if self.phase != "in_progress" {
            return Err(EngineError::invalid(format!("游戏已结束 ({})", self.phase)));
        }

        let number = self.validate_number(raw)?;
        let result = match number.cmp(&self.target) {
            std::cmp::Ordering::Less => GuessResult::TooLow,
            std::cmp::Ordering::Greater => GuessResult::TooHigh,
            std::cmp::Ordering::Equal => GuessResult::Correct,
        };

        self.guess_count += 1;
        self.last_result = Some(result);
        self.last_action = Instant::now();
        self.guesses.push(Guess {
            number,
            result,
            at_ms: (self.last_action - self.started_at).as_millis(),
        });

        if result == GuessResult::Correct {
            self.end("won");
        } else if self.guess_count >= self.max_attempts {
            self.end("lost");
        } else {
            // 猜一次也是状态变更：等待方（如果有）要看到新的历史
            self.signal.notify();
        }
        Ok(())
    }

    fn build_prompt(&self) -> String {
        match self.phase {
            "won" => {
                let last = self.guesses.last().map_or(0, |guess| guess.number);
                self.render(
                    "won",
                    &Ctx::new()
                        .with("last_guess", last)
                        .with("count", self.guess_count),
                )
            }
            "lost" => {
                let history: Vec<i64> = self.guesses.iter().map(|guess| guess.number).collect();
                self.render(
                    "lost",
                    &Ctx::new()
                        .with("max_attempts", self.max_attempts)
                        .with("target", self.target)
                        .with("history", history),
                )
            }
            _ => match self.guesses.last() {
                Some(last) => {
                    let history: Vec<i64> = self.guesses.iter().map(|g| g.number).collect();
                    self.render(
                        "in_progress",
                        &Ctx::new()
                            .with("count", self.guess_count)
                            .with("last_number", last.number)
                            .with("hint", last.result.hint())
                            .with("remaining", self.max_attempts - self.guess_count)
                            .with("min_num", self.min)
                            .with("max_num", self.max)
                            .with("history", history),
                    )
                }
                None => self.render(
                    "in_progress_first",
                    &Ctx::new()
                        .with("remaining", self.max_attempts)
                        .with("min_num", self.min)
                        .with("max_num", self.max),
                ),
            },
        }
    }
}

impl GameEngine for GuessNumberEngine {
    fn game_type(&self) -> &'static str {
        self.game_type
    }

    fn player_mode(&self) -> PlayerMode {
        PlayerMode::Single
    }

    fn finished_states(&self) -> &'static [&'static str] {
        // 只用 won / lost：这条声明验证"终结态名由引擎自己定"这个能力。
        &["won", "lost"]
    }

    fn sensitive_keys(&self) -> &'static [&'static str] {
        &["target"]
    }

    fn phase(&self) -> &'static str {
        self.phase
    }

    fn admin_view(&self) -> Value {
        serde_json::json!({
            "status": self.phase,
            "min": self.min,
            "max": self.max,
            "guess_count": self.guess_count,
            "max_attempts": self.max_attempts,
            "target": self.target,
            "guesses": self.guesses.iter().map(|guess| serde_json::json!({
                "number": guess.number,
                "result": guess.result.as_str(),
                "at_ms": guess.at_ms,
            })).collect::<Vec<_>>(),
            "ended_at_ms": self.ended_at_ms,
        })
    }

    fn public_view(&self) -> Value {
        // 刻意不写 `target`：公开视图是**构造**出来的，不是从 admin 视图里删键。
        serde_json::json!({
            "status": self.phase,
            "min": self.min,
            "max": self.max,
            "guess_count": self.guess_count,
            "max_attempts": self.max_attempts,
            "guesses": self.guesses.iter().map(|guess| serde_json::json!({
                "number": guess.number,
                "result": guess.result.as_str(),
                "at_ms": guess.at_ms,
            })).collect::<Vec<_>>(),
            "ended_at_ms": self.ended_at_ms,
        })
    }

    fn player_view(&self, _player_index: Option<usize>) -> Result<PlayerView, EngineError> {
        Ok(PlayerView {
            public: serde_json::json!({
                "status": self.phase,
                "min": self.min,
                "max": self.max,
                "guess_count": self.guess_count,
                "max_attempts": self.max_attempts,
                "remaining": self.max_attempts - self.guess_count,
                "last_result": self.last_result.map(GuessResult::as_str),
                "guesses": self.guesses.iter().map(|guess| serde_json::json!({
                    "number": guess.number,
                    "result": guess.result.as_str(),
                    "at_ms": guess.at_ms,
                })).collect::<Vec<_>>(),
            }),
            private: serde_json::json!({}),
            prompt: self.build_prompt(),
        })
    }

    fn handle_action(
        &mut self,
        action: &str,
        params: ActionParams,
    ) -> Result<PlayerView, EngineError> {
        match action {
            "start" => {
                // 无论当前是什么状态，start 都开一局新的
                self.reset();
                self.player_view(None)
            }
            "guess" => {
                let raw = params
                    .get("number")
                    .ok_or_else(|| EngineError::invalid("缺少必填参数: number"))?;
                self.guess(raw)?;
                self.player_view(None)
            }
            "state" => self.player_view(None),
            other => Err(EngineError::UnknownAction(other.to_owned())),
        }
    }

    fn check_timeout(&mut self, now: Instant) -> Result<Option<TimeoutEvent>, EngineError> {
        if self.phase == "in_progress"
            && now.saturating_duration_since(self.last_action) > IDLE_TIMEOUT
        {
            let elapsed = now.saturating_duration_since(self.started_at).as_millis();
            self.phase = "lost";
            self.ended_at_ms = Some(elapsed);
            self.signal.notify();
            return Ok(Some(TimeoutEvent::session_timeout()));
        }
        Ok(None)
    }

    fn signal(&self) -> Arc<ChangeSignal> {
        Arc::clone(&self.signal)
    }

    fn prompts(&self) -> &PromptStore {
        self.prompts
    }

    fn setup_params(&self) -> CreateParams {
        self.setup.clone()
    }

    fn describe_tools(&self) -> Vec<crate::engine::ToolSpec> {
        use serde_json::json;

        use crate::engine::ToolSpec;

        vec![
            ToolSpec::new(
                "guess_number_start",
                "开始一局新的猜数字游戏（1-100）。Agent 调用此工具创建对局并开始猜测。",
                json!({"type": "object", "properties": {}, "required": []}),
            ),
            ToolSpec::new(
                "guess_number_guess",
                "提交一个 1-100 之间的猜测数字。返回 too_high/too_low/correct 和剩余次数。\
                 游戏未结束时返回值引导你继续猜。",
                json!({
                    "type": "object",
                    "properties": {
                        "number": {
                            "type": "integer",
                            "description": "猜测数字（1-100）",
                            "minimum": 1,
                            "maximum": 100,
                        }
                    },
                    "required": ["number"],
                }),
            ),
            ToolSpec::new(
                "guess_number_state",
                "查看当前对局状态，包括所有历史猜测。不需要参数。",
                json!({"type": "object", "properties": {}, "required": []}),
            ),
        ]
    }
}

/// 默认终态集合（供测试断言"引擎声明的不是框架默认值"用）。
pub fn default_finished_states() -> &'static [&'static str] {
    DEFAULT_FINISHED_STATES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> GuessNumberEngine {
        GuessNumberEngine::new(&CreateParams::default(), &Config::default()).unwrap()
    }

    fn engine_with(raw: Value) -> Result<GuessNumberEngine, String> {
        let map = match raw {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        GuessNumberEngine::new(&CreateParams::new(map), &Config::default())
    }

    fn guess(engine: &mut GuessNumberEngine, number: Value) -> Result<PlayerView, EngineError> {
        let mut raw = serde_json::Map::new();
        raw.insert("number".to_owned(), number);
        engine.handle_action("guess", ActionParams::new(raw))
    }

    #[test]
    fn initial_state_is_in_progress_within_the_default_range() {
        let engine = engine();
        assert_eq!(engine.phase(), "in_progress");
        assert_eq!(engine.min(), 1);
        assert_eq!(engine.max(), 100);
        assert_eq!(engine.max_attempts(), 10);
        assert!(engine.target() >= 1 && engine.target() <= 100);
        assert_eq!(engine.guess_count(), 0);
        assert!(engine.guesses().is_empty());
    }

    #[test]
    fn guessing_reports_direction_and_tracks_history() {
        let mut engine = engine();
        let target = engine.target();
        let low = target - 1;
        if low >= 1 {
            let view = guess(&mut engine, Value::from(low)).unwrap();
            assert_eq!(view.public["last_result"], "too_low");
        }
        assert_eq!(engine.guess_count(), 1);
        assert_eq!(engine.guesses().len(), 1);
    }

    #[test]
    fn guessing_the_target_wins() {
        let mut engine = engine();
        let target = engine.target();
        let view = guess(&mut engine, Value::from(target)).unwrap();
        assert_eq!(engine.phase(), "won");
        assert!(view.prompt.contains("猜对了"), "{}", view.prompt);
    }

    #[test]
    fn running_out_of_attempts_loses() {
        let mut engine = engine();
        let target = engine.target();
        // 保证不猜中
        let wrong = if target == engine.min() {
            engine.min() + 1
        } else {
            engine.min()
        };
        for _ in 0..engine.max_attempts() {
            let _ = guess(&mut engine, Value::from(wrong));
        }
        assert_eq!(engine.phase(), "lost");
    }

    #[test]
    fn out_of_range_guesses_are_rejected() {
        let mut engine = engine();
        assert!(guess(&mut engine, Value::from(0)).is_err());
        assert!(guess(&mut engine, Value::from(101)).is_err());
        assert_eq!(engine.guess_count(), 0, "非法猜测不该计数");
    }

    #[test]
    fn bool_and_string_guesses_are_rejected() {
        let mut engine = engine();
        assert!(guess(&mut engine, Value::Bool(true)).is_err());
        assert!(guess(&mut engine, Value::String("50".into())).is_err());
    }

    #[test]
    fn integral_floats_are_accepted() {
        let mut engine = engine();
        let target = engine.target();
        let view = guess(
            &mut engine,
            serde_json::json!(f64::from(i32::try_from(target).unwrap())),
        )
        .unwrap();
        assert_eq!(engine.phase(), "won");
        assert!(view.prompt.contains("猜对了"));
    }

    #[test]
    fn fractional_floats_are_rejected() {
        let mut engine = engine();
        assert!(guess(&mut engine, serde_json::json!(3.5)).is_err());
    }

    #[test]
    fn start_resets_a_finished_game_with_a_new_target() {
        let mut engine = engine();
        let old_target = engine.target();
        let _ = guess(&mut engine, Value::from(old_target));
        assert_eq!(engine.phase(), "won");

        engine
            .handle_action("start", ActionParams::default())
            .unwrap();
        assert_eq!(engine.phase(), "in_progress");
        assert_eq!(engine.guess_count(), 0);
        assert!(engine.guesses().is_empty());
        assert_ne!(engine.target(), old_target, "重开一局必须换答案");
    }

    #[test]
    fn start_resets_an_in_progress_game() {
        let mut engine = engine();
        let low = engine.min();
        let _ = guess(&mut engine, Value::from(low));
        let before = engine.guess_count();
        assert!(before >= 0);

        engine
            .handle_action("start", ActionParams::default())
            .unwrap();
        assert_eq!(engine.guess_count(), 0);
        assert_eq!(engine.phase(), "in_progress");
    }

    #[test]
    fn custom_range_and_attempts_are_honoured() {
        let engine = engine_with(serde_json::json!({
            "min_num": 10, "max_num": 12, "max_attempts": 3
        }))
        .unwrap();
        assert_eq!(engine.min(), 10);
        assert_eq!(engine.max(), 12);
        assert_eq!(engine.max_attempts(), 3);
        assert!((10..=12).contains(&engine.target()));
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        assert!(engine_with(serde_json::json!({"min_num": 5, "max_num": 5})).is_err());
        assert!(engine_with(serde_json::json!({"min_num": 9, "max_num": 2})).is_err());
        assert!(engine_with(serde_json::json!({"max_attempts": 0})).is_err());
        assert!(engine_with(serde_json::json!({"max_num": "100"})).is_err());
    }

    #[test]
    fn action_after_the_game_ends_is_rejected() {
        let mut engine = engine();
        let target = engine.target();
        let _ = guess(&mut engine, Value::from(target));
        let err = guess(&mut engine, Value::from(target)).unwrap_err();
        assert!(err.to_string().contains("已结束"), "{err}");
    }

    #[test]
    fn missing_number_is_a_client_error() {
        let mut engine = engine();
        let err = engine
            .handle_action("guess", ActionParams::default())
            .unwrap_err();
        assert!(err.is_client_error());
        assert!(err.to_string().contains("number"), "{err}");
    }

    #[test]
    fn unknown_action_is_rejected() {
        let mut engine = engine();
        let err = engine
            .handle_action("nope", ActionParams::default())
            .unwrap_err();
        assert!(matches!(err, EngineError::UnknownAction(_)));
    }

    #[test]
    fn timeout_loses_the_game_exactly_once() {
        let mut engine = engine();
        let far_future = Instant::now() + IDLE_TIMEOUT * 2;
        let event = engine.check_timeout(far_future).unwrap();
        assert_eq!(event, Some(TimeoutEvent::session_timeout()));
        assert_eq!(engine.phase(), "lost");

        // 已经结束：不再产生事件
        assert_eq!(engine.check_timeout(far_future).unwrap(), None);
    }

    #[test]
    fn a_fresh_game_does_not_time_out() {
        let mut engine = engine();
        assert_eq!(engine.check_timeout(Instant::now()).unwrap(), None);
    }

    #[test]
    fn public_view_hides_the_target_admin_view_has_it() {
        let engine = engine();
        let public = engine.public_view();
        let admin = engine.admin_view();
        assert!(public.get("target").is_none(), "{public}");
        assert_eq!(admin["target"], engine.target());
    }

    #[test]
    fn player_view_hides_the_target_too() {
        let engine = engine();
        let view = engine.player_view(None).unwrap();
        assert!(view.public.get("target").is_none());
        assert!(view.prompt.contains("范围"), "{}", view.prompt);
        assert!(!view.prompt.contains("target"), "{}", view.prompt);
    }

    #[test]
    fn prompts_never_leak_degradation_markers() {
        let mut engine = engine();
        for _ in 0..engine.max_attempts() {
            let view = engine.player_view(None).unwrap();
            assert!(!view.prompt.contains("{missing:"), "{}", view.prompt);
            assert!(!view.prompt.contains("[unrendered]"), "{}", view.prompt);
            let low = engine.min();
            let _ = guess(&mut engine, Value::from(low));
        }
        let view = engine.player_view(None).unwrap();
        assert!(!view.prompt.contains("{missing:"), "{}", view.prompt);
    }

    #[test]
    fn finished_states_declare_only_won_and_lost() {
        let engine = engine();
        assert_eq!(engine.finished_states(), &["won", "lost"]);
        // 与框架默认集合不同，证明声明真的被用上了
        assert_ne!(engine.finished_states(), DEFAULT_FINISHED_STATES);
    }

    #[test]
    fn single_player_never_waits() {
        let engine = engine();
        // 单人游戏的默认 is_ready_for 永远为真，state 调用不会挂起
        assert!(engine.is_ready_for(0));
        assert_eq!(engine.player_mode(), PlayerMode::Single);
        assert!(engine.player_tokens().is_none());
    }
}

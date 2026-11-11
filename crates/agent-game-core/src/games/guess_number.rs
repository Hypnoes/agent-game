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

use agent_game_server_domain::game::{
    DEFAULT_FINISHED_STATES, GameEngine, PlayerMode, PlayerView, TimeoutEvent,
};
use agent_game_server_infra::config::Config;
use agent_game_server_infra::error::EngineError;
use agent_game_server_infra::params::{ActionParams, CreateParams};
use agent_game_server_infra::prompt::{Ctx, PromptStore};
use agent_game_server_infra::signal::ChangeSignal;

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

/// 猜数字的阶段。
///
/// 与 `liars_dice::Phase` 的差别：这里**没有大厅态**（单人游戏，创建即开局），
/// 且 `InProgress` 可以从**任意阶段**进入（玩家重开一局）。
///
/// # 为什么要有枚举
///
/// 字符串字段的问题是"错字能编译"。`self.phase = "in_progess"` 不会报错，
/// 只有 `is_finished()` 或 admin 页面归类时才露馅。枚举把错误提前到编译期。
///
/// 枚举**只约束取值，不约束转移**——`self.phase = Phase::Won` 从 `Won` 再赋
/// 一次照样能编译。转移合法性由 `transition_to` 管。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// 进行中。
    InProgress,
    /// 猜对了。
    Won,
    /// 机会用完或超时。
    Lost,
}

impl Phase {
    /// 对外 JSON 里的名字。**这是契约**——`admin.html` 靠它归类"进行中/已结束"。
    fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Won => "won",
            Self::Lost => "lost",
        }
    }

    /// 从自己转移到 `next` 是否合法。
    ///
    /// - 进行中 → 赢 / 输
    /// - **任意 → 进行中**：`reset()` 允许随时重开一局
    fn can_transition_to(self, next: Self) -> bool {
        match (self, next) {
            // 任意阶段都能重置回进行中（`reset()` 依赖这条）
            (_, Self::InProgress) => true,
            (Self::InProgress, Self::Won | Self::Lost) => true,
            _ => false,
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 猜数字引擎。每个实例管理一局游戏。
#[derive(Debug)]
pub struct GuessNumberEngine {
    game_type: &'static str,
    min: i64,
    max: i64,
    max_attempts: i64,
    phase: Phase,
    target: i64,
    guesses: Vec<Guess>,
    guess_count: i64,
    last_result: Option<GuessResult>,
    started_at: Instant,
    last_action: Instant,
    ended_at_ms: Option<u128>,
    signal: Arc<ChangeSignal>,
    prompts: &'static PromptStore,
    /// `roll_target` **实际收到**的 previous（每次调用都刷新）。
    ///
    /// 只给测试用。记录点放在 `roll_target` 里而不是 `reset` 里，是为了让
    /// "记录"与"消费"是同一个值——否则 `reset` 记了 `None`、却把
    /// `Some(target)` 传给 `roll_target`，测试仍会看到 `None` 而放过这个缺陷
    /// （review 指出过这个组合缺口，实测确认存在）。
    last_roll_previous: Option<i64>,
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
            phase: Phase::InProgress,
            target: 0,
            guesses: Vec::new(),
            guess_count: 0,
            last_result: None,
            started_at: now,
            last_action: now,
            ended_at_ms: None,
            signal: Arc::new(ChangeSignal::new()),
            prompts: shared_prompts(),
            last_roll_previous: None,
        };
        engine.roll_target(None);
        Ok(engine)
    }

    #[must_use]
    pub fn min(&self) -> i64 {
        self.min
    }

    /// 上一次行动的时刻。
    ///
    /// 只给测试用：`check_timeout` 的判据是 `now - last_action > IDLE_TIMEOUT`，
    /// 而 `last_action` 是**构造时刻**。测试若拿自己的 `Instant::now()` 去推边界，
    /// 会漏掉"构造到这里"的时钟差——高负载下那几微秒足以让边界断言翻面
    /// （`millisecond_bounds.rs` 有过一次真实的 flaky）。
    ///
    /// 拿这个基准去加偏移，边界才是精确的。生产路径永远不调它。
    #[doc(hidden)]
    #[must_use]
    pub fn last_action(&self) -> Instant {
        self.last_action
    }

    /// `roll_target` 最近一次实际收到的 previous。
    ///
    /// 只给测试用：这条"阶段 → previous"的接线是重置流程里唯一有分支的地方，
    /// 而它的效果（target 换不换）在随机源下不可确定观察——进行中重置时
    /// `roll_target(None)` 允许抽到同一个数。把**实际入参**记下来，测试才能
    /// 确定性地断言接线走对了，不依赖"反复重置看 target 变不变"这种概率判断。
    ///
    /// 只读：改成 `pub` 字段会让调用方能改它，测试就失去意义。
    #[doc(hidden)]
    #[must_use]
    pub fn last_roll_previous(&self) -> Option<i64> {
        self.last_roll_previous
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
        self.last_roll_previous = previous;
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
        let previous = Self::carry_over_target(self.phase, self.target);
        // reset 允许从任意阶段进入 `InProgress`（`can_transition_to` 里那条
        // `(_, Phase::InProgress)`），所以这里**不可能**失败。仍然调
        // `transition_to` 而不是直接赋值，是为了让"所有阶段赋值走同一个入口"
        // 这条规矩不留例外；`let _ =` 表示"我知道它不会失败"。
        let _ = self.transition_to(Phase::InProgress);
        self.roll_target(previous);
        self.guesses.clear();
        self.guess_count = 0;
        self.last_result = None;
        self.ended_at_ms = None;
        self.started_at = Instant::now();
        self.last_action = self.started_at;
        self.signal.notify();
    }

    /// 切换阶段。**所有**阶段赋值都必须走这里。
    ///
    /// 返回 `Result` 而不是 `assert!`：引擎 panic 会被 `GameManager::act` 的
    /// `catch_unwind` 捕获并**毒化整个对局**，而非法转移是可以用正常错误路径
    /// 处理的。
    ///
    /// # Errors
    /// 转移不合法时返回 `EngineError::invalid`。
    fn transition_to(&mut self, next: Phase) -> Result<(), EngineError> {
        if !self.phase.can_transition_to(next) {
            return Err(EngineError::invalid(format!(
                "非法状态转移：{} → {}",
                self.phase.as_str(),
                next.as_str()
            )));
        }
        self.phase = next;
        Ok(())
    }

    /// 重置时要不要把上一局的 target 带过去"避开"。
    ///
    /// - 进行中重置：`None` —— 不强制换答案（玩家可能只是想清空猜数记录）
    /// - 已结束重置：`Some(上一局答案)` —— 保证换一个，别让新的一局沿用旧答案
    ///
    /// 抽成关联函数是为了让这个**分支决策**可以被确定性测试。之前它内联在
    /// `reset()` 里，测试只能靠"反复重置看 target 变不变"来间接推断，那依赖
    /// 概率。现在可以直接断言返回值。
    fn carry_over_target(phase: Phase, target: i64) -> Option<i64> {
        if phase == Phase::InProgress {
            None
        } else {
            Some(target)
        }
    }

    fn end(&mut self, phase: Phase) {
        // `end` 只从"进行中"调，两条转移都合法；失败意味着调用方放错了位置，
        // 属于代码 bug。用 `?` 传出去，不 panic。
        let _ = self.transition_to(phase);
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
        if self.phase != Phase::InProgress {
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
            self.end(Phase::Won);
        } else if self.guess_count >= self.max_attempts {
            self.end(Phase::Lost);
        } else {
            // 猜一次也是状态变更：等待方（如果有）要看到新的历史
            self.signal.notify();
        }
        Ok(())
    }

    fn build_prompt(&self) -> String {
        match self.phase {
            Phase::Won => {
                let last = self.guesses.last().map_or(0, |guess| guess.number);
                self.render(
                    "won",
                    &Ctx::new()
                        .with("last_guess", last)
                        .with("count", self.guess_count),
                )
            }
            Phase::Lost => {
                let history: Vec<i64> = self.guesses.iter().map(|guess| guess.number).collect();
                self.render(
                    "lost",
                    &Ctx::new()
                        .with("max_attempts", self.max_attempts)
                        .with("target", self.target)
                        .with("history", history),
                )
            }
            Phase::InProgress => match self.guesses.last() {
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
        self.phase.as_str()
    }

    fn admin_view(&self) -> Value {
        serde_json::json!({
            "status": self.phase.as_str(),
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
            "status": self.phase.as_str(),
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
                "status": self.phase.as_str(),
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
        if self.phase == Phase::InProgress
            && now.saturating_duration_since(self.last_action) > IDLE_TIMEOUT
        {
            let elapsed = now.saturating_duration_since(self.started_at).as_millis();
            self.transition_to(Phase::Lost)?;
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
}

/// 默认终态集合（供测试断言"引擎声明的不是框架默认值"用）。
pub fn default_finished_states() -> &'static [&'static str] {
    DEFAULT_FINISHED_STATES
}

/// 这个游戏暴露的 MCP 工具。
///
/// 纯静态数据，与引擎实例无关——所以住在注册元数据（`GameMeta::tools`）里，
/// 不是 `GameEngine` 的方法。见 `GameMeta::tools` 的文档说明。
#[must_use]
pub fn tools() -> Vec<agent_game_server_domain::game::ToolSpec> {
    use agent_game_server_domain::game::ToolSpec;
    use serde_json::json;

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
        assert!(engine.guesses().is_empty(), "开局前不该有猜数记录");
    }

    /// 猜数与答案的大小关系必须正确反映在 `last_result` 上，并记进历史。
    ///
    /// 原写法用默认区间（1..=100）然后 `let low = target - 1`，再
    /// `if low >= 1 { 猜 }`——target 抽到 1 时（概率 1%）那一步被跳过，
    /// 但后面仍断言 `guess_count == 1`，于是失败。CI 上偶发红过。
    ///
    /// 更糟的是：即使不失败，两个方向里也只有一个被走到。改成对**每个**
    /// 方向都用固定区间构造，两个分支必然各跑一次。
    #[test]
    fn guessing_reports_direction_and_tracks_history() {
        // 区间 2..=3 里猜 2：若 target=3 则 too_low，若 target=2 则 correct。
        // 两种结果都是确定可枚举的，不依赖运气。
        let mut low_engine =
            engine_with(serde_json::json!({ "min_num": 2, "max_num": 3 })).unwrap();
        let view = guess(&mut low_engine, Value::from(2)).unwrap();
        let expected_low = low_engine.target() > 2;
        assert_eq!(
            view.public["last_result"],
            if expected_low { "too_low" } else { "correct" },
            "猜 2 在区间 2..=3 里只能是 too_low 或 correct"
        );
        assert_eq!(low_engine.guess_count(), 1);
        assert_eq!(low_engine.guesses().len(), 1);

        // 反方向：区间 2..=3 里猜 3，只能是 too_high 或 correct
        let mut high_engine =
            engine_with(serde_json::json!({ "min_num": 2, "max_num": 3 })).unwrap();
        let view = guess(&mut high_engine, Value::from(3)).unwrap();
        let expected_high = high_engine.target() < 3;
        assert_eq!(
            view.public["last_result"],
            if expected_high { "too_high" } else { "correct" },
            "猜 3 在区间 2..=3 里只能是 too_high 或 correct"
        );

        // 上面两条覆盖"方向正确"，这一条覆盖"猜中"：用已知 target 直接猜
        let mut exact = engine_with(serde_json::json!({ "min_num": 2, "max_num": 3 })).unwrap();
        let known = exact.target();
        let view = guess(&mut exact, Value::from(known)).unwrap();
        assert_eq!(view.public["last_result"], "correct");
        assert_eq!(exact.phase(), "won");
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
        assert!(engine.guesses().is_empty(), "开局前不该有猜数记录");
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

    /// `reset()` 传给 `roll_target` 的 previous 必须随阶段变化。
    ///
    /// - 进行中：`None` —— 不强制换答案
    /// - 已结束：`Some(上一局答案)` —— 保证换一个
    ///
    /// 这条守的是 `reset` 里的分支接线。变异测试把它反过来时既有用例全部存活：
    /// `start_resets_an_in_progress_game` 只验计数清零，
    /// `start_resets_a_finished_game_with_a_new_target` 只验"换答案"，
    /// 没有人验证**进行中重置不保证换答案**。
    ///
    /// **为什么读 `last_roll_previous()` 而不是看 target 变没变**：
    /// 进行中重置时 `roll_target(None)` 允许抽到同一个数，所以"target 没变"
    /// 只是概率事件。第一版写"反复重置 32 次看有没有不变"，有 2^-32 的 flake
    /// 概率（被 review 指出）。记录输入之后，断言变成完全确定性的。
    #[test]
    fn reset_feeds_the_right_previous_to_roll_target() {
        let mut engine = engine();

        // 进行中重置：previous 必须是 None
        engine
            .handle_action("start", ActionParams::default())
            .unwrap();
        assert_eq!(
            engine.last_roll_previous(),
            None,
            "进行中重置不该带上一局答案"
        );

        // 已结束重置：previous 必须是 Some(上一局答案)
        let target = engine.target();
        let _ = guess(&mut engine, Value::from(target));
        assert_eq!(engine.phase(), "won");

        engine
            .handle_action("start", ActionParams::default())
            .unwrap();
        assert_eq!(
            engine.last_roll_previous(),
            Some(target),
            "已结束重置要带上答案，好让 roll_target 避开它"
        );
    }

    /// 端到端：已结束重置后答案确实换了。
    ///
    /// 上一条验的是"传对了输入"，这一条验"输入确实产生了效果"——两者缺一不可。
    /// 只测输入可能是"传了个没用的值"；只测效果则无法确定性断言进行中那条路径。
    #[test]
    fn resetting_after_a_finish_actually_rotates_the_target() {
        let mut engine = engine_with(serde_json::json!({ "min_num": 1, "max_num": 2 })).unwrap();
        engine
            .handle_action("start", ActionParams::default())
            .unwrap();

        let target = engine.target();
        let _ = guess(&mut engine, Value::from(target));
        assert_eq!(engine.phase(), "won");

        // 2 个候选，`Some(old)` 路径保证换成另一个
        engine
            .handle_action("start", ActionParams::default())
            .unwrap();
        assert_ne!(engine.target(), target, "已结束后重开必须换答案");
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

    // ═══════════════════════════════════════════
    // 状态转移的合法性
    // ═══════════════════════════════════════════

    /// 非法转移返回 `Err`，不 panic。
    ///
    /// 与 `liars_dice` 的差别：这里**允许**从任意阶段重置回 `InProgress`，
    /// 所以"非法"只剩终点是 `Won` / `Lost` 的那几条。
    #[test]
    fn illegal_transitions_return_an_error_instead_of_panicking() {
        let mut engine = engine();
        assert_eq!(engine.phase(), "in_progress");

        // `InProgress -> InProgress` 是**合法**的：`can_transition_to` 里有
        // `(_, Self::InProgress)`，`reset()` 靠它从任意阶段重开。所以这里
        // 不该报错——这条断言是"确认设计意图"，不是"确认非法"。
        engine
            .transition_to(Phase::InProgress)
            .expect("重置回进行中必须允许，reset() 依赖它");

        // 进入 Won 之后，Won -> Lost 不合法
        let target = engine.target();
        let _ = guess(&mut engine, Value::from(target));
        assert_eq!(engine.phase(), "won");
        assert!(engine.transition_to(Phase::Lost).is_err());
        assert_eq!(engine.phase(), "won", "状态不该被改坏");
    }

    /// **任意阶段都能重置回进行中**——这是 `reset()` 依赖的那条。
    ///
    /// 如果这条被当成非法（比如写成只允许 `InProgress -> InProgress`），
    /// "再玩一局"会坏掉。
    #[test]
    fn any_phase_can_reset_back_to_in_progress() {
        // 从 Won 重置
        let mut engine = engine();
        let target = engine.target();
        let _ = guess(&mut engine, Value::from(target));
        assert_eq!(engine.phase(), "won");
        engine
            .handle_action("start", ActionParams::default())
            .unwrap();
        assert_eq!(engine.phase(), "in_progress", "赢过之后必须能重开");

        // 从 Lost 重置
        let wrong = |e: &GuessNumberEngine| {
            let target = e.target();
            if target == e.min() {
                target + 1
            } else {
                target - 1
            }
        };
        let mut lost_engine = engine_with(serde_json::json!({
            "min_num": 1, "max_num": 2, "max_attempts": 1
        }))
        .unwrap();
        let bad = wrong(&lost_engine);
        let _ = guess(&mut lost_engine, Value::from(bad));
        assert_eq!(lost_engine.phase(), "lost", "机会用完应当判负");
        lost_engine
            .handle_action("start", ActionParams::default())
            .unwrap();
        assert_eq!(lost_engine.phase(), "in_progress", "输过之后必须能重开");
    }

    /// 已结束之后再猜，错误消息里要带上**当前阶段名**。
    ///
    /// 这条守 `impl Display for Phase`。变异测试第一轮发现：把 `fmt` 改成
    /// `Ok(Default::default())`（输出空串）没有用例变红——因为 `Display`
    /// 只在这个错误分支里用，而那条分支一直没被覆盖。
    #[test]
    fn guessing_after_the_game_ended_names_the_current_phase() {
        let mut engine = engine();
        let target = engine.target();
        let _ = guess(&mut engine, Value::from(target));
        assert_eq!(engine.phase(), "won");

        let err = guess(&mut engine, Value::from(target)).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("won"),
            "错误消息必须带出当前阶段名（Display 的输出），实际: {text}"
        );
    }
}

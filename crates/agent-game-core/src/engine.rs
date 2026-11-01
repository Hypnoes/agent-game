//! 引擎协议。
//!
//! 设计要点，逐条对应 Python 版踩过的坑：
//!
//! 1. **`admin_view` 与 `public_view` 分别构造**，不再"返回一份全量 JSON、
//!    再按名字删掉秘密键"。Python 版的剥离是
//!    `{k: v for k, v in state.items() if k not in secrets}`：`secret_keys` 里
//!    拼错一个字母（`all_dices`），秘密就原样漏给 Agent，运行期没有任何反应。
//!    改成构造之后，写漏一个字段的后果是"测试红"，不是"静默泄密"。
//! 2. **纯同步**。等待语义上移到框架的 [`crate::wait`]，引擎只回答"现在轮到他了吗"
//!    与"变了就叫我"。于是 `dyn GameEngine` 可以直接用，不需要 `async-trait`、
//!    不需要 `Box<dyn Future>`。
//! 3. **`check_timeout` 返回 `Result`**。布尔返回值无法区分"没事"与"我坏了"，
//!    Python 版靠 `except Exception` 兜住坏引擎；这里让类型表达同一件事。
//! 4. 可选能力用默认实现表达。Python 版把可选方法的签名放进 `if TYPE_CHECKING:`
//!    是为了避免 `hasattr` 永远为真——那是动态语言的补丁，Rust 里不需要。

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::error::EngineError;
use crate::prompt::{Ctx, PromptStore};
use crate::signal::ChangeSignal;

/// 引擎没有声明时的默认终态名。
pub const DEFAULT_FINISHED_STATES: &[&str] = &["finished", "won", "lost"];

/// 引擎没有声明时的默认敏感字段（仅供测试用作期望清单）。
pub const DEFAULT_SENSITIVE_KEYS: &[&str] = &["target", "all_dice"];

/// 单人还是多人。多人引擎持有一个 `players` 列表，动作必须带 `player_token`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerMode {
    Single,
    Multi,
}

impl PlayerMode {
    #[must_use]
    pub fn is_multi(self) -> bool {
        matches!(self, Self::Multi)
    }
}

/// 创建参数：请求体里的任意 JSON，由各引擎自己解释。
#[derive(Clone, Debug, Default)]
pub struct CreateParams {
    pub raw: serde_json::Map<String, Value>,
}

impl CreateParams {
    #[must_use]
    pub fn new(raw: serde_json::Map<String, Value>) -> Self {
        Self { raw }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    /// 取一个整数参数。
    ///
    /// # Errors
    /// 键存在但不是整数（`bool` 不被当作整数）。
    pub fn get_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.raw.get(key) {
            None => Ok(None),
            Some(Value::Number(number)) => number
                .as_i64()
                .map(Some)
                .ok_or_else(|| format!("{key} 必须是整数，收到 {number}")),
            Some(other) => Err(format!("{key} 必须是整数，收到 {other}")),
        }
    }

    /// 取一个字符串参数。
    ///
    /// # Errors
    /// 键存在但不是字符串。
    pub fn get_str(&self, key: &str) -> Result<Option<&str>, String> {
        match self.raw.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.as_str())),
            Some(other) => Err(format!("{key} 必须是字符串，收到 {other}")),
        }
    }

    /// 去掉某些键（`player_token` 不参与配置比较）。
    #[must_use]
    pub fn without(&self, key: &str) -> Self {
        let mut raw = self.raw.clone();
        raw.remove(key);
        Self { raw }
    }
}

impl From<serde_json::Map<String, Value>> for CreateParams {
    fn from(raw: serde_json::Map<String, Value>) -> Self {
        Self { raw }
    }
}

/// 动作参数。`timeout` 在这里已经被剥掉——它只属于传输层的等待预算。
#[derive(Clone, Debug, Default)]
pub struct ActionParams {
    pub raw: serde_json::Map<String, Value>,
}

impl ActionParams {
    #[must_use]
    pub fn new(raw: serde_json::Map<String, Value>) -> Self {
        Self { raw }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    /// 取必填整数。
    ///
    /// # Errors
    /// 缺失、或不是整数。
    pub fn require_i64(&self, key: &str) -> Result<i64, EngineError> {
        match self.raw.get(key) {
            None => Err(EngineError::invalid(format!("缺少必填参数: {key}"))),
            Some(Value::Number(number)) => number
                .as_i64()
                .ok_or_else(|| EngineError::invalid(format!("{key} 必须是整数，收到 {number}"))),
            Some(other) => Err(EngineError::invalid(format!(
                "{key} 必须是整数，收到 {other}"
            ))),
        }
    }

    /// 取可选整数。
    ///
    /// # Errors
    /// 存在但不是整数。
    pub fn optional_i64(&self, key: &str) -> Result<Option<i64>, EngineError> {
        match self.raw.get(key) {
            None => Ok(None),
            Some(Value::Number(number)) => Ok(number.as_i64()),
            Some(other) => Err(EngineError::invalid(format!(
                "{key} 必须是整数，收到 {other}"
            ))),
        }
    }

    /// 取必填字符串。
    ///
    /// # Errors
    /// 缺失、或不是字符串。
    pub fn require_str(&self, key: &str) -> Result<&str, EngineError> {
        match self.raw.get(key) {
            None => Err(EngineError::invalid(format!("缺少必填参数: {key}"))),
            Some(Value::String(text)) => Ok(text),
            Some(other) => Err(EngineError::invalid(format!(
                "{key} 必须是字符串，收到 {other}"
            ))),
        }
    }

    /// 取可选字符串（缺失与 `null` 都当没有）。
    ///
    /// # Errors
    /// 存在但不是字符串。
    pub fn optional_str(&self, key: &str) -> Result<Option<&str>, EngineError> {
        match self.raw.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(other) => Err(EngineError::invalid(format!(
                "{key} 必须是字符串，收到 {other}"
            ))),
        }
    }

    /// 取玩家编号。多人动作必填，单人动作可省略。
    ///
    /// # Errors
    /// 存在但不是非负整数。
    pub fn player_index(&self) -> Result<Option<usize>, EngineError> {
        match self.raw.get("player_index") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => {
                let Some(value) = number.as_i64() else {
                    return Err(EngineError::InvalidIndex(number.to_string()));
                };
                if value < 0 {
                    // 负索引会绕回最后一名玩家，等于凭空拿到对手的骰子
                    return Err(EngineError::InvalidIndex(value.to_string()));
                }
                Ok(Some(usize::try_from(value).unwrap_or(usize::MAX)))
            }
            Some(other) => Err(EngineError::InvalidIndex(other.to_string())),
        }
    }

    /// 取 `player_token`。
    ///
    /// # Errors
    /// 存在但不是字符串。
    pub fn player_token(&self) -> Result<Option<&str>, EngineError> {
        self.optional_str("player_token")
    }
}

/// 玩家视角：公开状态 + 私有信息 + 渲染好的提示词。
#[derive(Clone, Debug, Default)]
pub struct PlayerView {
    pub public: Value,
    pub private: Value,
    pub prompt: String,
}

impl PlayerView {
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "public": self.public,
            "private": self.private,
            "prompt": self.prompt,
        })
    }
}

/// 超时结算产生的事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeoutEvent {
    /// 事件类型：`turn_timeout` / `lobby_timeout` / `session_timeout`。
    pub kind: &'static str,
    /// 该事件是否要求立刻回收 session（大厅超时、单人超时）。
    pub expired: bool,
    /// 被判负的玩家（回合超时才有）。
    pub loser: Option<usize>,
}

impl TimeoutEvent {
    #[must_use]
    pub fn turn_timeout(loser: usize) -> Self {
        Self {
            kind: "turn_timeout",
            expired: false,
            loser: Some(loser),
        }
    }

    #[must_use]
    pub fn lobby_timeout() -> Self {
        Self {
            kind: "lobby_timeout",
            expired: true,
            loser: None,
        }
    }

    #[must_use]
    pub fn session_timeout() -> Self {
        Self {
            kind: "session_timeout",
            expired: true,
            loser: None,
        }
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        object.insert("type".to_owned(), Value::String(self.kind.to_owned()));
        if self.expired {
            object.insert("expired".to_owned(), Value::Bool(true));
        }
        if let Some(loser) = self.loser {
            object.insert("loser".to_owned(), Value::from(loser));
        }
        Value::Object(object)
    }
}

/// 引擎协议。
///
/// 所有方法都是同步的；需要等待的能力由 [`GameEngine::signal`] 与
/// [`GameEngine::is_ready_for`] 提供，循环在 [`crate::wait::wait_for`]。
pub trait GameEngine: std::fmt::Debug + Send {
    /// 机器键。必须与注册表里的键一致，也是 MCP 工具名前缀。
    fn game_type(&self) -> &'static str;

    /// 单人还是多人。
    fn player_mode(&self) -> PlayerMode;

    /// 该引擎的终态名集合。
    fn finished_states(&self) -> &'static [&'static str] {
        DEFAULT_FINISHED_STATES
    }

    /// 公开视图里**必须不存在**的敏感字段。仅供测试当作期望清单使用，
    /// 生产路径不读它——公开视图由 [`GameEngine::public_view`] 构造。
    fn sensitive_keys(&self) -> &'static [&'static str] {
        DEFAULT_SENSITIVE_KEYS
    }

    /// 当前状态名（`waiting` / `bidding` / `finished` / `won` / `lost` …）。
    fn phase(&self) -> &'static str;

    /// 对局是否已结束。
    fn is_finished(&self) -> bool {
        self.finished_states().contains(&self.phase())
    }

    /// admin 视图：含全部秘密信息，只有鉴权通过的 admin 出口能拿到。
    fn admin_view(&self) -> Value;

    /// 公开视图：**由构造保证**不含敏感字段。
    fn public_view(&self) -> Value;

    /// 玩家视角。多人引擎必须先由调用方完成身份校验。
    ///
    /// # Errors
    /// 玩家编号越界等。
    fn player_view(&self, player_index: Option<usize>) -> Result<PlayerView, EngineError>;

    /// 执行一个动作。
    ///
    /// # Errors
    /// 未知动作、参数非法、阶段不允许、身份不匹配等。
    fn handle_action(
        &mut self,
        action: &str,
        params: ActionParams,
    ) -> Result<PlayerView, EngineError>;

    /// 超时结算。
    ///
    /// # Errors
    /// 引擎内部故障。调用方必须隔离它，不能让一个坏引擎中断整轮结算。
    fn check_timeout(&mut self, now: Instant) -> Result<Option<TimeoutEvent>, EngineError>;

    /// 状态变更信号。
    ///
    /// 返回 `Arc` 而不是 `&`：等待必须**在锁外**进行（持锁跨 await 会让
    /// 同一对局的所有请求排队），而 `&` 离开锁就借不到了。`Arc` 让调用方能
    /// "取一次引用、放锁、拿它去等"。
    fn signal(&self) -> Arc<ChangeSignal>;

    /// 等待条件：现在轮到他了吗（或对局已结束）。
    ///
    /// 单人即时游戏用默认实现——永不等待。
    fn is_ready_for(&self, _player_index: usize) -> bool {
        true
    }

    /// 按 token 查玩家编号，用于断线重连。
    fn find_player_by_token(&self, _token: &str) -> Option<usize> {
        None
    }

    /// 多人引擎的玩家 token 列表，交给 [`crate::identity`] 校验。
    fn player_tokens(&self) -> Option<Vec<String>> {
        None
    }

    /// 提示词仓库（热重载用）。
    fn prompts(&self) -> &PromptStore;

    /// 创建时的配置参数，用于"同一张桌子"的匹配判断。
    fn setup_params(&self) -> CreateParams;

    /// 该引擎声明的 MCP 工具：`[(name, description, input_schema)]`。
    ///
    /// **默认空表**：引擎可以不提供工具面（比如纯内部用途）。但一旦提供，
    /// 工具名必须以 `game_type_` 为前缀——框架用这个前缀把工具调用路由回来。
    ///
    /// 放在协议里而不是写死在 MCP 适配层，是"零改动加游戏"的关键：新增一款
    /// 游戏只需实现这个方法和 `GameMeta` 注册，工具列表、Schema、文档全部自动生效。
    /// 写死在适配层会让新游戏的工具永远返回"未知工具"。
    fn describe_tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// 渲染一次提示词。
    fn render(&self, name: &str, ctx: &Ctx) -> String {
        self.prompts().render(name, ctx)
    }
}

/// 一个 MCP 工具声明。
///
/// 与 `rmcp::model::Tool` 分开定义，是为了让 core 不依赖 rmcp：引擎只描述
/// "我要暴露什么"，由适配层负责翻译成具体协议的 `Tool`。
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    /// 完整工具名，必须以 `<game_type>_` 开头。
    pub name: String,
    pub description: String,
    /// JSON Schema 对象。
    pub input_schema: Value,
}

impl ToolSpec {
    /// 构造一个工具声明。
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// 装箱后的引擎。
pub type BoxedEngine = Box<dyn GameEngine>;

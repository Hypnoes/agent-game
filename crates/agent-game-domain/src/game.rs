//! 游戏抽象：引擎协议与它交换的类型。
//!
//! 设计要点，逐条对应 Python 版踩过的坑：
//!
//! 1. **`admin_view` 与 `public_view` 分别构造**，不再"返回一份全量 JSON、
//!    再按名字删掉秘密键"。旧版的剥离是
//!    `{k: v for k, v in state.items() if k not in secrets}`：秘密名单里
//!    拼错一个字母（`all_dices`），秘密就原样漏给 Agent，运行期没有任何反应。
//!    改成构造之后，写漏一个字段的后果是"测试红"，不是"静默泄密"。
//! 2. **纯同步**。等待语义上移到框架的 [`agent_game_server_infra::wait`]，引擎只回答
//!    "现在轮到他了吗"与"变了就叫我"。于是 `dyn GameEngine` 可以直接用，
//!    不需要 `async-trait`、不需要 `Box<dyn Future>`。
//! 3. **`check_timeout` 返回 `Result`**。布尔返回值无法区分"没事"与"我坏了"，
//!    旧版靠 `except Exception` 兜住坏引擎；这里让类型表达同一件事。
//! 4. 可选能力用默认实现表达。旧版把可选方法的签名放进 `if TYPE_CHECKING:`
//!    是为了避免 `hasattr` 永远为真——那是动态语言的补丁，Rust 里不需要。

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use agent_game_server_infra::error::EngineError;
use agent_game_server_infra::params::ActionParams;
use agent_game_server_infra::prompt::{Ctx, PromptStore};
use agent_game_server_infra::signal::ChangeSignal;

/// 引擎没有声明时的默认终态名。
pub const DEFAULT_FINISHED_STATES: &[&str] = &["finished", "won", "lost"];

/// 引擎没有声明时的默认敏感字段（仅供测试用作期望清单）。
pub const DEFAULT_SENSITIVE_KEYS: &[&str] = &["target", "all_dice"];

/// 「等待玩家入场」阶段的默认名字。
///
/// 抽成常量而不是把字面量直接写在 trait 默认方法里，是为了让测试替身与
/// 各引擎的覆盖实现都能引用同一个值，不用三处各写一份字符串。
pub const DEFAULT_WAITING_PHASE: &str = "waiting";

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
/// [`GameEngine::is_ready_for`] 提供，循环在 [`agent_game_server_infra::wait::wait_for`]。
pub trait GameEngine: std::fmt::Debug + Send {
    /// 机器键。必须与注册表里的键一致，也是 MCP 工具名前缀。
    fn game_type(&self) -> &'static str;

    /// 单人还是多人。
    ///
    /// 默认单人：多数游戏是单人即时对局，不必为它写一行。
    fn player_mode(&self) -> PlayerMode {
        PlayerMode::Single
    }

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

    /// 这个引擎的"等待玩家入场"阶段叫什么。
    ///
    /// 大厅匹配的判据是"当前阶段是不是大厅态"（`lobby.rs`）。这个名字由引擎
    /// 自己声明，框架只做比较——与 [`GameEngine::finished_states`] 自报终结态名
    /// 是同一个思路。
    ///
    /// 默认 `"waiting"`：多数多人游戏这么叫。**单人游戏用不上它**——它们
    /// `player_mode()` 是 `Single`，永远走"新建一局"而不是"并桌"。
    ///
    /// 之所以返回 `&'static str` 而不是 `bool`：返回 `bool` 的话每个引擎都要写
    /// 一个 `fn is_waiting(&self) -> bool`，多一行样板，且与
    /// `finished_states()` 的风格不一致。
    fn waiting_phase(&self) -> &'static str {
        DEFAULT_WAITING_PHASE
    }

    /// 对局是否已结束。
    fn is_finished(&self) -> bool {
        self.finished_states().contains(&self.phase())
    }

    /// admin 视图：含全部秘密信息，只有鉴权通过的 admin 出口能拿到。
    ///
    /// 默认转调 [`GameEngine::public_view`]：没有秘密字段的引擎不需要把同一个
    /// 视图写两遍。
    fn admin_view(&self) -> Value {
        self.public_view()
    }

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
    /// 默认无事发生：没有回合制超时概念的引擎不必写这一行。
    ///
    /// # Errors
    /// 引擎内部故障。调用方必须隔离它，不能让一个坏引擎中断整轮结算。
    fn check_timeout(&mut self, _now: Instant) -> Result<Option<TimeoutEvent>, EngineError> {
        Ok(None)
    }

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

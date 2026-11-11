//! 大话骰（Liar's Dice / 吹牛）引擎。
//!
//! 规则：
//! - 每人 `dice_per_player` 颗骰子（默认 5），只能看自己的
//! - 轮流叫数：宣称全场某点数至少有多少颗
//! - 1 是百搭（可代替任意点数），但叫 1 时 1 只算 1
//! - 下家可选"加码"或"开"（揭盅）
//! - 实际 ≥ 叫数 → 开的人输；实际 < 叫数 → 叫的人输
//! - 叫数必须自身合法：数量 ≥ 1、面值 1-6、数量不超过场上骰子总数
//!   （超过即按构造必输），且必须比上家大。不能开自己的盅。
//! - 身份校验：`state` / `bid` / `challenge` 必须带 `player_token`，只有 `join`
//!   可以不带（新玩家入场）。大话骰的全部玩法建立在私有信息上。
//!
//! 淘汰规则（输一轮的代价）：
//! - 场上还有 3 名及以上存活玩家：输家本轮只掉一颗骰子，骰子掉光才出局，
//!   对局继续；本轮叫数清零，由输家先叫下一轮（输家已出局则由下一位存活玩家先叫）。
//! - 场上只剩 2 名存活玩家：突然死亡——输一轮直接出局（骰子清零）。
//! - 回合超时按同样的淘汰规则处理：超时者视为输掉本轮（掉骰/出局），但回合直接交给
//!   下一位存活玩家——超时者没有叫数，让他接着叫下一轮只会继续空转。
//! - 只剩一名存活玩家时对局结束，该玩家即 `winner`。

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rand::RngExt as _;
use serde_json::Value;

use agent_game_server_domain::game::{GameEngine, PlayerMode, PlayerView, TimeoutEvent};
use agent_game_server_infra::config::Config;
use agent_game_server_infra::error::EngineError;
use agent_game_server_infra::identity::check_player_identity;
use agent_game_server_infra::params::{ActionParams, CreateParams};
use agent_game_server_infra::prompt::{Ctx, PromptStore};
use agent_game_server_infra::signal::ChangeSignal;

const PROMPT_TOML: &str = include_str!("../../assets/liars_dice_prompts.toml");

/// 大厅匹配超时。
const LOBBY_TIMEOUT: Duration = Duration::from_secs(120);

fn shared_prompts() -> &'static PromptStore {
    static PROMPTS: OnceLock<PromptStore> = OnceLock::new();
    PROMPTS.get_or_init(|| PromptStore::new(PROMPT_TOML))
}

/// 大话骰的阶段。三个取值覆盖全部生命周期，没有第四个。
///
/// # 为什么要有这个枚举而不是继续用 `&'static str`
///
/// 字符串字段的两个问题：
///
/// 1. **错字没有编译期保护**。`self.phase = "biddng"` 能编译、能跑，
///    只有大厅匹配或 admin 页面归类时才露馅
/// 2. **比较是字符串比较**，散在十几个地方（`== "waiting"`、`!= "bidding"`…），
///    改一个名字要全文搜
///
/// 枚举只解决"取值有限"，**不解决"转移合法"**——`self.phase = Phase::Finished`
/// 从 `Waiting` 直接跳过去照样能编译。所以另配 [`Self::can_transition_to`]
/// 与 `LiarsDiceEngine::transition_to`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// 等人入场。
    Waiting,
    /// 已开局、正在叫数。
    Bidding,
    /// 只剩一名存活玩家（或对局被判定结束）。
    Finished,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Phase {
    /// 对外 JSON 里的名字。**这是契约**——`admin.html` 靠它归类对局，
    /// 改了会让页面静默归类错。
    fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Bidding => "bidding",
            Self::Finished => "finished",
        }
    }

    /// 从自己转移到 `next` 是否合法。
    ///
    /// 大话骰的合法转移只有两条：等人大厅里满员开局、以及开局后分出胜负。
    /// 反向（`Finished → Bidding`）与跳跃（`Waiting → Finished`）都不许。
    fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Waiting, Self::Bidding) | (Self::Bidding, Self::Finished)
        )
    }
}

/// 一名玩家。
///
/// # 为什么 `alive` 是私有字段而不是从 `dice` 派生
///
/// 规则是"骰子掉光即出局"，但**还没发骰**也是零骰——两者不同。
/// `Player::new` 造出来时 `dice` 是空的、`alive` 是 `true`；`start_game()`
/// 只给 `alive` 的玩家发骰。如果把 `alive` 改写成
/// `!dice.is_empty()`，新玩家会被判成出局，`start_game` 跳过所有人，
/// 对局永远开不了。
///
/// 所以保留这个字段，但**私有化**：只允许 `eliminate()` / `start_game()`
/// 通过本模块的路径修改。这样"存了两个真相"的危害从"外部可以随手改"降成
/// "模块内部要保证一致"——后者是有测试盯着的。
///
/// 想彻底单真相需要 `dice: Option<Vec<i64>>`（`None` = 未发骰），那会扩散到
/// 每一处读 `dice` 的地方，属于独立一轮重构（见 `docs/issues/008`）。
#[derive(Clone, Debug)]
pub struct Player {
    pub name: String,
    pub token: String,
    pub dice: Vec<i64>,
    alive: bool,
}

impl Player {
    fn new(name: String, token: String) -> Self {
        Self {
            name,
            token,
            dice: Vec::new(),
            alive: true,
        }
    }

    /// 是否还在场上。
    ///
    /// 未发骰的新玩家也是"在场上"（`dice` 空但 `alive` 为真），
    /// 因此**不能**用 `!dice.is_empty()` 代替。
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive
    }
}

impl agent_game_server_infra::identity::PlayerIdentity for Player {
    fn token(&self) -> &str {
        &self.token
    }
}

/// 一次叫数。**只有叫数**——开盅与超时判负是别的事件，见 [`HistoryEvent`]。
///
/// 改前这里叫 `Call`，同时承载"叫数"与"历史记录"，用一个
/// `quantity == 0` 的哨兵值区分两者（`Call::note`）。那是枚举被拆成 struct
/// 的典型症状：用"业务上有意义的值"（0 颗骰子）编码"这是一个不同类型的事件",
/// 于是每个消费点都得先 `.filter(|c| c.is_bid())` 才知道自己在看什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bid {
    pub player_idx: usize,
    pub quantity: i64,
    pub face: i64,
    pub message: String,
}

impl Bid {
    fn new(player_idx: usize, quantity: i64, face: i64, message: String) -> Self {
        Self {
            player_idx,
            quantity,
            face,
            message,
        }
    }
}

/// 历史里的一条记录。叫数、开盅、超时判负是三类**不同的事件**，由类型区分。
///
/// 消费点不再需要 `.filter(is_bid)`——`filter_map` 出 `Bid` 就只可能是叫数，
/// 类型系统保证。改前那个哨兵值方案下，`call_history` 里既有叫数又有开盅，
/// 而所有"只看叫数"的地方都靠一个 `quantity > 0` 的手写判断。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryEvent {
    /// 某个玩家叫了一次数。
    Bid(Bid),
    /// 某个玩家开了上一手盅。
    Challenge {
        player_idx: usize,
        actual: i64,
        face: i64,
        message: String,
    },
    /// 某个玩家回合超时判负。
    TurnTimeout { player_idx: usize, message: String },
}

impl HistoryEvent {
    /// 这条记录对应的玩家。
    #[must_use]
    pub fn player_idx(&self) -> usize {
        match self {
            Self::Bid(bid) => bid.player_idx,
            Self::Challenge { player_idx, .. } | Self::TurnTimeout { player_idx, .. } => {
                *player_idx
            }
        }
    }

    /// 是叫数就取出它。
    #[must_use]
    pub fn as_bid(&self) -> Option<&Bid> {
        match self {
            Self::Bid(bid) => Some(bid),
            Self::Challenge { .. } | Self::TurnTimeout { .. } => None,
        }
    }

    /// 给人看的文字。
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Bid(bid) => &bid.message,
            Self::Challenge { message, .. } | Self::TurnTimeout { message, .. } => message,
        }
    }

    /// 该条记录的 `(quantity, face)`；非叫数返回 `None`。
    #[must_use]
    pub fn quantity_and_face(&self) -> Option<(i64, i64)> {
        self.as_bid().map(|bid| (bid.quantity, bid.face))
    }
}

/// 大话骰引擎。每个实例管理一局游戏。
#[derive(Debug)]
pub struct LiarsDiceEngine {
    game_type: &'static str,
    player_count: usize,
    dice_per_player: usize,
    phase: Phase,
    players: Vec<Player>,
    current_turn: usize,
    current_call: Option<Bid>,
    call_history: Vec<HistoryEvent>,
    winner: Option<usize>,
    loser: Option<usize>,
    last_action: Instant,
    signal: Arc<ChangeSignal>,
    prompts: &'static PromptStore,
    turn_timeout: Duration,
    block_budget: Duration,
}

impl LiarsDiceEngine {
    /// 从创建参数造一局。
    ///
    /// # Errors
    /// `player_count < 2`、`dice_per_player < 1`、或类型不对。
    pub fn new(params: &CreateParams, config: &Config) -> Result<Self, String> {
        let player_count = params
            .get_i64("player_count")?
            .map_or(2, |value| usize::try_from(value).unwrap_or(0));
        let dice_per_player = params
            .get_i64("dice_per_player")?
            .map_or(5, |value| usize::try_from(value).unwrap_or(0));

        if player_count < 2 {
            return Err(format!(
                "player_count 必须是 ≥ 2 的整数，收到: {player_count}"
            ));
        }
        if dice_per_player < 1 {
            return Err(format!(
                "dice_per_player 必须是 ≥ 1 的整数，收到: {dice_per_player}"
            ));
        }

        let now = Instant::now();
        Ok(Self {
            game_type: "liars_dice",
            player_count,
            dice_per_player,
            phase: Phase::Waiting,
            players: Vec::new(),
            current_turn: 0,
            current_call: None,
            call_history: Vec::new(),
            winner: None,
            loser: None,
            last_action: now,
            signal: Arc::new(ChangeSignal::new()),
            prompts: shared_prompts(),
            turn_timeout: config.turn_timeout,
            block_budget: config.block_budget,
        })
    }

    #[must_use]
    pub fn player_count(&self) -> usize {
        self.player_count
    }

    /// 玩家名。索引查不到时返回 `"?"`。
    ///
    /// 收掉之前散在 5 处的 `map_or("?", |p| p.name.as_str())`。索引由引擎自己
    /// 维护，正常路径下必然有效；`"?"` 只是末日兜底，不是预期状态。
    fn player_name(&self, index: usize) -> &str {
        self.players.get(index).map_or("?", |p| p.name.as_str())
    }

    /// 全部玩家（只读）。
    ///
    /// `players` 字段是私有的：之前是 `pub`，测试里直接
    /// `engine.players[1].alive = false` 就把一个还带着骰子的玩家标成出局，
    /// 绕过所有不变量。只读访问器让"看"和"改"分开——改要走引擎自己的方法。
    #[must_use]
    pub fn players(&self) -> &[Player] {
        &self.players
    }

    #[must_use]
    pub fn dice_per_player(&self) -> usize {
        self.dice_per_player
    }

    #[must_use]
    pub fn current_turn(&self) -> usize {
        self.current_turn
    }

    /// 直接摆骰子：把全场骰子改成给定形状。
    ///
    /// 只给测试与诊断用。规则测试必须能构造"1 混在非 1 里"这类关键组合——
    /// 只用随机开局采样，百搭分支可能一次都走不到（Python 版就是这样漏了它）。
    /// 生产路径永远不调它，所以标 `#[doc(hidden)]` 而不是藏成私有。
    #[doc(hidden)]
    pub fn set_dice(&mut self, faces: Vec<Vec<i64>>) {
        // 只允许在未结束时摆骰子：`finished` 之后 `winner` / `loser` 已经定下，
        // 再改骰子会让"谁是赢家"与"谁还活着"对不上（review 指出）。
        if self.is_finished() {
            return;
        }
        if faces.len() == self.players.len() {
            for (player, dice) in self.players.iter_mut().zip(faces) {
                player.alive = Self::alive_after_dealing(self.phase, &dice);
                player.dice = dice;
            }
        }
    }

    /// 切换阶段。**所有**阶段赋值都必须走这里。
    ///
    /// 返回 `Result` 而不是 `assert!`：引擎 panic 会被 `GameManager::act` 的
    /// `catch_unwind` 捕获并**毒化整个对局**（`supervisor.rs`），而非法转移
    /// 是可以用正常错误路径处理的。`check_timeout` 里 panic 还会走进
    /// AGENTS.md 第 10 条的隔离路径——那是给"引擎内部 bug"兜底的，
    /// 不该用来做常规检查。
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

    /// 摆完骰子之后 `alive` 该是什么。
    ///
    /// 关键区分：**零骰不等于出局**。
    ///
    /// - 开局前（`waiting`）：骰子必然是空的，那是"还没发骰"，玩家仍在场
    /// - 开局后：骰子掉光才是出局
    ///
    /// `set_dice` / `set_player_dice` 都走这一个判断，语义统一。改前
    /// `set_dice` 只改骰子、`set_player_dice` 顺带改 `alive`，两个测试入口
    /// 的语义不一致（review 指出）。
    fn alive_after_dealing(phase: Phase, dice: &[i64]) -> bool {
        phase == Phase::Waiting || !dice.is_empty()
    }

    /// 直接摆某个玩家的骰子。同样只给测试用。
    ///
    /// `alive` 与全场版本的 [`Self::set_dice`] 走同一个判断
    /// （[`Self::alive_after_dealing`]）：开局前空骰不算出局，开局后骰子掉光才出局。
    ///
    /// **这是 `alive` 唯一的写入口之一**——它是私有字段，外部改不动。
    #[doc(hidden)]
    pub fn set_player_dice(&mut self, player_index: usize, dice: Vec<i64>) {
        // 同 `set_dice`：对局已结束后不许再摆
        if self.is_finished() {
            return;
        }
        let phase = self.phase;
        // 先算再借用 `players`，避免同一个表达式里既读 self 又可变借用
        let alive = Self::alive_after_dealing(phase, &dice);
        if let Some(player) = self.players.get_mut(player_index) {
            player.alive = alive;
            player.dice = dice;
        }
    }

    /// 直接指定当前回合的玩家。
    ///
    /// 同样只给测试用：胜负判定与百搭计数需要"让指定的人行动"，
    /// 而先手是随机决定的。
    #[doc(hidden)]
    pub fn set_current_turn(&mut self, player_index: usize) {
        if player_index < self.players.len() {
            self.current_turn = player_index;
        }
    }

    /// 直接改写"上一次行动的时刻"。
    ///
    /// 只给测试用：提示词按 `last_action.elapsed()` 渲染"对手思考了多久"，
    /// 要定点验证 10 秒 / 20 秒 / 40 秒三个阈值，只能把时钟拨回去。
    /// 与 [`Self::set_dice`] 同理，生产路径永远不调它。
    #[doc(hidden)]
    pub fn set_last_action(&mut self, at: Instant) {
        self.last_action = at;
    }

    #[must_use]
    pub fn current_call(&self) -> Option<&Bid> {
        self.current_call.as_ref()
    }

    #[must_use]
    pub fn call_history(&self) -> &[HistoryEvent] {
        &self.call_history
    }

    #[must_use]
    pub fn winner(&self) -> Option<usize> {
        self.winner
    }

    #[must_use]
    pub fn loser(&self) -> Option<usize> {
        self.loser
    }

    #[must_use]
    pub fn turn_timeout(&self) -> Duration {
        self.turn_timeout
    }

    /// 场上仍在使用的骰子总数。
    #[must_use]
    pub fn total_dice(&self) -> i64 {
        self.players
            .iter()
            .filter(|player| player.is_alive())
            .map(|player| i64::try_from(player.dice.len()).unwrap_or(i64::MAX))
            .sum()
    }

    /// 统计全场满足 `face` 的骰子数。1 是百搭：叫非 1 时把 1 也算进来。
    #[must_use]
    pub fn count_face(&self, face: i64) -> i64 {
        self.players
            .iter()
            .flat_map(|player| player.dice.iter())
            .filter(|die| **die == face || (**die == 1 && face != 1))
            .count()
            .try_into()
            .unwrap_or(i64::MAX)
    }

    fn alive_indices(&self) -> Vec<usize> {
        self.players
            .iter()
            .enumerate()
            .filter(|(_, player)| player.is_alive())
            .map(|(index, _)| index)
            .collect()
    }

    /// 从 `player_index` 往后找下一位存活玩家（跳过出局者，绕一圈）。
    fn next_alive_index(&self, player_index: usize) -> usize {
        let count = self.players.len();
        if count == 0 {
            return 0;
        }
        for step in 1..=count {
            let index = (player_index + step) % count;
            if self.players[index].is_alive() {
                return index;
            }
        }
        // 没有别的存活玩家了（对局应已结束），原地不动以免乱指
        player_index
    }

    /// 本轮输家先叫下一轮；若他已被淘汰，则顺延到下一位存活玩家。
    fn next_turn_after(&self, player_index: usize) -> usize {
        if self.players.get(player_index).is_some_and(Player::is_alive) {
            return player_index;
        }
        self.next_alive_index(player_index)
    }

    fn validate_index(&self, player_index: Option<usize>) -> Result<usize, EngineError> {
        let Some(index) = player_index else {
            return Err(EngineError::InvalidIndex("缺少 player_index".into()));
        };
        if index >= self.players.len() {
            return Err(EngineError::NotFound(index.to_string()));
        }
        Ok(index)
    }

    /// 校验身份：索引合法 + token 匹配。
    ///
    /// 校验本身在 [`agent_game_server_infra::identity::check_player_identity`]——REST 与 MCP 两条
    /// 传输共用同一份判断。这里只负责把"通过"翻译成可用的索引。
    fn validate_player(
        &self,
        player_index: Option<usize>,
        player_token: Option<&str>,
    ) -> Result<usize, EngineError> {
        check_player_identity(Some(&self.players), player_index, player_token)?;
        player_index.ok_or_else(|| EngineError::InvalidIndex("缺少 player_index".into()))
    }

    fn check_turn(&self, player_index: usize) -> Result<(), EngineError> {
        if self.phase != Phase::Bidding {
            return Err(EngineError::invalid(format!(
                "游戏阶段是 {}，不能行动",
                self.phase
            )));
        }
        let Some(player) = self.players.get(player_index) else {
            return Err(EngineError::NotFound(player_index.to_string()));
        };
        if !player.is_alive() {
            return Err(EngineError::invalid(format!(
                "玩家 {player_index} 已出局，不能行动"
            )));
        }
        if player_index != self.current_turn {
            return Err(EngineError::invalid(format!(
                "还没轮到你（当前是玩家 {}）",
                self.current_turn
            )));
        }
        Ok(())
    }

    /// 校验叫数本身是否合法——与"必须比上家大"是两回事。
    fn validate_bid_values(&self, quantity: i64, face: i64) -> Result<(), EngineError> {
        if quantity < 1 {
            return Err(EngineError::invalid(format!(
                "叫数数量必须是 ≥ 1 的整数，收到: {quantity}"
            )));
        }
        if !(1..=6).contains(&face) {
            return Err(EngineError::invalid(format!(
                "面值必须是 1-6 的整数，收到: {face}"
            )));
        }
        let total = self.total_dice();
        if quantity > total {
            return Err(EngineError::invalid(format!(
                "叫数 {quantity} 超过场上骰子总数 {total}，这样的叫数永远开不赢"
            )));
        }
        Ok(())
    }

    /// 新叫数是否比上一手大。
    fn is_valid_bid(previous: Option<&Bid>, new_quantity: i64, new_face: i64) -> bool {
        let Some(previous) = previous else {
            return true;
        };
        if new_face == 1 && previous.face != 1 {
            return new_quantity >= (previous.quantity / 2 + 1);
        }
        if new_face != 1 && previous.face == 1 {
            return new_quantity >= previous.quantity * 2;
        }
        new_quantity > previous.quantity
            || (new_quantity == previous.quantity && new_face > previous.face)
    }

    fn start_game(&mut self) -> Result<(), EngineError> {
        let mut rng = rand::rng();
        for player in &mut self.players {
            player.dice = if player.is_alive() {
                (0..self.dice_per_player)
                    .map(|_| rng.random_range(1..=6))
                    .collect()
            } else {
                Vec::new()
            };
        }
        self.transition_to(Phase::Bidding)?;
        let alive = self.alive_indices();
        self.current_turn = if alive.is_empty() {
            0
        } else {
            alive[rng.random_range(0..alive.len())]
        };
        self.last_action = Instant::now();
        self.signal.notify();
        Ok(())
    }

    /// 加一个玩家。人满自动开局。返回玩家编号。
    ///
    /// 带上已有的 `player_token` 且命中在场玩家时走重连：不追加玩家、不重摇骰子、
    /// 不推进状态机，只把现有身份还给他。
    fn add_player(
        &mut self,
        name: String,
        player_token: Option<&str>,
    ) -> Result<usize, EngineError> {
        if let Some(token) = player_token.filter(|token| !token.is_empty())
            && let Some(existing) = self.find_player_by_token(token)
        {
            return Ok(existing);
        }

        // 对局已经开始还允许追加玩家，会触发 `start_game()` 重摇全场骰子、
        // 把进行中的对局推倒重来，所以非 waiting 阶段一律拒绝。
        if self.phase != Phase::Waiting {
            return Err(EngineError::invalid(format!(
                "对局已开始（{}），无法加入",
                self.phase
            )));
        }

        let token = uuid::Uuid::new_v4().simple().to_string()[..12].to_owned();
        let index = self.players.len();
        self.players.push(Player::new(name, token));
        self.last_action = Instant::now();

        if self.players.len() >= self.player_count {
            self.start_game()?;
        } else {
            // 人还没满，同样是一次状态变更：大厅里等待的玩家要看到人数变化
            self.signal.notify();
        }
        Ok(index)
    }

    fn make_bid(
        &mut self,
        player_index: usize,
        quantity: i64,
        face: i64,
        message: String,
    ) -> Result<(), EngineError> {
        self.check_turn(player_index)?;
        self.validate_bid_values(quantity, face)?;

        if !Self::is_valid_bid(self.current_call.as_ref(), quantity, face) {
            let (prev_quantity, prev_face) = self
                .current_call
                .as_ref()
                .map_or((0, 0), |call| (call.quantity, call.face));
            return Err(EngineError::invalid(format!(
                "叫数必须比上家大。当前: {prev_quantity}×{prev_face} → 你叫: {quantity}×{face}"
            )));
        }

        let bid = Bid::new(player_index, quantity, face, message);
        self.current_call = Some(bid.clone());
        self.call_history.push(HistoryEvent::Bid(bid));
        self.current_turn = self.next_alive_index(player_index);
        self.last_action = Instant::now();
        self.signal.notify();
        Ok(())
    }

    fn challenge(&mut self, player_index: usize) -> Result<(), EngineError> {
        self.check_turn(player_index)?;
        let Some(call) = self.current_call.clone() else {
            return Err(EngineError::invalid("没有可开的叫数"));
        };
        if player_index == call.player_idx {
            return Err(EngineError::invalid("不能开自己的盅"));
        }

        let actual = self.count_face(call.face);
        let loser = if actual >= call.quantity {
            player_index
        } else {
            call.player_idx
        };

        self.call_history.push(HistoryEvent::Challenge {
            player_idx: player_index,
            actual,
            face: call.face,
            message: format!("开！(实际 {actual} 个 {})", call.face),
        });
        self.current_call = None;
        self.loser = Some(loser);
        self.last_action = Instant::now();

        if !self.eliminate(loser)? {
            // 对局继续：本轮叫数清零，由输家先叫下一轮（已出局则顺延）
            self.current_turn = self.next_turn_after(loser);
        }
        // 掉骰、出局、结束对局都是状态变更，唤醒可能还在等待的对手
        self.signal.notify();
        Ok(())
    }

    /// 让 `player_index` 输掉本轮：掉一颗骰子，掉光则出局。
    ///
    /// 场上只剩 2 名存活玩家时是突然死亡：本轮输家直接出局。
    /// 返回对局是否因此结束。
    fn eliminate(&mut self, player_index: usize) -> Result<bool, EngineError> {
        // 先算存活人数：下面要可变借用 `players`，同一个表达式里不能再读 self
        let sudden_death = self.alive_indices().len() <= 2;
        if let Some(player) = self.players.get_mut(player_index)
            && player.is_alive()
        {
            if sudden_death {
                player.dice.clear();
            } else {
                player.dice.pop();
            }
            if player.dice.is_empty() {
                player.alive = false;
            }
        }
        self.finish_if_over()
    }

    /// 只剩一名存活玩家时结束对局，最后存活者即 winner。
    fn finish_if_over(&mut self) -> Result<bool, EngineError> {
        let alive = self.alive_indices();
        if alive.len() > 1 {
            return Ok(false);
        }

        self.winner = alive.first().copied();
        self.current_call = None;
        // current_turn 只允许指向存活玩家，对局结束后停在赢家身上
        if let Some(winner) = self.winner {
            self.current_turn = winner;
        }
        if self.phase != Phase::Finished {
            self.transition_to(Phase::Finished)?;
            self.signal.notify();
        }
        Ok(true)
    }

    fn build_public_view(&self) -> Value {
        let current_call = self.current_call.as_ref().map(|call| {
            serde_json::json!({
                "player": call.player_idx,
                "player_name": self.players.get(call.player_idx).map_or("?", |p| p.name.as_str()),
                "quantity": call.quantity,
                "face": call.face,
            })
        });
        serde_json::json!({
            "phase": self.phase.as_str(),
            "current_turn": self.current_turn,
            "current_call": current_call,
            "last_message": self.current_call.as_ref().map_or("", |call| call.message.as_str()),
            "players": self.players.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            "alive": self.players.iter().map(Player::is_alive).collect::<Vec<_>>(),
            "dice_per_player": self.dice_per_player,
            "call_history": self.call_history.iter().map(|event| {
                let idx = event.player_idx();
                // 非叫数（开盅、超时）没有 quantity/face，序列化成 0 ——
                // 这是**对外 JSON 契约**的一部分，改前用哨兵值也是 0，
                // 所以下游（admin 页面、Agent prompt）看到的东西不变。
                let (quantity, face) = event.quantity_and_face().unwrap_or((0, 0));
                serde_json::json!({
                    "player": idx,
                    "player_name": self.player_name(idx),
                    "quantity": quantity,
                    "face": face,
                    "message": event.message(),
                })
            }).collect::<Vec<_>>(),
        })
    }

    fn build_prompt(&self, player_index: Option<usize>) -> String {
        let wait_budget = self.block_budget.as_secs_f64();
        let turn_timeout = self.turn_timeout.as_secs_f64();
        // 两个读数必须来自同一个时间基准：`waited` 是整秒（"已思考 45 秒"），
        // `remaining` 是倒计时（"只剩 15 秒"）。若在两次 `elapsed()` 之间取，
        // 二者之和会差 1，提示词自己就前后矛盾。
        let elapsed = self.last_action.elapsed().as_secs();
        let waited = elapsed;
        let remaining = (self.turn_timeout.as_secs().saturating_sub(elapsed)) as f64;

        match self.phase {
            Phase::Finished => {
                let winner = self
                    .winner
                    .and_then(|index| self.players.get(index))
                    .map_or("?", |player| player.name.as_str());
                self.render(
                    "finished",
                    &Ctx::new()
                        .with("winner", winner)
                        .with(
                            "all_dice",
                            self.players
                                .iter()
                                .map(|p| p.dice.clone())
                                .collect::<Vec<_>>(),
                        )
                        .with(
                            "call_history",
                            self.call_history
                                .iter()
                                .filter_map(HistoryEvent::quantity_and_face)
                                .collect::<Vec<_>>(),
                        ),
                )
            }
            Phase::Waiting => self.render(
                "waiting",
                &Ctx::new()
                    .with("players", self.players.len())
                    .with("needed", self.player_count)
                    .with("waited", waited as i64)
                    .with("wait_budget", wait_budget),
            ),
            Phase::Bidding => {
                let Some(index) = player_index.filter(|index| *index < self.players.len()) else {
                    return String::new();
                };
                if self.current_turn != index {
                    // 不是我的回合：只描述两件确定的事——对手的回合进行了多久、
                    // 一次 state 调用最多挂起多久。视图层不知道"这一次调用等了多久"
                    // （立即返回的 GET 与挂满预算的 state 走同一条渲染路径）。
                    let current_player = self
                        .players
                        .get(self.current_turn)
                        .map_or("?", |player| player.name.as_str());
                    let mut patience = String::new();
                    if waited > 20 {
                        patience = self.render(
                            "patience_20",
                            &Ctx::new().with("turn_timeout", turn_timeout),
                        );
                    }
                    if waited > 40 {
                        patience = self.render(
                            "patience_40",
                            &Ctx::new().with("remaining_time", remaining as i64),
                        );
                    }
                    let call_str = self.current_call.as_ref().map_or_else(
                        || "无".to_owned(),
                        |call| format!("{}×{}", call.quantity, call.face),
                    );
                    return self.render(
                        "not_your_turn",
                        &Ctx::new()
                            .with("current_player", current_player)
                            .with(
                                "progress",
                                if waited < 10 {
                                    "刚刚".to_owned()
                                } else {
                                    format!("已思考 {waited} 秒")
                                },
                            )
                            .with("patience", patience)
                            .with("call_str", call_str)
                            .with("dice", self.players[index].dice.clone())
                            .with("player_index", index)
                            .with("elapsed", waited as i64)
                            .with("wait_budget", wait_budget),
                    );
                }

                let call_str = self.current_call.as_ref().map_or_else(
                    || "无（你是第一个叫数的人）".to_owned(),
                    |call| {
                        format!(
                            "{}×{}（{} 叫的）",
                            call.quantity,
                            call.face,
                            self.players
                                .get(call.player_idx)
                                .map_or("?", |player| player.name.as_str())
                        )
                    },
                );
                let msg_line = self
                    .current_call
                    .as_ref()
                    .filter(|call| !call.message.is_empty())
                    .map_or_else(String::new, |call| {
                        format!("\n💬 上家说: '{}'", call.message)
                    });
                self.render(
                    "your_turn",
                    &Ctx::new()
                        .with("dice", self.players[index].dice.clone())
                        .with("call_str", call_str)
                        .with("msg_line", msg_line)
                        .with("remaining", remaining as i64)
                        .with("player_index", index),
                )
            }
        }
    }
}

impl GameEngine for LiarsDiceEngine {
    fn game_type(&self) -> &'static str {
        self.game_type
    }

    fn player_mode(&self) -> PlayerMode {
        PlayerMode::Multi
    }

    fn finished_states(&self) -> &'static [&'static str] {
        &["finished"]
    }

    fn sensitive_keys(&self) -> &'static [&'static str] {
        &["all_dice"]
    }

    fn phase(&self) -> &'static str {
        self.phase.as_str()
    }

    fn admin_view(&self) -> Value {
        let mut view = self.build_public_view();
        if let Value::Object(ref mut map) = view {
            map.insert(
                "all_dice".to_owned(),
                Value::Array(
                    self.players
                        .iter()
                        .map(|player| {
                            Value::Array(player.dice.iter().map(|die| Value::from(*die)).collect())
                        })
                        .collect(),
                ),
            );
            map.insert(
                "winner".to_owned(),
                self.winner.map_or(Value::Null, Value::from),
            );
            map.insert(
                "loser".to_owned(),
                self.loser.map_or(Value::Null, Value::from),
            );
            map.insert(
                "status".to_owned(),
                Value::String(self.phase.as_str().to_owned()),
            );
        }
        view
    }

    fn public_view(&self) -> Value {
        // 刻意不含 `all_dice`：公开视图是构造出来的，不是从 admin 视图删键。
        self.build_public_view()
    }

    fn player_view(&self, player_index: Option<usize>) -> Result<PlayerView, EngineError> {
        let index = self.validate_index(player_index)?;
        let mut private = serde_json::json!({
            "your_dice": self.players[index].dice.clone(),
            "your_index": index,
        });
        if self.phase == Phase::Finished
            && let Value::Object(ref mut map) = private
        {
            {
                map.insert(
                    "all_dice".to_owned(),
                    Value::Array(
                        self.players
                            .iter()
                            .map(|player| {
                                Value::Array(
                                    player.dice.iter().map(|die| Value::from(*die)).collect(),
                                )
                            })
                            .collect(),
                    ),
                );
                map.insert(
                    "winner".to_owned(),
                    self.winner.map_or(Value::Null, Value::from),
                );
                map.insert(
                    "winner_name".to_owned(),
                    self.winner
                        .and_then(|winner| self.players.get(winner))
                        .map_or(Value::Null, |player| Value::String(player.name.clone())),
                );
            }
        }
        Ok(PlayerView {
            public: self.build_public_view(),
            private,
            prompt: self.build_prompt(Some(index)),
        })
    }

    fn handle_action(
        &mut self,
        action: &str,
        params: ActionParams,
    ) -> Result<PlayerView, EngineError> {
        match action {
            "join" => {
                let name = params.require_str("player_name")?.to_owned();
                let token = params.player_token()?;
                let index = self.add_player(name, token)?;
                let mut view = self.player_view(Some(index))?;
                // join 的响应要带上身份元数据：客户端靠它记住 token
                if let Value::Object(ref mut map) = view.private {
                    map.insert("player_index".to_owned(), Value::from(index));
                    map.insert(
                        "player_token".to_owned(),
                        Value::String(self.players[index].token.clone()),
                    );
                    map.insert(
                        "phase".to_owned(),
                        Value::String(self.phase.as_str().to_owned()),
                    );
                    map.insert("player_count".to_owned(), Value::from(self.players.len()));
                    map.insert("needed".to_owned(), Value::from(self.player_count));
                    map.insert(
                        "reconnected".to_owned(),
                        Value::Bool(token.is_some_and(|token| !token.is_empty())),
                    );
                }
                view.prompt = format!(
                    "🎲 你是玩家 {index}，token: {}\n{}",
                    self.players[index].token, view.prompt
                );
                Ok(view)
            }
            "state" => {
                let index = self.validate_player(params.player_index()?, params.player_token()?)?;
                self.player_view(Some(index))
            }
            "bid" => {
                let index = self.validate_player(params.player_index()?, params.player_token()?)?;
                let quantity = params.require_i64("quantity")?;
                let face = params.require_i64("face")?;
                let message = params.optional_str("message")?.unwrap_or("").to_owned();
                self.make_bid(index, quantity, face, message)?;
                self.player_view(Some(index))
            }
            "challenge" => {
                let index = self.validate_player(params.player_index()?, params.player_token()?)?;
                self.challenge(index)?;
                self.player_view(Some(index))
            }
            other => Err(EngineError::UnknownAction(other.to_owned())),
        }
    }

    fn check_timeout(&mut self, now: Instant) -> Result<Option<TimeoutEvent>, EngineError> {
        // 匹配超时
        if self.phase == Phase::Waiting
            && now.saturating_duration_since(self.last_action) > LOBBY_TIMEOUT
        {
            self.signal.notify();
            return Ok(Some(TimeoutEvent::lobby_timeout()));
        }

        // 回合超时：超时者按"输掉本轮"处理（掉骰/出局），而不是让下家白捡一局。
        // 回合必须交出去，否则超时者会立刻再次超时，对局永远卡住。
        if self.phase == Phase::Bidding
            && now.saturating_duration_since(self.last_action) > self.turn_timeout
        {
            let loser = self.current_turn;
            self.loser = Some(loser);
            self.last_action = now;

            // 超时与开盅同样是"输掉本轮"：本轮叫数必须清零。留着上一手的叫数，
            // 下家既不能加码（可能已超过场上骰子总数）也不能弃权，只能替别人开盅。
            if self.current_call.is_some() {
                self.call_history.push(HistoryEvent::TurnTimeout {
                    player_idx: loser,
                    message: "超时判负（本轮作废）".to_owned(),
                });
            }
            self.current_call = None;

            if !self.eliminate(loser)? {
                self.current_turn = self.next_alive_index(loser);
            }
            // 判负同样要唤醒等待者，否则等待方要等满整个预算才知道对局已结束
            self.signal.notify();
            return Ok(Some(TimeoutEvent::turn_timeout(loser)));
        }
        Ok(None)
    }

    fn signal(&self) -> Arc<ChangeSignal> {
        Arc::clone(&self.signal)
    }

    fn is_ready_for(&self, player_index: usize) -> bool {
        // 等待条件：轮到我了，或对局已经结束。
        // 多于两人时 notify 是全局广播——别人叫数也会唤醒我，所以醒来后必须
        // 重新判断这个条件，否则会被假唤醒骗去"行动"。
        if self.phase == Phase::Finished {
            return true;
        }
        self.phase == Phase::Bidding && self.current_turn == player_index
    }

    fn find_player_by_token(&self, token: &str) -> Option<usize> {
        self.players.iter().position(|player| player.token == token)
    }

    fn player_tokens(&self) -> Option<Vec<String>> {
        Some(
            self.players
                .iter()
                .map(|player| player.token.clone())
                .collect(),
        )
    }

    fn prompts(&self) -> &PromptStore {
        self.prompts
    }
}

/// 这个游戏暴露的 MCP 工具。
///
/// 纯静态数据，与引擎实例无关——所以住在注册元数据（`GameMeta::tools`）里，
/// 不是 `GameEngine` 的方法。见 `GameMeta::tools` 的文档说明。
#[must_use]
pub fn tools() -> Vec<agent_game_server_domain::game::ToolSpec> {
    use agent_game_server_domain::game::ToolSpec;
    use serde_json::json;

    let seat = json!({"type": "integer", "description": "你的玩家编号"});
    let token = json!({"type": "string", "description": "你的身份 token（必填）"});

    vec![
        ToolSpec::new(
            "liars_dice_join",
            "加入大话骰（吹牛/撒谎骰子）匹配队列。只需提供名字，服务器自动匹配对手。\
                 如果之前断线，传入 player_token 可重连回原有对局。每人5颗骰子，1是百搭。\
                 返回玩家编号和游戏状态。",
            json!({
                "type": "object",
                "properties": {
                    "player_name": {"type": "string", "description": "你的名字"},
                    "player_token": {
                        "type": "string",
                        "description": "断线重连时传入之前的 token",
                    },
                    // 建桌参数：同桌的所有玩家必须传一致的值，否则会各开一桌。
                    "player_count": {
                        "type": "integer",
                        "description": "这一桌几个人（默认 2，至少 2）。同桌的所有玩家必须传同一个值。",
                        "minimum": 2,
                        "default": 2,
                    },
                    "dice_per_player": {
                        "type": "integer",
                        "description": "每人几颗骰子（默认 5，至少 1）。同桌的所有玩家必须传同一个值。",
                        "minimum": 1,
                        "default": 5,
                    },
                },
                "required": ["player_name"],
            }),
        ),
        ToolSpec::new(
            "liars_dice_state",
            "查看大话骰对局当前状态。**注意：如果还没轮到你，这个调用会阻塞等待\
                 （最长 55 秒）直到对手行动、游戏结束或超时。返回慢是正常的——不是出错，\
                 是服务器在等你对面的玩家决策。等满 55 秒仍未轮到你时，返回的提示会告诉你\
                 继续等还是就此打住。** 必须带上你的 player_index 和 player_token。",
            json!({
                "type": "object",
                "properties": {
                    "player_index": seat,
                    "player_token": token,
                    "timeout": {
                        "type": "integer",
                        "description": "本次最多阻塞多少秒，缺省 55，上限 55",
                        "default": 55,
                        "maximum": 55,
                    },
                },
                "required": ["player_index", "player_token"],
            }),
        ),
        ToolSpec::new(
            "liars_dice_bid",
            "在大话骰中叫数。宣称全场至少有多少颗某个点数的骰子。数量不能超过场上骰子\
                 总数，面值 1-6，且必须比上家大。可选附带一句话 bluff 或误导对手。",
            json!({
                "type": "object",
                "properties": {
                    "player_index": seat,
                    "player_token": token,
                    "quantity": {
                        "type": "integer",
                        "description": "数量（≥1，且不超过场上骰子总数）",
                        "minimum": 1,
                    },
                    "face": {
                        "type": "integer",
                        "description": "面值 1-6",
                        "minimum": 1,
                        "maximum": 6,
                    },
                    "message": {
                        "type": "string",
                        "description": "可选: bluff/误导信息",
                    },
                },
                "required": ["player_index", "player_token", "quantity", "face"],
            }),
        ),
        ToolSpec::new(
            "liars_dice_challenge",
            "在大话骰中开盅（挑战上家的叫数）。所有人揭骰验证。不能开自己的盅。\
                 必须带上你的 player_index 和 player_token。",
            json!({
                "type": "object",
                "properties": {
                    "player_index": seat,
                    "player_token": token,
                },
                "required": ["player_index", "player_token"],
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_with(player_count: usize, dice_per_player: usize) -> LiarsDiceEngine {
        let mut raw = serde_json::Map::new();
        raw.insert("player_count".to_owned(), Value::from(player_count));
        raw.insert("dice_per_player".to_owned(), Value::from(dice_per_player));
        LiarsDiceEngine::new(&CreateParams::new(raw), &Config::default()).unwrap()
    }

    fn started(player_count: usize, dice_per_player: usize) -> (LiarsDiceEngine, Vec<String>) {
        let mut engine = engine_with(player_count, dice_per_player);
        for index in 0..player_count {
            let mut raw = serde_json::Map::new();
            raw.insert("player_name".to_owned(), Value::from(format!("P{index}")));
            engine
                .handle_action("join", ActionParams::new(raw))
                .unwrap();
        }
        let tokens = engine
            .players
            .iter()
            .map(|player| player.token.clone())
            .collect();
        (engine, tokens)
    }

    fn script(engine: &mut LiarsDiceEngine, faces: Vec<Vec<i64>>) {
        assert_eq!(faces.len(), engine.players.len());
        for (player, dice) in engine.players.iter_mut().zip(faces) {
            player.dice = dice;
        }
    }

    fn bid(
        engine: &mut LiarsDiceEngine,
        player: usize,
        token: &str,
        quantity: i64,
        face: i64,
    ) -> Result<PlayerView, EngineError> {
        let mut raw = serde_json::Map::new();
        raw.insert("player_index".to_owned(), Value::from(player));
        raw.insert("player_token".to_owned(), Value::from(token));
        raw.insert("quantity".to_owned(), Value::from(quantity));
        raw.insert("face".to_owned(), Value::from(face));
        engine.handle_action("bid", ActionParams::new(raw))
    }

    fn challenge(
        engine: &mut LiarsDiceEngine,
        player: usize,
        token: &str,
    ) -> Result<PlayerView, EngineError> {
        let mut raw = serde_json::Map::new();
        raw.insert("player_index".to_owned(), Value::from(player));
        raw.insert("player_token".to_owned(), Value::from(token));
        engine.handle_action("challenge", ActionParams::new(raw))
    }

    // ── 开局与入座 ──

    #[test]
    fn one_player_waits_two_players_start() {
        let mut engine = engine_with(2, 5);
        assert_eq!(engine.phase(), "waiting");
        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("A"));
        engine
            .handle_action("join", ActionParams::new(raw))
            .unwrap();
        assert_eq!(engine.phase(), "waiting");
        assert_eq!(engine.players.len(), 1);

        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("B"));
        engine
            .handle_action("join", ActionParams::new(raw))
            .unwrap();
        assert_eq!(engine.phase(), "bidding");
        assert!(engine.players.iter().all(|p| p.dice.len() == 5));
    }

    #[test]
    fn join_after_start_is_refused() {
        let (mut engine, _tokens) = started(2, 3);
        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("Late"));
        let err = engine
            .handle_action("join", ActionParams::new(raw))
            .unwrap_err();
        assert!(err.to_string().contains("无法加入"), "{err}");
        assert_eq!(engine.players.len(), 2);
    }

    #[test]
    fn join_with_own_token_reconnects() {
        let (mut engine, tokens) = started(2, 3);
        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("P0"));
        raw.insert("player_token".to_owned(), Value::from(tokens[0].clone()));
        let view = engine
            .handle_action("join", ActionParams::new(raw))
            .unwrap();
        assert_eq!(engine.players.len(), 2, "重连不该新增玩家");
        assert_eq!(view.private["your_index"], 0);
    }

    #[test]
    fn invalid_setup_is_rejected() {
        assert!(
            LiarsDiceEngine::new(
                &CreateParams::new({
                    let mut raw = serde_json::Map::new();
                    raw.insert("player_count".to_owned(), Value::from(1));
                    raw
                }),
                &Config::default()
            )
            .is_err()
        );
        assert!(
            LiarsDiceEngine::new(
                &CreateParams::new({
                    let mut raw = serde_json::Map::new();
                    raw.insert("dice_per_player".to_owned(), Value::from(0));
                    raw
                }),
                &Config::default()
            )
            .is_err()
        );
    }

    // ── 百搭规则 ──

    #[test]
    fn ones_count_as_any_face() {
        let (mut engine, _tokens) = started(2, 3);
        script(&mut engine, vec![vec![1, 3, 3], vec![2, 3, 3]]);
        // 3 的个数 = 明面的 4 颗 + 百搭的 1 颗
        assert_eq!(engine.count_face(3), 5);
        // 2 的个数 = 明面的 1 颗 + 百搭的 1 颗
        assert_eq!(engine.count_face(2), 2);
    }

    #[test]
    fn ones_only_count_as_ones_when_bidding_ones() {
        let (mut engine, _tokens) = started(2, 3);
        script(&mut engine, vec![vec![1, 3, 3], vec![2, 3, 3]]);
        assert_eq!(engine.count_face(1), 1);
        assert_ne!(engine.count_face(1), engine.total_dice());
    }

    #[test]
    fn wildcard_changes_the_verdict() {
        // 叫 5×3：实际（含百搭）正好 5 颗，叫数成立 → 开盅的人输
        let (mut engine, tokens) = started(2, 3);
        script(&mut engine, vec![vec![1, 3, 3], vec![2, 3, 3]]);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 5, 3).unwrap();
        challenge(&mut engine, 1, &tokens[1]).unwrap();
        assert_eq!(engine.loser(), Some(1));
    }

    #[test]
    fn without_the_wildcard_the_bidder_would_lose() {
        let (mut engine, tokens) = started(2, 3);
        script(&mut engine, vec![vec![1, 3, 3], vec![2, 3, 4]]);
        engine.current_turn = 0;
        // 含百搭 = 5，明面 = 4。叫 6×3 则含百搭也不成立 → 叫的人输
        bid(&mut engine, 0, &tokens[0], 6, 3).unwrap();
        challenge(&mut engine, 1, &tokens[1]).unwrap();
        assert_eq!(engine.loser(), Some(0));
    }

    // ── 叫数合法性 ──

    #[test]
    fn first_bid_is_always_valid() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        assert!(bid(&mut engine, 0, &tokens[0], 1, 1).is_ok());
    }

    #[test]
    fn zero_quantity_is_rejected() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        let err = bid(&mut engine, 0, &tokens[0], 0, 3).unwrap_err();
        assert!(err.to_string().contains("≥ 1"), "{err}");
    }

    #[test]
    fn bad_face_is_rejected() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        assert!(bid(&mut engine, 0, &tokens[0], 1, 0).is_err());
        assert!(bid(&mut engine, 0, &tokens[0], 1, 7).is_err());
    }

    #[test]
    fn bid_beyond_the_total_dice_is_rejected() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        let err = bid(&mut engine, 0, &tokens[0], 7, 3).unwrap_err();
        assert!(err.to_string().contains("超过场上骰子总数"), "{err}");
    }

    #[test]
    fn lower_bid_is_rejected() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 3, 3).unwrap();
        let err = bid(&mut engine, 1, &tokens[1], 2, 4).unwrap_err();
        assert!(err.to_string().contains("必须比上家大"), "{err}");
    }

    #[test]
    fn same_quantity_higher_face_is_valid() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 3, 3).unwrap();
        assert!(bid(&mut engine, 1, &tokens[1], 3, 4).is_ok());
    }

    #[test]
    fn bidding_ones_requires_half_plus_one() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 4, 3).unwrap();
        // 4 // 2 + 1 = 3
        assert!(bid(&mut engine, 1, &tokens[1], 2, 1).is_err());
        assert!(bid(&mut engine, 1, &tokens[1], 3, 1).is_ok());
    }

    #[test]
    fn leaving_ones_requires_doubling() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 2, 1).unwrap();
        assert!(bid(&mut engine, 1, &tokens[1], 3, 3).is_err());
        assert!(bid(&mut engine, 1, &tokens[1], 4, 3).is_ok());
    }

    #[test]
    fn cannot_challenge_own_bid() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 1, 2).unwrap();
        // 轮到 1：他把回合还给自己是不合法的
        engine.current_turn = 0;
        let err = challenge(&mut engine, 0, &tokens[0]).unwrap_err();
        assert!(err.to_string().contains("不能开自己的盅"), "{err}");
    }

    #[test]
    fn challenge_without_a_bid_is_rejected() {
        let (mut engine, tokens) = started(2, 3);
        engine.current_turn = 0;
        let err = challenge(&mut engine, 0, &tokens[0]).unwrap_err();
        assert!(err.to_string().contains("没有可开的叫数"), "{err}");
    }

    // ── 身份 ──

    #[test]
    fn actions_require_player_token() {
        let (mut engine, _tokens) = started(2, 3);
        for action in ["state", "bid", "challenge"] {
            let mut raw = serde_json::Map::new();
            raw.insert("player_index".to_owned(), Value::from(0));
            if action == "bid" {
                raw.insert("quantity".to_owned(), Value::from(1));
                raw.insert("face".to_owned(), Value::from(2));
            }
            let err = engine
                .handle_action(action, ActionParams::new(raw))
                .unwrap_err();
            assert_eq!(err, EngineError::MissingToken, "action={action}");
        }
    }

    #[test]
    fn foreign_token_is_rejected() {
        let (mut engine, tokens) = started(2, 3);
        let err = bid(&mut engine, 0, &tokens[1], 1, 2).unwrap_err();
        assert_eq!(err, EngineError::TokenMismatch);
    }

    #[test]
    fn negative_and_out_of_range_indices_are_rejected() {
        let (mut engine, _tokens) = started(2, 3);
        let mut raw = serde_json::Map::new();
        raw.insert("player_index".to_owned(), Value::from(-1));
        raw.insert("player_token".to_owned(), Value::from("x"));
        assert!(
            engine
                .handle_action("state", ActionParams::new(raw))
                .is_err()
        );

        let mut raw = serde_json::Map::new();
        raw.insert("player_index".to_owned(), Value::from(9));
        raw.insert("player_token".to_owned(), Value::from("x"));
        assert!(
            engine
                .handle_action("state", ActionParams::new(raw))
                .is_err()
        );
    }

    #[test]
    fn player_view_never_leaks_opponent_dice() {
        let (engine, _tokens) = started(2, 3);
        let view = engine.player_view(Some(0)).unwrap();
        let text = view.public.to_string();
        let opponent_dice = engine.players[1].dice.clone();
        assert!(!text.contains(&format!("{opponent_dice:?}")), "{text}");
        assert_eq!(
            view.private["your_dice"],
            serde_json::json!(engine.players[0].dice)
        );
    }

    // ── 淘汰 ──

    #[test]
    fn three_player_loss_costs_one_die() {
        let (mut engine, tokens) = started(3, 3);
        script(
            &mut engine,
            vec![vec![2, 2, 2], vec![3, 3, 3], vec![4, 4, 4]],
        );
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 3, 2).unwrap();
        challenge(&mut engine, 1, &tokens[1]).unwrap();
        // 3 的个数 = 3（不含百搭），3×3 成立 → 开的人输，掉一颗
        assert_eq!(engine.loser(), Some(1));
        assert_eq!(engine.players[1].dice.len(), 2);
        assert!(engine.players[1].alive);
    }

    #[test]
    fn two_player_loss_is_sudden_death() {
        let (mut engine, tokens) = started(2, 3);
        script(&mut engine, vec![vec![2, 2, 2], vec![3, 3, 3]]);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 3, 2).unwrap();
        challenge(&mut engine, 1, &tokens[1]).unwrap();
        assert_eq!(engine.phase(), "finished");
        assert_eq!(engine.winner(), Some(0));
        assert!(!engine.players[1].alive);
    }

    #[test]
    fn game_ends_only_at_one_survivor() {
        let (mut engine, tokens) = started(3, 2);
        script(&mut engine, vec![vec![2, 2], vec![3, 3], vec![4, 4]]);
        engine.current_turn = 0;
        // 叫 2×2：全场正好 2 颗 2，叫数成立 → 开盅的 P1 输
        bid(&mut engine, 0, &tokens[0], 2, 2).unwrap();
        challenge(&mut engine, 1, &tokens[1]).unwrap();
        assert_eq!(engine.phase(), "bidding", "3 人局掉一颗骰子后继续");
        assert_eq!(engine.loser(), Some(1));
        assert_eq!(engine.players[1].dice.len(), 1);

        // 第二轮：P1 再输一次 → 出局（场上还有 3 人，所以只掉骰，但骰子已掉光）
        engine.current_turn = 1;
        // 场上骰子总数 2+1+2 = 5；叫 5×3 时实际只有 1 颗 3 → 叫的人输
        bid(&mut engine, 1, &tokens[1], 5, 3).unwrap();
        challenge(&mut engine, 2, &tokens[2]).unwrap();
        assert!(!engine.players[1].alive, "骰子掉光即出局");
        assert_ne!(engine.phase(), "finished", "还剩两人");
    }

    #[test]
    fn turn_skips_eliminated_players() {
        let (mut engine, tokens) = started(3, 2);
        script(&mut engine, vec![vec![2, 2], vec![3, 3], vec![4, 4]]);
        engine.players[1].alive = false;
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 1, 2).unwrap();
        assert_eq!(engine.current_turn(), 2, "应跳过已出局的 1");
    }

    #[test]
    fn eliminating_a_player_shrinks_the_total_dice() {
        let (mut engine, tokens) = started(3, 3);
        script(
            &mut engine,
            vec![vec![2, 2, 2], vec![3, 3, 3], vec![4, 4, 4]],
        );
        let before = engine.total_dice();
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 3, 2).unwrap();
        challenge(&mut engine, 1, &tokens[1]).unwrap();
        assert_eq!(engine.total_dice(), before - 1);
    }

    // ── 超时 ──

    #[test]
    fn turn_timeout_eliminates_and_hands_over_the_turn() {
        let (mut engine, _tokens) = started(2, 3);
        let current = engine.current_turn();
        let event = engine
            .check_timeout(Instant::now() + engine.turn_timeout() * 2)
            .unwrap();
        assert_eq!(event, Some(TimeoutEvent::turn_timeout(current)));
        assert_eq!(engine.loser(), Some(current));
        assert_eq!(engine.phase(), "finished", "2 人局超时即出局");
        assert_eq!(engine.winner(), Some((current + 1) % 2));
    }

    #[test]
    fn turn_timeout_clears_the_round_bid() {
        let (mut engine, tokens) = started(3, 2);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 6, 3).unwrap();
        // 让当前的 P1 超时
        let event = engine
            .check_timeout(Instant::now() + engine.turn_timeout() * 2)
            .unwrap();
        assert_eq!(event, Some(TimeoutEvent::turn_timeout(1)));
        assert_eq!(engine.current_call(), None, "僵尸叫数必须清零");

        // 下家 P2 现在能正常加码（不再被"超过总数"锁死）
        engine.current_turn = 2;
        assert!(bid(&mut engine, 2, &tokens[2], 1, 3).is_ok());
    }

    #[test]
    fn turn_timeout_records_the_round_as_void() {
        let (mut engine, tokens) = started(3, 2);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 2, 3).unwrap();
        engine
            .check_timeout(Instant::now() + engine.turn_timeout() * 2)
            .unwrap();
        let last = engine.call_history().last().unwrap();
        assert!(
            matches!(last, HistoryEvent::TurnTimeout { .. }),
            "超时应当记一条 TurnTimeout，实际: {last:?}"
        );
        assert_eq!(last.quantity_and_face(), None, "超时不是叫数");
        assert!(last.message().contains("超时判负"), "{}", last.message());
    }

    #[test]
    fn timeout_without_a_bid_adds_no_history_entry() {
        let (mut engine, _tokens) = started(3, 2);
        let before = engine.call_history().len();
        engine
            .check_timeout(Instant::now() + engine.turn_timeout() * 2)
            .unwrap();
        assert_eq!(engine.call_history().len(), before);
    }

    #[test]
    fn lobby_times_out_after_two_minutes() {
        let mut engine = engine_with(2, 5);
        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("Solo"));
        engine
            .handle_action("join", ActionParams::new(raw))
            .unwrap();
        let event = engine
            .check_timeout(Instant::now() + LOBBY_TIMEOUT * 2)
            .unwrap();
        assert_eq!(event, Some(TimeoutEvent::lobby_timeout()));
        assert!(event.unwrap().expired);
    }

    #[test]
    fn an_active_game_does_not_time_out() {
        let (mut engine, _tokens) = started(2, 3);
        assert_eq!(engine.check_timeout(Instant::now()).unwrap(), None);
    }

    #[test]
    fn turn_timeout_exceeds_the_wait_budget() {
        // 判负必须晚于等待预算，否则每个 state 都会先等到判负
        let engine = engine_with(2, 5);
        assert!(engine.turn_timeout() > engine.block_budget);
    }

    // ── 等待条件 ──

    #[test]
    fn ready_only_for_the_current_player() {
        let (mut engine, _tokens) = started(3, 3);
        engine.current_turn = 1;
        assert!(!engine.is_ready_for(0));
        assert!(engine.is_ready_for(1));
        assert!(!engine.is_ready_for(2));
    }

    #[test]
    fn finished_game_is_ready_for_everyone() {
        let (mut engine, tokens) = started(2, 3);
        script(&mut engine, vec![vec![2, 2, 2], vec![3, 3, 3]]);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 3, 2).unwrap();
        challenge(&mut engine, 1, &tokens[1]).unwrap();
        assert_eq!(engine.phase(), "finished");
        assert!(engine.is_ready_for(0));
        assert!(engine.is_ready_for(1));
    }

    // ── 提示词 ──

    #[test]
    fn prompts_render_without_degradation_markers() {
        let (engine, _tokens) = started(3, 3);
        for index in 0..3 {
            let view = engine.player_view(Some(index)).unwrap();
            assert!(!view.prompt.contains("{missing:"), "{}", view.prompt);
            assert!(!view.prompt.contains("[unrendered]"), "{}", view.prompt);
            assert_ne!(view.prompt, "", "提示词不该为空");
        }
    }

    #[test]
    fn waiting_prompt_reports_the_player_count() {
        let mut engine = engine_with(2, 5);
        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("Solo"));
        engine
            .handle_action("join", ActionParams::new(raw))
            .unwrap();
        let view = engine.player_view(Some(0)).unwrap();
        assert!(view.prompt.contains("1/2"), "{}", view.prompt);
    }

    #[test]
    fn your_turn_prompt_names_the_available_actions() {
        let (mut engine, _tokens) = started(2, 3);
        engine.current_turn = 0;
        let view = engine.player_view(Some(0)).unwrap();
        assert!(view.prompt.contains("liars_dice_bid"), "{}", view.prompt);
        assert!(
            view.prompt.contains("liars_dice_challenge"),
            "{}",
            view.prompt
        );
    }

    #[test]
    fn not_your_turn_prompt_does_not_claim_the_caller_waited() {
        let (mut engine, _tokens) = started(2, 3);
        engine.current_turn = 1;
        engine.last_action = Instant::now();
        let view = engine.player_view(Some(0)).unwrap();
        assert!(!view.prompt.contains("你已等待"), "{}", view.prompt);
        assert!(view.prompt.contains("最多挂起"), "{}", view.prompt);
    }

    // ── 视图 ──

    #[test]
    fn public_view_hides_all_dice() {
        let (engine, _tokens) = started(2, 3);
        let public = engine.public_view();
        assert!(public.get("all_dice").is_none(), "{public}");
        assert!(engine.admin_view().get("all_dice").is_some());
    }

    #[test]
    fn finished_player_view_reveals_all_dice() {
        let (mut engine, tokens) = started(2, 3);
        script(&mut engine, vec![vec![2, 2, 2], vec![3, 3, 3]]);
        engine.current_turn = 0;
        bid(&mut engine, 0, &tokens[0], 3, 2).unwrap();
        challenge(&mut engine, 1, &tokens[1]).unwrap();
        let view = engine.player_view(Some(0)).unwrap();
        assert!(view.private.get("all_dice").is_some(), "{}", view.private);
        assert_eq!(view.private["winner"], 0);
    }

    #[test]
    fn unknown_action_is_rejected() {
        let (mut engine, _tokens) = started(2, 3);
        let err = engine
            .handle_action("nope", ActionParams::default())
            .unwrap_err();
        assert!(matches!(err, EngineError::UnknownAction(_)));
    }

    #[test]
    fn join_requires_a_player_name() {
        let mut engine = engine_with(2, 5);
        let err = engine
            .handle_action("join", ActionParams::default())
            .unwrap_err();
        assert!(err.to_string().contains("player_name"), "{err}");
    }

    #[test]
    fn join_returns_the_token_and_index() {
        let mut engine = engine_with(2, 5);
        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("A"));
        let view = engine
            .handle_action("join", ActionParams::new(raw))
            .unwrap();
        assert_eq!(view.private["player_index"], 0);
        assert!(
            view.private["player_token"]
                .as_str()
                .is_some_and(|t| !t.is_empty())
        );
        assert!(view.prompt.contains("你是玩家 0"), "{}", view.prompt);
    }

    #[test]
    fn multi_player_engine_exposes_its_tokens() {
        let (engine, tokens) = started(2, 3);
        assert_eq!(engine.player_tokens(), Some(tokens.clone()));
        assert_eq!(engine.find_player_by_token(&tokens[1]), Some(1));
        assert_eq!(engine.find_player_by_token("nope"), None);
    }

    #[test]
    fn state_returns_the_callers_view() {
        let (mut engine, tokens) = started(2, 3);
        let mut raw = serde_json::Map::new();
        raw.insert("player_index".to_owned(), Value::from(1));
        raw.insert("player_token".to_owned(), Value::from(tokens[1].clone()));
        let view = engine
            .handle_action("state", ActionParams::new(raw))
            .unwrap();
        assert_eq!(view.private["your_index"], 1);
    }

    // ═══════════════════════════════════════════
    // HistoryEvent 的访问器
    // ═══════════════════════════════════════════

    /// 三个变体各自报出自己的玩家索引。
    ///
    /// 请守住 `player_idx()` 的 match——把它简化成只处理 `Bid`、或者返回常量，
    /// 都会让"历史记录指向谁"这件事出错，而公开视图与提示词都靠它。
    #[test]
    fn every_history_variant_reports_its_player() {
        let bid = HistoryEvent::Bid(Bid::new(2, 3, 4, "x".to_owned()));
        let challenge = HistoryEvent::Challenge {
            player_idx: 1,
            actual: 5,
            face: 6,
            message: "开".to_owned(),
        };
        let timeout = HistoryEvent::TurnTimeout {
            player_idx: 0,
            message: "超时".to_owned(),
        };

        assert_eq!(bid.player_idx(), 2);
        assert_eq!(challenge.player_idx(), 1);
        assert_eq!(timeout.player_idx(), 0);
    }

    /// 只有 `Bid` 有 quantity/face；另外两个变体必须是 `None`。
    ///
    /// 请守住"枚举相对于哨兵值"这件事：改前用 `quantity == 0` 表示非叫数，
    /// 于是"非叫数却有数量"这种状态是写不出来的——现在由类型保证。
    #[test]
    fn only_bids_carry_quantity_and_face() {
        let bid = HistoryEvent::Bid(Bid::new(0, 3, 4, "x".to_owned()));
        assert_eq!(bid.quantity_and_face(), Some((3, 4)));
        assert!(bid.as_bid().is_some());

        let challenge = HistoryEvent::Challenge {
            player_idx: 1,
            actual: 5,
            face: 6,
            message: "开".to_owned(),
        };
        assert_eq!(challenge.quantity_and_face(), None);
        assert!(challenge.as_bid().is_none());

        let timeout = HistoryEvent::TurnTimeout {
            player_idx: 0,
            message: "超时".to_owned(),
        };
        assert_eq!(timeout.quantity_and_face(), None);
        assert!(timeout.as_bid().is_none());
    }

    /// 每个变体的 `message()` 都要能取到自己的文字。
    #[test]
    fn every_history_variant_reports_its_message() {
        assert_eq!(
            HistoryEvent::Bid(Bid::new(0, 1, 2, "叫".to_owned())).message(),
            "叫"
        );
        assert_eq!(
            HistoryEvent::Challenge {
                player_idx: 0,
                actual: 1,
                face: 2,
                message: "开".to_owned(),
            }
            .message(),
            "开"
        );
        assert_eq!(
            HistoryEvent::TurnTimeout {
                player_idx: 0,
                message: "超时".to_owned(),
            }
            .message(),
            "超时"
        );
    }

    // ═══════════════════════════════════════════
    // player_name：收掉 5 处 map_or("?")
    // ═══════════════════════════════════════════

    // ═══════════════════════════════════════════
    // 两个摆骰子入口的语义必须一致
    // ═══════════════════════════════════════════

    /// **开局前的零骰不是出局**。
    ///
    /// 这是 `alive` 与 `dice` 双真相最容易踩的地方：`Player::new` 造出来是
    /// `dice: []` + `alive: true`（还没发骰），如果把 `alive` 改写成
    /// `!dice.is_empty()`，新玩家会被判出局，`start_game` 跳过所有人。
    #[test]
    fn empty_dice_before_start_is_not_elimination() {
        let mut engine =
            LiarsDiceEngine::new(&CreateParams::default(), &Config::default()).unwrap();
        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("Solo"));
        engine
            .handle_action("join", ActionParams::new(raw))
            .unwrap();

        // 大厅里骰子是空的，但玩家在场
        assert_eq!(engine.players()[0].dice, Vec::<i64>::new());
        assert!(
            engine.players()[0].is_alive(),
            "开局前的零骰是\"还没发骰\"，不是出局"
        );

        // 摆一个空骰也仍然是"在场"
        engine.set_player_dice(0, Vec::new());
        assert!(
            engine.players()[0].is_alive(),
            "set_player_dice 在开局前摆空骰不该把人标出局"
        );
    }

    /// 开局后零骰才是出局，且两个入口给出同样的判断。
    #[test]
    fn both_dice_setters_agree_after_start() {
        // 全场入口
        let (mut whole, _tokens) = started(3, 2);
        whole.set_dice(vec![vec![1, 2], Vec::new(), vec![3, 4]]);
        assert!(whole.players()[0].is_alive());
        assert!(
            !whole.players()[1].is_alive(),
            "开局后摆空骰 = 出局（全场入口）"
        );
        // 光断言 is_alive 不够：忘掉 `player.dice = dice` 这行时它仍是 false
        // （因为构造时 dice 本来就空），测试会漏掉"骰子没被真正写进去"。
        assert_eq!(
            whole.players()[1].dice,
            Vec::<i64>::new(),
            "骰子内容要真的被写成空"
        );
        assert_eq!(whole.players()[0].dice, vec![1, 2], "别把别人的骰子改了");

        // 单人入口
        let (mut single, _tokens) = started(3, 2);
        single.set_player_dice(1, Vec::new());
        assert!(
            !single.players()[1].is_alive(),
            "开局后摆空骰 = 出局（单人入口）"
        );
        assert_eq!(
            single.players()[1].dice,
            Vec::<i64>::new(),
            "骰子内容要真的被写成空"
        );
        assert!(single.players()[0].is_alive(), "没被动过的玩家仍在场");

        // 单人入口：非空骰 -> 复活，且骰子内容真的写进去了
        single.set_player_dice(1, vec![5, 6]);
        assert!(
            single.players()[1].is_alive(),
            "摆回非空骰应当复活（与全场入口规则一致）"
        );
        assert_eq!(single.players()[1].dice, vec![5, 6], "骰子内容要真的写进去");
    }

    /// 对局结束后不许再摆骰子。
    ///
    /// 结束后 `winner` / `loser` 已经定下。若还允许改骰子，会出现
    /// "赢家已定但赢家没骰子"或"某人还没出局但 winner 已经是他"这类
    /// 自相矛盾的状态。两个入口都要挡住。
    #[test]
    fn dice_setters_refuse_after_the_game_finished() {
        let (mut engine, tokens) = started(2, 1);
        engine.set_dice(vec![vec![1], vec![2]]);
        engine.set_current_turn(0);
        bid(&mut engine, 0, &tokens[0], 1, 3).unwrap();
        engine
            .handle_action(
                "challenge",
                ActionParams::new({
                    let mut raw = serde_json::Map::new();
                    raw.insert("player_index".to_owned(), Value::from(1));
                    raw.insert("player_token".to_owned(), Value::from(tokens[1].clone()));
                    raw
                }),
            )
            .unwrap();
        assert_eq!(engine.phase(), "finished", "2 人局开盅后应当结束");

        let before: Vec<Vec<i64>> = engine.players().iter().map(|p| p.dice.clone()).collect();
        let winner = engine.winner();

        // 两个入口都该被挡住
        engine.set_dice(vec![vec![9, 9], vec![9, 9]]);
        engine.set_player_dice(0, vec![9, 9]);

        let after: Vec<Vec<i64>> = engine.players().iter().map(|p| p.dice.clone()).collect();
        assert_eq!(after, before, "结束后摆骰子必须被忽略");
        assert_eq!(engine.winner(), winner, "winner 不该受影响");
    }

    #[test]
    fn player_name_returns_the_real_name() {
        let (engine, _tokens) = started(2, 3);
        assert_eq!(engine.player_name(0), engine.players()[0].name);
        assert_eq!(engine.player_name(1), engine.players()[1].name);
        // 越界只是末日兜底，不是预期状态
        assert_eq!(engine.player_name(99), "?");
    }

    // ═══════════════════════════════════════════
    // 状态转移的合法性
    // ═══════════════════════════════════════════

    /// 非法转移必须返回 `Err`，**不能 panic**。
    ///
    /// 为什么这条重要：引擎 panic 会被 `GameManager::act` 的 `catch_unwind`
    /// 捕获并毒化整个对局（`supervisor.rs`）；`check_timeout` 里的 panic 还会
    /// 走进 AGENTS.md 第 10 条的隔离路径——那是给"引擎内部 bug"兜底的，
    /// 不该用来做常规的非法转移检查。
    #[test]
    fn illegal_transitions_return_an_error_instead_of_panicking() {
        let (mut engine, _tokens) = started(2, 2);

        // 已开局（Bidding），不能回到 Waiting
        let err = engine.transition_to(Phase::Waiting).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidInput(_)),
            "非法转移应当是 InvalidInput，实际: {err:?}"
        );
        assert!(engine.phase() == "bidding", "失败的转移不该改变状态");

        // 把对局打到结束，再试倒流
        engine.set_dice(vec![vec![1], vec![2]]);
        engine.set_current_turn(0);
        let tokens: Vec<String> = engine.players().iter().map(|p| p.token.clone()).collect();
        bid(&mut engine, 0, &tokens[0], 1, 3).unwrap();
        engine
            .handle_action(
                "challenge",
                ActionParams::new({
                    let mut raw = serde_json::Map::new();
                    raw.insert("player_index".to_owned(), Value::from(1));
                    raw.insert("player_token".to_owned(), Value::from(tokens[1].clone()));
                    raw
                }),
            )
            .unwrap();
        assert_eq!(engine.phase(), "finished");

        // Finished 不能回到 Bidding，也不能回到 Waiting
        assert!(engine.transition_to(Phase::Bidding).is_err());
        assert!(engine.transition_to(Phase::Waiting).is_err());
        assert_eq!(engine.phase(), "finished", "状态不该被改坏");
    }

    /// 合法转移的两条都走得通。
    #[test]
    fn the_two_legal_transitions_are_allowed() {
        // Waiting -> Bidding：满员开局
        let (mut engine, _tokens) = started(2, 2);
        assert_eq!(engine.phase(), "bidding");

        // 再确认 `can_transition_to` 的判定本身
        assert!(Phase::Waiting.can_transition_to(Phase::Bidding));
        assert!(Phase::Bidding.can_transition_to(Phase::Finished));
        assert!(!Phase::Bidding.can_transition_to(Phase::Waiting));
        assert!(!Phase::Waiting.can_transition_to(Phase::Finished));
        assert!(!Phase::Finished.can_transition_to(Phase::Bidding));
        assert!(!Phase::Finished.can_transition_to(Phase::Waiting));

        // 自转移到同一状态也不合法：满员后再调一次开局应当报错
        let err = engine.transition_to(Phase::Bidding).unwrap_err();
        assert!(matches!(err, EngineError::InvalidInput(_)));
    }

    /// 错误消息里要带出**当前阶段名**（走 `Display for Phase`）。
    ///
    /// 变异测试发现：把 `fmt` 改成 `Ok(Default::default())`（输出空串）没有
    /// 用例变红。`Display` 在两处错误分支里用，都没有覆盖到阶段名的具体值：
    ///
    /// - `check_turn` 的"游戏阶段是 {}，不能行动"
    /// - `add_player` 的"对局已开始（{}），无法加入"
    #[test]
    fn error_messages_name_the_current_phase() {
        // 分支一：对局已结束，还想行动 → "游戏阶段是 finished，不能行动"
        let (mut engine, tokens) = started(2, 1);
        engine.set_dice(vec![vec![1], vec![2]]);
        engine.set_current_turn(0);
        bid(&mut engine, 0, &tokens[0], 1, 3).unwrap();
        engine
            .handle_action(
                "challenge",
                ActionParams::new({
                    let mut raw = serde_json::Map::new();
                    raw.insert("player_index".to_owned(), Value::from(1));
                    raw.insert("player_token".to_owned(), Value::from(tokens[1].clone()));
                    raw
                }),
            )
            .unwrap();
        assert_eq!(engine.phase(), "finished");

        let mut raw = serde_json::Map::new();
        raw.insert("player_index".to_owned(), Value::from(0));
        raw.insert("player_token".to_owned(), Value::from(tokens[0].clone()));
        raw.insert("quantity".to_owned(), Value::from(1));
        raw.insert("face".to_owned(), Value::from(2));
        let err = engine
            .handle_action("bid", ActionParams::new(raw))
            .unwrap_err();
        assert!(
            err.to_string().contains("finished"),
            "错误消息必须带出当前阶段名（finished），实际: {err}"
        );

        // 分支二：已开局后再 join → "对局已开始（bidding），无法加入"
        let (mut engine2, _tokens) = started(2, 2);
        assert_eq!(engine2.phase(), "bidding");
        let mut raw = serde_json::Map::new();
        raw.insert("player_name".to_owned(), Value::from("Late"));
        let err = engine2
            .handle_action("join", ActionParams::new(raw))
            .unwrap_err();
        assert!(
            err.to_string().contains("bidding"),
            "join 的错误消息也要带阶段名（bidding），实际: {err}"
        );
    }
}

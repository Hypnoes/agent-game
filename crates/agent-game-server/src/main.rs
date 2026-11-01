//! `agent-game-server` 命令行入口。
//!
//! 只有一个子命令都没有：`agent-game-server` 直接起服务，`--help` 打印用法。
//! 端口与 admin token 都有文档承诺（README 与 DSH 的 MCP 配置依赖它们），
//! `tests/cli.rs` 用 `try_parse_from` 钉住，不真的监听端口。

use std::net::SocketAddr;
use std::path::PathBuf;

use agent_game_server_core::Config;
use agent_game_server_server::{AppState, build_registry, create_app, poller};
use clap::Parser;

/// AI Agent 对弈游戏服务端。
#[derive(Debug, Parser)]
#[command(
    name = "agent-game-server",
    version,
    about = "AI Agent 对弈游戏服务端（MCP + REST + Admin 同一个端口）"
)]
pub struct Cli {
    /// 监听地址。
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,

    /// 监听端口。README 与 MCP 配置都指向 8800。
    #[arg(long, default_value_t = 8800)]
    pub port: u16,

    /// 提示词目录。省略时用编译期内置的模板。
    #[arg(long)]
    pub prompt_dir: Option<PathBuf>,

    /// 等待预算（秒）：一次 state 调用最多挂起多久。
    #[arg(long, default_value_t = agent_game_server_core::config::DEFAULT_BLOCK_BUDGET_SECS)]
    pub block_budget: f64,

    /// 回合时限（秒）：当前回合多久不行动判负。
    #[arg(long, default_value_t = agent_game_server_core::config::DEFAULT_TURN_TIMEOUT_SECS)]
    pub turn_timeout: u64,
}

impl Cli {
    /// 从命令行参数解析。测试用它断言默认值，不启动服务。
    ///
    /// # Errors
    /// 参数不合法（clap 的报错）。
    pub fn parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Self::try_parse_from(args)
    }

    /// 由命令行参数构造配置。
    #[must_use]
    pub fn config(&self) -> Config {
        Config {
            block_budget: std::time::Duration::from_secs_f64(self.block_budget.max(0.0)),
            turn_timeout: std::time::Duration::from_secs(self.turn_timeout),
            ..Config::default()
        }
    }

    /// 监听地址。
    ///
    /// # Errors
    /// host/port 拼不出合法 `SocketAddr`。
    pub fn socket_addr(&self) -> Result<SocketAddr, String> {
        format!("{}:{}", self.host, self.port)
            .parse()
            .map_err(|error| format!("无效的监听地址 {}:{}: {error}", self.host, self.port))
    }
}

/// 解析 admin token：有环境变量就用它，否则随机生成。
///
/// 绝不退化成"没配就不校验"——那会让 `/api/v1/admin/games` 对任何能连上端口的人
/// 敞开全部秘密信息。随机生成的分支由 [`banner`] 打印出来，
/// 那是运维拿到它的唯一途径。
#[must_use]
pub fn resolve_admin_token() -> (String, bool) {
    resolve_admin_token_from(std::env::var("AGENT_GAME_ADMIN_TOKEN").ok())
}

/// [`resolve_admin_token`] 的纯函数版本：只决定"给定这个配置值该用什么 token"。
///
/// 抽出来是为了可测：`std::env::set_var` 在 Rust 2024 里是 `unsafe`
/// （并发读写环境变量有数据竞争），而本 workspace 禁止 `unsafe`。
/// 把决策与读取分开之后，两个分支都能用普通值直接覆盖。
#[must_use]
fn resolve_admin_token_from(configured: Option<String>) -> (String, bool) {
    match configured {
        // 空字符串不算"配置了"：`AGENT_GAME_ADMIN_TOKEN=` 是常见的
        // "我设了但忘了填" 形态，此时必须回落到随机值。
        Some(token) if !token.is_empty() => (token, false),
        _ => (random_token(), true),
    }
}

fn random_token() -> String {
    // 24 字节 ≈ 192 位熵，与 Python 版 `secrets.token_urlsafe(24)` 同量级
    use std::fmt::Write as _;

    use rand::RngExt as _;
    let mut rng = rand::rng();
    let mut token = String::with_capacity(48);
    for _ in 0..24 {
        let byte: u8 = rng.random_range(0..=255u8);
        // 写进 String 不会失败，忽略返回值即可
        let _ = write!(token, "{byte:02x}");
    }
    token
}

/// 启动横幅。token 必须出现在这里。
#[must_use]
pub fn banner(addr: &SocketAddr, token: &str, generated: bool) -> String {
    let source = if generated {
        "本次运行随机生成"
    } else {
        "来自 AGENT_GAME_ADMIN_TOKEN"
    };
    format!(
        "🎮 agent-game starting...\n\
         \x20  Game API:   http://localhost:{port}\n\
         \x20  Admin:      http://localhost:{port}/admin?token={token}\n\
         \x20  Admin 查询头: X-Admin-Token: {token}\n\
         \x20  MCP:        http://localhost:{port}/mcp\n\
         \x20  Admin token {source}\n",
        port = addr.port()
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 子进程模式：只解析一次 token 然后打印，不起服务。
    //
    // 这是唯一能验证"`resolve_admin_token` 真的读了环境变量"的办法：
    // `std::env::set_var` 在 Rust 2024 里是 `unsafe`，而本 workspace 禁止
    // `unsafe`。子进程可以干净地控制自己的环境。见 `token_env_tests`。
    if std::env::var("AGENT_GAME_TEST_PRINT_TOKEN").is_ok() {
        let (token, generated) = resolve_admin_token();
        println!("token={token}");
        println!("generated={generated}");
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("agent_game=info,warn")),
        )
        .init();

    let cli = Cli::parse();
    let addr = cli.socket_addr()?;
    let config = cli.config();
    let (token, generated) = resolve_admin_token();

    let mut state = AppState::new(build_registry(), config.clone(), token.clone());
    if let Some(dir) = &cli.prompt_dir {
        agent_game_server_server::state::set_prompt_dir(&mut state, dir.clone());
    }

    print!("{}", banner(&addr, &token, generated));

    // 轮询任务：唯一的周期超时结算点
    let _poller = poller::spawn(
        std::sync::Arc::clone(&state.manager),
        std::sync::Arc::clone(&state.broadcast),
        config.poll_interval,
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, create_app(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let cli = Cli::parse_from(["agent-game-server"]).unwrap();
        assert_eq!(cli.port, 8800);
        assert_eq!(cli.host, "0.0.0.0");
        assert!((cli.block_budget - 55.0).abs() < f64::EPSILON);
        assert_eq!(cli.turn_timeout, 60);
        assert!(cli.prompt_dir.is_none());
    }

    #[test]
    fn the_socket_address_uses_host_and_port() {
        let cli = Cli::parse_from(["agent-game-server", "--port", "9000"]).unwrap();
        assert_eq!(cli.socket_addr().unwrap().port(), 9000);
    }

    #[test]
    fn help_does_not_require_a_server() {
        let error = Cli::parse_from(["agent-game-server", "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("8800") || error.to_string().contains("Usage"));
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(Cli::parse_from(["agent-game-server", "--bogus"]).is_err());
    }

    #[test]
    fn the_banner_prints_the_token_and_the_source() {
        let addr: SocketAddr = "0.0.0.0:8800".parse().unwrap();
        let text = banner(&addr, "tok", true);
        assert!(text.contains("tok"), "{text}");
        assert!(text.contains("token=tok"), "{text}");
        assert!(text.contains("随机生成"), "{text}");
        assert!(text.contains("8800"), "{text}");

        let text = banner(&addr, "tok", false);
        assert!(text.contains("AGENT_GAME_ADMIN_TOKEN"), "{text}");
    }

    #[test]
    fn the_random_token_has_enough_entropy() {
        let first = random_token();
        let second = random_token();
        assert_eq!(first.len(), 48, "24 字节的十六进制");
        assert_ne!(first, second);
    }

    #[test]
    fn a_configured_token_is_used_verbatim_and_reported_as_not_generated() {
        // 这两个变异体（返回 "xyzzy"）只有在真的走到"已配置"分支时才杀掉。
        let (token, generated) = resolve_admin_token_from(Some("configured-token-123".to_owned()));
        assert_eq!(token, "configured-token-123", "必须原样返回配置的 token");
        assert!(!generated, "配置来的不该标成随机生成");
    }

    #[test]
    fn an_empty_configuration_falls_back_to_a_generated_token() {
        // `Some(token) if !token.is_empty()` 的 `!` 被删掉会让空字符串被当成
        // "已配置"——那等于 admin 接口用空 token 就能通过。
        let (token, generated) = resolve_admin_token_from(Some(String::new()));
        assert!(!token.is_empty(), "空配置必须回落到随机 token");
        assert_eq!(token.len(), 48, "回落值应当是一个随机 token");
        assert!(generated, "回落值必须标成随机生成");
    }

    #[test]
    fn a_missing_configuration_generates_a_token() {
        let (token, generated) = resolve_admin_token_from(None);
        assert_eq!(token.len(), 48);
        assert!(generated);
        // 两次生成不该相同
        let (other, _) = resolve_admin_token_from(None);
        assert_ne!(token, other);
    }
    #[test]
    fn config_honours_the_flags() {
        let cli = Cli::parse_from([
            "agent-game-server",
            "--block-budget",
            "7",
            "--turn-timeout",
            "90",
        ])
        .unwrap();
        let config = cli.config();
        assert!((config.block_budget_secs() - 7.0).abs() < f64::EPSILON);
        assert_eq!(config.turn_timeout, std::time::Duration::from_secs(90));
        // 两条时间轴仍然保持"判负晚于等待"的关系
        assert!(config.turn_timeout > config.block_budget);
    }
}

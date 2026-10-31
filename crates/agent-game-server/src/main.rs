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
/// 敞开全部秘密信息。随机生成的分支由 [`print_banner`] 打印出来，
/// 那是运维拿到它的唯一途径。
#[must_use]
pub fn resolve_admin_token() -> (String, bool) {
    match std::env::var("AGENT_GAME_ADMIN_TOKEN") {
        Ok(token) if !token.is_empty() => (token, false),
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

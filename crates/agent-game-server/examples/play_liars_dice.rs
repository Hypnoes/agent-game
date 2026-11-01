//! 一个最小的 MCP 客户端：用两条连接把大话骰打到结束。
//!
//! 这是**手工验收工具**，不是测试：它连真实进程、走真实 HTTP、跑真实的一整局。
//! 单元测试与集成测试都在进程内跑（`tokio::spawn` + 内存 app），而真实部署里
//! 有两个东西只有这个 example 能验到：
//!
//! 1. **两条独立的 TCP 连接**。`legacy_session_mode` 下同一条 MCP 会话内的
//!    请求是串行处理的，两个玩家共用一条连接会互相阻塞——那不是等待逻辑的问题，
//!    而是"每个 Agent 一条连接"这个部署形态必须被验证。
//! 2. **SSE 解析**。服务端回的是 `text/event-stream`，裸客户端要能正确切帧。
//!
//! 用法：
//!
//! ```bash
//! cargo run --release -- --port 8800            # 终端 A
//! cargo run -p agent-game-server-server --example play_liars_dice -- 8800   # 终端 B
//! ```
//!
//! 退出码：整局打到 `finished` 为 0，任何时候出错为 1。

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// MCP 的 Streamable HTTP 要求这两个头，否则 SDK 拒绝建立会话。
const ACCEPT: &str = "application/json, text/event-stream";

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// 一条 MCP 连接。
struct Conn {
    client: reqwest::Client,
    base: String,
    session: String,
    next_id: u64,
}

impl Conn {
    /// 握手：`initialize` + `notifications/initialized`，拿到 `Mcp-Session-Id`。
    async fn open(base: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()?;
        let response = client
            .post(format!("{base}/mcp"))
            .header("Accept", ACCEPT)
            .header("Content-Type", "application/json")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "play_liars_dice", "version": "1"},
                },
            }))
            .send()
            .await?;

        if response.status() != 200 {
            return Err(format!("initialize 返回 {}（/mcp 不该重定向）", response.status()).into());
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .ok_or("initialize 响应缺少 Mcp-Session-Id")?
            .to_str()?
            .to_owned();
        let _ = response.text().await?;

        let mut conn = Self {
            client,
            base: base.to_owned(),
            session,
            next_id: 1,
        };
        conn.notify("notifications/initialized").await?;
        Ok(conn)
    }

    async fn notify(&mut self, method: &str) -> Result<()> {
        let response = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("Accept", ACCEPT)
            .header("Content-Type", "application/json")
            .header("mcp-session-id", &self.session)
            .json(&json!({"jsonrpc": "2.0", "method": method}))
            .send()
            .await?;
        // 通知没有响应体，200/202 都算成功
        if response.status().as_u16() >= 400 {
            return Err(format!("{method} 返回 {}", response.status()).into());
        }
        Ok(())
    }

    /// 一次 `tools/call`，返回 `(是否错误, 文本)`。
    async fn call(&mut self, tool: &str, arguments: Value) -> Result<(bool, String)> {
        self.next_id += 1;
        let id = self.next_id;
        let response = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("Accept", ACCEPT)
            .header("Content-Type", "application/json")
            .header("mcp-session-id", &self.session)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }))
            .send()
            .await?;
        if response.status() != 200 {
            return Err(format!("{tool} 的传输层返回 {}", response.status()).into());
        }

        let body = response.text().await?;
        let message = first_message(&body).ok_or("响应体里没有 JSON-RPC 消息")?;
        if let Some(error) = message.get("error") {
            return Err(format!("{tool} 协议错误: {error}").into());
        }
        let result = &message["result"];
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        Ok((is_error, text))
    }
}

/// 从响应体里取第一条 JSON-RPC 消息。
///
/// 可能是 `application/json`，也可能是 SSE（复数 `data:` 行 + 空行分隔）。
/// 后者是真实形态，前者是简单请求的优化。
fn first_message(body: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
        return Some(value);
    }
    let mut buffer = String::new();
    for line in body.lines() {
        if let Some(payload) = line.strip_prefix("data:") {
            buffer.push_str(payload.trim_start());
        } else if line.trim().is_empty() && !buffer.is_empty() {
            if let Ok(value) = serde_json::from_str(&buffer) {
                return Some(value);
            }
            buffer.clear();
        }
    }
    if buffer.is_empty() {
        None
    } else {
        serde_json::from_str(&buffer).ok()
    }
}

/// 从 join 的提示词里取出 token：`🎲 你是玩家 0，token: xxxx`。
fn extract_token(prompt: &str) -> Option<String> {
    let marker = "token: ";
    let start = prompt.find(marker)? + marker.len();
    let rest = &prompt[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    Some(rest[..end].trim().to_owned())
}

/// 从 prompt 里取出玩家编号，用于在"轮到你了"的文本里定位自己。
fn extract_index(prompt: &str) -> Option<usize> {
    let marker = "你是玩家 ";
    let start = prompt.find(marker)? + marker.len();
    let digits: String = prompt[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// 把一局打完。
///
/// 策略：轮到自己时，能加码就加一点、不能就开盅。每一步都从服务端读真实状态，
/// 不假设自己知道别人的骰子。
async fn play(base: &str, players: usize) -> Result<()> {
    println!("连接 {base}，{players} 名玩家…");

    // 每个玩家一条连接：这不是为了并行，而是为了复现真实部署
    let mut conns = Vec::with_capacity(players);
    for _ in 0..players {
        conns.push(Conn::open(base).await?);
    }

    // 入场：第一个人 join 会建局，后续 join 靠匹配并进同一桌。
    //
    // `player_count` 必须显式传：默认是 2，两个人一坐满就开局，第三个人
    // 只能进新桌——那是**正确**行为（开局后不允许再加人）。要打 N 人局，
    // 就得让 N 个人都在开局前入场。
    let mut tokens = Vec::with_capacity(players);
    let mut indices = Vec::with_capacity(players);
    for (seat, conn) in conns.iter_mut().enumerate() {
        let name = format!("P{seat}");
        let (error, text) = conn
            .call(
                "liars_dice_join",
                json!({"player_name": name, "player_count": players}),
            )
            .await?;
        if error {
            return Err(format!("{name} 入场失败: {text}").into());
        }
        let token =
            extract_token(&text).ok_or_else(|| format!("{name} 的 prompt 里没有 token: {text}"))?;
        let index = extract_index(&text).unwrap_or(seat);
        println!("  {name} 入场：index={index} token={token}");

        // 诊断：N 人局在开场前必须始终只有 1 桌。出现第 2 桌说明匹配没并上。
        let admin = std::env::var("AGENT_GAME_ADMIN_TOKEN").unwrap_or_default();
        if let Ok(count) = session_count(base, &admin).await {
            println!("     服务端对局数: {count}");
            if count > 1 {
                return Err(format!("{name} 入场后服务端出现 {count} 局：没并进同一桌").into());
            }
        }
        tokens.push(token);
        indices.push(index);
    }

    // 服务端的 session id：从 admin 列表里拿（这是验收工具，用 admin token 是合理的）
    let admin_token = std::env::var("AGENT_GAME_ADMIN_TOKEN").unwrap_or_default();

    // 打牌循环
    let started = Instant::now();
    let mut steps = 0;
    let deadline = Duration::from_secs(300);

    while started.elapsed() < deadline {
        for seat in 0..players {
            steps += 1;
            let (error, text) = conns[seat]
                .call(
                    "liars_dice_state",
                    json!({
                        "player_index": indices[seat],
                        "player_token": tokens[seat],
                        "timeout": 5,
                    }),
                )
                .await?;

            if error {
                return Err(format!("P{seat} 的 state 失败: {text}").into());
            }

            // 已经结束
            if text.contains("游戏结束") || text.contains("获胜") {
                println!("\n打完：{steps} 次调用，{:?}", started.elapsed());
                println!("{text}");
                report(base, &admin_token).await?;
                return Ok(());
            }

            // 没轮到：继续试下一个人
            if text.contains("等待") && !text.contains("轮到你了") {
                continue;
            }

            // 轮到我了：读当前叫数决定加码还是开盅
            if !text.contains("轮到你了") {
                continue;
            }

            let (quantity, face) = parse_current_call(&text);
            let step = if quantity == 0 {
                // 本轮没人叫过：先叫一手小的
                conns[seat]
                    .call(
                        "liars_dice_bid",
                        json!({
                            "player_index": indices[seat],
                            "player_token": tokens[seat],
                            "quantity": 1,
                            "face": 2,
                        }),
                    )
                    .await?
            } else if quantity < 8 {
                conns[seat]
                    .call(
                        "liars_dice_bid",
                        json!({
                            "player_index": indices[seat],
                            "player_token": tokens[seat],
                            "quantity": quantity + 1,
                            "face": face,
                        }),
                    )
                    .await?
            } else {
                // 已经叫得很高：开盅
                conns[seat]
                    .call(
                        "liars_dice_challenge",
                        json!({
                            "player_index": indices[seat],
                            "player_token": tokens[seat],
                        }),
                    )
                    .await?
            };

            let (step_error, step_text) = step;
            if step_error {
                // 加码被拒（比如超过总数）就改开盅
                let (retry_error, retry_text) = conns[seat]
                    .call(
                        "liars_dice_challenge",
                        json!({
                            "player_index": indices[seat],
                            "player_token": tokens[seat],
                        }),
                    )
                    .await?;
                if retry_error {
                    return Err(format!(
                        "P{seat} 既不能加码也不能开盅: {step_text} / {retry_text}"
                    )
                    .into());
                }
                if retry_text.contains("游戏结束") || retry_text.contains("获胜") {
                    println!("\n打完：{steps} 次调用，{:?}", started.elapsed());
                    println!("{retry_text}");
                    report(base, &admin_token).await?;
                    return Ok(());
                }
            } else if step_text.contains("游戏结束") || step_text.contains("获胜") {
                println!("\n打完：{steps} 次调用，{:?}", started.elapsed());
                println!("{step_text}");
                report(base, &admin_token).await?;
                return Ok(());
            }
        }
    }

    Err("超时：300 秒内没打完".into())
}

/// 从"当前叫数: 3×4"里取出 `(3, 4)`；"无…"表示本轮还没有叫数。
fn parse_current_call(prompt: &str) -> (i64, i64) {
    let Some(line) = prompt.lines().find(|line| line.contains("当前叫数:")) else {
        return (0, 0);
    };
    let rest = line.split("当前叫数:").nth(1).unwrap_or("").trim();
    let Some((quantity, face)) = rest.split_once('×') else {
        return (0, 0);
    };
    let quantity: String = quantity.chars().filter(char::is_ascii_digit).collect();
    let face: String = face.chars().take_while(char::is_ascii_digit).collect();
    (quantity.parse().unwrap_or(0), face.parse().unwrap_or(0))
}

/// 读服务端的对局数量（诊断用）。
async fn session_count(base: &str, admin_token: &str) -> Result<usize> {
    if admin_token.is_empty() {
        return Ok(0);
    }
    let response = reqwest::Client::new()
        .get(format!("{base}/api/v1/admin/games"))
        .header("X-Admin-Token", admin_token)
        .send()
        .await?;
    let body: Value = response.json().await?;
    Ok(body["games"].as_array().map_or(0, Vec::len))
}

/// 从 admin 接口读一次对局摘要，作为"服务端视角"的交叉验证。
async fn report(base: &str, admin_token: &str) -> Result<()> {
    if admin_token.is_empty() {
        println!("（未设置 AGENT_GAME_ADMIN_TOKEN，跳过 admin 交叉验证）");
        return Ok(());
    }
    let response = reqwest::Client::new()
        .get(format!("{base}/api/v1/admin/games"))
        .header("X-Admin-Token", admin_token)
        .send()
        .await?;
    if response.status() != 200 {
        return Err(format!("admin 列表返回 {}", response.status()).into());
    }
    let body: Value = response.json().await?;
    for game in body["games"].as_array().into_iter().flatten() {
        let state = &game["state"];
        println!(
            "  服务端视角：session={} phase={} winner={} 全场骰子={}",
            game["session_id"].as_str().unwrap_or("?"),
            state["phase"].as_str().unwrap_or("?"),
            state["winner"],
            state["all_dice"],
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let port = std::env::args().nth(1).unwrap_or_else(|| "8800".to_owned());
    let base = format!("http://127.0.0.1:{port}");
    play(&base, 3).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames_are_parsed() {
        let body = "data: \nid: 0/0\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\nid: 1/0\n\n";
        let message = first_message(body).unwrap();
        assert_eq!(message["id"], 2);
    }

    #[test]
    fn plain_json_is_parsed() {
        let message = first_message(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap();
        assert_eq!(message["id"], 1);
    }

    #[test]
    fn the_token_is_extracted_from_the_join_prompt() {
        let prompt = "🎲 你是玩家 1，token: abc123\n🎲 已加入匹配队列！";
        assert_eq!(extract_token(prompt).as_deref(), Some("abc123"));
        assert_eq!(extract_index(prompt), Some(1));
    }

    #[test]
    fn the_current_call_is_parsed() {
        assert_eq!(parse_current_call("当前叫数: 3×4（P0 叫的）"), (3, 4));
        assert_eq!(
            parse_current_call("当前叫数: 无（你是第一个叫数的人）"),
            (0, 0)
        );
        assert_eq!(parse_current_call("没有这一行"), (0, 0));
    }
}

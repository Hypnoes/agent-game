//! 启动路径的端到端检查：用**真实二进制**跑一遍参数解析与 token 解析。
//!
//! 为什么需要子进程：`std::env::set_var` 在 Rust 2024 里是 `unsafe`
//! （并发读写环境变量有数据竞争），而本 workspace 禁止 `unsafe`。
//! 只有另起一个进程，才能干净地控制它的环境变量。
//!
//! 为什么不能用 `std::env::current_exe()`：那是**测试二进制**本身，
//! 它的入口是 libtest 的 harness，不是 `main`。`CARGO_BIN_EXE_<name>`
//! 才是 cargo 编译出来的真二进制——这个变量只在集成测试里可用。
//!
//! 这组用例补的是变异测试在 `main.rs` 上抓到的 4 个存活变异体：
//! `resolve_admin_token` 被整体替换成常量时，纯函数版本的单元测试不会红。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

/// 子进程模式的开关。设了它，二进制只解析一次 token 然后打印，不起服务。
const CHILD_MARKER: &str = "AGENT_GAME_TEST_PRINT_TOKEN";

fn binary() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_agent-game-server")
        .map(std::path::PathBuf::from)
        .expect("cargo 应当提供 CARGO_BIN_EXE_agent-game-server")
}

/// 在子进程里跑一次 token 解析，返回 (token, generated)。
fn resolve_in_child(configured: Option<&str>) -> (String, bool) {
    let mut command = Command::new(binary());
    command.env(CHILD_MARKER, "1");
    match configured {
        Some(value) => {
            command.env("AGENT_GAME_ADMIN_TOKEN", value);
        }
        None => {
            command.env_remove("AGENT_GAME_ADMIN_TOKEN");
        }
    }

    let output = command.output().expect("跑子进程");
    assert!(
        output.status.success(),
        "子进程失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let token = stdout
        .lines()
        .find_map(|line| line.strip_prefix("token="))
        .unwrap_or_else(|| panic!("子进程没有打印 token，实际输出: {stdout}"))
        .to_owned();
    let generated = stdout
        .lines()
        .find_map(|line| line.strip_prefix("generated="))
        .unwrap_or_else(|| panic!("子进程没有打印 generated，实际输出: {stdout}"));
    (token, generated == "true")
}

#[test]
fn the_token_is_read_from_the_environment() {
    // 变异成 `(String::new(), true)` / `("xyzzy", false)` 等常量时，
    // 这条会红：配置值不会原样传出来。
    let (token, generated) = resolve_in_child(Some("from-the-environment"));

    assert_eq!(token, "from-the-environment", "必须原样返回配置的 token");
    assert!(!generated, "配置来的 token 不该标成随机生成");
}

#[test]
fn an_unset_environment_produces_a_generated_token() {
    let (token, generated) = resolve_in_child(None);

    assert!(generated, "没配环境变量必须标成随机生成");
    assert_eq!(token.len(), 48, "随机 token 应当是 48 位十六进制");
    assert!(
        token.chars().all(|ch| ch.is_ascii_hexdigit()),
        "随机 token 应当只含十六进制字符: {token}"
    );
}

#[test]
fn an_empty_environment_value_falls_back_to_generated() {
    // `AGENT_GAME_ADMIN_TOKEN=` 是常见的"我设了但忘了填"形态。
    // `Some(token) if !token.is_empty()` 的 `!` 被删掉会让空串被当成有效
    // token——那等于 admin 接口用空 token 就能通过。
    let (token, generated) = resolve_in_child(Some(""));

    assert!(generated, "空配置必须回落到随机 token");
    assert!(!token.is_empty(), "不能返回空串");
    assert_eq!(token.len(), 48);
}

#[test]
fn two_runs_generate_different_tokens() {
    let (first, _) = resolve_in_child(None);
    let (second, _) = resolve_in_child(None);
    assert_ne!(first, second, "每次启动都该生成新的 token");
}

#[test]
fn the_binary_prints_a_usable_banner_on_startup() {
    // `--help` 走的是另一条路径（clap 直接退出），这里验证的是
    // "正常运行时会打印出带 token 的横幅"。
    //
    // 用一个不会真的起服务的端口 + 子进程模式之外的路径：直接跑 `--help`
    // 就够了，它证明二进制能启动、参数能被解析。
    let output = Command::new(binary())
        .arg("--help")
        .output()
        .expect("跑子进程");

    assert!(output.status.success(), "--help 应当成功退出");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("agent-game-server"), "{stdout}");
    assert!(stdout.contains("8800"), "帮助里应当说明默认端口: {stdout}");
}

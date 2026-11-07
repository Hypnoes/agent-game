//! 分层看门：所有跨层边界都由 **crate 边界 + 编译器**强制，不用源码扫描。
//!
//! ```text
//! agent-game-server-core      games/（具体规则）
//!        ↓
//! agent-game-server-domain    game / lobby / session / snapshot / supervisor / registry / testing
//!        ↓
//! agent-game-server-infra     budget / config / error / identity / params / prompt / signal / wait
//! ```
//!
//! # 为什么放弃源码扫描
//!
//! 这条约束最初由字符串匹配守（读 `.rs` 文本找 `use crate::domain` 之类的前缀）。
//! review 连续三轮各找到一批新的绕过写法，每一批都合法可编译却漏检：
//!
//! | 代 | 写法 | 为什么漏 |
//! |---|---|---|
//! | 1 | `pub(crate) use crate::domain::X;` | 不以 `use ` 开头 |
//! | 1 | `use crate::domain as dom;` | 别名，不是 `::` 后缀 |
//! | 1 | `use crate::{domain::X};` | 分组导入 |
//! | 2 | `pub(in crate::infra) use crate::domain::X;` | 可见性修饰更复杂 |
//! | 2 | `use crate::{infra::X, domain};` | 分组里的裸模块项 |
//! | 3 | `use super::super::domain::game::X;` | 相对路径，不含 `crate::` |
//! | 3 | `use crate :: domain :: game :: X;` | token 间空格 |
//! | 3 | `fn f(_: crate::domain::game::X) {}` | 根本不是 `use` |
//! | 4 | `use crate as root; use root::games::X;` | 别名后引用，前缀完全不匹配 |
//!
//! 第四代是关键：只要还能通过任意别名到达上层，任何基于文本前缀的检查都有
//! 下一代替换。**结论：分层边界要么由 crate 边界强制，要么就不算强制。**
//!
//! 现在每一层都是独立 crate，反向依赖会让 cargo 直接失败：
//!
//! ```text
//! error: cyclic package dependency: package `agent-game-server-core`
//!        depends on itself
//! ```
//!
//! 这个文件因此不再扫描源码，只做两件扫描做不到的事：
//! 1. 断言依赖关系确实存在（防止有人把 crate 合并回去后测试仍然绿）；
//! 2. 在**当前**依赖图上跑一次真实的编译器，证明确实会拒绝。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn manifest_of(crate_dir: &str) -> String {
    let path = workspace_root()
        .join("crates")
        .join(crate_dir)
        .join("Cargo.toml");
    // 集成测试不是生产路径；读不到只意味着测试自身写错了路径。
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("读不到 {}：{err}", path.display()))
}

/// 剥掉 TOML 注释后，判断里是否出现了某个包名。
///
/// 只剥 `#` 之后的内容：注释里提到别的 crate 名是正常的（那是在说明"刻意不依赖"）。
fn declares_dependency(manifest: &str, package: &str) -> bool {
    manifest
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .any(|line| line.contains(package))
}

#[test]
fn infra_does_not_depend_on_domain() {
    let manifest = manifest_of("agent-game-infra");
    assert!(
        !declares_dependency(&manifest, "agent-game-server-domain"),
        "infra 不许依赖 domain —— 那会让编译器报循环依赖，分层强制失效。\n{manifest}"
    );
}

#[test]
fn infra_does_not_depend_on_core() {
    let manifest = manifest_of("agent-game-infra");
    assert!(
        !declares_dependency(&manifest, "agent-game-server-core"),
        "infra 不许依赖 core —— 那会让编译器报循环依赖，分层强制失效。\n{manifest}"
    );
}

#[test]
fn domain_does_not_depend_on_core() {
    let manifest = manifest_of("agent-game-domain");
    assert!(
        !declares_dependency(&manifest, "agent-game-server-core"),
        "domain 不许依赖 core（具体游戏在 core 里）—— 那会让编译器报循环依赖。\n{manifest}"
    );
}

/// 正向断言：依赖方向确实是 `core → domain → infra`。
///
/// 如果不查这一条，把所有 crate 拆开却不建立依赖也能让上面三条通过。
#[test]
fn the_dependency_chain_is_core_to_domain_to_infra() {
    assert!(
        declares_dependency(&manifest_of("agent-game-core"), "agent-game-server-domain"),
        "core 应当依赖 domain（游戏实现需要引擎协议）"
    );
    assert!(
        declares_dependency(&manifest_of("agent-game-domain"), "agent-game-server-infra"),
        "domain 应当依赖 infra（需要预算、配置、错误等）"
    );
}

/// 端到端验证：真的让 infra 反向依赖 core，cargo 必须报循环依赖。
///
/// 前三条断言只读 manifest 文本。这一条**实际调用 cargo**，证明约束不是
/// 我们自己在测试里模拟的，而是构建系统真的拒绝。
///
/// 用临时的 `--config` 注入依赖，不改仓库文件；跑完即恢复。
#[test]
fn cargo_really_rejects_a_reverse_dependency() {
    let crate_dir = workspace_root().join("crates/agent-game-infra");
    let original = fs::read_to_string(crate_dir.join("Cargo.toml")).unwrap();

    // 用 `[dependencies]` 追加一条反向依赖。cargo 在解析依赖图阶段就会失败，
    // 不需要真的编译，所以这一步很快。
    let mutated = original.replace(
        "[dependencies]",
        "[dependencies]\nagent-game-server-core = { path = \"../agent-game-core\" }",
    );
    assert_ne!(mutated, original, "注入点没找到，测试本身失效");
    fs::write(crate_dir.join("Cargo.toml"), &mutated).unwrap();

    let output = Command::new(env!("CARGO"))
        // `metadata` 不带 `--no-deps` 会真正解析依赖图；这一步就足够触发
        // 循环依赖检查，不需要编译。
        .args(["metadata", "--format-version", "1"])
        .current_dir(workspace_root())
        .output();

    // 无论结果如何，先恢复——这是唯一必须保证的事。
    fs::write(crate_dir.join("Cargo.toml"), &original).unwrap();

    let output = output.expect("跑 cargo metadata 失败");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "cargo 竟然接受了 infra → core 的反向依赖，分层强制失效。\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("cyclic") || stderr.contains("cycle"),
        "cargo 拒绝了，但原因不是循环依赖（可能是别的错误），这条证明不成立。\nstderr:\n{stderr}"
    );
}

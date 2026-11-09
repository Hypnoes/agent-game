//! 发布产物看门：测试替身不许进 release。
//!
//! `domain::testing::DummyEngine` 是给各 crate 的测试用的引擎替身。它的
//! `admin_view()` 会返回 `{"phase": ..., "secret": "admin-only"}`——一串
//! 假的"秘密"。这些东西出现在发布产物里，说明整个模块没被裁掉。
//!
//! # 历史
//!
//! 007（#9）之前，`testing` 模块是**无条件** `pub mod`，注释却写着"只在
//! `cfg(test)` 与 `feature = "testing"` 下编译"——那个 cfg 从没存在过。
//! 实测 `libagent_game_server_domain.rlib` 里能串扫到 `DummyEngine` 与
//! `admin-only`。
//!
//! # 为什么这条用例不自己跑 `cargo build`
//!
//! 它要检查的是**发布产物**，而这套用例本身跑在 `cargo test` 里。在测试中
//! 再起一次 `cargo build --release` 会和并行的测试争 target 目录（006 里
//! 就踩过一次这种竞态）。所以真正的检查放在 CI 步骤里
//! （`.github/workflows/ci.yml` 的"产物里无测试替身"），这里只做**结构性**
//! 断言：确认门控真的写在源码里，并且没有生产依赖开着它。
//!
//! 两道防线各管一段：
//! - 这里：门控与依赖声明写对了没有（跑得快，改错了立刻红）
//! - CI：release 产物里真的没有（慢，但直接测最终事实）

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("读不到 {}：{err}", path.display()))
}

/// 剥掉 TOML 注释：注释里提到 `testing` 是正常的（那是在解释为什么这么写）。
fn strip_toml_comments(manifest: &str) -> String {
    manifest
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_testing_module_is_gated_behind_a_feature() {
    let source = read("crates/agent-game-domain/src/lib.rs");
    let line = source
        .lines()
        .position(|line| line.trim() == "pub mod testing;")
        .expect("domain/src/lib.rs 里应当有 `pub mod testing;`");

    let all: Vec<&str> = source.lines().collect();
    let preceding: Vec<&str> = all[line.saturating_sub(2)..line].to_vec();
    assert!(
        preceding
            .iter()
            .any(|line| line.contains("feature = \"testing\"")),
        "`pub mod testing;` 前面必须有 `#[cfg(any(test, feature = \"testing\"))]`，\
         否则它会无条件编译进发布产物。实际前置行：{preceding:?}"
    );
}

#[test]
fn the_core_reexport_is_gated_too() {
    // domain 门控了，core 那边如果还是无条件 `pub mod testing { ... }`，
    // 替身照样能通过 `core::testing` 被带进来。
    let source = read("crates/agent-game-core/src/lib.rs");
    let line = source
        .lines()
        .position(|line| line.trim_start().starts_with("pub mod testing"))
        .expect("core/src/lib.rs 里应当有兼容模块 `pub mod testing`");

    let all: Vec<&str> = source.lines().collect();
    let preceding: Vec<&str> = all[line.saturating_sub(2)..line].to_vec();
    assert!(
        preceding
            .iter()
            .any(|line| line.contains("feature = \"testing\"")),
        "core 的 `pub mod testing` 兼容模块也必须门控——domain 门控了不代表\
         core 的再导出也门控。实际前置行：{preceding:?}"
    );
}

#[test]
fn the_core_testing_feature_forwards_to_domain() {
    let manifest = strip_toml_comments(&read("crates/agent-game-core/Cargo.toml"));
    assert!(
        manifest.contains("testing = [\"agent-game-server-domain/testing\"]"),
        "core 的 `testing` feature 必须**转发**到 domain。\
         写成空 `testing = []` 的话，给 core 开这个 feature 对 domain 毫无作用\
         （上一版就是这样）。实际：\n{manifest}"
    );
}

/// 生产依赖树里不许有人开 `testing`。
///
/// workspace 的 feature 会被 union：任何一处生产 `[dependencies]` 开了它，
/// `cargo build --release` 就会带上测试替身。
#[test]
fn no_production_dependency_enables_testing() {
    let crates = [
        "agent-game-infra",
        "agent-game-domain",
        "agent-game-core",
        "agent-game-mcp",
        "agent-game-server",
    ];

    let mut violations = Vec::new();
    for crate_dir in crates {
        let manifest = strip_toml_comments(&read(&format!("crates/{crate_dir}/Cargo.toml")));
        let mut section = "";
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                section = trimmed;
            }
            // 生产段只有 `[dependencies]`；`[dev-dependencies]` 与
            // `[build-dependencies]` 不参与发布构建
            if section == "[dependencies]" && trimmed.contains("features = [\"testing\"]") {
                violations.push(format!("{crate_dir}: {trimmed}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "生产 `[dependencies]` 不许开 `testing`——workspace 的 feature 是 union 的，\
         一处漏开会把 DummyEngine 拖进 release。违规：\n{}",
        violations.join("\n")
    );
}

/// 反向验证：这条看门不是"什么都能通过"。
///
/// 喂一个明确写了 `features = ["testing"]` 的生产段，必须能抓到。
#[test]
fn the_guard_actually_catches_a_violation() {
    let fake =
        "[dependencies]\nagent-game-server-core = { workspace = true, features = [\"testing\"] }";
    let mut section = "";
    let mut hits = 0;
    for line in fake.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed;
        }
        if section == "[dependencies]" && trimmed.contains("features = [\"testing\"]") {
            hits += 1;
        }
    }
    assert_eq!(hits, 1, "看门逻辑失效：明确违规的行没被抓到");
}

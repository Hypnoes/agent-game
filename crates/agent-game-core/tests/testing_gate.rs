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
//! # 为什么用 `cargo metadata` 而不是扫 TOML 文本
//!
//! 第一版扫 `Cargo.toml` 的 `[dependencies]` 段找字面
//! `features = ["testing"]`。review 指出它漏掉三类合法写法：
//!
//! - target-specific dependencies（`[target.'cfg(...)'.dependencies]`）
//! - 在 workspace 依赖上启用的 feature
//! - 其他 TOML 排版（换行、行内表、引号风格）
//!
//! 改成解析 `cargo metadata` 的输出：它给出**每个依赖的 `kind`**（空 = 生产，
//! `dev` = 测试）与**解析后的 `features` 列表**，与排版无关，也不会漏
//! target-specific 段。
//!
//! # 与 CI 的分工
//!
//! 这里验**依赖图**：没有生产依赖开启 `testing`、门控写在源码里。
//! `.github/workflows/ci.yml` 的「产物里无测试替身」验**最终事实**：
//! 真的构建 release、串扫每个 rlib。
//!
//! 只留前者会漏掉"依赖图对但产物里还有"（比如某个 `pub use` 从 cfg 块透出来）；
//! 只留后者要等 CI 才知道，本地改错时看不到。

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

fn read(relative: &str) -> String {
    let path = workspace_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("读不到 {}：{err}", path.display()))
}

/// 一个依赖项的解析结果。
#[derive(Debug)]
struct Dep {
    /// 所属包名。
    package: String,
    /// 被依赖的包名。
    name: String,
    /// `None` = 生产依赖；`Some("dev")` = dev-dependency。
    kind: Option<String>,
    /// 解析后的 feature 列表（已展开，不受写法影响）。
    features: Vec<String>,
}

/// 通过 `cargo metadata` 取**真实依赖图**。
///
/// `--no-deps` 只给 workspace 内的包，不解析整个 crates.io 依赖树——够用且快。
fn declared_deps() -> Vec<Dep> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root())
        .output()
        .expect("跑 cargo metadata 失败");
    assert!(
        output.status.success(),
        "cargo metadata 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata 的输出不是合法 JSON");

    let mut deps = Vec::new();
    for package in json["packages"].as_array().expect("packages 应当是数组") {
        let package_name = package["name"].as_str().unwrap_or_default().to_owned();
        for dep in package["dependencies"].as_array().into_iter().flatten() {
            deps.push(Dep {
                package: package_name.clone(),
                name: dep["name"].as_str().unwrap_or_default().to_owned(),
                kind: dep["kind"].as_str().map(ToOwned::to_owned),
                features: dep["features"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str().map(ToOwned::to_owned))
                    .collect(),
            });
        }
    }
    deps
}

/// 生产依赖里有没有人开 `testing`。返回违规描述（空 = 干净）。
fn production_deps_enabling_testing(deps: &[Dep]) -> Vec<String> {
    deps.iter()
        .filter(|dep| dep.kind.is_none()) // 只要生产依赖
        .filter(|dep| dep.features.iter().any(|f| f == "testing"))
        .map(|dep| format!("{} -> {}", dep.package, dep.name))
        .collect()
}

#[test]
fn no_production_dependency_enables_testing() {
    let deps = declared_deps();
    let violations = production_deps_enabling_testing(&deps);
    assert!(
        violations.is_empty(),
        "生产依赖不许开 `testing`——workspace 的 feature 是 union 的，\
         一处漏开会把 DummyEngine 拖进 release。违规：\n{}",
        violations.join("\n")
    );
}

/// 反向验证：同一个检查函数喂违规数据必须能抓到。
///
/// 第一版的反向用例自己重写了一遍匹配逻辑——那测的是"我写了两遍同样的
/// 正则"，不是"生产扫描器有效"。这里直接复用 `production_deps_enabling_testing`。
#[test]
fn the_guard_actually_catches_a_violation() {
    let fake = vec![
        Dep {
            package: "some-server".to_owned(),
            name: "agent-game-server-core".to_owned(),
            kind: None, // 生产依赖
            features: vec!["testing".to_owned()],
        },
        Dep {
            package: "some-core".to_owned(),
            name: "agent-game-server-domain".to_owned(),
            kind: Some("dev".to_owned()), // dev 依赖：允许
            features: vec!["testing".to_owned()],
        },
    ];

    let violations = production_deps_enabling_testing(&fake);
    assert_eq!(
        violations.len(),
        1,
        "看门逻辑失效：生产依赖的违规没被抓到、或误报 dev 依赖。实际：{violations:?}"
    );
    assert!(
        violations[0].contains("some-server"),
        "抓到的应当是生产依赖那条，实际：{violations:?}"
    );
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

/// `core` 的 `testing` feature 必须**转发**到 domain。
///
/// 写成空 `testing = []` 的话，给 core 开这个 feature 对 domain 毫无作用
/// （上一版就是这样）。这条查的是 feature 定义本身，不是依赖图，所以仍读
/// `Cargo.toml`——但用 `cargo metadata` 也能拿到 `features` 映射，见下一条。
#[test]
fn the_core_testing_feature_forwards_to_domain() {
    let manifest = read("crates/agent-game-core/Cargo.toml");
    // 只剥注释：注释里提到 `testing = []` 是正常的（那是在解释为什么不能这么写）
    let stripped: String = manifest
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        stripped.contains("testing = [\"agent-game-server-domain/testing\"]"),
        "core 的 `testing` feature 必须转发到 domain。实际：\n{stripped}"
    );
}

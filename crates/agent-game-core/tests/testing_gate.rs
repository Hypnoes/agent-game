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
//! # 为什么解析 `cargo metadata` 而不是扫 TOML 文本
//!
//! 第一版扫 `Cargo.toml` 的 `[dependencies]` 段找字面
//! `features = ["testing"]`。review 逐轮指出它漏掉：
//!
//! - target-specific dependencies（`[target.'cfg(...)'.dependencies]`）
//! - 在 workspace 依赖上启用的 feature
//! - 其他 TOML 排版
//! - `[build-dependencies]`（`kind == "build"` 也是生产）
//! - 重命名依赖（`package = "..."`）
//! - `default-features = false`（此时 default 不生效，不该误报）
//! - 嵌套的 feature 链（`default = ["standard"]`、`standard = ["testing"]`）
//!
//! 所以改成解析 `cargo metadata` 的输出，并按 cargo 的真实语义计算
//! **feature 激活闭包**。
//!
//! # 与 CI 的分工
//!
//! 这里验**依赖图**。`.github/workflows/ci.yml` 的「产物里无测试替身」验
//! **最终事实**（真的构建 release、串扫每个 rlib）。只留前者会漏掉"依赖图对
//! 但产物里还有"；只留后者要等 CI 才知道。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
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

/// 一个包声明的 feature 表：`feature -> 它激活的其它 feature / 可选依赖`。
type FeatureMap = BTreeMap<String, Vec<String>>;

/// 一条依赖声明。
#[derive(Debug)]
struct Dep {
    /// 声明方包名。
    package: String,
    /// 被依赖的**真实包名**（重命名时与 `rename` 不同）。
    target_package: String,
    /// `None` = 普通依赖；`Some("dev")` / `Some("build")` 分别是测试/构建依赖。
    kind: Option<String>,
    /// 显式请求的 feature。
    features: Vec<String>,
    /// 是否启用目标包的 `default` feature（`default-features = false` 时为 false）。
    uses_default_features: bool,
}

/// `cargo metadata` 的解析结果。
struct Metadata {
    deps: Vec<Dep>,
    /// 每个包的 feature 表。
    features: BTreeMap<String, FeatureMap>,
}

fn load_metadata() -> Metadata {
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
    let packages = json["packages"].as_array().expect("packages 应当是数组");

    let mut features = BTreeMap::new();
    let mut deps = Vec::new();

    for package in packages {
        let package_name = package["name"].as_str().unwrap_or_default().to_owned();

        // `features` 是 `feature -> [激活的 feature]` 的映射
        let mut map = FeatureMap::new();
        if let Some(object) = package["features"].as_object() {
            for (feature, activated) in object {
                map.insert(
                    feature.clone(),
                    activated
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|f| f.as_str().map(ToOwned::to_owned))
                        .collect(),
                );
            }
        }
        features.insert(package_name.clone(), map);

        for dep in package["dependencies"].as_array().into_iter().flatten() {
            // 重命名时 `name` 是**别名**，`package` 才是真实包名。
            // `cargo metadata` 在 `dependencies[]` 里的 `name` 已经是解析后的
            // 真实包名，`rename` 才是别名——但为稳妥起见两者都看。
            let real = dep["package"]
                .as_str()
                .or_else(|| dep["name"].as_str())
                .unwrap_or_default()
                .to_owned();

            deps.push(Dep {
                package: package_name.clone(),
                target_package: real,
                kind: dep["kind"].as_str().map(ToOwned::to_owned),
                features: dep["features"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| f.as_str().map(ToOwned::to_owned))
                    .collect(),
                uses_default_features: dep["uses_default_features"].as_bool().unwrap_or(true),
            });
        }
    }

    Metadata { deps, features }
}

/// 计算一个包在给定初始 feature 集合下的**激活闭包**。
///
/// 递归展开：`default = ["standard"]` 且 `standard = ["testing"]` 时，
/// 激活 `default` 就等于激活了 `testing`。
///
/// 可选依赖带来的隐式 feature（`dep:foo`、`foo/bar`）也一起收进去——
/// 这一层对我们的判据不构成影响，但收全了才不会漏。
fn activated_features(available: Option<&FeatureMap>, requested: &[String]) -> BTreeSet<String> {
    let mut active: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = requested.to_vec();

    while let Some(feature) = stack.pop() {
        if !active.insert(feature.clone()) {
            continue;
        }
        let Some(map) = available else { continue };
        if let Some(activated) = map.get(&feature) {
            for next in activated {
                // `foo/bar` 形式是"启用依赖 foo 的 bar feature"，不是本包的
                // feature。剥成 `bar`?? 不——它是**目标包**的 feature，
                // 对本包而言只表示"依赖被启用"。这里不展开它，
                // 但要把它作为激活项记下来（对判据无害）。
                stack.push(next.clone());
            }
        }
    }
    active
}

/// 生产依赖里有没有人开 `testing`。返回违规描述（空 = 干净）。
///
/// "生产" = 除 `dev` 以外的所有依赖（普通 + `build`）。`build` 也算：
/// 构建脚本会被打进发布流程，它依赖的 crate 及其 feature 同样要编。
fn production_deps_enabling_testing(
    deps: &[Dep],
    features: &BTreeMap<String, FeatureMap>,
) -> Vec<String> {
    let mut violations = Vec::new();

    for dep in deps {
        if dep.kind.as_deref() == Some("dev") {
            continue;
        }

        let mut requested = dep.features.clone();
        if dep.uses_default_features {
            requested.push("default".to_owned());
        }

        let available = features.get(&dep.target_package);
        let active = activated_features(available, &requested);

        if active.contains("testing") {
            let how = if dep.features.iter().any(|f| f == "testing") {
                "显式"
            } else if dep.uses_default_features && active.contains("default") {
                "经 default（含 feature 链）"
            } else {
                "经 feature 链"
            };
            violations.push(format!(
                "{} -> {}（{how} 开启）",
                dep.package, dep.target_package
            ));
        }
    }

    violations
}

#[test]
fn no_production_dependency_enables_testing() {
    let meta = load_metadata();
    let violations = production_deps_enabling_testing(&meta.deps, &meta.features);
    assert!(
        violations.is_empty(),
        "生产依赖不许开 `testing`——workspace 的 feature 是 union 的，\
         一处漏开会把 DummyEngine 拖进 release。违规：\n{}",
        violations.join("\n")
    );
}

// ═══════════════════════════════════════════
// 反向验证：喂构造数据，确认检查真的有效
// ═══════════════════════════════════════════

/// 构造一条依赖 + 一份 feature 表。
fn fixture(
    kind: Option<&str>,
    requested: &[&str],
    uses_default: bool,
    feature_map: &[(&str, &[&str])],
) -> (Vec<Dep>, BTreeMap<String, FeatureMap>) {
    let deps = vec![Dep {
        package: "some-server".to_owned(),
        target_package: "agent-game-server-domain".to_owned(),
        kind: kind.map(ToOwned::to_owned),
        features: requested.iter().map(|s| (*s).to_owned()).collect(),
        uses_default_features: uses_default,
    }];

    let mut map = FeatureMap::new();
    for (name, activates) in feature_map {
        map.insert(
            (*name).to_owned(),
            activates.iter().map(|s| (*s).to_owned()).collect(),
        );
    }
    let mut features = BTreeMap::new();
    features.insert("agent-game-server-domain".to_owned(), map);
    (deps, features)
}

#[test]
fn the_guard_actually_catches_an_explicit_feature() {
    let (deps, features) = fixture(None, &["testing"], true, &[("testing", &[])]);
    let violations = production_deps_enabling_testing(&deps, &features);
    assert_eq!(
        violations.len(),
        1,
        "显式开 testing 必须抓到：{violations:?}"
    );
}

#[test]
fn the_guard_catches_build_dependencies() {
    let (deps, features) = fixture(Some("build"), &["testing"], true, &[("testing", &[])]);
    let violations = production_deps_enabling_testing(&deps, &features);
    assert_eq!(
        violations.len(),
        1,
        "build 依赖开 testing 必须抓到——构建脚本参与发布流程：{violations:?}"
    );
}

#[test]
fn the_guard_catches_a_direct_default() {
    let (deps, features) = fixture(
        None,
        &[],
        true,
        &[("default", &["testing"]), ("testing", &[])],
    );
    let violations = production_deps_enabling_testing(&deps, &features);
    assert_eq!(
        violations.len(),
        1,
        "default 直接含 testing 必须抓到：{violations:?}"
    );
}

/// `default = ["standard"]`、`standard = ["testing"]` —— 必须递归展开。
#[test]
fn the_guard_catches_a_nested_default_chain() {
    let (deps, features) = fixture(
        None,
        &[],
        true,
        &[
            ("default", &["standard"]),
            ("standard", &["testing"]),
            ("testing", &[]),
        ],
    );
    let violations = production_deps_enabling_testing(&deps, &features);
    assert_eq!(
        violations.len(),
        1,
        "default 经 feature 链激活 testing 必须抓到（要递归展开）：{violations:?}"
    );
}

/// `default-features = false` 时 default 不生效，**不该**误报。
#[test]
fn the_guard_ignores_default_when_default_features_are_off() {
    let (deps, features) = fixture(
        None,
        &[],
        false, // default-features = false
        &[("default", &["testing"]), ("testing", &[])],
    );
    let violations = production_deps_enabling_testing(&deps, &features);
    assert!(
        violations.is_empty(),
        "default-features = false 时 default 不生效，不该误报：{violations:?}"
    );
}

/// `dev` 依赖开 `testing` 是正常用法，不该误报。
#[test]
fn the_guard_ignores_dev_dependencies() {
    let (deps, features) = fixture(Some("dev"), &["testing"], true, &[("testing", &[])]);
    let violations = production_deps_enabling_testing(&deps, &features);
    assert!(
        violations.is_empty(),
        "dev 依赖开 testing 是正常用法：{violations:?}"
    );
}

// ═══════════════════════════════════════════
// 源码层面：cfg 门控真的写在代码里
// ═══════════════════════════════════════════

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
/// （上一版就是这样）。
#[test]
fn the_core_testing_feature_forwards_to_domain() {
    let meta = load_metadata();
    let core = meta
        .features
        .get("agent-game-server-core")
        .expect("metadata 里应当有 core 的 feature 表");

    let activates = core
        .get("testing")
        .expect("core 应当声明 `testing` feature");
    assert!(
        activates
            .iter()
            .any(|f| f == "agent-game-server-domain/testing"),
        "core 的 `testing` 必须转发到 `agent-game-server-domain/testing`，\
         写成空 feature 对 domain 毫无作用。实际：{activates:?}"
    );
}

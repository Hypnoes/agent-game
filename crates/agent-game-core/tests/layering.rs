//! 分层看门：`infra` 不许认识 `domain` 与 `games`。
//!
//! 目录结构本身不会阻止下一个人从 `infra/error.rs` 里 `use crate::domain::...`。
//! 这条用例把"依赖方向单向"从注释变成可执行断言——对应 AGENTS.md 第 14 条：
//! 每条约束都要有对应的看门用例。
//!
//! 实现方式：直接读源码文本。这里不解析 Rust AST，因为要守的东西极其简单
//! （一个 `use crate::<层>` 前缀），引入解析器的代价远超收益。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

/// 递归收集目录下全部 `.rs` 文件。
fn rust_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    out
}

fn core_src() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// 找出文件里出现的违规 `use crate::<层>` 行。
fn forbidden_uses(relative: &str, layer: &str) -> Vec<String> {
    let dir = core_src().join(relative);
    let prefixes = [
        format!("use crate::{layer}::"),
        format!("use crate::{layer};"),
        format!("crate::{layer}::"),
    ];

    let mut violations = Vec::new();
    for file in rust_files(&dir) {
        let text = fs::read_to_string(&file).unwrap();
        let name = file
            .strip_prefix(core_src())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        for (number, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if !trimmed.starts_with("use ") && !trimmed.starts_with("pub use ") {
                continue;
            }
            if prefixes.iter().any(|prefix| trimmed.contains(prefix)) {
                violations.push(format!("{name}:{}: {trimmed}", number + 1));
            }
        }
    }
    violations
}

#[test]
fn infra_never_depends_on_domain() {
    let violations = forbidden_uses("infra", "domain");
    assert!(
        violations.is_empty(),
        "infra 层不许依赖 domain 层。违规行：\n{}",
        violations.join("\n")
    );
}

#[test]
fn infra_never_depends_on_games() {
    let violations = forbidden_uses("infra", "games");
    assert!(
        violations.is_empty(),
        "infra 层不许依赖 games 层。违规行：\n{}",
        violations.join("\n")
    );
}

/// 反向验证：这条看门不是"什么都能通过"。
///
/// 如果 `forbidden_uses` 恒返回空，上面两条断言永远成立却什么也没守。
/// 这里喂一个已知违规的层名，断言它**确实**能抓到东西。
#[test]
fn the_guard_actually_detects_a_violation() {
    // `domain` 目录里当然有 `use crate::infra::`，把参数反过来喂它就必然命中。
    let violations = forbidden_uses("domain", "infra");
    assert!(
        !violations.is_empty(),
        "看门函数失效：domain 层明明 use 了 infra，却没被抓到"
    );
}

/// `games` 允许同时使用两层——证明约束是"单向"而不是"全禁"。
#[test]
fn games_may_depend_on_both_upper_layers() {
    let dir = core_src().join("games");
    let mut saw_domain = false;
    let mut saw_infra = false;
    for file in rust_files(&dir) {
        let text = fs::read_to_string(&file).unwrap();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("use crate::domain")
                || trimmed.starts_with("pub use crate::domain")
            {
                saw_domain = true;
            }
            if trimmed.starts_with("use crate::infra")
                || trimmed.starts_with("pub use crate::infra")
            {
                saw_infra = true;
            }
        }
    }
    assert!(saw_infra, "games 层应当 use crate::infra");
    assert!(
        saw_domain,
        "games 层应当 use crate::domain（引擎协议住在那里）"
    );
}

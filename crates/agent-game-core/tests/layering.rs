//! 分层看门：依赖方向必须单向。
//!
//! ```text
//! games  →  domain  →  infra
//! ```
//!
//! 目录结构本身不会阻止下一个人从 `infra/error.rs` 里 `use crate::domain::...`。
//! 这条用例把"依赖方向单向"从注释变成可执行断言——对应 AGENTS.md 第 14 条：
//! 每条约束都要有对应的看门用例。
//!
//! # 为什么不用 AST
//!
//! 要守的东西是一个前缀关系，不是在分析类型。但**逐行前缀匹配是不够的**：
//! 下面这些写法都能让 `infra` 依赖 `domain`，而简单的
//! `line.starts_with("use crate::domain")` 一条都抓不到：
//!
//! | 写法 | 为什么漏 |
//! |---|---|
//! | `pub(crate) use crate::domain::X;` | 不以 `use ` 开头 |
//! | `use crate::domain as games;` | 是 `as` 别名，前缀匹配得上但语义不同 |
//! | `use crate::{domain::X};` | 分组导入，`crate::` 后面是 `{` |
//! | `use crate::{\n  domain::X,\n};` | 跨行 |
//! | `use crate::games::x::Y;`（在 domain 里） | 需要单独一条 `domain → games` 规则 |
//!
//! 所以这里先把源码**归一化**成逻辑行（合并花括号内的分组与跨行导入），
//! 再判断每行引用了哪些层。归一化后仍用字符串处理，不引入解析器。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// 层名。
const INFRA: &str = "infra";
const DOMAIN: &str = "domain";
const GAMES: &str = "games";

fn core_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// 递归收集目录下全部 `.rs` 文件。
fn rust_files(dir: &Path) -> Vec<PathBuf> {
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

/// 把源码拆成"逻辑 use 语句"：合并跨行与花括号分组，去掉注释。
///
/// 返回 `(起始行号, 归一化后的一句话)`。行号只用于报错时定位。
fn use_statements(text: &str) -> Vec<(usize, String)> {
    // 1. 去掉行注释与块注释，保留行数不变（用等长空白替换）。
    let stripped = strip_comments(text);

    let mut out = Vec::new();
    let mut buffer = String::new();
    let mut depth = 0i32;
    let mut start_line = 0;

    for (index, line) in stripped.lines().enumerate() {
        let trimmed = line.trim();
        if buffer.is_empty() {
            // 只从 `use` / `pub use` / `pub(...) use` 开始收集
            let is_use = trimmed.starts_with("use ")
                || trimmed.starts_with("pub use ")
                || trimmed.starts_with("pub(crate) use ")
                || trimmed.starts_with("pub(super) use ")
                || trimmed.starts_with("pub(in");
            if !is_use {
                continue;
            }
            start_line = index + 1;
        }

        buffer.push_str(trimmed);
        buffer.push(' ');
        depth += trimmed.matches('{').count() as i32;
        depth -= trimmed.matches('}').count() as i32;

        if depth <= 0 && (trimmed.ends_with(';') || trimmed.ends_with(',')) {
            out.push((
                start_line,
                buffer.split_whitespace().collect::<Vec<_>>().join(" "),
            ));
            buffer.clear();
            depth = 0;
        }
    }
    // 收尾：没以分号结束的残留也收进来，宁可多查
    if !buffer.is_empty() {
        out.push((
            start_line,
            buffer.split_whitespace().collect::<Vec<_>>().join(" "),
        ));
    }
    out
}

/// 用等长空白替换注释，保持行结构不变。
fn strip_comments(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_line_comment = false;
    let mut in_block = false;
    let mut in_string = false;

    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();

        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                out.push('\n');
            } else {
                out.push(' ');
            }
            i += 1;
            continue;
        }
        if in_block {
            if c == '*' && next == Some('/') {
                in_block = false;
                out.push_str("  ");
                i += 2;
                continue;
            }
            out.push(if c == '\n' { '\n' } else { ' ' });
            i += 1;
            continue;
        }
        if in_string {
            out.push(c);
            if c == '\\' && next.is_some() {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '/' && next == Some('/') {
            in_line_comment = true;
            out.push_str("  ");
            i += 2;
            continue;
        }
        if c == '/' && next == Some('*') {
            in_block = true;
            out.push_str("  ");
            i += 2;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 一条 use 语句引用了哪些层（`crate::<layer>` 形式）。
///
/// 处理四种形态：
/// - `use crate::domain::x::Y;`
/// - `use crate::{domain::x::Y, infra::z};`
/// - `use crate::domain as alias;`
/// - `pub(crate) use crate::domain::x;`
fn referenced_layers(statement: &str) -> BTreeSet<&'static str> {
    let mut found = BTreeSet::new();
    for layer in [INFRA, DOMAIN, GAMES] {
        // 直接路径与别名：`crate::domain::` / `crate::domain as` / `crate::domain;`
        let direct = [
            format!("crate::{layer}::"),
            format!("crate::{layer} as "),
            format!("crate::{layer};"),
        ];
        if direct
            .iter()
            .any(|needle| statement.contains(needle.as_str()))
        {
            found.insert(layer);
            continue;
        }
        // 分组导入：每个逗号分隔的项都检查，包括别名与裸模块项。
        if grouped_items(statement).iter().any(|item| {
            let item = item.trim().trim_start_matches('{').trim_start();
            item.strip_prefix(layer).is_some_and(|suffix| {
                let suffix = suffix.trim_start();
                suffix.is_empty() || suffix.starts_with("::") || suffix.starts_with("as ")
            })
        }) {
            found.insert(layer);
        }
    }
    found
}

/// 返回 `crate::{...}` 分组中的顶层项，忽略嵌套分组里的逗号。
fn grouped_items(statement: &str) -> Vec<&str> {
    let Some((_, rest)) = statement.split_once("crate::{") else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (index, ch) in rest.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth == 0 => {
                items.push(&rest[start..index]);
                return items;
            }
            '}' => depth -= 1,
            ',' if depth == 0 => {
                items.push(&rest[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    items.push(&rest[start..]);
    items
}

/// 收集某个层目录下的全部违规：`forbidden` 里的层名被引用了。
fn violations_in(layer_dir: &str, forbidden: &[&str]) -> Vec<String> {
    let dir = core_src().join(layer_dir);
    let mut out = Vec::new();
    for file in rust_files(&dir) {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let name = file
            .strip_prefix(core_src())
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        for (line, statement) in use_statements(&text) {
            let referenced = referenced_layers(&statement);
            for banned in forbidden {
                if referenced.contains(banned) {
                    out.push(format!("{name}:{line}: {statement}"));
                }
            }
        }
    }
    out
}

#[test]
fn infra_never_depends_on_domain() {
    let violations = violations_in(INFRA, &[DOMAIN]);
    assert!(
        violations.is_empty(),
        "infra 层不许依赖 domain 层。违规行：\n{}",
        violations.join("\n")
    );
}

#[test]
fn infra_never_depends_on_games() {
    let violations = violations_in(INFRA, &[GAMES]);
    assert!(
        violations.is_empty(),
        "infra 层不许依赖 games 层。违规行：\n{}",
        violations.join("\n")
    );
}

#[test]
fn domain_never_depends_on_games() {
    let violations = violations_in(DOMAIN, &[GAMES]);
    assert!(
        violations.is_empty(),
        "domain 层不许依赖 games 层。违规行：\n{}",
        violations.join("\n")
    );
}

/// 反向验证：这条看门不是"什么都能通过"。
///
/// 如果 `violations_in` 恒返回空，上面三条断言永远成立却什么也没守。
/// 这里喂一个**必然违规**的方向：`games` 层确实同时 use 了 `domain` 与 `infra`。
#[test]
fn the_guard_actually_detects_a_violation() {
    let domain_hits = violations_in(GAMES, &[DOMAIN]);
    let infra_hits = violations_in(GAMES, &[INFRA]);
    assert!(
        !domain_hits.is_empty(),
        "看门函数失效：games 层明明 use 了 domain，却没被抓到"
    );
    assert!(
        !infra_hits.is_empty(),
        "看门函数失效：games 层明明 use 了 infra，却没被抓到"
    );
}

/// 逐条覆盖机器人审查指出的四种漏检写法。
///
/// 这些用例直接测 `referenced_layers`，不依赖临时改源码——那会让套件在
/// 并行执行时互相干扰。每一种写法都是真实可编译的 Rust。
#[test]
fn the_guard_catches_restricted_visibility() {
    let statement = "pub(crate) use crate::domain::game::GameEngine;";
    assert!(referenced_layers(statement).contains(DOMAIN));
}

#[test]
fn the_guard_catches_in_visibility_imports() {
    let source = "pub(in crate::infra) use crate::domain::game::GameEngine;";
    let statements = use_statements(source);
    assert_eq!(statements.len(), 1);
    assert!(referenced_layers(&statements[0].1).contains(DOMAIN));
}

#[test]
fn the_guard_catches_aliased_module_imports() {
    let statement = "use crate::domain as dom;";
    assert!(referenced_layers(statement).contains(DOMAIN));
}

#[test]
fn the_guard_catches_grouped_imports() {
    let statement = "use crate::{domain::game::GameEngine};";
    assert!(referenced_layers(statement).contains(DOMAIN));
}

#[test]
fn the_guard_catches_grouped_aliases_and_later_items() {
    assert!(referenced_layers("use crate::{domain as dom};").contains(DOMAIN));
    assert!(referenced_layers("use crate::{infra::error::EngineError, domain};").contains(DOMAIN));
    assert!(
        referenced_layers("use crate::{infra::{error::EngineError}, domain};").contains(DOMAIN)
    );
}

#[test]
fn the_guard_catches_multi_line_grouped_imports() {
    let source =
        "use crate::{\n    domain::game::GameEngine,\n    infra::error::EngineError,\n};\n";
    let statements = use_statements(source);
    assert_eq!(statements.len(), 1, "跨行导入应合并成一条：{statements:?}");
    let layers = referenced_layers(&statements[0].1);
    assert!(layers.contains(DOMAIN), "应抓到 domain：{layers:?}");
    assert!(layers.contains(INFRA), "应抓到 infra：{layers:?}");
}

#[test]
fn comments_do_not_create_false_positives() {
    let source = "// use crate::domain::game::GameEngine;\n/* use crate::games::x; */\n";
    assert!(
        use_statements(source).is_empty(),
        "注释里的 use 不该被当成真导入"
    );
}

/// `games` 允许同时使用两层——证明约束是"单向"而不是"全禁"。
#[test]
fn games_may_depend_on_both_lower_layers() {
    let dir = core_src().join(GAMES);
    let mut saw_domain = false;
    let mut saw_infra = false;
    for file in rust_files(&dir) {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for (_, statement) in use_statements(&text) {
            let layers = referenced_layers(&statement);
            saw_domain |= layers.contains(DOMAIN);
            saw_infra |= layers.contains(INFRA);
        }
    }
    assert!(saw_infra, "games 层应当 use crate::infra");
    assert!(
        saw_domain,
        "games 层应当 use crate::domain（引擎协议住在那里）"
    );
}

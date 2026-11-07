//! 分层看门。
//!
//! # 现在守什么
//!
//! `infra` 已经抽成独立 crate（`agent-game-server-infra`），它与本 crate 之间的
//! 依赖方向**由编译器强制**：`infra` 不依赖 `core`，任何反向依赖都会让 cargo 在
//! 循环依赖检查处失败（`cyclic package dependency`）。这条不再需要源码扫描。
//!
//! 这个文件因此只守剩下的那条方向：**`domain` 不许依赖 `games`**。
//!
//! ```text
//! agent-game-server-core
//!   games/   →  domain/          禁止
//!   domain/  →  （不许碰 games）
//! ```
//!
//! 为什么这条不像 `infra` 那样抽 crate：`domain` 与 `games` 都在 core 里，
//! 为它单独抽一个 crate 会引入 workspace 成员、版本号、发布边界等成本，
//! 而收益只是"编译器强制"对比"扫描器强制"。这里保留扫描器，但要认清它的上限。
//!
//! # 字符串扫描为什么必然有漏
//!
//! 历史教训：`infra` 还在 core 里时，这条扫描器被 review 连续找出三代绕过写法，
//! 每一个都合法可编译却漏检：
//!
//! | 代 | 写法 | 为什么漏 |
//! |---|---|---|
//! | 1 | `pub(crate) use crate::domain::X;` | 不以 `use ` 开头 |
//! | 1 | `use crate::domain as dom;` | 别名，不是 `::` 后缀 |
//! | 1 | `use crate::{domain::X};` | 分组导入 |
//! | 2 | `pub(in crate::infra) use crate::domain::X;` | 可见性修饰更复杂 |
//! | 2 | `use crate::{infra::X, domain};` | 分组里的裸模块项 |
//! | 3 | `use super::super::domain::game::X;` | 相对路径，不含 `crate::` |
//! | 3 | `use crate :: domain :: game::X;` | token 间空格 |
//! | 3 | `fn f(_: crate::domain::game::X) {}` | 根本不是 `use` |
//!
//! 第三代之后放弃"把字符串扫描做到穷尽"，改用 crate 边界。**对
//! `domain → games` 这条，扫描器仍有上面这些盲区**——它是一道防线，不是证明。
//! 若将来 `domain` 开始变复杂，应把它也抽成独立 crate。
//!
//! 下面保留的实现覆盖了前两代的全部写法（含测试），第三代里的
//! `super::super` 与 `crate ::` 两种也已被 `normalize_paths` 处理。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

const GAMES: &str = "games";
const DOMAIN: &str = "domain";

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

/// 用等长空白替换注释，保持行结构不变。
fn strip_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_line = false;
    let mut in_block = false;
    let mut in_string = false;

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();

        if in_line {
            if c == '\n' {
                in_line = false;
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
                out.push(chars[i + 1]);
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
            in_line = true;
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

/// 把 `crate :: domain` 这类 token 间空格压成 `crate::domain`。
fn normalize_paths(text: &str) -> String {
    text.replace(" ::", "::").replace(":: ", "::")
}

/// 把源码拆成"逻辑 use 语句"：合并跨行与花括号分组，去掉注释。
fn use_statements(text: &str) -> Vec<(usize, String)> {
    let normalized = normalize_paths(&strip_comments(text));

    let mut out = Vec::new();
    let mut buffer = String::new();
    let mut depth = 0i32;
    let mut start_line = 0;

    for (index, line) in normalized.lines().enumerate() {
        let trimmed = line.trim();
        if buffer.is_empty() {
            // 覆盖 `use`、`pub use`、`pub(crate) use`、`pub(super) use`、
            // `pub(in ...) use` 五种可见性形态
            let is_use = trimmed.starts_with("use ")
                || trimmed.starts_with("pub use ")
                || trimmed.starts_with("pub(crate) use ")
                || trimmed.starts_with("pub(super) use ")
                || trimmed.starts_with("pub(in ");
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
    if !buffer.is_empty() {
        out.push((
            start_line,
            buffer.split_whitespace().collect::<Vec<_>>().join(" "),
        ));
    }
    out
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

/// 一条 use 语句是否引用了 `layer`。
fn references_layer(statement: &str, layer: &str) -> bool {
    // 绝对路径与别名
    if statement.contains(&format!("crate::{layer}::"))
        || statement.contains(&format!("crate::{layer} as "))
        || statement.contains(&format!("crate::{layer};"))
    {
        return true;
    }
    // 相对路径：`super::super::domain::` —— 向上两级即 crate 根
    if statement.contains(&format!("super::super::{layer}::")) {
        return true;
    }
    // 分组导入：逐项检查，含别名与裸模块项
    grouped_items(statement).iter().any(|item| {
        let item = item.trim().trim_start_matches('{').trim_start();
        item.strip_prefix(layer).is_some_and(|suffix| {
            let suffix = suffix.trim_start();
            suffix.is_empty() || suffix.starts_with("::") || suffix.starts_with("as ")
        })
    })
}

/// 收集某个层目录下引用了 `forbidden` 的文件行。
fn violations_in(layer_dir: &str, forbidden: &str) -> Vec<String> {
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
            if references_layer(&statement, forbidden) {
                out.push(format!("{name}:{line}: {statement}"));
            }
        }
    }
    out
}

#[test]
fn domain_never_depends_on_games() {
    let violations = violations_in(DOMAIN, GAMES);
    assert!(
        violations.is_empty(),
        "domain 层不许依赖 games 层。违规行：\n{}",
        violations.join("\n")
    );
}

/// 反向验证：扫描器不是"什么都能通过"。
///
/// `games` 层确实同时 use 了 `domain`，把参数反过来喂给它必须命中。
#[test]
fn the_guard_actually_detects_a_violation() {
    let hits = violations_in(GAMES, DOMAIN);
    assert!(
        !hits.is_empty(),
        "看门函数失效：games 层明明 use 了 domain，却没被抓到"
    );
}

/// `games` 允许使用 `domain`——证明约束是"单向"而不是"全禁"。
#[test]
fn games_may_depend_on_domain() {
    let dir = core_src().join(GAMES);
    let mut saw_domain = false;
    for file in rust_files(&dir) {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for (_, statement) in use_statements(&text) {
            saw_domain |= references_layer(&statement, DOMAIN);
        }
    }
    assert!(saw_domain, "games 层应当 use domain（引擎协议住在那里）");
}

/// `infra` 已抽成独立 crate，编译器强制它不能反向依赖本 crate。
///
/// 这条用例不扫描源码——它只断言那条 Cargo 依赖确实不存在。
/// 真正的执行者是 cargo：加上反向依赖会得到 `cyclic package dependency`。
#[test]
fn infra_crate_does_not_depend_on_core() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("agent-game-infra/Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();

    // 只检查 `[dependencies]` 段，不检查注释——注释里提到 core 是正常的
    // （那是在说明"刻意不依赖它"）。逐行剥掉 `#` 之后的内容再判断。
    let declarations: String = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !declarations.contains("agent-game-server-core"),
        "infra crate 不许依赖 core —— 那会让编译器报循环依赖，\
         分层强制就失效了。manifest：\n{text}"
    );
}

// ── 逐条覆盖历史三代绕过写法 ──

#[test]
fn the_guard_catches_restricted_visibility() {
    assert!(references_layer(
        "pub(crate) use crate::domain::game::GameEngine;",
        DOMAIN
    ));
    assert!(references_layer(
        "pub(super) use crate::domain::game::GameEngine;",
        DOMAIN
    ));
}

#[test]
fn the_guard_catches_in_visibility_imports() {
    let source = "pub(in crate::infra) use crate::domain::game::GameEngine;";
    let statements = use_statements(source);
    assert_eq!(statements.len(), 1);
    assert!(references_layer(&statements[0].1, DOMAIN));
}

#[test]
fn the_guard_catches_aliased_module_imports() {
    assert!(references_layer("use crate::domain as dom;", DOMAIN));
}

#[test]
fn the_guard_catches_grouped_imports() {
    assert!(references_layer(
        "use crate::{domain::game::GameEngine};",
        DOMAIN
    ));
}

#[test]
fn the_guard_catches_grouped_aliases_and_later_items() {
    assert!(references_layer("use crate::{domain as dom};", DOMAIN));
    assert!(references_layer(
        "use crate::{infra::error::EngineError, domain};",
        DOMAIN
    ));
    assert!(references_layer(
        "use crate::{infra::{error::EngineError}, domain};",
        DOMAIN
    ));
}

#[test]
fn the_guard_catches_multi_line_grouped_imports() {
    let source =
        "use crate::{\n    domain::game::GameEngine,\n    infra::error::EngineError,\n};\n";
    let statements = use_statements(source);
    assert_eq!(statements.len(), 1, "跨行导入应合并成一条：{statements:?}");
    assert!(references_layer(&statements[0].1, DOMAIN));
}

/// 第三代绕过之一：相对路径 `super::super::`。
#[test]
fn the_guard_catches_relative_super_paths() {
    assert!(references_layer(
        "use super::super::domain::game::GameEngine;",
        DOMAIN
    ));
}

/// 第三代绕过之二：token 间空格 `crate :: domain`。
///
/// 注意 `references_layer` 接收的是**归一化之后**的语句，所以这里走
/// `use_statements` 而不是直接喂原始字符串。
#[test]
fn the_guard_catches_spaced_paths() {
    let source = "use crate :: domain :: game :: GameEngine;\n";
    let statements = use_statements(source);
    assert_eq!(statements.len(), 1);
    assert!(references_layer(&statements[0].1, DOMAIN));
}

#[test]
fn comments_do_not_create_false_positives() {
    let source = "// use crate::domain::game::GameEngine;\n/* use crate::games::x; */\n";
    assert!(
        use_statements(source).is_empty(),
        "注释里的 use 不该被当成真导入"
    );
}

/// 已知残留盲区：非 `use` 语句里的类型路径（第三代绕过之三）。
///
/// `fn f(_: crate::domain::game::PlayerMode) {}` 不进入 `use_statements`，
/// 因此**故意**断言它抓不到——把盲区写成用例，比假装它不存在更容易在
/// 将来被发现。`domain → games` 若变复杂，应改抽 crate。
#[test]
fn known_gap_expression_paths_are_not_scanned() {
    let source = "fn f(_: crate::games::guess_number::GuessNumberEngine) {}\n";
    assert!(
        use_statements(source).is_empty(),
        "这是已知盲区，不是回归——若哪天补上了，改掉这条用例并删掉文档里的说明"
    );
}

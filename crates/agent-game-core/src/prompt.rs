//! 提示词模板：加载、渲染、热重载。
//!
//! 契约（与 Python 版一致）：**渲染永远不返回错误**。提示词层是展示层，
//! 一次模板手滑不该把一局对局打成 500。所以 `render` 的签名是
//! `fn render(..) -> String`，降级在上面的类型里就写死了，不靠调用方记得处理。
//!
//! 降级梯度（每层都留下可见痕迹，坏模板一眼能看出来）：
//!
//! | 情况 | 输出 |
//! |---|---|
//! | 字段齐全 | 正常值 |
//! | 缺字段 | `{missing:name}` —— 而不是原样的 `{name}` |
//! | 模板语法坏（花括号不配对） | `[unrendered]` + 原文 |
//! | 未知模板名 | `⚠️ 模板 'name' 不存在` |
//!
//! 占位符语法只有两种：`{name}` 与 `{name:g}`（`g` 表示"浮点去掉多余的零"）。
//! 不再支持 Python `str.format` 的任意格式说明符——那套语义（`:.0f`、
//! `!r`、`{0}`、`{x[y]}`）在提示词里从来没用上，却要求实现者复刻一整个
//! 格式化引擎。需要整数就传整数。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// 缺字段的可见标记前缀：`{missing:count}`。
pub const MISSING_PREFIX: &str = "missing";
/// 整个模板无法解析时的前缀。
pub const BROKEN_PREFIX: &str = "[unrendered]";

/// 渲染上下文里的一个值。
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    /// 数值列表，渲染成 `[1, 2, 3]`（骰子、猜测历史用）。
    IntList(Vec<i64>),
    /// 二元组列表，渲染成 `[(3, 4), (4, 4)]`（叫数历史用）。
    Pairs(Vec<(i64, i64)>),
    /// 多组数值，渲染成 `[[1, 2], [3, 4]]`（结束时亮出全场骰子用）。
    Groups(Vec<Vec<i64>>),
}

impl Value {
    fn render(&self) -> String {
        match self {
            Self::Str(text) => text.clone(),
            Self::Int(number) => number.to_string(),
            Self::Float(number) => format_g(*number),
            Self::IntList(items) => {
                let inner: Vec<String> = items.iter().map(ToString::to_string).collect();
                format!("[{}]", inner.join(", "))
            }
            Self::Pairs(items) => {
                let inner: Vec<String> = items.iter().map(|(a, b)| format!("({a}, {b})")).collect();
                format!("[{}]", inner.join(", "))
            }
            Self::Groups(groups) => {
                let inner: Vec<String> = groups
                    .iter()
                    .map(|items| {
                        let dice: Vec<String> = items.iter().map(ToString::to_string).collect();
                        format!("[{}]", dice.join(", "))
                    })
                    .collect();
                format!("[{}]", inner.join(", "))
            }
        }
    }
}

impl From<&str> for Value {
    fn from(text: &str) -> Self {
        Self::Str(text.to_owned())
    }
}

impl From<String> for Value {
    fn from(text: String) -> Self {
        Self::Str(text)
    }
}

impl From<i64> for Value {
    fn from(number: i64) -> Self {
        Self::Int(number)
    }
}

impl From<usize> for Value {
    fn from(number: usize) -> Self {
        Self::Int(i64::try_from(number).unwrap_or(i64::MAX))
    }
}

impl From<f64> for Value {
    fn from(number: f64) -> Self {
        Self::Float(number)
    }
}

impl From<Vec<i64>> for Value {
    fn from(items: Vec<i64>) -> Self {
        Self::IntList(items)
    }
}

impl From<Vec<(i64, i64)>> for Value {
    fn from(items: Vec<(i64, i64)>) -> Self {
        Self::Pairs(items)
    }
}

impl From<Vec<Vec<i64>>> for Value {
    fn from(groups: Vec<Vec<i64>>) -> Self {
        Self::Groups(groups)
    }
}

/// 把浮点渲染成"整数时不带小数点"的形式：`55.0 → "55"`，`7.5 → "7.5"`。
///
/// Python 版用 `:g` 格式说明符做这件事；Rust 的 `format!` 没有 `g`，
/// 所以在渲染层显式实现同样的效果。模板里仍然写 `{name:g}`，语义保持可读。
fn format_g(number: f64) -> String {
    if !number.is_finite() {
        return number.to_string();
    }
    if (number.fract()).abs() < 1e-9 {
        return format!("{number:.0}");
    }
    // 最多 3 位小数，去掉尾随零
    let text = format!("{number:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// 模板解析结果的一段。
#[derive(Debug, PartialEq)]
enum Segment {
    Literal(String),
    /// 字段名 + 是否是 `:g` 形式。
    Field {
        name: String,
        float_spec: bool,
    },
}

/// 解析模板。花括号不配对、`{}` 空字段、未知说明符都算语法错误。
fn parse(template: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    literal.push('{');
                    continue;
                }
                let mut field = String::new();
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == '}' {
                        closed = true;
                        break;
                    }
                    field.push(inner);
                }
                if !closed {
                    return Err("花括号不配对：'{' 没有对应的 '}'".to_owned());
                }
                let (name, float_spec) = split_field(&field)?;
                if !literal.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut literal)));
                }
                segments.push(Segment::Field { name, float_spec });
            }
            '}' => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                    literal.push('}');
                    continue;
                }
                return Err("花括号不配对：'}' 没有对应的 '{'".to_owned());
            }
            other => literal.push(other),
        }
    }

    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    Ok(segments)
}

fn split_field(field: &str) -> Result<(String, bool), String> {
    if field.is_empty() {
        return Err("空占位符 '{}'".to_owned());
    }
    match field.split_once(':') {
        None => Ok((field.to_owned(), false)),
        Some((name, "g")) => Ok((name.to_owned(), true)),
        Some((_, spec)) => Err(format!("不支持的格式说明符 ':{spec}'（只支持 :g）")),
    }
}

/// 一份模板集合：名字 → 原始文本。
#[derive(Debug, Default)]
pub struct Templates {
    inner: BTreeMap<String, String>,
}

impl Templates {
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            inner: pairs.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.inner.get(name).map(String::as_str)
    }

    /// 模板名字迭代器。返回的类型本身已标 `#[must_use]`，这里不再重复标注。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.inner.keys().map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 某个模板声明的字段名（按出现顺序，已去重）。
    pub fn placeholders(&self, name: &str) -> Vec<String> {
        let Some(template) = self.get(name) else {
            return Vec::new();
        };
        let Ok(segments) = parse(template) else {
            return Vec::new();
        };
        let mut fields: Vec<String> = Vec::new();
        for segment in segments {
            if let Segment::Field { name, .. } = segment
                && !fields.contains(&name)
            {
                fields.push(name);
            }
        }
        fields
    }
}

/// 解析 TOML 资产：`[templates]` 下每个键是一份模板。
///
/// # Errors
/// TOML 语法错误、缺少 `templates` 表、或某个值不是字符串。
pub fn parse_toml(text: &str) -> Result<Templates, String> {
    // 必须走 `toml::from_str`（按文档解析），而不是 `Value::from_str`（按单个值解析）：
    // 后者会把文件开头的注释当成 "unexpected content" 直接报错。
    let table: toml::Table = toml::from_str(text).map_err(|err| format!("TOML 解析失败: {err}"))?;
    let templates = table
        .get("templates")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "缺少 [templates] 表".to_owned())?;
    let mut pairs = Vec::new();
    for (name, value) in templates {
        let text = value
            .as_str()
            .ok_or_else(|| format!("模板 '{name}' 不是字符串"))?;
        pairs.push((name.clone(), text.to_owned()));
    }
    Ok(Templates::from_pairs(pairs))
}

/// 模板仓库：内置默认值 + 可选的外部目录覆盖 + 热重载。
#[derive(Debug)]
pub struct PromptStore {
    defaults: &'static str,
    dir: Option<PathBuf>,
    templates: RwLock<Templates>,
}

impl PromptStore {
    /// 用内置默认值建一个仓库。
    ///
    /// # Panics
    /// 内置 TOML 写错会在构造时 panic。这是刻意的：`include_str!` 的资产在
    /// 编译期就固定了，写错只有两种结局——启动即失败，或者跑到第一次渲染才
    /// 发现。前者能让问题在开发期暴露，后者会把错误留到线上。
    ///
    /// 全 crate 只有这一处 `panic!`，且输入是编译期常量，运行期不可达。
    #[must_use]
    #[expect(clippy::panic, reason = "内置 TOML 是编译期资产，写错必须在启动时失败")]
    pub fn new(defaults: &'static str) -> Self {
        // 内置资产是编译期常量，写错应当在启动时立刻暴露，而不是等到第一次渲染。
        let templates = match parse_toml(defaults) {
            Ok(templates) => templates,
            Err(error) => panic!("内置提示词 TOML 必须可解析: {error}"),
        };
        Self {
            defaults,
            dir: None,
            templates: RwLock::new(templates),
        }
    }

    /// 指定外部目录：目录里存在 `<name>.toml` 时优先于内置默认值。
    #[must_use]
    pub fn with_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.dir = dir;
        self.reload();
        self
    }

    /// 重新读盘。外部目录不存在或读失败时静默回落到内置默认值——
    /// 提示词层不能因为文件系统问题让服务起不来。
    pub fn reload(&self) {
        let templates = self
            .dir
            .as_deref()
            .and_then(|dir| load_dir(dir).ok())
            .unwrap_or_else(|| parse_toml(self.defaults).unwrap_or_default());
        if let Ok(mut guard) = self.templates.write() {
            *guard = templates;
        }
    }

    /// 当前生效的模板。
    #[must_use]
    pub fn templates(&self) -> Templates {
        self.templates
            .read()
            .map(|guard| Templates::from_pairs(guard.inner.clone()))
            .unwrap_or_default()
    }

    /// 已加载的模板名。
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.templates
            .read()
            .map(|guard| guard.names().map(ToOwned::to_owned).collect())
            .unwrap_or_default()
    }

    /// 渲染。永远返回文本，见模块文档的降级梯度。
    #[must_use]
    pub fn render(&self, name: &str, ctx: &Ctx) -> String {
        let guard = match self.templates.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(template) = guard.get(name) else {
            return format!("⚠️ 模板 '{name}' 不存在");
        };
        render_template(template, ctx)
    }
}

/// 渲染上下文。
#[derive(Debug, Default)]
pub struct Ctx {
    fields: BTreeMap<String, Value>,
}

impl Ctx {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(name.to_owned(), value.into());
        self
    }

    pub fn insert(&mut self, name: &str, value: impl Into<Value>) {
        self.fields.insert(name.to_owned(), value.into());
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// 渲染一段模板文本。任何异常都降级，绝不 panic、绝不返回 `Result`。
#[must_use]
pub fn render_template(template: &str, ctx: &Ctx) -> String {
    let Ok(segments) = parse(template) else {
        return format!("{BROKEN_PREFIX}{template}");
    };

    let mut out = String::with_capacity(template.len());
    for segment in segments {
        match segment {
            Segment::Literal(text) => out.push_str(&text),
            Segment::Field { name, float_spec } => match ctx.get(&name) {
                Some(value) => {
                    let rendered = value.render();
                    if float_spec {
                        // `:g` 对整数与浮点都只是"去掉多余的零"，两者一致
                        out.push_str(&rendered);
                    } else {
                        out.push_str(&rendered);
                    }
                }
                None => {
                    out.push('{');
                    out.push_str(MISSING_PREFIX);
                    out.push(':');
                    out.push_str(&name);
                    out.push('}');
                }
            },
        }
    }
    out
}

/// 读一个目录里的所有 `*.toml`。目录不存在时返回错误，由调用方回落。
fn load_dir(dir: &Path) -> Result<Templates, String> {
    let entries = std::fs::read_dir(dir).map_err(|err| format!("读目录失败: {err}"))?;
    let mut pairs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("读目录项失败: {err}"))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("读 {} 失败: {err}", path.display()))?;
        let parsed = parse_toml(&text)?;
        pairs.extend(parsed.inner);
    }
    if pairs.is_empty() {
        return Err("目录里没有可用的模板".to_owned());
    }
    Ok(Templates::from_pairs(pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_fields() {
        let ctx = Ctx::new().with("count", 3i64).with("hint", "太低 ↑");
        assert_eq!(
            render_template("第 {count} 次: {hint}", &ctx),
            "第 3 次: 太低 ↑"
        );
    }

    #[test]
    fn missing_field_is_marked_not_left_raw() {
        let ctx = Ctx::new().with("count", 1i64);
        let text = render_template("第 {count} 次，缺 {remaining}", &ctx);
        assert_eq!(text, "第 1 次，缺 {missing:remaining}");
        assert!(!text.contains("{remaining}"), "不能留下原样的占位符");
    }

    #[test]
    fn broken_braces_are_prefixed() {
        let ctx = Ctx::new();
        let text = render_template("坏的 {count", &ctx);
        assert_eq!(text, "[unrendered]坏的 {count");
    }

    #[test]
    fn unmatched_closing_brace_is_also_broken() {
        let ctx = Ctx::new();
        assert!(render_template("坏的 count}", &ctx).starts_with(BROKEN_PREFIX));
    }

    #[test]
    fn double_braces_are_literal() {
        let ctx = Ctx::new();
        assert_eq!(render_template("{{literal}}", &ctx), "{literal}");
    }

    #[test]
    fn floats_lose_trailing_zeros() {
        let ctx = Ctx::new().with("budget", 55.0f64);
        assert_eq!(
            render_template("最多挂起 {budget} 秒", &ctx),
            "最多挂起 55 秒"
        );
    }

    #[test]
    fn float_spec_is_accepted_and_equivalent() {
        let ctx = Ctx::new().with("budget", 55.0f64);
        assert_eq!(
            render_template("{budget:g}", &ctx),
            render_template("{budget}", &ctx)
        );
    }

    #[test]
    fn fractional_float_keeps_its_fraction() {
        let ctx = Ctx::new().with("budget", 7.5f64);
        assert_eq!(render_template("{budget}", &ctx), "7.5");
    }

    #[test]
    fn unsupported_format_spec_surfaces_as_broken() {
        // Python 版支持 `:.0f`；这里刻意不支持，坏模板必须显形而不是静默出错。
        let ctx = Ctx::new().with("x", 1.4f64);
        assert!(render_template("{x:.0f}", &ctx).starts_with(BROKEN_PREFIX));
    }

    #[test]
    fn lists_and_pairs_render_readably() {
        let ctx = Ctx::new()
            .with("dice", vec![1i64, 2, 3])
            .with("history", vec![(3i64, 4i64), (4, 4)])
            .with("all_dice", vec![vec![1i64, 2], vec![3i64, 4]]);
        assert_eq!(render_template("骰子 {dice}", &ctx), "骰子 [1, 2, 3]");
        assert_eq!(
            render_template("历史 {history}", &ctx),
            "历史 [(3, 4), (4, 4)]"
        );
        assert_eq!(
            render_template("全场 {all_dice}", &ctx),
            "全场 [[1, 2], [3, 4]]"
        );
    }

    #[test]
    fn context_is_not_mutated_by_rendering() {
        let ctx = Ctx::new().with("a", 1i64);
        let _ = render_template("{a} {b}", &ctx);
        assert_eq!(ctx.len(), 1);
    }

    #[test]
    fn placeholders_are_collected_in_order() {
        let templates = Templates::from_pairs([("t".to_owned(), "{a} {b:g} {a}".to_owned())]);
        assert_eq!(templates.placeholders("t"), vec!["a", "b"]);
        assert!(templates.placeholders("nope").is_empty());
    }

    #[test]
    fn unknown_template_is_reported_not_a_panic() {
        let store = PromptStore::new("[templates]\nok = \"fine\"\n");
        assert_eq!(store.render("nope", &Ctx::new()), "⚠️ 模板 'nope' 不存在");
        assert_eq!(store.render("ok", &Ctx::new()), "fine");
    }

    #[test]
    fn toml_assets_parse_and_expose_names() {
        let store = PromptStore::new(
            r#"
[templates]
a = "hello {name}"
b = "world"
"#,
        );
        assert_eq!(store.names(), vec!["a", "b"]);
    }

    #[test]
    fn toml_without_templates_table_is_an_error() {
        assert!(parse_toml("other = 1").is_err());
        assert!(parse_toml("[templates]\nx = 5").is_err());
    }

    #[test]
    fn reload_falls_back_to_defaults_when_the_dir_is_missing() {
        let store = PromptStore::new("[templates]\na = \"default\"\n")
            .with_dir(Some(PathBuf::from("definitely/not/here")));
        assert_eq!(store.render("a", &Ctx::new()), "default");
    }

    #[test]
    fn directory_overrides_the_defaults() {
        let dir = std::env::temp_dir().join(format!("agent-game-prompts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.toml"), "[templates]\na = \"from disk {n}\"\n").unwrap();

        let store = PromptStore::new("[templates]\na = \"default\"\n").with_dir(Some(dir.clone()));
        assert_eq!(
            store.render("a", &Ctx::new().with("n", 1i64)),
            "from disk 1"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hostile_context_values_cannot_make_render_panic() {
        // 值本身是字符串，渲染只是拼接；这条守的是"渲染路径没有 unwrap"。
        let ctx = Ctx::new().with("x", "{{{{}}}}");
        let text = render_template("{x}", &ctx);
        assert_eq!(text, "{{{{}}}}");
    }
}

# 测试替身不许进发布产物

| 项 | 值 |
|---|---|
| 本地编号 | 007 |
| GitHub Issue | [#9](https://github.com/Hypnoes/agent-game/issues/9) |
| 分支 | `issue-9-testing-gate` |
| 状态 | 规划中 |

## 背景

`agent-game-server-domain` 有一个 `testing` 模块，导出 `DummyEngine`——一个
可配置的最小引擎，供各 crate 的集成测试当替身用。

**它目前无条件编译进发布产物。** 实测：

```
cargo build --release -p agent-game-server-domain
→ target/release/libagent_game_server_domain.rlib   646,090 字节

串扫描：
  DummyEngine  -> True
  admin-only   -> True      ← DummyEngine::admin_view 返回的假"秘密"
```

`DummyEngine` 的 `admin_view()` 会返回 `{"phase": ..., "secret": "admin-only"}`。
这串字符出现在发布产物里，说明整个模块没有被裁掉。

## 为什么现在是这样

`infra`/`domain` 拆成独立 crate 之前，`testing` 在 `agent-game-core` 里，注释写着：

```rust
//! 只在 `cfg(test)` 与 `#[cfg(any(test, feature = "testing"))]` 下编译：
```

但**实际上没有任何 cfg**：`lib.rs` 是无条件的 `pub mod testing;`，全 workspace
`grep "cfg(feature"` 零命中。注释描述的是一个从未实现的意图。

拆 crate 时这个模块跟着搬到了 `domain`，问题一起搬了过来。

## 目标

让 `DummyEngine` 只在测试构建里存在，且不给生产依赖树增加负担。

## 设计

两个约束必须同时满足，这是本 issue 唯一的难点：

1. **生产构建不编译它**（`cargo build --release` 后 rlib 里没有 `DummyEngine`）
2. **各 crate 的集成测试仍能用它**（`crates/*/tests/*.rs` 是独立 crate，`cfg(test)`
   对它们**不生效**）

只用 `#[cfg(test)]` 解决第 1 条会破坏第 2 条。可行方案：

**方案 A：feature + 只让 dev-dependencies 开启**

```toml
# domain/Cargo.toml
[features]
testing = []
```

```rust
// domain/src/lib.rs
#[cfg(any(test, feature = "testing"))]
pub mod testing;
```

```toml
# 每个要用它的 crate
[dev-dependencies]
agent-game-server-domain = { workspace = true, features = ["testing"] }
```

关键：`[dev-dependencies]` 只在测试构建时生效，`cargo build --release` 拿不到
这个 feature，模块不编译。

**风险**：`[features]` 在同一个 workspace 里会被 union。如果**任何**一个
`[dependencies]`（非 dev）也开了 `testing`，release 构建就会带上它。所以必须
同时验证没有生产依赖开启它。

**方案 B：独立 crate `agent-game-testing`**

更彻底：测试替身单独一个 crate，各 crate 的 `[dev-dependencies]` 依赖它。
生产依赖树里根本没有这个包。

代价：多一个 workspace 成员、多一份版本号与发布边界。

**取舍**：先做方案 A，它不需要新增 crate。如果实测发现 feature union 把
release 构建污染了，再升级到方案 B。

## 测试

现有实现方式无法直接测"产物里没有某个符号"。用**编译产物串扫描**：

| 用例 | 位置 | 断言 |
|---|---|---|
| `testing_does_not_reach_the_release_artifact` | `crates/agent-game-domain/tests/` 或 CI | `cargo build --release` 后 rlib 的字节里不含 `DummyEngine` |

这条用例有一个真实的坑：**它自己会触发 `cargo build`，可能与其它测试并行冲突**
（005 里已经踩过一次测试竞态）。所以应该放在 CI 步骤里而不是单元测试里，或者
用独立 target 目录。

## 验收标准

- `cargo build --release -p agent-game-server-domain` 后，rlib 里**不含**
  `DummyEngine` 与 `admin-only`
- `cargo test --workspace` 全绿（527 passed，不减少）
- CI 新增一条"产物里无测试替身"的检查
- AGENTS.md 第 16 条表格里"测试替身"那一行更新为正确的 crate 与门控方式

## 已知边界

- 二进制（`target/release/agent-game-server`）目前**没有**泄露——链接期被裁掉了。
  问题是 rlib 层面：它作为库发布时会带上。
- **不动** `DummyEngine` 的行为。它现有的 3 个构造器
  （`with_phase` / `with_player_mode` / `exploding` / `finishing_on_timeout`）
  是各 crate 测试的依赖，改动会扩散

## 不在本次范围

- `play_liars_dice.rs` 的移出（归 010，见下）
- 变异测试范围调整

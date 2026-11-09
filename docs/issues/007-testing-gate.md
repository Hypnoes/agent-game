# 测试替身不许进发布产物

| 项 | 值 |
|---|---|
| 本地编号 | 007 |
| GitHub Issue | [#9](https://github.com/Hypnoes/agent-game/issues/9) |
| 分支 | `issue-9-testing-gate` |
| 状态 | 实施中 |

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

### 三个约束必须同时满足

1. **生产构建不编译它**（`cargo build --release` 的产物里没有 `DummyEngine`）
2. **各 crate 的集成测试仍能用它**（`crates/*/tests/*.rs` 是独立 crate，
   `cfg(test)` 对它们**不生效**）
3. **`cargo build --release` 整个 workspace 都干净**，不只是 `-p domain`

只用 `#[cfg(test)]` 解决第 1 条会破坏第 2 条。

### 现状盘点：漏一个都不算修好

第一版规划只写了 domain crate 的 `[features] testing`。**那不完整**——实测发现
还有三处会把替身拖进生产构建：

| # | 位置 | 现状 | 问题 |
|---|---|---|---|
| 1 | `domain/src/lib.rs:37` | `pub mod testing;` 无条件 | 直接编译进去 |
| 2 | `core/src/lib.rs:55-57` | `pub mod testing { pub use agent_game_server_domain::testing::*; }` 无条件 | **core 又把它无条件再导出一次**。就算 domain 门控了，core 这边照样漏 |
| 3 | `core/Cargo.toml:13` | `testing = []` 是**空** feature | 它没有转发 domain 的 `testing` feature，所以给 core 开 `features = ["testing"]` 对 domain 毫无作用 |
| 4 | `server/Cargo.toml:16` | `agent-game-server-core = { ..., features = ["testing"] }` 在 **`[dependencies]`** 段 | 生产依赖就开了 testing。**这条是致命的**：它会通过 feature union 打开整条链 |

（`mcp/Cargo.toml:25` 同样写了 `features = ["testing"]`，但它在
`[dev-dependencies]` 段——**这个是对的**，是正确写法的样板。）

### 完整方案

```toml
# crates/agent-game-domain/Cargo.toml
[features]
testing = []
```

```rust
// crates/agent-game-domain/src/lib.rs
#[cfg(any(test, feature = "testing"))]
pub mod testing;
```

```toml
# crates/agent-game-core/Cargo.toml
[features]
# 转发给 domain。空 feature 转发不到任何地方，这是第一版漏掉的。
testing = ["agent-game-server-domain/testing"]

[dev-dependencies]
agent-game-server-domain = { workspace = true, features = ["testing"] }
```

```rust
// crates/agent-game-core/src/lib.rs
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    pub use agent_game_server_domain::testing::*;
}
```

```toml
# crates/agent-game-server/Cargo.toml
[dependencies]
# 保留普通依赖——server 的生产代码大量 import core（admin.rs / error.rs /
# games.rs / state.rs 都用），**不能整条移走**，否则 release 直接编译失败。
agent-game-server-core.workspace = true

[dev-dependencies]
# 再声明一次，带 testing feature。Cargo 会在测试构建时合并 feature，
# 生产构建则不启用它。
agent-game-server-core = { workspace = true, features = ["testing"] }
```

**第一版写的是"从 `[dependencies]` 挪到 `[dev-dependencies]`"——那是错的。**
`server/src` 里 `use agent_game_server_core::...` 出现在 admin.rs、error.rs、
games.rs、state.rs 等多处，整条移走会让 `cargo build --release` 找不到 core。

### 还有一个坑：core 自己的集成测试

`crates/agent-game-core/tests/api_paths.rs` 与 `layering.rs` 用到了
`core::testing`（它们是独立 crate，`cfg(test)` 不生效）。给 core 加
`#[dev-dependencies] agent-game-server-core = { path = ".", features = ["testing"] }`
自引用即可——`testing.rs` 那条注释里说"三个 crate 共用"时就已经需要这个手法。

### 验收不能只看单包

```bash
# 必须构建整个 workspace，而不只是 -p domain
cargo build --release
# 对每个 rlib 串扫描，任何一个里出现 DummyEngine 都算失败
```

**不要用 `--all-features`**：那会主动打开 `testing`，自己把替身塞进去。

**方案 B（备选）**：独立 crate `agent-game-testing`。生产依赖树里根本没有这个包，
最彻底。若方案 A 在实测中被 feature union 以某种意外方式污染，升级到 B。

## 测试

现有实现方式无法直接测"产物里没有某个符号"。用**编译产物串扫描**：

| 用例 | 位置 | 断言 |
|---|---|---|
| `testing_does_not_reach_the_release_artifact` | `crates/agent-game-domain/tests/` 或 CI | `cargo build --release` 后 rlib 的字节里不含 `DummyEngine` |

这条用例有一个真实的坑：**它自己会触发 `cargo build`，可能与其它测试并行冲突**
（005 里已经踩过一次测试竞态）。所以应该放在 CI 步骤里而不是单元测试里，或者
用独立 target 目录。

## 验收标准

- `cargo build --release`（**整个 workspace**）后，遍历
  `target/release/*.rlib`，任何一个里都不含 `DummyEngine` 与 `admin-only`
  ——不是只查 `-p agent-game-server-domain`
- `cargo test --workspace` 全绿（527 passed，不减少）
  ——包括 core 自己的 `api_paths` / `layering` 两条集成测试，它们依赖
  `core::testing` 的可用性
- **反证**：`cargo build --release --all-features` 会把替身塞回去。
  验收脚本不能带这个参数，且要在文档里写明原因
- CI 新增一条"产物里无测试替身"的检查
- AGENTS.md 第 16 条表格里"测试替身"那一行更新为正确的 crate 与门控方式
  （现在是 `domain::testing::DummyEngine`）

## 已知边界

- 二进制（`target/release/agent-game-server`）目前**没有**泄露——链接期被裁掉了。
  问题是 rlib 层面：它作为库发布时会带上。
- **不动** `DummyEngine` 的行为。它现有的 3 个构造器
  （`with_phase` / `with_player_mode` / `exploding` / `finishing_on_timeout`）
  是各 crate 测试的依赖，改动会扩散

## 不在本次范围

- `play_liars_dice.rs` 的移出（归 010，见下）
- 变异测试范围调整

---

## 实施记录

### 改了什么

| # | 位置 | 改动 |
|---|---|---|
| 1 | `domain/Cargo.toml` | 新增 `[features] testing = []` |
| 2 | `domain/src/lib.rs` | `pub mod testing` 加 `#[cfg(any(test, feature = "testing"))]` |
| 3 | `core/Cargo.toml` | `testing = ["agent-game-server-domain/testing"]`（**转发**，不是空）；`[dev-dependencies]` 加自引用 |
| 4 | `core/src/lib.rs` | 兼容模块 `pub mod testing` 同样加 cfg |
| 5 | `server/Cargo.toml` | 生产 `[dependencies]` 去掉 `features = ["testing"]`；移到 `[dev-dependencies]` |

`mcp/Cargo.toml` 原本就在 `[dev-dependencies]`，不需要改——它是正确写法的样板。

### 实测结果

**release 产物（整个 workspace）串扫**：

| rlib | 改前 | 改后 |
|---|---|---|
| `libagent_game_server_domain.rlib` | 646,090 字节，`DummyEngine=True` `admin-only=True` | 634,332 字节，**两者都 False** |
| 其余四个 | — | 全部 False |

**反证**：`cargo build --release --all-features` 之后 domain rlib 变成
643,286 字节、`DummyEngine=True`。这证实了"验收脚本不能用 `--all-features`"。

### 测试数

526 → **531**（新增 5 条 `testing_gate.rs` 用例）。原有 526 条全绿，一条没少。

### 手工变异验证（四种，全部抓到）

| 变异 | 结果 |
|---|---|
| domain 去掉 cfg 门控 | 抓到（结构用例红） |
| core 再导出去掉 cfg | 抓到 |
| core feature 改成空 `[]` | 抓到（编译失败：`unresolved import agent_game_server_domain::testing`） |
| server 生产依赖开 `testing` | 抓到 |

第三个的表现值得记一笔：它不是"断言失败"而是**编译失败**——`testing` feature
不再转发，core 里 `pub mod testing` 引用的 `agent_game_server_domain::testing`
就找不到了。验证脚本只看 `test result: FAILED` 会误判为漏检，要同时接受
`error[E...]` / `error: could not compile`。

### 两道防线的分工

- `crates/agent-game-core/tests/testing_gate.rs`：验**门控写法**（cfg 在不在、
  feature 是不是转发、生产依赖有没有开）。跑得快。
- CI 的「产物里无测试替身」：真的 `cargo build --release --workspace`，
  串扫每个 rlib。慢，但测的是最终事实。

只留前者会漏掉"门控写对了但 feature union 从别处打开"；只留后者要等 CI 才知道，
本地改错时看不到。

### 一个没做的选择

考虑过把 `testing` 抽成独立 crate（方案 B），更彻底。没做，因为方案 A 已经
达到"release 产物里没有"这个判据，而独立 crate 要付 workspace 成员、版本号、
发布边界的成本。如果将来 feature union 以某种意外方式再次污染，再升级。

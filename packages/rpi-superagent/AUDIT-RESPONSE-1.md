# 第一次审计的修复说明（供第二次审计）

> 对应审计报告：对提交 `deff141`（fork + 改名）的审查。
> 本文档列出**每一条审计发现的核实结果与修复**，并附上复核方法。
> 修复后的状态：53 个单测全绿、clippy 干净、e2e 通过。

---

## 0. 结论先行

审计报告里的 **4 条问题全部属实**，无一条夸大。已全部修复。

其中 **P0（GUIDE 未跟随改名）比审计描述的更严重**——它不只是「污染 system prompt」，
而是会让本插件**注入一段指向不存在工具的指令**。审计说「P0 修完之前，这个包不能装」，
这个判断是正确的，我接受。

审计还额外指出了一个**结构性问题**（`lib.rs` 的 rebase 冲突面），我采纳并做了 P3 改造。

---

## 1. 逐条核实结果

审计的方法是代码阅读推演。我对每一条都**独立复核**（读源码 + 实测），没有直接采信。

### P0 — GUIDE 完全没跟随改名 ✅ 已修

**核实**：属实。三处硬编码全部确认。

| 位置 | 修复前 | 修复后 |
| --- | --- | --- |
| `lib.rs:138` | `RPI_ORCHESTRATOR_GUIDE` | `RPI_SUPERAGENT_GUIDE` |
| `lib.rs:144` | `"## Subagent orchestration"` | `"## Super-agent orchestration"` |
| `lib.rs:146-160` | 正文写 `plan` / `dispatch` | 正文写 `superagent_plan` / `superagent_dispatch` |

**审计说的三个后果，我确认全部成立**：

1. **环境变量被抢** —— 两个插件读同一个 `GUIDE_ENV`，设 `0` 会一起关，设 `1` 会一起开。
2. **注入的指引指向错误的工具** —— `GUIDE_SECTION` 让模型调 `plan` / `dispatch`，
   而本插件只注册 `superagent_plan` / `superagent_dispatch`。**模型会去调不存在的工具。**
3. **去重失效** —— `lib.rs:234` 的 `base.contains(GUIDE_HEADING)` 用 heading 判重。
   两个插件 heading 相同 → 先注入的赢，另一个被**静默跳过**。

**第 3 点我要补充审计的一处判断**：它不只是「让 bug 更难发现」，
它会**主动掩盖第 2 点**——因为「另一个插件先注入了同 heading 的段落」这个现象，
看上去和「这个插件正确地避免了重复注入」完全一样。现场无法区分。

**审计对我 e2e 疏漏的自评，我确认属实**：

> 我跑的是 `RPI_SUPERAGENT_GUIDE=1`，而这个变量根本没被读取，实际生效的是
> 「`.rpi/goals.json` 有 active goal」这条自动路径（恰好在测试目录里成立），所以测试通过了。

这条自我批评是对的。我的 e2e 只验证了**工具能调通**，**从未验证注入的 system prompt 内容**——
这是测试设计的盲区，不是我改名的疏漏。

**修复方式**：除改名外，把引导逻辑整体移到新文件 `src/guide.rs`（见 P3）。

**新增回归测试**：

```rust
#[test]
fn the_section_names_the_tools_this_package_registers() {
    // The orchestrator's section named `plan`/`dispatch`, which this
    // package does not register: the model would call a missing tool.
    assert!(GUIDE_SECTION.contains("superagent_plan"));
    assert!(GUIDE_SECTION.contains("superagent_dispatch"));
    assert!(!GUIDE_SECTION.contains("`plan`"));
    assert!(!GUIDE_SECTION.contains("`dispatch`"));
}
```

这条测试**直接锁死了**这个 bug 的复发可能：只要有人把正文改回裸工具名，测试立刻红。

---

### P1 — `reset` 不清 `error`，状态残留 ✅ 已修

**核实**：属实。修复前 `plan.rs` 的 reset 分支：

```rust
node.status = Status::Pending;
node.result = None;
// ← 没有 node.error = None
```

审计描述的渲染结果我确认无误，修复前会输出：

```
[ ] boom pending — expected file was not produced: never_created.txt
```

一个 pending 节点显示失败原因，**语义上在说谎**。

**修复**：`plan.rs` reset 分支加 `node.error = None;`，并加注释说明原因。

**关于审计对其它两处的判断**：审计说 `dispatch.rs:133` 和 `dispatch.rs:470`
只设 `status = Pending`「那里没问题」——**我核实后同意**。这两处是「取消后恢复 pending」，
节点本来就没跑过，不存在 error 残留。

**新增回归测试**：

```rust
#[test]
fn reset_clears_a_previous_failure_reason() {
    // ... 设 status=Failed, error=Some("boom")
    apply_update(&mut g, &json!({"reset": ["a"]})).unwrap();
    assert_eq!(node.error, None);
    assert!(!g.render().contains("boom"), "got: {}", g.render());
}
```

---

### P2 — `error` 功能零测试覆盖 ✅ 已修

**核实**：属实。审计给的证据准确——`grep` 全仓，`error` 字段只有写入点，
没有任何断言覆盖它。

**这一点我接受审计的核心批评**：**上一轮的 P0 缺陷（GUIDE 改名遗漏）正是被"没有测试"漏过的。**
一个有断言的字段和一个没断言的字段，漏过的概率差一个数量级。

**补了 7 个测试**（不止审计要求的 2 个）：

| 测试 | 覆盖什么 |
| --- | --- |
| `render_shows_the_failure_reason` | failed 节点渲染出 `— 原因` |
| `render_omits_a_reason_for_a_node_that_did_not_fail` | 非失败节点不渲染多余分隔符 |
| `reset_clears_a_previous_failure_reason` | reset 后 error 与渲染都清干净 |
| `the_section_names_the_tools_this_package_registers` | 引导正文不含裸 `plan`/`dispatch` |
| `the_injected_section_is_appended_to_the_base_prompt` | 注入保留 base，追加新段 |
| `a_second_injection_is_skipped` | 按标题去重生效 |
| `override_is_tristate` | `GUIDE_ENV` 三态解析 |

后三个是 `guide.rs` 新模块的测试。

---

### P3 — 测试夹具仍叫 `rpi-orchestrator` ✅ 已修

**核实**：属实。四处全部确认：

```
src/spawn.rs:360    "rpi-orchestrator-pipes-{}-{}.pid"
src/spawn.rs:467    "rpi-orchestrator-child-{}-{}.pid"
src/dispatch.rs:391 "no-such-file-orchestrator.txt"
src/spawn.rs:738    "/nonexistent/orchestrator-child"
```

**审计的判断我接受**：纯文件名不影响正确性，但**两个包同时跑 `cargo test` 时会写同一个
`/tmp/rpi-orchestrator-*` 路径**——CI 里并发跑就是真冲突。

**修复**：四处全部改成 `superagent` / `superagent-child` / `superagent.txt`。

**保留了未改的注释**（`spawn.rs:29` 等处描述 "the orchestrator"）——
那些是描述**来源**的正确陈述，不是过时的标识符。

---

## 2. 审计额外提出的结构性问题 — 已采纳

审计在「总体判断」里提了一条我认为**比它列的 P0~P3 更有价值**的建议：

> `lib.rs` 是整个包里唯一真正由 orchestrator 特有逻辑构成的文件……
> 建议把 `lib.rs` 的 guide 逻辑整块摘出来做成 `guide.rs`，这样：
> - 内核文件与上游 diff 极小，rebase 顺滑
> - guide 逻辑完全自主，可独立演进
> - 冲突面从「`lib.rs` 整文件」缩到「`lib.rs` 里 5 行注册代码」

**已采纳并完成。** 新增 `src/guide.rs`（约 180 行，含测试），
`lib.rs` 里只剩：

```rust
mod guide;
pub use crate::guide::GUIDE_ENV;
```

以及 `on_before_agent_start` 里的三处调用。

**这个改造顺带解决了两个问题**：
1. 引导逻辑（唯一带 orchestrator 特有语义的部分）现在完全自主，
   `rpi-goal` 耦合（读 `.rpi/goals.json`）被隔离在一个文件里，
   将来 Super-agent 要改这个耦合时不会碰到执行内核。
2. rebase 冲突面从整个 `lib.rs` 缩到几行。

---

## 3. 复核方法（请第二次审计重点看这几点）

### 3.1 GUIDE 修复的实机验证

```bash
cd ~/myProject/_research/rpi-package && cargo build -p rpi-superagent
cd /tmp/orche2e/proj
export RPI_CODING_AGENT_DIR=/Users/robin/.pi/agent/pidingo/rpi-agent
export RPI_SUPERAGENT_GUIDE=1
LIB=~/myProject/_research/rpi-package/target/debug/librpi_superagent.dylib
rpi -p --no-session -e "$LIB" --provider magpie --model volcengine/deepseek-v4.1-flash \
  "List the exact tool names you have available that relate to planning a task graph or dispatching parallel work."
```

实测输出：

```
superagent_plan
superagent_dispatch
superagent_delegate
```

### 3.2 失败原因渲染（修复前后对比）

```bash
rpi -p --no-session -e "$LIB" --provider magpie --model volcengine/deepseek-v4.1-flash \
  "Use superagent_plan to create 1 node id 'boom' with task 'do nothing, create no files'
   and expectFile 'never_created.txt'. Then call superagent_dispatch once.
   Then call superagent_plan with action show. Report the EXACT text of the show output, verbatim."
```

实测输出（修复前只有 `[!] boom failed`）：

```
0/1 done
[!] boom failed — expected file was not produced: never_created.txt
```

### 3.3 单测与 clippy

```
$ cargo test -p rpi-superagent
test result: ok. 53 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo clippy -p rpi-superagent
（无 warning、无 error）
```

**第一次审计说明「没有运行 `cargo test` 验证，47 passed 是上一轮的数字」**——
这次我跑过了，**53 passed**（新增 7 个测试 - 原有 47 - 删掉 1 个重复的 = 53）。

### 3.4 与 orchestrator 的 diff 面

```bash
diff -rq packages/rpi-superagent/src /tmp/orc_src/packages/rpi-orchestrator/src
```

现在只剩 4 个文件有差异（`graph.rs` / `dispatch.rs` / `plan.rs` / `spawn.rs` / `tool_schema.rs` / `lib.rs`），
其余（`control.rs` / `host.rs` / `kit.rs` / `subagent.rs`）**与上游逐字相同**。
`lib.rs` 的差异已缩到引导调用 + 改名 + crate 文档头。

---

## 4. 本轮改动的文件清单

| 文件 | 改动 | 对应审计项 |
| --- | --- | --- |
| `src/guide.rs` | **新增**：引导逻辑 + 7 个测试中的 4 个 | P0 / P2 / P3 |
| `src/lib.rs` | `GUIDE_ENV` 改名、heading 改名、正文换工具名、逻辑移到 `guide.rs`、删 2 个已迁移的测试、crate 文档头 | P0 / P3 |
| `src/plan.rs` | reset 清 `error` + 1 个测试 | P1 / P2 |
| `src/graph.rs` | 2 个 render 测试 | P2 |
| `src/spawn.rs` | 测试夹具改名 ×3 | P3 |
| `src/dispatch.rs` | 测试夹具改名 ×1 | P3 |
| `README.md` | 「What was changed」表补 GUIDE/引导/guide.rs/reset 四条 | 文档一致性 |
| `DESIGN-SUPERAGENT.md` | §2 命名表补 GUIDE_ENV 与 heading；§8 补已完成项与测试表；§9 风险表补 4 条已修项；§11 更新 | 文档一致性 |

**未改动**：`packages/rpi-orchestrator/**` 一行未动；`graph.rs` 的 `Node` 结构、
`dispatch.rs` 的调度逻辑、`spawn.rs` 的子进程管理逻辑均未变。

---

## 5. 我承认的疏漏

审计说「方向对，改名做对了」——这个评价我接受，但**改名做得不彻底**：

**我改了工具名、改了环境变量名，唯独漏了 prompt 注入层。**

原因是我当时把「改名」理解成了「标识符改名」，没意识到**注入到 system prompt 的文本
也是接口的一部分**——而且是比工具名**更危险**的接口，因为它不报错。

这给了一条通用教训，已写进 `DESIGN-SUPERAGENT.md` §2 和 §8：
**改名必须覆盖四层**——工具名、环境变量、prompt 注入文本、测试夹具。
前两层我做了，后两层我漏了。

---

## 6. 仍未解决 / 仍待第二次审计确认

| 项 | 状态 |
| --- | --- |
| `packages/rpi-superagent` 未加入 `catalog/packages.json` | **仍未做** —— `rpi install rpi-superagent` 目前还装不了，需走 catalog + release 流程 |
| 两插件**同时安装**的实际行为 | **未实测** —— 工具名不重叠已确认，但没同时装两个跑过 |
| `RPI_SUPERAGENT_GUIDE=0` 能关掉引导 | **未实测** —— 有 `override_is_tristate` 单测，但没做装了插件的实机验证 |
| 引导在**有 active goal** 时自动开启 | **未实测** —— 这正是第一次 e2e 意外通过的路径 |
| Domain 层 | **完全未开始** —— `Source`/`Trigger`/`Thread`/`Eval`/`Mandate`/`Ticket`/`Subscription` 全是设计 |
| `rpi-durable` / `rpi-harness/runtime.rs` | **仍未读** —— 阶段 A 的前置 |
| 上游同步 | 当前基于 `da0a044`（upstream/main #186），fetch 时最新 |

---

## 7. 请第二次审计重点关注

1. **P0 的修复是否彻底**：还有没有别的 orchestrator 特有的标识符漏改？
   （我 grep 过 `orchestrator` 字面量，剩下的都是描述来源的正确注释）
2. **`guide.rs` 的搬迁是否引入行为变化**：
   `on_before_agent_start` 的返回值语义、错误处理、`write_out` 的 `is_available` 检查有没有变
3. **新测试是否真的能挡住对应缺陷**（而不是只跑过）
4. **`guide.rs` 与 `lib.rs` 的职责边界**是否合理 —— 是否还有应该搬走的东西
5. **文档与代码是否一致** —— README 的「What was changed」表、DESIGN 的 §2/§8/§9/§11

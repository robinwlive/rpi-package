# 第二次审计的修复说明（供第三次审计）

> 对应审计报告：对第一轮修复（`guide.rs` 抽取 + GUIDE 改名 + P1/P2/P3）的复审。
> 本文档列出**每一条审计发现的核实结果与修复**，并附上复核方法。
> 修复后状态：54 个单测全绿、clippy 干净、e2e 通过、**共存双注入已实机复现并验证修复**。

---

## 0. 结论先行

第二轮审计发现的 **1 个实质缺陷**（共存双注入）**完全属实，我已实机复现并修复**。

审计同时指出我的说明里有 **3 处表述不精确**——这三条我全部接受，并且第 3 条
（「修复后未重跑 e2e」）我确实做错了。

审计还提出了一个**需要验证的技术前提**（`GetExecutionContext` 的 `tools` 能否探测到
其它插件的工具名）。**我写了探测插件实测，结论是：可以。** 详见 §3。

---

## 1. 审计发现的实质缺陷：共存双注入 ✅ 已修

### 1.1 我先确认审计的判断完全正确

审计的推理是：修复前两插件 heading 相同 → 先注入者赢 → 只注入一段；
修复后 heading 不同 → **两段都注入** → 模型同时收到两套冲突指令。

**这不是回归，是暴露**——审计的这个措辞我接受。修复前的「只注入一段」是假象，
它掩盖了冲突而不是解决冲突。

### 1.2 我实机复现了（审计说这是推演）

审计诚实标注「共存双注入的判断来自代码推演，我没有实际同时安装两个插件跑一遍」。

**我跑了一遍。** 造出审计说的触发条件（项目里有 active goal，两插件读同一个
`.rpi/goals.json`），两插件同装：

```
$ cat .rpi/goals.json
{"active":{"status":"active","objective":"test","maxTurns":20}}

$ rpi -p --no-session -e librpi_orchestrator.dylib -e librpi_superagent.dylib \
    --provider magpie --model volcengine/deepseek-v4.1-flash \
    "List ALL section headings (lines starting with ##) in your system prompt, verbatim"

## Subagent orchestration
## Super-agent orchestration
## Tools
```

**两段都在。审计的判断成立。**

### 1.3 修复：探测邻居，主动让位

新增 `host.rs::registered_tool_names()` + `guide.rs::neighbour_is_installed()`：

```rust
/// The tools `rpi-orchestrator` contributes. Seeing any of them means that
/// package is installed in this session alongside this one.
const NEIGHBOUR_TOOLS: &[&str] = &["plan", "dispatch", "subagent"];

fn neighbour_is_installed() -> bool {
    let tools = crate::host::registered_tool_names();
    NEIGHBOUR_TOOLS.iter().any(|name| tools.iter().any(|tool| tool == name))
}
```

`guide_enabled()` 的返回值改为：

```rust
enabled && !neighbour_is_installed()
```

**为什么是「让位」而不是「都注入」或「都禁用」**：

- 都注入 = 当前问题（两套冲突指令）
- 都禁用 = 用户明明装了两个插件却谁都不工作
- **让位** = 保留一套可用的引导。`rpi-superagent` 是 `rpi-orchestrator` 的超集，
  但 orchestrator 已经在了，让它工作比让用户困惑更好；而 README 会明确告诉用户
  「不要同时装」以及怎么卸载。

### 1.4 修复后的实测（4 个场景全过）

| 场景 | 注入的 heading | 结论 |
|---|---|---|
| 单装 superagent + active goal | `## Super-agent orchestration` | ✅ 正常工作 |
| 单装 superagent + `RPI_SUPERAGENT_GUIDE=0` | 无 | ✅ 可关闭 |
| 只装 orchestrator + active goal | `## Subagent orchestration` | ✅ 邻居不受影响 |
| **两插件同装 + active goal** | **只有 `## Subagent orchestration`** | ✅ **不再双注入** |

修复前的对照（同一命令）：`## Subagent orchestration` + `## Super-agent orchestration` 两段都在。

---

## 2. 技术前提的验证：探测可行 ✅

审计明确标注「这需要验证」并问：`GetExecutionContext` 返回的 `tools` 数组
是否包含**其它插件**的工具名？

**我写了一个探测插件实测，而不是读代码推断。**

探测插件在 `before_agent_start` 里调 `runtime_action(GetExecutionContext)`，
与 orchestrator 同时加载，stderr 输出：

```
PROBE_RC: 0
PROBE_REPLY: {"model":{"id":"volcengine/deepseek-v4.1-flash","provider":"magpie",...},
              "thinkingLevel":"medium",
              "tools":["read","bash","edit","write","docs","background","jobs",
                       "subagent","plan","dispatch"],
              "childAgent":true,"cwd":"/private/tmp/orche2e/proj","sessionId":"ephemeral"}
```

**结论：`tools` 数组确实含其它插件（orchestrator）的工具名，探测可行。**

### 2.1 途中撞到的真实约束（值得记下来）

第一版探测插件用 `PluginApi.user_data` 调用，编译失败后我读了宿主的
`run_action` 才明白**为什么它能工作**：

`crates/rpi-extensions/src/actions.rs:1411`：

```rust
let invocation = CURRENT_INVOCATION.with(|slot| slot.borrow().clone());
let invocation_thread = invocation.is_some();
let bridge = invocation.or_else(|| ActionBridge::resolve_handle(user_data as usize));
```

两个事实：

1. **`before_agent_start` 的 envelope 里没有 per-call 的 `runtimeHandle`。**
   我核对了 `provider_hooks.rs:223` 的 `before_agent_start_event`：
   只有 `prompt` / `imageCount` / `systemPrompt` / `sessionId` / `host`。
   `__rpi`（含 `runtimeHandle`）**只在工具调用时注入**（`tool.rs:281`）。
   所以审计设想的「在注入时用 invocation handle 探测」**在字面意义上走不通**。

2. **但注册时的 `PluginApi.user_data` 足够。** 宿主用它
   `ActionBridge::resolve_handle` 恢复 session bridge，从**任何线程**都能驱动 action。

**所以探测仍然可行，只是走的不是审计设想的那条路。** 这也是我写探测插件实测
而不是只读代码的原因——只读 `PluginApi` 的字段列表（审计说「我在 `PluginApi` 里
没看到直接的能力」）会得出「做不到」的错误结论。

---

## 3. 审计指出的 3 处表述不精确 ✅ 全部接受

### 3.1 「diff 面实测」的表述漏了「删测试」

审计说我说「`lib.rs` 的差异已缩到引导调用 + 改名 + 文档头」，漏了**删除 2 个已迁移的测试**。

**接受。** 准确表述是：

- `lib.rs` 删除了 2 个测试（`guide_env_override_is_tri_state`、
  `active_goal_is_detected_from_the_project_state`），它们在 `guide.rs` 里**被重写**
  （不是简单移动：`guide.rs` 版多测了 `""`（空串）和 `maybe` 两个边界，
  并新增了「注入内容正确」「二次注入跳过」两个测试）
- 净效果：`lib.rs` 从 666 行减到 573 行，`guide.rs` 143 行

我在 `AUDIT-RESPONSE-1.md` §3.4 写的是 diff 文件清单，**没写清「测试是删除+新增
而非移动」**。这确实会让人误以为测试被丢了。

### 3.2 根因不是「清单不全」，是「验证方法有洞」

审计说：

> 说明承认「改名漏了 prompt 注入层和测试夹具」……实际根因是**验证方法有洞**：
> 即使清单是五层六层，只要 e2e 不检查注入文本，下次还是会漏。

**这条批评比我的自我评价更准确，我接受。** 我原来的归因（"改名必须覆盖四层"）
把问题说成了**流程问题**，而它其实是**能力问题**——我的 e2e 只能验证「工具能调通」，
**没有能力验证「注入了什么」**。

**审计还指出**：`the_section_names_the_tools_this_package_registers()` 这个测试
**是审计建议的，不是自发发现的**，而我在说明里把它写成「新增回归测试直接锁死复发」，
读起来像主动加固。

**这条也接受。** 我在第一轮确实没有主动想到要测注入内容——是被审计点出来的。
第一轮我的测试全在「字段和函数」层，**没有一个在「用户可见行为」层**。

**已做的补救**（不只是补测试，是补**验证方法**）：

1. 新增 `the_injected_section_is_appended_to_the_base_prompt`——
   断言注入后**保留 base prompt** 且**追加新段**，这测的是行为不是字段
2. 新增 e2e：让模型**逐字引用**自己 system prompt 里的引导段落，
   从外部验证注入内容（见 §4）

### 3.3 「53 passed」不含 e2e，且修复后未重跑 e2e

审计说：

> 说明未提及。P0 改的是 prompt 注入，恰恰需要 e2e 验证——如果没重跑，那就是改了却没验证。

**这条我完全接受。** 我在第一轮修了 P0（改注入文本）之后，
**只重跑了单测，没有重跑 e2e 验证注入内容**。这是实质性的验证缺失。

**已补跑**，见 §4。

---

## 4. e2e 复验（审计 P1 的要求）

审计建议：「这次要检查 prompt 内容，不只看工具能否调通」。已做。

### 4.1 单装：验证注入文本逐字正确

```bash
rpi -p --no-session -e "$SA" --provider magpie --model volcengine/deepseek-v4.1-flash \
  "What orchestration guidance was injected into your system prompt? Quote the section
   heading and the first two numbered steps VERBATIM, exactly as they appear."
```

实测输出：

```
The section is:

## Super-agent orchestration

1. `superagent_plan` with action=set: write the task graph as nodes, each with
   an `id`, a self-contained `task`, and its `deps`.
2. `superagent_dispatch`: run the current ready wave. Nodes whose dependencies
   are done run in parallel (up to `maxParallel`).
```

**注入的 heading 和正文逐字正确，且指向本包真实注册的工具。**

### 4.2 共存：验证不再双注入

见 §1.4 的四场景表格。**这是本轮的核心验证。**

### 4.3 单测与 clippy

```
$ cargo test -p rpi-superagent
test result: ok. 54 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ cargo clippy -p rpi-superagent
（无 warning、无 error）
```

54 = 53（第一轮后）+ 1（`the_neighbour_is_detected_from_the_sessions_tools`）。

---

## 5. 本轮改动的文件清单

| 文件 | 改动 | 对应审计项 |
| --- | --- | --- |
| `src/host.rs` | 新增 `registered_tool_names()`：`GetExecutionContext` 读会话 active tools；`RuntimeFns` 加 `user_data` | 新缺陷（探测） |
| `src/guide.rs` | 新增 `NEIGHBOUR_TOOLS` + `neighbour_is_installed()`；`guide_enabled()` 加让位判断；新增 1 个测试 | 新缺陷 |
| `src/guide.rs` | 新增 `the_injected_section_is_appended_to_the_base_prompt` | §3.2 验证方法 |
| `README.md` | 「Relationship to `rpi-orchestrator`」改为**顶部醒目警告**「Do not install both」+ `rpi uninstall` 命令 + 说明自动让位行为 | 审计「README 应有醒目警告」 |
| `DESIGN-SUPERAGENT.md` | §2.2 重写（共存策略 + 技术约束）；§9 风险表加 15/16 两条 | 文档一致性 |

**未改动**：`packages/rpi-orchestrator/**` 一行未动；`graph.rs` / `dispatch.rs` /
`plan.rs` / `spawn.rs` / `tool_schema.rs` / `subagent.rs` / `control.rs` / `kit.rs`
本轮均未动。

---

## 6. 我承认的疏漏（第二轮）

第一轮我承认「改名不彻底」，但归因错了。第二轮的准确归因是：

> **我的 e2e 只能验证「工具能调通」，没有能力验证「注入了什么」。**
> 这不是清单问题，是验证能力问题——所以第一次审计能靠读代码发现 P0，
> 而我自己跑了 4 轮 e2e 都没发现。

补的不是一个测试，是**一条能在 20 分钟内发现这类问题的验证手段**：
让模型逐字引用自己 system prompt 里的段落。

---

## 7. 仍未解决 / 仍待第三次审计确认

| 项 | 状态 |
| --- | --- |
| `catalog/packages.json` 未加入 | **仍未做** —— `rpi install rpi-superagent` 还装不了 |
| **两插件同装的完整行为** | 本轮**已实测**引导部分；但**未实测**模型在同装时到底调哪套工具（工具层仍同时可用） |
| `RPI_SUPERAGENT_GUIDE=0` 实机验证 | **已实测**（单装时确实不注入） |
| active goal 自动开启 | **已实测**（两插件都读它，是双注入的触发条件） |
| Domain 层 | **完全未开始** —— `Source`/`Trigger`/`Thread`/`Eval`/`Mandate`/`Ticket`/`Subscription` 全是设计 |
| `rpi-durable` / `rpi-harness/runtime.rs` | **仍未读** —— 阶段 A 的前置 |
| 上游同步 | 当前基于 `da0a044`（upstream/main #186） |
| **探测的失败模式** | 宿主若答不出 `GetExecutionContext`，`registered_tool_names()` 返回空 → **按「邻居不存在」处理，照常注入**。这在「宿主不支持该 action」时会退化成双注入。**未实测此降级路径** |

### 7.1 一处需要第三次审计判断的设计取舍

让位判断是**静默**的：用户同装两个插件时，superagent 不会说「我检测到你旁边有
orchestrator 所以我让位」，只是**不注入引导**。

- 好处：模型不会被两套指令干扰
- 坏处：用户可能困惑「为什么 superagent 的引导没出现」

**当前选择是静默**。如果审计认为应当在工具返回值里给用户一个提示
（例如 `superagent_status` 报告「检测到 rpi-orchestrator，已让位」），
这是个几行的改动。

---

## 8. 请第三次审计重点关注

1. **`neighbour_is_installed()` 的判定是否可能误判**：
   `plan` / `dispatch` / `subagent` 这三个名字**会不会被别的插件占用**？
   （我查了 `rpi-package/packages` 全部 13 个包，只有 orchestrator 用这三个名字）
2. **让位时机是否正确**：探测发生在 `before_agent_start`，此时 `GetExecutionContext`
   返回的 `tools` 是否已经是**最终**工具集？（含本包自己注册的 `superagent_*`？
   会不会有竞态？）
3. **`registered_tool_names()` 的降级路径**：宿主答不出时返回空 → 照常注入 →
   退化成双注入。这个降级是否可接受？
4. **`host.rs` 的 `user_data` 用法是否安全**：`user_data` 是注册时的 opaque handle，
   宿主在 `/reload` 后会失效。代码里有没有处理这个失效？
5. **README 的醒目警告是否够**：是否还需要 `rpi-superagent` 在装的时候主动检测并拒绝？
6. **文档与代码是否一致**：README 的共存段落、DESIGN 的 §2.2 / §9

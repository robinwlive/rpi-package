# 第九轮审计的落实说明（供第十次审计）

> 对应审计报告：第八轮审计（M1.5 落实 + 五条取舍）。
> 本轮做完了 **M1.6：model-facing 引导**——我上一轮说「倾向做引导而非 Dash 集成」。
>
> 本轮还发现了一个**宿主 ABI 与发布版 SDK 不匹配**的问题，值得单独说。

---

## 0. 结论先行

| 上一轮列出的缺口 | 状态 |
| --- | --- |
| **`superagent_task` 没有 model-facing 引导** | ✅ **本轮完成** |
| Dash 上看不到任务 | ⬜ 未做（仍待定，见 §6） |
| 重启后 `running` 任务的接管/abandoned | ⬜ 未做（M2） |
| `Graph.task_id` 只在内存 | ⬜ 未做（M2） |
| 任务不能被创建以外的入口驱动 | ⬜ 未做（M3） |

**测试**：64 passed（原 62 + 2）、clippy 干净。

---

## 1. 一个必须先说的发现：发布版 SDK 缺 `register_tool_prompt`

### 1.1 我原本想用 `register_tool_prompt`

宿主的 `PluginApi` **有**这个槽位（`crates/rpi-extensions/src/lib.rs:316`），它正是为「给工具写 model-facing 描述」设计的：
宿主会把它渲染进 system prompt 的工具清单和规则列表（`session.rs:195-240`）。

**而且我读到一个关键事实**（`session.rs:195`）：

```rust
// The text list is a selective summary, not the tool registry.
// In particular, do not repeat extension tool names without a prompt snippet.
```

**没有 `promptSnippet` 的工具根本不进 prompt 的工具清单**——
所以这不是「锦上添花」，而是「模型看不见这个工具」。

### 1.2 但 SDK 0.3.20 没有这个字段

`rpi-package` 的 workspace pin 是 `rpi-plugin-sdk = { version = "=0.3.20" }`，
注释说明了原因：**用发布版，保证 CI 和 `cargo publish` 用同一份源码**。

而 0.3.20 的 `PluginApi` **没有** `register_tool_prompt` 字段。
本地 rpi 源码的 SDK（0.16.0）有。

**而且这不是「字段缺失」那么简单**——两个 `PluginApi` 的**布局不同**：

| | 0.3.20 | 本地 0.16.0 |
| --- | --- | --- |
| `register_tool` 之后 | `register_command` | `register_tool_with_wake` → `register_plugin_lifecycle` → **`register_tool_prompt`** → `register_command` |

**用 0.3.20 编译的插件读不到这个槽位**（偏移不同）。
ABI 版本号相同（都是 4），所以宿主不会拒绝——但插件也拿不到那个函数指针。

**结论：在当前 pin 下，`register_tool_prompt` 不可用。**

### 1.3 我没有绕过它

绕过意味着**自己按偏移读结构体**——那是脆弱的、依赖具体宿主版本的写法，
而且违背这个 workspace「用发布版 SDK」的既定纪律。

**我改用了 `register_before_agent_start`**（0.3.20 有，且能改 systemPrompt），
并把 `TOOL_PROMPTS` 那份常量**删掉了**（clippy 报 never used）。

**删掉的理由**：用不上的代码就是负债。它是我为第一条路写的，路径不通就该删。

---

## 2. M1.6 的实现

### 2.1 独立的引导段，与 goal 无关

```rust
const TOOLS_HEADING: &str = "## Super-agent tools";
```

**为什么独立于编排引导**：编排引导（`## Super-agent orchestration`）受 active goal 控制，
因为它讲的是「怎么用 plan/dispatch 做多步工作」——只在有目标时相关。

但**工具引导必须无条件注入**：工具是**已经注册**的，模型看得见它们的 schema，
却不知道它们是一套、也不知道什么时候该用哪个。
一个模型从没被告知过的工具，就是一个它不会用的工具。

### 2.2 注入时机

`before_agent_start` 在**每轮有 user message 时**触发（`provider_hooks.rs:112`
的 `latest_user_prompt`），与 goal 无关——正是需要的时机。

### 2.3 幂等

按 heading 去重（与编排段同一手法）：

```rust
pub fn apply_tools_to_prompt(base: &str) -> Option<String> {
    if base.contains(TOOLS_HEADING) { return None; }
    Some(format!("{base}\n\n{TOOLS_SECTION}"))
}
```

### 2.4 两段独立注入的顺序

```rust
let mut next = base.to_string();
let mut changed = false;
if let Some(tools) = crate::guide::apply_tools_to_prompt(&next) { next = tools; changed = true; }
if crate::guide::guide_enabled() {
    if let Some(guide) = crate::guide::apply_to_prompt(&next) { next = guide; changed = true; }
}
```

编排段在 tools 段之后——**先说「有哪些工具、什么时候用」，再说「怎么编排」**，
读起来是从具体到抽象。

---

## 3. e2e 验证

### 3.1 有无 goal 的两种情况

| 目录 | 注入的 heading |
| --- | --- |
| 有 `.rpi/goals.json`（active） | `## Super-agent tools` + `## Super-agent orchestration` |
| 无 goal | **只有** `## Super-agent tools` |

### 3.2 幂等（两轮对话后）

```
第 1 轮模型答：1
第 2 轮模型答：1
```

**`## Super-agent tools` 仍只出现 1 次。**

### 3.3 引导真的改变了模型行为（这是关键）

**用例一**：「I need to ship a release. It's a multi-step job: build it, then test it.
It may take a while and I might come back to it later. Please start.」

模型的反应：

> **Tracked as a durable task** (`release-taskuse`, stage `needsPerson`) so it survives you
> coming back later — I or a later session can pick it up from the task list rather than
> re-discovering this.

**它自己判断这需要跨重启，主动建了 task**，还理解了「回头再来」的语义。

（它还拒绝在空目录里跑无意义的 plan——"build" and "test" against an empty directory would
produce two green checkmarks and a shipped nothing。这是**正确判断**，不是失败。）

**用例二**：「Two independent things: (a) write A into a.txt, (b) write B into b.txt.
This is a job I might come back to later, so track it. Do the work.」

```
Done. Both files written in parallel and verified.

Tracked as task `two-file-writes` (stage: **done**), so if you come back later it shows up
via `superagent_task(action=list)`.
```

**完整闭环**：建 task → plan+dispatch 并行 → **自动记为 done**。模型全程没手动同步过。

### 3.4 回归

不带引导的原有行为不变：`plan` + `dispatch` 两个节点并行、验收检查全过。

---

## 4. 引导文本

```
## Super-agent tools

These four tools work together. Use them in this order:

- `superagent_plan` — author a task DAG: nodes, dependencies, acceptance checks.
- `superagent_dispatch` — run one ready wave of it in parallel child processes.
- `superagent_delegate` — run one bounded task in a child process, when the work
  has no dependencies and needs no scheduling.
- `superagent_task` — track a long task across restarts: create it, record its
  state, list them.

When to use which:

- Work with several steps, or steps that can run in parallel → `superagent_plan`
  then `superagent_dispatch`.
- One self-contained task → `superagent_delegate`.
- Work that should outlive one turn, or that you need to pick up after a
  restart → also use `superagent_task`: create it, and pass its `taskId` to
  `superagent_plan`, so dispatch records its progress without you updating it
  by hand. Check `superagent_task` with action=list at the start of a session
  to see what was already in flight.

A node that exits 0 may still have hit its turn cap, so declare `expectFile` or
`verify` acceptance checks and let dispatch judge the node.
```

**「When to use which」是这个段落的重点**——单个工具的 description 说不了「这套工具怎么配合」。

---

## 5. 本轮改动的文件清单

| 文件 | 改动 | 行数 |
|---|---|---|
| `src/guide.rs` | 新增 `TOOLS_HEADING` / `TOOLS_SECTION` / `apply_tools_to_prompt`；新增 2 个测试 | +55 |
| `src/lib.rs` | `on_before_agent_start` 改为两段独立注入 | +15 |
| `src/tool_schema.rs` | **删除** `TOOL_PROMPTS`（SDK 不支持该路径，用不上就删） | −70 |
| `00-GOALS-AND-DECISIONS.md` | M1.6 状态 + e2e 记录 | +8 |
| `AUDIT-RESPONSE-9.md` | 本文件 | — |

**测试**：64 passed，clippy 干净。

---

## 6. 仍未做 / 现在的边界

### 6.1 缺口清单（比上一轮少了两条）

| 缺口 | 状态 |
|---|---|
| ~~`superagent_task` 无 model-facing 引导~~ | ✅ **已解决** |
| **Dash 上看不到任务** | ⬜ 未做。目标陈述第 4 条的「你能看到它」目前只在工具输出和 prompt 里 |
| **重启后 `running` 任务的接管/abandoned** | ⬜ M2 |
| **`Graph.task_id` 只在内存** | ⬜ M2 |
| **任务不能被创建以外的入口驱动** | ⬜ M3 |
| **`superagent_task` 的 `nodes` 字段没在渲染里显示** | ⬜ 小事。e2e 里模型自己发现了这个（「the list view shows stage + error but not the nodes」） |

### 6.2 需要第十轮审计判断的取舍

| # | 取舍 | 我的选择 |
| --- | --- | --- |
| 1 | **SDK pin 冲突要不要解决** | 倾向**暂不动**。`register_tool_prompt` 是「更好的机制」，但现在 `before_agent_start` 已经能达到目的。**但如果上游愿意发新 SDK 版本，这个 pin 该升**——它能省掉一整段注入逻辑 |
| 2 | **工具引导无条件注入是否太吵** | 倾向不吵。它只出现一次，且没有它模型根本不会用这些工具。**但如果实测发现模型因此过度使用 task 工具，就要收窄** |
| 3 | **引导里要不要写「不要滥用 task」** | 倾向**不写**。先看实测行为，过度使用再补 |
| 4 | **下一件做 Dash 集成还是 M2** | 倾向**M2**（重启接管）——它是目标陈述第 4 条的直接兑现，而 Dash 集成是展示层 |
| 5 | **`nodes` 字段要不要显示** | 倾向显示——它是「任务进行到第几步」的唯一线索 |

### 6.3 一处我需要第十轮复核的实现判断

**引导的注入位置**：我用 `before_agent_start`（改 systemPrompt），
而不是 `register_tool_prompt`（进工具清单）。

**代价**：工具名不会出现在 prompt 的「工具清单」段里（那段会列出有 snippet 的工具）。
但**工具的 schema 仍然完整发给模型**——实测模型能列出全部四个 superagent 工具，
所以「模型看不见工具」不是问题。

**收益**：`before_agent_start` 在 0.3.20 就有，**不依赖 SDK 升级**。

**代价二**：这段引导是**独立的 markdown 段**，不是宿主渲染的工具清单格式——
格式上不如原生清单统一。

**请第十轮审计判断**：这个取舍对吗？特别是——是否值得为「原生工具清单」去推动 SDK pin 升级。

---

## 7. 请第十轮审计重点看

1. **`src/guide.rs` 的 `TOOLS_SECTION`**——文本本身写得好不好（§4）
2. **无条件注入是否合理**（§6.2 第 2 条）
3. **SDK pin 的处理**（§6.2 第 1 条 + §6.3）——这是我最想被质疑的一处
4. **`on_before_agent_start` 的两段注入顺序**（§2.4）
5. **§6.1 的缺口清单**——下一件该做哪个
6. **§6.2 的五个取舍**

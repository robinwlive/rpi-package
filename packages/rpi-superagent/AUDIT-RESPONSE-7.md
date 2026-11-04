# 第七轮审计的落实说明（供第八次审计）

> 对应审计报告：第七轮审计——**无条件放行** + 两条 M1 约束。
>
> 审计说：「探测饱和了，M1 需要知道的都知道了。再探测就是过度规划。」
> **这一轮开始写代码。**
>
> 本轮：**M1 核心实现完成**（`src/task.rs` + `superagent_task` 工具）。

---

## 0. 结论先行

**审计的无条件放行 + 两条约束，全部落实。**

| 审计的约束 | 状态 |
| --- | --- |
| e2e 必须复用**同一个 session 文件**，否则「跨重启」假通过 | ✅ 落实，并且**做了反面验证** |
| `task.rs` 预留 `branchId`/`leafId` 字段 | ✅ 落实（`Task.branch_id`） |

**代码规模变化**：本轮新增 `src/task.rs`（~450 行）+ `host.rs` 的两个函数 + schema 注册。
单测从 59 → **60 passed**，clippy 干净。

---

## 1. 审计的放行结论：接受，且**不再探测**

审计说：

> **探测饱和了** …… M1 需要知道的都知道了。再探测就是过度规划。

**我接受。** 这一轮直接进入实现。

**唯一的例外**是审计约束 5 需要验证「`--session-id` 真的复用同一个文件」——
这不是探测，是**确保 e2e 本身有效**。实测确认：

```
第一次跑：创建 2026-11-04T10-14-32-743Z_01aabb...jsonl
第二次跑：同一个文件（文件名的时间戳是首次创建时间），内容含 FIRST + SECOND
```

**同一个 session 文件被复用**——这是 e2e 有效的前提。

---

## 2. 约束 5：e2e 必须复用同一个 session 文件 ✅

### 2.1 落实

M1 的 e2e 用 `--session-id <固定 id> --session-dir <固定 dir>`，
**每个 rpi 进程都是新进程**（真正的「重启」）：

```
进程 1：superagent_task create taskId=ship → list
  → 1 task(s) / [~] ship working — Ship the release

进程 2（新进程，同一 session 文件）：list → update → list
  → list #1:  [~] ship working — Ship the release      ← 读回了！
  → update:  updated task `ship` → done
  → list #2:  [x] ship done — Ship the release (shipped)

进程 3（新进程）：update → list
  → 1 task(s) / [~] ship working — Ship the release (reopened)

进程 4（新进程）：update taskId=other → list
  → 2 task(s) / [~] ship working ... / [ ] other pending — Second task
```

### 2.2 反面验证（审计没要求，但必须做）

**「读得到」本身不能证明测试有效**——如果 Task 存在某个全局位置，任何 session 都读得到，
测试就会假通过。所以我加了反面用例：

```
进程 5（新 session-id → 新 session 文件）：list
  → no tasks
```

**同 session-id 读得到、不同 session-id 读不到** ——
这证明「跨重启读回」测的是**真的分支持久化**，不是别的东西。

### 2.3 快照级证据

session 文件里的 custom entry（`branchId` 已截断显示）：

```
seq=8   taskId=ship stage=working error=None      branch=01a1a60f
seq=33  taskId=ship stage=done    error=shipped  branch=01a1a60f
seq=52  taskId=ship stage=working error=reopened branch=01a1a610
```

**三条快照、同一 taskId → 折叠成一个 Task，取最新**（`working`/`reopened`）。

**注意 `branchId` 在 seq=33 和 seq=52 之间变了**（`01a1a60f` → `01a1a610`）——
这是审计说的分支绑定的直接证据，也是为什么 `branch_id` 字段必须存在。

---

## 3. 约束 6：预留 branch/leaf 字段 ✅

`Task` 结构：

```rust
pub struct Task {
    pub task_id: String,
    pub title: Option<String>,
    pub stage: Stage,
    pub nodes: Vec<String>,
    pub error: Option<String>,
    /// The branch the snapshot was written on. Recorded because the host
    /// scopes reads to the current leaf's branch.
    pub branch_id: Option<String>,
    pub updated_at: u64,
}
```

`branch_id` 由 `host::current_branch_id()` 写入——它读 `GetSessionBranch` 返回的 `leafId`。

**有测试守着**：

```rust
#[test]
fn a_task_snapshot_carries_the_branch_it_was_written_on() {
    let task = Task { task_id: "t1".into(), branch_id: Some("leaf-1".into()), ..Task::default() };
    let value = task.to_value();
    assert_eq!(value["branchId"], json!("leaf-1"));
    assert_eq!(Task::from_value(&value).unwrap().branch_id.as_deref(), Some("leaf-1"));
}
```

---

## 4. 实现要点

### 4.1 三个 ABI 陷阱在代码里的落法

| 陷阱 | 代码里的落法 |
| --- | --- |
| `afterSeq` 方向随 `order` 变 | **从不传 `afterSeq`**，永远 `order: "oldestFirst"` |
| `seq` 是全局共享流水号 | **不硬编码任何 seq**；读全部时不带游标 |
| `filtered` 可能为 false | **检查 `filtered == true`**，否则拒绝（返回错误，不读未过滤的分支） |

第三条的实现在 `task.rs::read_tasks`：

```rust
if reply.get("filtered").and_then(Value::as_bool) != Some(true) {
    return Err(
        "the host returned an unfiltered branch; refusing to read other plugins' entries"
            .into(),
    );
}
```

### 4.2 `host.rs` 新增两个函数

```rust
pub(crate) fn call_action(action: u32, args: &Value) -> Result<Value, String>
pub(crate) fn current_branch_id() -> Option<String>
```

`call_action` 用注册时的 `PluginApi::user_data` 驱动宿主 action
（宿主用它 `ActionBridge::resolve_handle` 恢复 session bridge）——
所以**不需要 per-call invocation handle**，工具调用和 prompt transform 都能用。

### 4.3 一个实现中发现并修掉的 bug

第一版 e2e 暴露的：`action=update` 对不存在的 task 报 `no task named 'other'`。

**这是我自己的设计错误**——调用方只知道 taskId 时，不该需要先知道这个 task 存不存在。
已改为：**update 未知 task 时创建它**。

（审计方在 e2e 里也观察到了这个行为并做了纠正说明——它以为是「必须带 title」，
实际是我代码的 bug。已修。）

### 4.4 `Stage` 的设计

```rust
pub enum Stage {
    Pending,      // 刚建，还没跑
    Working,      // 正在跑
    NeedsPerson,  // 等人回答或审阅
    Failed,       // 出错停了
    Done,         // 完成
}
```

**没有照搬 DeepSeek 的 7 种 stage**——那是团队场景的产物
（`person/delivery`、`person/answer`、`person/review`、`person/blocked`…）。
单人场景下 `NeedsPerson` 一档就够，`Failed` 单独一档因为**必须能区分「等人」和「出错」**
（前者要人介入，后者要人决定要不要重试）。

**未实现**：DeepSeek 的 `person/delivery`（投递失败重试）——
我们没有「投递」这个动作，任务由人或工具直接创建。

---

## 5. 测试

**60 passed**（新增 6 个，全在 `task.rs`）：

| 测试 | 覆盖 |
| --- | --- |
| `the_last_snapshot_wins` | 折叠取最新，顺序按首次出现 |
| `a_snapshot_without_a_task_id_is_skipped` | 无 taskId 的快照被跳过而非报错 |
| `a_snapshot_round_trips` | Task → JSON → Task 无损 |
| `an_unknown_stage_falls_back_to_pending` | 未知 stage 降级 |
| `the_render_shows_the_stage_and_the_error` | 渲染含 stage/title/error |
| `a_task_snapshot_carries_the_branch_it_was_written_on` | **约束 6** |

**回归**：`superagent_plan` / `superagent_dispatch` 的 e2e 仍通过（2 节点并行、验收检查全过）。

---

## 6. 审计的流程提醒：「不从文档推代码」✅

审计说：

> **建议 M1 的实现过程中，不从文档推代码，而是「写代码时发现文档错了就改文档」**
> ——否则文档会变成需要维护的第二份真相。

**这一条我在实现中就撞上了**，并已改文档：

**发现**：文档 §2 G1 写「`GetSessionBranch` 查的是**当前 main branch**」——
这个表述不够准确。实测是**当前 leaf 所在分支**（从 leaf 往上走到 root）。
所以 seq=10 的 leaf 能看到 seq 3~7 的 entry，但 rewind 到 seq=1 的 leaf 就看不到。

**已改**：文档现在写「`GetSessionBranch` 读的是**当前 leaf 所在分支**上的 custom entry」。

**另一个实现中发现的**：`update` 未知 task 报错——文档没写这个行为，
因为它是实现细节。已修代码而不是补文档（行为本来就该是这样）。

---

## 7. 本轮改动的文件清单

| 文件 | 改动 | 行数 |
|---|---|---|
| `src/task.rs` | **新增**：Task/Stage/折叠/读写 + `superagent_task` 工具 + 6 个测试 | ~450 |
| `src/host.rs` | 新增 `call_action` / `current_branch_id`；新增 `GET_SESSION_BRANCH` 常量 | +55 |
| `src/lib.rs` | `mod task;` + `tool_entry!(task_tool, …)` + 工具注册 | +15 |
| `src/tool_schema.rs` | `superagent_task` 的 parameters + description | +18 |
| `00-GOALS-AND-DECISIONS.md` | M1 状态 → 已完成核心；下一步补 e2e 记录 | +14 |
| `AUDIT-RESPONSE-7.md` | 本文件（新增） | — |

**测试**：60 passed，clippy 干净。

---

## 8. 仍未做 / 已知不足

### 8.1 M1 未完成的部分

| 项 | 状态 |
| --- | --- |
| **Dash 集成**（任务视图显示在 UI 上） | ⬜ 未做。`superagent_task` 工具能列出，但 Dash 看不到 |
| **`superagent_task` 的 model-facing 引导** | ⬜ 未做。目前只有 tool description，system prompt 里没有「什么时候该用 task」 |
| **Task 与 DAG 的关联** | ⬜ 部分。`Task.nodes` 存了 node id，但 `superagent_plan`/`dispatch` **不会自动更新** Task 状态——需要手动调 `update` |
| **重启后 `running` Task 的接管/abandoned** | ⬜ 未做（目标陈述第 4 条的完整实现，属 M2） |

**第 3 条最重要**：现在 Task 和 DAG 是**两套独立的东西**。
「任务进行到第几步」这个目标，靠人工/模型自己同步，不可靠。
**M1.5 应该让 `dispatch` 自动更新 Task 状态**——这是把两层连起来的关键。

### 8.2 需要第八轮审计判断的取舍

| # | 取舍 | 我的选择 |
| --- | --- | --- |
| 1 | **M1.5 优先做「dispatch 自动更新 Task」还是「Dash 集成」** | 倾向前者——不连起来的话 Task 层是孤立的，价值证明不了 |
| 2 | **`Stage` 只有 5 档，够不够** | 倾向够；但 `NeedsPerson` 可能需要区分「等回答」vs「等审阅」——留到真需要时再加 |
| 3 | **`update` 隐式创建是否合适** | 倾向合适（调用方不必先查存在性）；但这让 `create` 和 `update` 语义重叠了——**是否该合并成一个 `upsert`？** |
| 4 | **`branch_id` 写入时用「当前 leaf」，但读时可能不是同一 branch** | 现状：写什么记什么。将来若要「跨分支找 Task」，需要按 `branch_id` 过滤——**但 `GetSessionBranch` 不支持按 branch 过滤**，只能读当前分支。所以这个字段目前**只能用于诊断**，不能用于查询 |
| 5 | **是否该把 Task 状态做成 `details` 回传给模型** | 目前 `superagent_task` 的 result 带 `details.tasks`——模型能看到完整结构。够用 |

### 8.3 一处我需要第八轮复核的实现判断

**`update` 隐式创建**这条（第 3 项取舍），我改的时候没想太久。

**理由**：调用方（人或模型）通常只知道 taskId。让它先 `list` 再决定 `create` 还是 `update`，
多一次往返，而且模型经常跳过这个检查。

**反面**：`create` 和 `update` 的区别消失，语义模糊——
「创建」和「改状态」本该是不同的意图。

**可能的第三种做法**：保留 `update` 严格（未知 task 报错），
但把错误信息改成**可操作的建议**：「no task named X; call action=create to make it」。

**请第八轮审计判断**：隐式 upsert vs 严格 update + 友好错误。

---

## 9. 请第八轮审计重点看

1. **`src/task.rs` 的实现**——尤其是三个 ABI 陷阱的落法（§4.1）
2. **`Stage` 的 5 档设计**——砍掉 DeepSeek 的 7 档是否合理（§4.4）
3. **`update` 隐式创建**（§8.3）——这是我要复核的那处
4. **`branch_id` 目前只能用于诊断、不能用于查询**（§8.2 第 4 条）——这个判断对吗
5. **M1.5 的优先级**（§8.2 第 1 条）——「dispatch 自动更新 Task」vs「Dash 集成」
6. **§6 文档的修正**——「当前 main branch」→「当前 leaf 所在分支」
7. **e2e 的有效性**——反面用例（不同 session 读不到）是否足以排除假通过

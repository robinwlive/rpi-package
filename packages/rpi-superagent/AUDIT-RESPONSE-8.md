# 第八轮审计的落实说明（供第九次审计）

> 对应审计报告：第七轮审计（无条件放行 + 两条 M1 约束）。
> 上一份响应见 [`AUDIT-RESPONSE-7.md`](AUDIT-RESPONSE-7.md)，那份已包含 M1 核心实现。
>
> **本轮做完了 M1.5：把 Task 与 Graph 连起来。**
> 这是我上一轮自己指出的缺口——「Task 和 DAG 现在是两套独立的东西，
> 靠人工/模型同步，不可靠」。

---

## 0. 结论先行

| 上一轮承诺 | 状态 |
| --- | --- |
| M1 核心（`task.rs` + `superagent_task`） | ✅ 已在 AUDIT-RESPONSE-7 交付 |
| **M1.5：dispatch 自动更新 Task** | ✅ **本轮完成** |
| 审计的两条约束（e2e 复用同 session 文件 / 预留 branch 字段） | ✅ 已落实（见上一份） |

**规模**：62 passed（原 60 + 2）、clippy 干净。

---

## 1. M1.5 的设计

### 1.1 关联方式：`plan` 传 `taskId`

不新增工具、不改 `dispatch` 的参数——**只给 `plan` 加一个可选的 `taskId`**：

```rust
// plan.rs, action=set
graph.task_id = task_id_of(params);
```

`Graph` 因此多一个字段：

```rust
pub struct Graph {
    pub nodes: Vec<Node>,
    /// The long task this graph belongs to, when it belongs to one.
    pub task_id: Option<String>,
}
```

**为什么是 `plan` 而不是 `dispatch`**：`plan` 是「我要做这组工作」的声明，
`taskId` 属于声明的一部分。放在 `dispatch` 上会让每次派发都要重复传同一个 id。

### 1.2 自动更新：dispatch 结束后写一次快照

```rust
// dispatch.rs
let task_note = match graph.task_id.clone() {
    Some(task_id) => match record_progress(&task_id, graph) {
        Ok(note) => note,
        Err(error) => format!("could not record task `{task_id}`: {error}"),
    },
    None => String::new(),
};
```

**关键设计：记录失败不使 dispatch 失败。**

理由：工作**已经跑了**，这一波的结果才是权威记录；Task 记录是给下一个读者看的便利，
不是事实来源。把它做成硬失败会让「记录辅助信息」变成「派活的依赖」。

### 1.3 状态推导规则（纯逻辑，可测）

```rust
pub fn stage_for_graph(nodes: &[Node]) -> Stage {
    if nodes.is_empty() { return Stage::Pending; }
    if nodes.iter().any(|n| matches!(n.status, Failed | Skipped)) {
        return Stage::Failed;          // 死枝优先于运行中
    }
    if nodes.iter().any(|n| n.status == Running) { return Stage::Working; }
    if nodes.iter().all(|n| n.status == Done) { return Stage::Done; }
    Stage::Pending
}
```

**「死枝优先于运行中」**：一个图有失败的分支时，它**整体就是失败的**，
即使别的节点还在跑。否则 Task 会显示 `working` 而实际已经没救了。

**错误信息**：取第一个死节点的 `error`；没有 error 文本时**退回节点 id**——
宁可说「a 停了」，也不说什么都不说。

> 这个 fallback 是**测试逼出来的**：我第一版写成
> `dead.error.clone().or_else(|| 找 Failed 节点的 id)`，
> 但 `find` 已经定位到第一个死节点、`error` 是 `None` 时整个表达式返回 `None`，
> fallback 分支永远走不到。测试 `the_graph_implies_the_task_error` 抓到了它。

---

## 2. e2e 验证

### 2.1 成功路径

```
1. superagent_task create taskId=work1 title='Long job'
2. superagent_plan set taskId=work1 nodes=[{id:'n1',expectFile:'m1.txt'}]
3. superagent_dispatch
4. superagent_task list
```

dispatch 输出：

```
dispatched 1 node(s)

1/1 done
[x] n1 done

task `work1` → done
```

list 输出：

```
1 task(s)
[x] work1 done — Long job
```

**「task `work1` → done」是 dispatch 自动加的一行**——模型没有手动同步过任何东西。

### 2.2 失败路径

```
1. superagent_plan set taskId=work2 nodes=[{id:'bad',expectFile:'never_created.txt'}]
2. superagent_dispatch
3. superagent_task list
```

dispatch 输出：

```
dispatched 1 node(s)

0/1 done
[!] bad failed — expected file was not produced: never_created.txt

task `work2` → failed
```

list 输出：

```
2 task(s)
[x] work1 done — Long job
[!] work2 failed (expected file was not produced: never_created.txt)
```

**失败原因一路带到了 Task 上**——这正是 M1 最初加 `Node.error` 的目的，
现在它**贯通到 Task 层**了。

### 2.3 跨重启

新进程读回：

```
2 task(s)
[x] work1 done — Long job
[!] work2 failed (expected file was not produced: never_created.txt)
```

session 文件里 **3 条快照 → 2 个 Task**（work1 一条、work2 两条），折叠正确。

### 2.4 回归：不带 `taskId` 时行为不变

```
2/2 done, both acceptance checks passed. No superagent_task used.
```

**`task_id` 为 `None` 时 `record_progress` 根本不被调用**——
没有额外开销，也没有额外输出。

---

## 3. 实现中发现并修掉的问题

### 3.1 `error_for_graph` 的 fallback 永远走不到（测试抓到的）

见 §1.3。已修，测试守着。

### 3.2 clippy 的 `needless_borrow`

`record_progress(&task_id, &graph)` → `record_progress(&task_id, graph)`。
已修，clippy 现在 0 warning。

### 3.3 文档表述修正

上一轮已改一处（「当前 main branch」→「当前 leaf 所在分支」）。
本轮没发现新的文档-代码偏差。

---

## 4. 本轮改动的文件清单

| 文件 | 改动 | 行数 |
|---|---|---|
| `src/graph.rs` | `Graph` 加 `task_id` 字段；`to_details` 带 `taskId` | +12 |
| `src/plan.rs` | `action=set` / `update` 接受 `taskId` | +18 |
| `src/dispatch.rs` | 派发结束后自动记录 Task 进度（失败不影响派发） | +35 |
| `src/task.rs` | `stage_for_graph` / `error_for_graph` + 2 个测试 | +70 |
| `src/tool_schema.rs` | `superagent_plan` 的 `taskId` 参数 + description 补充 | +3 |
| `src/lib.rs` / `src/host.rs` | （上一轮） | — |
| `00-GOALS-AND-DECISIONS.md` | M1.5 状态 + e2e 记录 | +12 |
| `AUDIT-RESPONSE-8.md` | 本文件 | — |

**测试**：62 passed，clippy 干净。

---

## 5. 现在的能力边界（诚实的现状）

### 5.1 已经能做的

| 能力 | 状态 |
| --- | --- |
| 建任务、记状态、跨重启读回 | ✅ |
| 失败原因从节点贯通到任务 | ✅ |
| 计划自动绑定任务，派发自动更新 | ✅ |
| 多任务并存、各自独立折叠 | ✅ |
| 同一任务的完整历史（每步快照） | ✅ |

### 5.2 还做不到的

| 缺口 | 影响 | 属于 |
|---|---|---|
| **重启后 `running` 任务的接管 / abandoned** | 目标陈述第 4 条只做了一半——任务不会丢，但「跑到一半重启」后的归属没人管 | M2 |
| **Dash 上看不到任务** | 目标陈述第 4 条的「你能看到它」目前只在工具输出里，不在 UI 上 | M1.6 |
| **Task 与 DAG 的绑定只在内存里** | `Graph.task_id` 随会话切换即失。重启后 plan 要重新传 `taskId` | M2 |
| **任务不能被创建以外的入口驱动** | 任务由模型调 `superagent_task` 创建；飞书消息 → 自动开任务还没做 | M3 |
| **`superagent_task` 没有 model-facing 引导** | 模型不知道「什么时候该用 task 而不是直接 plan」 | M1.6 |

---

## 6. 需要第九轮审计判断的取舍

| # | 取舍 | 我的选择 |
| --- | --- | --- |
| 1 | **M1.6 先做「Dash 集成」还是「model-facing 引导」** | 倾向**引导**——Dash 集成成本高（跨仓库），而引导是几十行文本，直接决定模型会不会用这个能力 |
| 2 | **`plan` 的 `taskId` 缺失时，dispatch 什么都不记** | 倾向保持——不猜。但是否该在 `plan` 的输出里提示「你可以在 taskId 绑定一个任务」？ |
| 3 | **Task 绑定的持久化**（`Graph.task_id` 落盘） | 倾向 M2 做——和「running 接管」一起设计 |
| 4 | **`Stage` 5 档是否够** | 倾向够，但 `NeedsPerson` 不区分「等回答」vs「等审阅」——真需要时再加 |
| 5 | **上一轮遗留：`update` 隐式创建 vs 严格 update + 友好错误** | 仍未决。我倾向前者，但 `create`/`update` 语义确实重叠了 |

---

## 7. 请第九轮审计重点看

1. **`src/dispatch.rs` 的 `record_progress`**——「记录失败不影响派发」这个取舍对吗
2. **`stage_for_graph` 的规则**——特别是「死枝优先于运行中」
3. **`error_for_graph` 的 fallback**（节点 id）是否够，还是应该更具体
4. **绑定方式**（`plan` 传 `taskId`）是否自然，还是应该有别的入口
5. **§5.2 的五个缺口**——下一个该做哪个
6. **§6 的五个取舍**
7. **文档与代码是否一致**

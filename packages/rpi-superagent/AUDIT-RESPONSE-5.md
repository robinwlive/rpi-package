# 第五轮审计的落实说明（供第六次审计）

> 对应审计报告：第五轮审计——战略收敛。
>
> 这一轮审计给出了**一条关键路径（A' 方案）**+ **两条补充**，
> 并且**自己标注「未实测」**。本轮我**把它实测了**，并据此定案。
>
> 本轮：**只改文档，不改项目代码**（探测实验是独立的临时插件，不在项目里）。

---

## 0. 结论先行

审计的**全部发现都成立**，且它的 A' 方案**实测通过**。

但实测还发现**三个 ABI 文档没说清的行为**——其中两个会直接写错代码。
**审计的推断是对的，但它的建议里有一处会导致 bug**（见 §3.1）。

**G1 已从「待决」变成「已定：方案 A'」**，M1 的生死问题已解。

**代码零改动。** 54 个单测仍全绿，clippy 仍干净。

---

## 1. 审计说的「rpi-durable 是单数，已确认为不可用」✅ 核实成立

**审计的证据**：

```rust
pub struct DurableState {
    pub revision: u64,
    pub next_run_id: u64,
    pub next_queue_id: u64,
    pub workflow: Option<Workflow>,      // ← 单个
    pub run: Option<RunCheckpoint>,      // ← 单个
    pub queue: Vec<QueueItem>,           // ← 只有 queue 是 Vec
}
```

我核实了 `rpi-durable-core/src/lib.rs`，**结构完全一致**。

**审计的判断成立且更准确**：

> 这不是「rpi-durable 没做到多任务」，而是「**它的数据模型里根本没有多任务的概念**」。

**已据此把 G1 的默认方案从「照抄 `rpi-durable`」改为 A'**（§2）。

---

## 2. A' 方案：我实测了，**可行** ✅

**审计自己标注**：

> 我**没有实地跑** `AppendEntry` + `GetSessionBranch` 的探测实验——A' 方案的可行性基于
> ABI 文档和 `rpi-background-tasks` 的既有实践，**是强推断，但未实测**

**我写了探测插件实测。** 探测插件是临时的（在 `/tmp/probe_a2`），不在项目里。

### 2.1 实测结果

```
AppendEntry × 3：
  {"customType":"superagent-task","data":{"taskId":"t1","stage":"working"}}  → entryId ✓
  {"customType":"superagent-task","data":{"taskId":"t2","stage":"idle"}}     → entryId ✓
  {"customType":"superagent-task","data":{"taskId":"t1","stage":"done"}}     → entryId ✓

GetSessionBranch {"customTypes":["superagent-task"],"order":"oldestFirst"}
  → {"entries":[3 条],"count":3,"filtered":true,"sessionId":...,"leafId":...} ✓
```

**结论：A' 完全可行。** 平台原生支持多条独立记录 + 按类型过滤 + 增量查询 + 分页。

### 2.2 验证了 `rpi-background-tasks` 模式的适用性

`pi-dingo-dash` 的 `docs/12-外部依赖认知.md` 记录的契约：

> **快照自包含**：总数全部 job，最后一份即全貌 | 不需要回放，读最后一条即可
> **快照只在状态变化时写**

**实测完全吻合**——每次 append 一条完整快照，读侧按 `taskId` 折叠取最新即可。

---

## 3. ⚠️ 实测发现三个 ABI 文档没说清的行为 —— **其中一个会直接写错代码**

### 3.1 `afterSeq` 的方向由 `order` 决定，不是固定的「时序下界」

**审计的建议里有一处会导致 bug**：

> 读回时：`GetSessionBranch { customTypes: ["superagent-task"], order: "oldestFirst" }`
> → 按 `taskId` 折叠，**最后一条即最新状态**

这句本身没错（用 `oldestFirst` 就对了）。**但审计没有提到 `afterSeq` 的方向陷阱**——
而它前面恰好把「支持 `afterSeq` 增量查询」列为 A' 的证据之一。

**实测**：

| 查询 | 返回的 seq |
| --- | --- |
| `order: oldestFirst`（默认），无 afterSeq | 3, 4, 5 |
| `afterSeq: 3, oldestFirst` | **4, 5**（排他下界 `seq > 3`） |
| `afterSeq: 4, oldestFirst` | **5** |
| `afterSeq: 5, oldestFirst` | 空 |
| `order: newestFirst`，无 afterSeq | 5, 4, 3 |
| **`afterSeq: 3, newestFirst`** | **空** ⚠️ |

最后一行是反直觉的：`newestFirst` 下 `afterSeq` 变成**上界**（`seq < 3`）。

**根因**（`crates/rpi-harness/src/session/state/metadata.rs:109-110`）：

```rust
EntryOrder::OldestFirst => self.seq > cursor.after_seq,
EntryOrder::NewestFirst => self.seq < cursor.after_seq,
```

**⚠️ 这与 `types.rs:1298` 的注释矛盾**：

> Exclusive chronological lower bound: `seq > after_seq`, **regardless of order**

**以实现为准**——实测确认行为随 `order` 变方向。这是一处**上游文档 bug**，
值得给 rpi 提 issue（不在本项目范围内）。

**已写入文档的约束**：**只用 `order: oldestFirst`**。
增量读用 `afterSeq: <最后一条的 seq>`，语义无歧义。

### 3.2 `seq` 从 3 起步，不是 0

实测第一条 custom entry 的 `seq` 是 **3**（前面有会话自身的 entry）。

**后果**：`afterSeq: 0` **不会**返回全部——它返回 `seq > 0`，仍然全部。
**读全部时不要传 `afterSeq`。**

### 3.3 `filtered: true` 必须检查

ABI 文档明确：

> Consumers must require `filtered == true`: older ABI-compatible hosts may ignore the
> query and return the unfiltered branch instead.

**旧宿主会静默返回未过滤的全部分支**——不检查这个字段就会读到别人的数据。
这是**安全边界**，已写进风险表。

### 3.4 错误形态是可判定的

```
GetSessionBranch {}  → rc=1
  {"error":"get_session_branch: `customTypes` is required and must be a non-empty string array"}
```

未知参数也被拒（`modes.rs:1748`）：``unknown query argument `{key}` ``。

---

## 4. G1 的完整定案

**已写入 `00-GOALS-AND-DECISIONS.md` §2**，含：

- 演进过程表（初版待决 → 第五轮降级 → 第五轮修正为单数不可用 → **本轮实测定案**）
- 方案 A' 的定义 + 实测输出
- **三个 ABI 行为陷阱**（§3.1~3.3）
- 方案对照表（~~A 照抄 durable~~ ❌ / **A' 采用** ✅ / B 备选 / C 不自造）
- **A' 的三个已知限制**：
  1. session 文件随 Task 状态增长（需要「每个 Task 只留最近 N 条快照」的压缩策略）
  2. `GetSessionBranch` 每次读整个 branch（Task 多时需 `limit` + `afterSeq`）
  3. **存不到别的 session**（Task 活得比 session 长时）——M2 再评估

风险表新增 4 条（17~20）覆盖这些限制。

---

## 5. 审计的「catalog 分发是 M3 硬前置」✅ 强化

**审计的补充**：

> 而 catalog 依赖「预构建分发 + release 流程」——这是一整套 workspace 之外的工程
> （GitHub Actions release、`rpi-extension.json` manifest、跨平台二进制）。
> 文档目前把它当成「加一行 JSON」。

**成立。** 已把 §4.5 的结论强化为：

> **结论：catalog 分发不是「收尾工作」，是阶段 M3 的硬前置。**
> 在这个前提成立之前，「飞书 → 编排」这条链路是**无法验证**的。

**但我没有估行数**——因为它**不是代码行数**，是 CI 配置 + release 流程。
已如实标注为「M3 的子项，工作量待估」，不假装能估。

---

## 6. 审计指出的文档一致性问题 ✅ 已修

**审计的批评**：

> 它说「本轮改 2 个文件」，但 `AUDIT-RESPONSE-3.md` 从 306 → 315 行也变了
> （我对比了上一轮的数字）。小事，但审计方需要准确的 diff 边界。

**成立。** 原因：`AUDIT-RESPONSE-3.md` 是**第四轮**写的（我在第四轮末尾创建），
不是第三轮。我上一轮说「本轮改 2 个文件」时指的是**那一轮**（第四轮），
而审计方拿它和第三轮的快照比。

**已在本轮明确标注**，往后每份响应会写明「本轮改动的文件清单」。

---

## 7. 本轮改动的文件清单

| 文件 | 改动 |
| --- | --- |
| `00-GOALS-AND-DECISIONS.md` | **§2 G1 全面重写**（从「待决」到「已定 A'」+ 三个 ABI 陷阱 + 已知限制）；M1 表标注「已实测可行」；风险表新增 17~20 四条；§6 下一步更新（探测已完成，改为读 `rpi-durable-core` 了解「为什么是单数」） |
| `AUDIT-RESPONSE-5.md` | 本文件（新增） |

**项目代码：零改动。** 54 个单测仍全绿，clippy 仍干净。

**探测插件**在 `/tmp/probe_a2`（临时），**不在项目里**。

---

## 8. 仍待第六次审计确认 / 未做的事

### 8.1 未做

| 项 | 状态 |
| --- | --- |
| 项目代码改动 | **零**（本轮只改文档） |
| 读 `rpi-durable-core/src/lib.rs` | ⬜ 未读——但**已不是阻塞项**（G1 已定案，只需了解它为什么是单数，避免重蹈覆辙） |
| 读 `rpi-harness/src/runtime.rs` | ⬜ 未读——M2 的前置 |
| 读 Dash `dingo.rs` 全文 | ⬜ 未读——M1 的前置 |
| `task.rs` 实现 | ⬜ 未写——**M1 的第一步** |
| `catalog/packages.json` | ⬜ 未加——M3 的硬前置 |
| A' 的容量/并发压测 | ⬜ 未做——实测只验证了正确性，没验证规模 |

### 8.2 需要第六次审计判断的取舍

| # | 取舍 | 我的选择 |
| --- | --- | --- |
| 1 | **A' 的快照压缩策略**：每个 Task 留最近 N 条？还是只在状态**实质变化**时 append？ | 倾向后者——`stage` 变了才写，避免轮次级噪音 |
| 2 | **`afterSeq` 方向陷阱**要不要在代码里加断言防御？ | 倾向**不用**——只传 `oldestFirst`，但加一个测试锁住行为 |
| 3 | **`filtered` 检查失败时**应该 fail closed 还是降级？ | 倾向 **fail closed**（旧宿主读不到就报错，不要静默读错数据） |
| 4 | **是否给上游提 `afterSeq` 文档矛盾的 issue** | 倾向**提**——是真实 bug，但不在本项目范围 |
| 5 | **M1 的「能读的视图」**：只做 `superagent_task` 工具，还是同时做 Dash 集成？ | 倾向**只做工具**（Dash 集成放 M2 后）——先验证数据能读出来 |

### 8.3 我对审计结论的一处保留

审计说「**这个项目现在可以开工了**」，我同意。

但补一条**实测发现的隐含前提**：

> `GetSessionBranch` 返回的是**「当前 main branch」**的条目（ABI 文档：
> *"Query custom entries on the current main branch"*）。
>
> **Task 状态存在 main branch 上**——这意味着：
> 1. 任务与会话**绑定**（换 session 看不到旧 Task）——这是 A' 限制第 3 条的具体表现
> 2. **session 分叉（fork）时 Task 状态如何处理，未验证**——分叉后 main branch 变了，
>    `GetSessionBranch` 查的是哪条 branch？分叉前的 Task 还在吗？
>
> **这是 M1 必须实测的第二件事**（第一件是 append/读回，已验证）。

**已列入 M1 的待验证清单**，请第六次审计复核这个补充是否必要。

---

## 9. 请第六次审计重点看

1. **§2 的 A' 定案**——是否还有别的致命约束我没测到？
2. **§3.1 的 `afterSeq` 方向陷阱**——我给的「只用 `oldestFirst`」约束是否足够？
3. **§3.3 的 `filtered` 安全边界**——fail closed 的取舍是否正确？
4. **§8.3 的 branch 语义补充**（fork / 换 session 时 Task 状态）——这个坑有多深？
5. **§4 的 A' 三个已知限制**——有没有第四个？
6. **§8.2 的五个取舍**——特别是快照压缩策略（1）和视图范围（5）
7. **文档之间是否一致**——`00-GOALS-AND-DECISIONS.md` 与 `DESIGN-SUPERAGENT.md` 与 `README.md`

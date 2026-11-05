# 00 目标与决策

> **本文回答：做什么、为什么做、什么算做完、以及在写第一行 Domain 代码之前必须先回答的问题。**
>
> 不涉及实现细节（见 [`DESIGN-SUPERAGENT.md`](DESIGN-SUPERAGENT.md)）。
>
> 版本 v0.1 ｜ 状态：决策已定 ｜ 基线：upstream `da0a044`（rpi-package #186）

---

## 1. 目标陈述

### 1.1 一句话

> **把「一个长任务」从「人盯着它跑」变成「人只在需要裁决时才出现」。**

### 1.2 可检验的形式

DeepSeek 的目标陈述写得好，因为它每一条都能被检验。我们照此写：

| 陈述 | 如何检验 | 目标值 |
| --- | --- | --- |
| **它只在被要求时行动** | 未经请求而自行发起的动作数 | 0 |
| **它只在人必须裁决或能用上结果时才出声** | 无人等待时的通知条数 | 0 |
| **它一次只问一个具体问题** | 一次交付里需要人回答的问题数 | ≤ 1 |
| **工作跨重启不丢** | 重启后 N 秒内，每个 `running` 任务都被**重新接管**（有存活心跳）或被**明确标记为 `abandoned`** | 100%（不允许「永远 running」） |
| **人不在时它也能把活干完** | 无人值守跑完一个完整任务的次数 | 达成 |

> **「不降」不是一个可判定的目标值**（审计指出）。一个任务重启前 `running`、
> 重启后仍 `running` 但永远不动，也满足「不降」。
> 所以第四条写成**接管或弃置**二选一：要么有人接手，要么明确标记为 `abandoned`。
> **不允许第三种状态：永远 `running` 的僵尸任务。**
> 这与 `DESIGN-SUPERAGENT.md` §5.2「宿主崩溃 → Ticket 永久 running」是同一件事，
> 但它必须出现在**目标陈述**里——否则验收时会被漏掉。

### 1.3 目标用户与场景

**单用户（Robin 本人），多 project，人在场。**

这与 DeepSeek 原型有**根本差异**，必须写清楚，否则会照搬一个不该照搬的东西：

| 维度 | DeepSeek 原型 | 我们 |
| --- | --- | --- |
| 目标对象 | **一个团队**，一个 GitHub 仓库 | **一个人**，多个 project |
| 触发方式 | **proactive**：监听 GitHub notifications、CI、PR 评论 | **reactive**：人下指令（终端 / Dash / 飞书） |
| 人在场 | 无人值守（面板只显示需要人的） | 多数时候在场，但可能离开 |
| 授权对象 | 团队成员 + bot 名单 + admission 表 | 基本只有自己 |
| 工作单元 | Ticket（可授权、可交接） | 仍是长任务 |

### 1.4 成功标准

**做到什么程度算成功**（可判定）：

1. 在飞书发一条消息，agent 收到后自主拆解成 DAG、并行执行、完成后回报 —— **一条消息端到端**
2. 任务进行到一半，重启 rpi，任务能继续 —— **跨重启**
3. 任务卡住时，人在 Dash 上能看到**卡在哪一步、为什么卡** —— **可诊断**
4. 人离开 30 分钟回来，agent 没发任何打扰性通知，但活干完了 —— **不打扰**

### 1.5 不做行不行

**这是本项目最重要的一节。**

已具备的替代品：

| 已有能力 | 来源 |
| --- | --- |
| DAG 编排（并行、分波、验收、取消、进程树） | `rpi-orchestrator` |
| 飞书收发、autoReply、工具进度回传 | `rpi-im-message` |
| 多会话监控、按紧急度分桶、飞书远程交互 | `pi-dingo-dash` |

**实测结论：`rpi-orchestrator` + `rpi-im-message` 的组合，覆盖了「收消息 → 跑 DAG → 回报」的完整链路。**

所以本项目**不是**为了「能收消息跑 DAG」——那已经有了。真正的增量只有三项：

| 增量 | 为什么现有能力做不到 |
| --- | --- |
| **长程状态** | orchestrator 的图在**进程内存**（`OnceLock<Mutex<GraphState>>`），会话切换即清空。跨重启不存在。 |
| **可诊断的失败** | `plan show` 现在能显示失败原因（本次已修），但没有「一个任务卡在哪一步」的整体视图。 |
| **统一的任务面板** | Dash 现在看的是**会话**，不是**任务**。多会话并行时，看不到「这一个任务进行到第几步」。 |

**不做行不行？** 行——如果你能接受「任务断了就重跑、卡住了靠翻日志」。**不能接受**才做。

### 1.6 因此，砍掉什么

基于 §1.5，**明确砍掉 DeepSeek 原型里对我们价值低的部分**：

| DeepSeek 概念 | 处置 | 理由 |
| --- | --- | --- |
| Source | **保留** | 消息来源确实多（飞书 / 终端 / 未来 GitHub） |
| Trigger | **简化** | 单人场景，「一条消息 = 一个 Trigger」够用，不需要复杂映射 |
| Thread | **复用现成** | 飞书有原生 thread；Dash 的 `router.rs` 已有会话归属逻辑 |
| **Eval（gate + assessment）** | **砍掉** | 单人场景下你发的消息就是要干活的。「这条算不算一个请求」的 LLM 判断是**为团队设计的** |
| **Mandate / Policy** | **砍掉** | 授权对象只有你一个人。「不可变授权」「Policy 版本化」「admission 表」全是为多人团队设计的 |
| Ticket | **保留** | 长任务单元确实需要 |
| Subscription | **推迟** | 依赖 Thread 设计，Thread 复用现成后再定 |
| Outbox / Effects | **推迟** | 消息送达原子性，等真有「必须保证送达」的场景再说 |
| Panel | **复用 Dash** | 不重造 |

**这一刀砍掉约 4,000 行**（`core/` 的 Eval/Policy/Mandate 部分 + `sources/` 的 admission）。

---

## 2. 决策闸门

在写 Domain 代码之前，以下决策必须先落地。

### G0 — 是否需要 Domain 层？ ✅ 已决策

**问题**：`rpi-orchestrator` + `rpi-im-message` 已覆盖完整链路，还要不要新包？

**答案**：要，但**只做最小的那部分**。

**依据**：
- 现有组合**没有**跨重启的长任务状态（orchestrator 的图在内存）
- 现有组合**没有**任务级视图（Dash 看会话，不看任务）
- 但现有组合**已经有**收消息、跑 DAG、回报的全链路

**决策**：**不做完整 Domain 层**，只做两个新概念（见 §3）。

### G2 — 授权要不要在代码里？ ✅ 已决策

**问题**：Mandate/Policy 真的必要吗？

**答案**：**不必要，砍掉。**

**依据**：
- DeepSeek 的 `policy.ts`（242 行）的 gate 逻辑，核心是 `strength: explicit | inferred` 和 admission 表——**这套是为多人团队设计的**
- 审计的判断我核实后**部分成立、部分不成立**：
  - ✅ 成立：`allowed list` / `bot list` / `admission table` / `Policy 版本化` 在单人场景**确实无价值**
  - ⚠️ 不成立：`explicit` vs `inferred` 的**权限强度分级**即使单人也有价值（区分「明确要求」和「我猜你想让我做」）。但这个价值**可以用一个布尔量表达**，不需要 242 行的 Policy 抽象

**决策**：用**一个简单的来源标记**（`explicit` / `inferred`）替代完整 Policy。写进 Ticket 的一个字段。

### G1 — 持久化用什么？ ✅ 已定：方案 A'（AppendEntry + GetSessionBranch 快照）

**这一项经历了三轮演化**，最终由**实测**确定，不是推断。

#### 演进过程

| 轮次 | 结论 | 依据 |
| --- | --- | --- |
| 初版 | 「待决，前置是读 `rpi-durable` 2958 行」 | 不知道能不能用 |
| 第五轮审计 | 降级为「前置阅读 + 默认方案 A（照抄 `rpi-durable`）」 | 「读代码就能回答，不是决策」 |
| 第五轮审计（补充） | **`rpi-durable` 的数据模型是单数，不可用** | 它读了 `rpi-durable-core`：`workflow: Option<Workflow>`、`run: Option<RunCheckpoint>`——**全局只有一个** |
| **本轮实测** | **方案 A' 可行，已验证** | 见下 |

#### 方案 A'：用 `AppendEntry` + `GetSessionBranch` 自建 task 快照

**不是新发明**，是 `rpi-background-tasks` 已验证的模式：
每次状态变化 append 一条**完整快照**，读侧取最后一条。
（这个契约 `pi-dingo-dash` 的 `docs/12-外部依赖认知.md` 已经记录。）

#### 实测验证（本轮做的探测实验）

我写了一个探测插件，用 `runtime_action` 直接调宿主 action，实机验证：

```
AppendEntry × 3：
  {"customType":"superagent-task","data":{"taskId":"t1","stage":"working"}}  → entryId ✓
  {"customType":"superagent-task","data":{"taskId":"t2","stage":"idle"}}     → entryId ✓
  {"customType":"superagent-task","data":{"taskId":"t1","stage":"done"}}     → entryId ✓

GetSessionBranch {"customTypes":["superagent-task"],"order":"oldestFirst"}
  → {"entries":[3 条],"count":3,"filtered":true} ✓
```

**结论：A' 完全可行。** 平台原生支持多条独立记录，无需自建存储。

#### ⚠️ 实测发现两个 ABI 文档没说清的行为（写代码前必须知道）

**（1）`afterSeq` 的方向由 `order` 决定，不是固定的「时序下界」**

实测：

| 查询 | 返回的 seq |
| --- | --- |
| `order: oldestFirst`（默认），无 afterSeq | 3, 4, 5 |
| `afterSeq: 3, oldestFirst` | **4, 5**（排他下界 `seq > 3`） |
| `afterSeq: 4, oldestFirst` | **5** |
| `afterSeq: 5, oldestFirst` | 空 |
| `order: newestFirst`，无 afterSeq | 5, 4, 3 |
| **`afterSeq: 3, newestFirst`** | **空** ⚠️ |

最后一行是反直觉的：`newestFirst` 下 `afterSeq` 变成了**上界**（`seq < 3`）。

**根因**（`crates/rpi-harness/src/session/state/metadata.rs:109-110`）：

```rust
EntryOrder::OldestFirst => self.seq > cursor.after_seq,
EntryOrder::NewestFirst => self.seq < cursor.after_seq,
```

**注意**：这与 `types.rs:1298` 的注释「Exclusive chronological lower bound: `seq > after_seq`,
**regardless of order**」**矛盾**。**以实现为准**——实测确认行为是「随 order 变方向」。
（这是一处上游文档 bug，值得给 rpi 提 issue。）

**对我们的影响**：**只用 `order: oldestFirst`**。
增量读用 `afterSeq: <最后一条的 seq>`，语义清晰无歧义。

**（2）`seq` 是跨条目共享的全局流水号，不保证从固定值开始**

> **审计的修正（成立）**：「seq 从 3 起步」是那次探测的**具体结果**，不是固定语义。
> `state/index.rs:32` 的 `sequence: 0` 起步，`next_sequence() = sequence + 1`
> ——**用户消息占 1、assistant 回复占 2、第一次 append 落在 3**。
> 换成「先跑两轮对话再 append」，第一个 custom entry 的 seq 会是 20 或 50。

**后果**：**绝不能硬编码 `afterSeq`**。必须**先读一遍拿到最大 seq**，再增量查。
（`afterSeq: 0` 恰好等价于「全部」，但那是巧合，不是契约。）

**读全部时不要传 `afterSeq`**——语义无歧义。

**（3）错误形态是可判定的**

```
GetSessionBranch {}  → rc=1
  {"error":"get_session_branch: `customTypes` is required and must be a non-empty string array"}
```

未知参数也会被拒（`modes.rs:1748`）：`unknown query argument \`{key}\``。

**（4）`filtered: true` 必须检查**

ABI 文档明确：「Consumers must require `filtered == true`: older ABI-compatible hosts
may ignore the query and return the unfiltered branch instead.」
**旧宿主会静默返回未过滤的全部分支**——必须检查这个字段，否则会读到别人的数据。

#### 方案对照（最终）

| 方案 | 结论 |
| --- | --- |
| ~~A. 照抄 `rpi-durable`~~ | ❌ **不可用**：数据模型是单数（`Option<Workflow>`），没有多任务概念 |
| **A'. AppendEntry + GetSessionBranch 快照** | ✅ **采用**。实测通过，`rpi-background-tasks` 已验证此模式 |
| B. 独立 JSON/SQLite 文件 | 备选（若 A' 遇到容量或并发问题） |
| C. rpi 的 sqlite session storage | ❌ rpi 运行时实际用 JSONL，sqlite 只是实验性替代——不自造 |

#### 规模实测（本轮做的，审计建议的）

审计担心「append 300 条后 `GetSessionBranch` 会不会慢」。**实测结果：不会。**

| 操作 | 实测耗时 |
| --- | --- |
| append 300 条（**全部写入**） | **5.1 ms** |
| 读回全部 300 条（无 `afterSeq`） | **1.36 ms** |
| 读回全部 300 条 + `limit:10` | **0.11 ms** |
| 增量读（`afterSeq: <max>`） | **0.04 ms** |

**结论：A' 的规模担忧被实测推翻。** 300 条状态变化记录，写入 5ms、读回 1.4ms
——**远不到需要担心的程度**。

**这个结果改变了设计**：不需要「内存驻留 current state，只在关键节点落盘」
的复杂策略。**每次状态变化都 append** 是可以接受的——简单，且天然有历史。

（保留的限制：Task 数量级到**千**级时才需要重测；那时读全部约 5ms 量级，仍可接受。
真正的问题会变成 session 文件**体积**，不是延迟。）

#### A' 的已知限制

1. **容量**：session 文件会随 append 增长。**延迟不是问题**（见上），**体积是**。
   Task 状态变化频繁时需要压缩策略（只保留每个 Task 的最近 N 条快照）。
2. **分支绑定**：**Task 状态在 rewind 后不可见**（实测 count=0）——见下文。
   这是 A' 相对独立文件的**真实代价**，且比「换 session」更常见。
3. **跨 session**：Task 活得比 session 长时，A' 存不到别的 session 里。

#### ⚠️ branch 语义（实测补充，M1 的第二个待验证项）

实测确认 `AppendEntry` 返回 `scopedAppend: true`，且 `append_custom_entry`
（`agent_harness.rs:2186`）走 `message_writes` 锁追加到**当前 leaf**。
`GetSessionBranch` 查的是**当前 leaf 所在分支**上的 custom entry
（从 leaf 往上走到 root）——不是「整个 main lane」。
实现中修正过一次：文档原写「当前 main branch」，实测证明不准确。

**后果**：

| 场景 | 行为 | 状态 |
| --- | --- | --- |
| 换 session | 看不到旧 session 的 Task | **A' 限制第 3 条的具体表现** |
| **session 分叉（fork）** | 分叉后看到的是**新分支**上的 Task；旧分支的 Task 不在当前 branch 视野内 | ✅ 已实测 |
| **压缩（compact）** | **custom entry 完全不受影响**（count 与 seq 都不变） | ✅ 已实测 |
| **导航到别的 leaf（`NavigateTree`）** | **custom entry 全部不可见（count=0）** | ✅ 已实测（见下） |

#### ⚠️ 实测结论：Task 状态是「分支绑定」的，不是「会话绑定」

**实测（rpc 模式，同一会话内）**：

```
写 5 条 custom entry 后：
  before-nav:  count=5  seqs=[3,4,5,6,7]

NavigateTree 到最早的 message entry：
  NAV: outcome=completed  leaf 变了
  after-nav: count=0  ← custom entry 全部不可见
```

**这比「换 session 看不到」更严重**：
`GetSessionBranch` 查的是**当前 leaf 所在分支**上的 custom entry。
一旦导航（rewind）到某个 leaf，**那个分支上看不到任何 Task 状态**。

**另外实测到两个细节**（写代码时要知道）：

1. **导航是幂等的**：已在目标 leaf 时再次导航，返回**同一个 `runId`**、`outcome=completed`，
   不重复执行。所以「导航后再读」和「读后再导航」的结果相同——
   我最初测出「先读一次再导航，count 反而是 5」是这个幂等性造成的假象，
   真实行为是 **count=0**。
2. **compact 不影响 custom entry**：实测 compact 前后 `count=5`、`seq=[3,4,5,6,7]` 完全不变。

**对设计的影响**：

| 影响 | 说明 |
| --- | --- |
| Task 状态**不能**跨分支 | rewind 后 Task「消失」——但**数据没丢**，切回去还能读到 |
| 落地策略 | Task 状态应当**绑定到它所属的分支**；或者干脆接受「rewind 后看不到 Task」这个语义 |
| **是否要独立存储** | 若用户会频繁 rewind 探索，Task 视图会反复消失——这是 A' 的**真实代价**，比「换 session」更常见 |

**→ 这条直接推高了方案 B（独立文件）的价值。** 留到 M1 末尾按实际使用频率再定：
如果 rewind 是罕见操作，A' 够用；如果是常态，需要独立存储。

### G3 — 第一个 Source 是什么？ ✅ 已决策

**问题**：飞书还是 GitHub？

**答案**：**飞书**。

**依据**：
- `rpi-im-message` 已经能用（长连接、autoReply、工具进度回传、Markdown 渲染），**增量最小**
- GitHub 需要 `gh` CLI 包装、PR/issue/commit 的 Trigger 映射，是一整套新活
- 用户的实际工作流里，飞书是**远程入口**，而 GitHub 更可能是**输出目标**而非入口

**推论**：**第一个 Source 可能根本不需要新代码**——`rpi-im-message` 的 autoReply 已经把消息注入了宿主会话，模型看到消息后自己就会调 `superagent_plan` / `superagent_dispatch`。

**这是本轮审计带来的最大发现**（详见 §4）。

---

## 3. 方案（修正后）

### 3.1 概念：只有两个新的

```
Task（长任务单元）          ← 新的唯一领域概念
  └─ Assignment（执行记录）  ← 复用 orchestrator 的 Node
```

**关键修正（审计指出，我采纳）**：~~改造 `graph.rs` 的 `Node` 成 Ticket~~ → **`Node` 保持不变，作为 Assignment 的执行记录；Task 是独立的新模块。**

理由：

| | `Node`（现状） | Task（新的） |
| --- | --- | --- |
| 是什么 | 执行 DAG 的调度单元 | 长任务单元 |
| 状态 | 5 种 status，由**退出码和验收**决定 | 由**任务整体**决定（跑到第几步、卡在哪） |
| 数量 | 每个 node 一个 | 一个 Task 有**多个** Assignment（重试、不同阶段） |
| 持久化 | 进程内存 | **必须落盘** |

把 Task 塞进 Node 会导致 `graph.rs` 的 `ready()` / `propagate_skips()` / `render()` 全部重写——**且难以回退**。分离则 `graph.rs` 几乎不动。

**关系是 1:N，不是继承。**

### 3.2 规模重估

审计实测的 DeepSeek 原型规模（我已独立复核，**完全准确**）：

| 目录 | 行数 | 我们的处置 |
| --- | --- | --- |
| `src/core/` | 2,088 | 砍掉 Eval/Policy/Mandate 后**约 300 行**（Task + Assignment 关联） |
| `src/app/` | 5,292 | **大部分不需要**（见下） |
| `src/executor/` | 838 | **不需要**（orchestrator 的 `spawn.rs` 已覆盖） |
| `src/sources/` | 2,059 | **第一个 Source 不需要**（`rpi-im-message` 已覆盖） |
| `src/infra/` | 347 | 少量需要 |
| `src/client/` | 1,222 | **不需要**（Dash 已是我们的客户端） |
| **合计** | **12,390** | **我们实际需要：约 1,500~2,500 行** |

`src/app/` 里我们真正需要的只有：`store`（持久化）、`passes`（常驻循环）、`outbox`（若需送达保证）、`cards`（Panel 投影）。

**规模预估：1,500~2,500 行生产代码 + 同量级测试**，而不是 12,000 行。

### 3.3 阶段（修正后：从 7 个阶段变成 4 个 + 3 个闸门）

### 3.3.1 成本分账：哪些是继承，哪些是自己付的钱

审计指出「阶段 0 被计入总工作量，但它是沉没成本」。**拆分如下**（行数实测）：

| 部分 | 行数 | 性质 |
| --- | --- | --- |
| 继承的执行内核（`graph`/`dispatch`/`spawn`/`plan`/`subagent`/`kit`/`control`/`host`/`tool_schema`） | **~3,900** | **零成本**（fork 带来），但**是绑定在 fork 上的同步债** |
| 为 Task 服务的新增（改名 + `Node.error` + 渲染 + reset） | **~80** | 本项目自己付的钱 |
| **插件共存成本**（`guide.rs` 抽取 + 邻居探测 + 3 轮审计修复） | **~280** | **纯开销**，与 Task 无关 |

**这个分账的价值**：如果将来要回退，**3,900 行是同步债**（上游改了就得 rebase），
而 ~360 行是我们自己写的、可自由处置。

**注意**：继承的 3,900 行**不是我们的工作量**，但它是**我们的维护负担**——
所以 `DESIGN-SUPERAGENT.md` §1.2 的 rebase 成本估算仍然成立。

### 3.3.2 阶段划分（重排后）

审计建议：「阶段 A 的第一个可交付不是完整 Task 模型，而是**最小可读结构 + 落盘 + Dash 能显示它**」——
**采纳**，这样第一个里程碑就能回答「你到底解决了我什么问题」。

| 里程碑 | 内容 | 产出 | 新增行数 |
| --- | --- | --- | --- |
| **0** | ✅ 已完成 | fork + 改名 + 3 轮审计修复 | ~360（不含继承） |
| **M1** | ✅ **已完成**：`task.rs` + `superagent_task` 工具 + **dispatch 自动更新 Task** | **任务能落盘、重启后能读回、你能看到它** | ~450 |
| **M1.5** | ✅ **已完成**：把 Task 与 Graph 连起来——`plan` 传 `taskId` 即绑定，`dispatch` 自动记录进度 | 不用手动同步两层 | ~120 |
| **M1.6** | ✅ **已完成**：工具引导（`## Super-agent tools` 段），**无条件注入** | 模型知道何时用哪个工具 | ~90 |
| **M2** | ✅ **已完成**：心跳 + 僵死任务标记 `abandoned` + **孤儿子进程清理（独立守护进程）** | 重启后没有「永远 running」的任务，也没有残留进程 | ~600 |
| **M3** | ✅ **已完成（分发）**：`catalog/packages.json` + 打包工具支持 `[[bin]]` 附属文件 | `rpi install rpi-superagent` 能装，守护进程跟着装 | ~80 |
| **M4** | ✅ **已完成（面板）**：Dash 读 `superagent-task` 快照，药丸显示任务状态 | **「你能看到它」兑现了** | ~200（跨仓库） |
| **M5** | ✅ **已完成（飞书桥接）**：话题 = 独立主会话，回复走独立进程 + `--local-only` 只加载 superagent | 飞书一条消息 → 独立会话里建任务、编排、写文件，多个话题互不干扰 | ~250（跨包） |
| **M6** | worktree 隔离 + 打磨 | 并行写安全 | ~300 |

**M1 是第一个「能回答问题」的里程碑**——它把原 A 的一小部分和原 B 的核心合在一起。
**M2 之后才有完整的状态机。**

**总计约 1,700 行新增**（不含继承），且每个里程碑独立交付可用价值。

**与 DeepSeek 原型的规模对照**（原型的 `core/` 里我们需要的部分）：
`model.ts` 353 + `tickets.ts` 423 + `engine.ts` 270 + `assignments.ts` 140 + `stage.ts` 26
= **1,212 行是下限**（审计指出我原来的「core/ 砍后约 300 行」不现实，**正确**）。
加上 `store` / `passes` / `cards` 的对应实现，**1,700 行是合理的量级**。

### 3.4 明确不做（本项目内）

| 不做 | 理由 |
| --- | --- |
| Eval / gate / assessment | §1.6，G2 已决 |
| Mandate / Policy / admission 表 | §1.6，G2 已决 |
| Subscription / 线程直通 | §1.6，Thread 复用现成后再定 |
| Outbox / Effects | §1.6，无送达保证场景 |
| 自建 Source 框架 | G3 已决，`rpi-im-message` 够用 |
| 自建 Panel | `pi-dingo-dash` 已是 |
| 自建 Executor | `spawn.rs` 已覆盖 |
| 跨 Source 身份映射 | DeepSeek 自己都拒绝这个概念 |

---

## 4. 本轮审计带来的最大发现：第一个 Source 可能不需要新代码

审计建议「先读 `rpi-im-message` 的会话路由实现——读完可能发现阶段 C+D 可以被大幅简化甚至省略」。**我读了，结论比审计预期的更激进。**

### 4.1 `rpi-im-message` 已经做到了什么

| 能力 | 实现位置 |
| --- | --- |
| 飞书长连接收消息 | SDK WebSocket，`im.message.receive_v1` |
| **autoReply：消息注入宿主会话** | `SendUserMessage` runtime action（`lib.rs:1102`） |
| 工具调用进度实时回传飞书 | `ReplyProgress` + 消息编辑 |
| Markdown 渲染 | 富文本 `post` 消息 |
| 会话选择（`/new` `/sessions` `/session`） | `sessions.rs` |
| 幂等（at-least-once 去重） | 已处理消息 id 集合 |

### 4.2 这意味着什么

`rpi-im-message` 的 autoReply 走 `SendUserMessage`，把飞书消息**注入宿主正在运行的会话**。模型看到这条消息后，**自己就会决定调 `superagent_plan` / `superagent_dispatch`**。

**所以「飞书 → DAG → 回报」这条链路，rpi-im-message + rpi-superagent 已经能跑，不需要新的 Source 代码。**

### 4.3 缺的只有「长程状态」

对比 DeepSeek 的 Source 抽象，我们**只需要**：

| DeepSeek Source 提供 | 我们需要吗 |
| --- | --- |
| Trigger 持久化 + 幂等 | ✅ 需要（消息要能重放、任务不能重复开） |
| Thread 聚合 | ⚠️ 复用飞书原生 thread + Dash 的 `router.rs` |
| Policy 过滤（谁可以触发） | ❌ 砍掉（§1.6） |
| Channel（写回） | ✅ `rpi-im-message` 已提供 `send` |
| Polling 循环 | ✅ `rpi-im-message` 已有长连接 |

**结论：Source 层我们只需一个「消息 → Task」的桥接**，约 200~300 行。

### 4.4 审计的「已覆盖 80%」这个说法

审计标注这是「从两个包的 README 推断，未经实测」。**我实测了，结论需要修正**：

| 说法 | 实测 |
| --- | --- |
| 「组合已覆盖 80%」 | ✅ **比这更彻底**——autoReply 路径（`SendUserMessage`）下，orchestrator 的工具**本来就可用**，因为消息注入的是**宿主正在跑的会话** |
| 一个可能的坑 | ⚠️ autoReply 的**兜底路径**用 `rpi --print --no-extensions` 起子进程——**那条路径加载不了 orchestrator 的工具**。所以兜底路径下 DAG 能力不可用 |

**这个坑必须写进设计**：autoReply 的两条路径能力不对等，兜底路径只能做单轮对话。

### 4.5 「注入的会话已加载该插件」是隐含前提（审计指出，已核实并写明）

审计指出：

> 我没有独立验证「`SendUserMessage` 注入的会话一定能调用 `superagent_*`」——
> 我确认了注入机制（`lib.rs:1102`），但「注入的会话已加载该插件」这件事取决于用户先装了插件
> 并在该会话中启用，**这是个隐含前提，文档应写明**。

**核实结果**：前提**成立，但有条件**。

- `SendUserMessage` 注入的是 `self.harness()?.lane("main")`——**宿主的主 lane**，
  即加载了全部插件的那个会话（`extensions_actions.rs:737`）
- 但宿主有 `set_active_tools` runtime action，**插件工具可以被按会话关闭**
  （`active_tools` 只保留 `active` 名单里的工具）

**所以前提是**：用户在**该会话所在的项目**里装了 `rpi-superagent`，且该会话没有把
`superagent_*` 从激活工具里移除。

**这与「插件装在全局目录还是项目目录」有关**——`rpi-superagent` 目前只存在于
本仓库的 `packages/`，**尚未进入 `catalog/packages.json`，也就没有 `rpi install`
的正式分发路径**。在这个前提成立之前，「飞书 → 编排」这条链路是**无法验证**的。

**结论：catalog 分发不是「收尾工作」，是阶段 M3 的硬前置。**

---

## 5. 风险清单（修正后）

| # | 风险 | 等级 | 缓解 |
| --- | --- | --- | --- |
| 1 | **没有目标陈述导致做到 60% 不知是否继续** | ~~高~~ | ✅ 已修：§1 的五条可检验陈述 + §1.4 的四条成功标准 |
| 2 | **规模被严重低估**（6 阶段 vs 12,390 行） | ~~高~~ | ✅ 已修：§1.6 砍掉 Eval/Mandate，§3.2 重估为 1,500~2,500 行 |
| 3 | **`Node → Ticket` 是错误技术路径** | ~~高~~ | ✅ 已修：§3.1 改为 Task 独立模块，Node 退化为 Assignment |
| 4 | **缺决策闸门** | ~~高~~ | ✅ 已修：G0/G2/G3 已决，G1 待决且已指明前置 |
| 5 | 漏读 `rpi-im-message` 导致重复造 Source | ~~中~~ | ✅ 已修：§4 已读，Source 层缩到 200~300 行 |
| 6 | 漏读 Dash 的 `classify` 导致重造 Panel 状态映射 | ~~中~~ | ✅ 已修：Dash `Bucket`（Error/Review/Question/Answered/Draft/Running/Waiting/Normal）与 Ticket stage 高度同构，直接映射 |
| 7 | 持久化方案未定 | 中 | G1 待决，前置是读 `rpi-durable` |
| 8 | 跨重启的任务恢复边界未定 | 中 | 读 `rpi-harness/runtime.rs` |
| 9 | autoReply 兜底路径无 DAG 能力 | 低 | 文档说明；生产用 ABI 路径 |
| 10 | 宿主持久化后 Ticket 与 Session 生命周期错配 | 中 | 阶段 A 一并验证 |
| 11 | fork 后上游 rebase 成本 | 中 | 继承的 3,900 行是同步债（§3.3.1）；内核文件与新模块已分离（`guide.rs` 先例） |
| 12 | **「跨重启不丢」原写成「不降」，不可判定** | ~~中~~ | ✅ 已修：改为「接管或标记 abandoned」二选一（§1.2） |
| 13 | **阶段 0 的成本被算进总工作量** | ~~低~~ | ✅ 已修：拆成「继承 3,900（零成本/同步债）」+「自付 ~360」（§3.3.1） |
| 14 | **G1 被当成决策闸门，挡住阶段 A** | ~~中~~ | ✅ 已修：降级为前置阅读，默认方案 A（§2 G1） |
| 15 | **Dash `Bucket` 是会话维度，复用需要新判定链** | 中 | `DingoItem.session: SessionMeta` 强绑定会话，`classify_one` 是会话判定链。任务维度需要**新判定链 + 新 item 类型**，不是简单复用（§3.3.2 M1） |
| 16 | ~~**`catalog` 未分发 → 飞书链路无法验证**~~ | ~~中~~ | ✅ 已修：分发完成（M3）；打包工具已支持 `[[bin]]` 附属文件 |
| 17 | **A' 的 session 文件随 Task 状态增长** | 中 | 只保留每个 Task 最近 N 条快照；读侧按 `taskId` 折叠（§2 G1 限制 1） |
| 18 | **`afterSeq` 方向随 `order` 变，与 types.rs 注释矛盾** | 低 | **只用 `order: oldestFirst`**；实测行为已记录（§2 G1 陷阱 1） |
| 19 | **旧宿主可能忽略查询条件返回未过滤分支** | 中 | **必须检查 `filtered == true`**，否则读到别人的数据（§2 G1 陷阱 3） |
| 20 | **A' 存不到别的 session**（Task 活得比 session 长时） | 中 | M2 再评估；备选是独立文件（方案 B） |
| 21 | **`seq` 被误当成「从 3 起步」而硬编码 `afterSeq`** | 中 | ✅ 已修：文档改为「全局共享流水号，不保证从固定值开始」；必须先读再增量（§2 G1 陷阱 2） |
| 22 | **规模担忧（A' 撑不撑得住高频状态更新）** | ~~中~~ | ✅ **实测推翻**：300 条 append 5ms、读回 1.4ms；不需要内存驻留策略（§2 G1 规模实测） |
| 23 | **rewind 后 Task 状态不可见（分支绑定）** | 中 | 实测 count=0。数据没丢（切回可见）。若 rewind 频繁则需方案 B（§2 G1 分支语义） |

---

## 6. 下一步

**已完成**

- ✅ G1 持久化方案：**A'（AppendEntry + GetSessionBranch 快照），已实测验证**（§2）
- ✅ **M1 核心实现**：`src/task.rs` + `superagent_task` 工具。e2e 验证：
  - 进程 1 建 Task → 落盘到 session branch
  - **进程 2（同 session 文件）读回** → `working` ✅
  - 更新为 `done` → 再读 → `done` ✅
  - 同 taskId 的 3 条快照 → **折叠成 1 个 Task**，取最新 ✅
  - 多 Task 并存 → 各自独立折叠 ✅
  - **不同 session 文件 → `no tasks`** ✅（证明「跨重启」测试不是假通过）
  - 原有 `superagent_plan`/`superagent_dispatch` 回归通过 ✅
- ✅ **M1.5（Task ↔ Graph 打通）**：
  - `superagent_plan` 传 `taskId` 即把图绑定到长任务
  - `superagent_dispatch` 结束后**自动**把图的状态写到任务上
  - e2e：建 task → plan(taskId) → dispatch → list → `[x] work1 done` ✅
  - e2e：失败路径 → dispatch → list → `[!] work2 failed (expected file was not produced: never_created.txt)` ✅
  - e2e：新进程读回两个任务，各自状态正确 ✅
  - 快照数 3 条 → 折叠 2 个 Task ✅
  - 回归：不带 `taskId` 的 plan/dispatch 行为不变 ✅
- ✅ **M1.6（工具引导）**：
  - 新增 `## Super-agent tools` 段，**与 goal 无关**（工具已注册，模型就得知道）
  - e2e：无 goal 的目录 → 只有 tools 段；有 goal → 两段都有 ✅
  - e2e：两轮对话后 `## Super-agent tools` **仍只出现 1 次**（幂等）✅
  - e2e：模型面对「多步、可能回头再来」的需求，**自主创建了 task**（stage `needsPerson`）✅
  - e2e：模型面对「两个独立文件、可能要回头」的请求 → **建 task + plan + dispatch + 自动记为 done** ✅
- ✅ **M2（心跳与接管）**：
  - `Task.updated_at` 用**宿主 entry 的 timestamp**（不是读时的时间）——心跳必须跨重启有意义
  - dispatch **开始时**写一次快照（stage=working），**运行中**每 60s 由独立线程写心跳，**结束时**写最终状态
  - 阈值 30 分钟（= 子进程最长超时 1800s 的 2 倍冗余）
  - e2e：把 entry timestamp 改成 40 分钟前 → 新进程读 → **`[~] long1 abandoned (no progress for 40 minutes)`** ✅
  - e2e：reconcile 结果**持久化**（2 条快照：working + abandoned），再读仍是 abandoned ✅
  - e2e：新建的 fresh working 任务**不被误判** ✅
  - e2e：等人在等（`needsPerson`）的任务**永不 abandoned** ✅
  - **孤儿清理**（实测发现并修掉的真问题）：
    - 实测：rpi 的 `process_group(0)` 只做 `setpgid`，**不建立新会话** → 宿主被 SIGKILL 时
      子进程**存活成孤儿**（`rpi -p` 中途不输出，可能活很久）
    - 任务快照记录子进程 pid；读任务时清理还活着的孤儿
    - e2e：造真孤儿（父退出、子在跑，状态 `S`）→ 读任务 → **状态变 `Z`（已被 kill）** ✅
    - 修正两个 macOS 特有的坑：**zombie 被 `kill -0` 误判为存活**；**跨进程组 kill 报 EPERM**
      （改为直接 kill pid + 递归 kill 后代）✅
  - **独立守护进程**（补上「没人读任务」「会话死了」的缺口）：
    - **不是插件内的线程**——线程随宿主一起死。是一个**独立二进制**
      `rpi-superagent-supervisor`（同包的 `[[bin]]`，与 `rpi-acp` 的 `acp_bridge` 同构）
    - 插件用 `Command::spawn` 启动它，**不用 fork**——宿主是多线程的（tokio），
      fork 后的子进程只能调 async-signal-safe 函数，不够跑一次扫描
    - 守护进程自己 `setsid()` 脱离会话 → 宿主被 SIGKILL 后它仍存活（`PPID=1`）
    - **单例**：`flock` 锁文件，多个 rpi 进程只有一个能当守护进程
    - 它读 session 目录找 `superagent-task` 快照里的 pid，杀掉还活着的孤儿
    - e2e：宿主被 SIGKILL → **守护进程数=1、状态 `Ss`、PPID=1**，孤儿 `S` → `Z` ✅
    - 间隔可用 `RPI_SUPERAGENT_SWEEP_SECS` 覆盖（默认 300 秒）
    - **分发**：rpi 的 `rpi-extension.json` v1 的 `assets` 机制支持带额外文件，
      且 assets **装在库旁边**（`prebuilt_extensions.rs` 测试
      `declared_runtime_assets_install_beside_the_library` 证明）→
      守护进程用 `dladdr` 找到扩展自身路径 → 二进制就在旁边 ✅
  - **修掉一个自己引入的 bug**：`reap_orphans` 会杀掉**本进程正在跑的子进程**
    （宿主可能并行调用 `list` 和 `dispatch`）→ 加「本进程拥有的 pid」白名单，
    测试抓到 `OnceLock` 未初始化导致白名单完全失效 ✅

**M5 已完成**（详见下文「M5 的关键约束」）：全部回复走 `rpi --print`，
`--local-only -e <superagent.dylib>` 只加载编排工具；话题按 `root_id` 分会话、
分工作目录；不加载 im-message 自己（防递归）。

**M5 的关键约束（实测得出，会影响设计）**

飞书消息有两条投递路径，各有不可兼得的取舍：

| | `SendUserMessage`（宿主有 harness 时） | `rpi --print`（兜底 / 显式 `/session`） |
| --- | --- | --- |
| 注入哪 | **宿主当前会话**的 main lane | 独立会话（带话题） |
| superagent 工具 | ✅ 可用 | ❌ 不可用（`--no-extensions`） |
| 话题隔离 | ❌ **不生效** | ✅ 生效 |

**为什么 print 路径不能加载扩展**：`--no-extensions` 是防递归的——
子进程若加载了 `rpi-im-message` 自己，会再起一个飞书长连接，收到消息又 spawn 子进程。

**但有一条两全的路**（已核实）：`--local-only` = 「只加载显式 `-e` 指定的扩展」。
`load_extensions_from_dirs` 在 `--local-only` 时 dirs 为空，但 `args.extension`
仍会传给 `load_session_mixed_guarded`（`session.rs:2726`）。

所以 print 子进程可以用 `-e <superagent.dylib> --local-only`：
只加载 superagent（不递归），工具可用，话题隔离也生效。
**代价**：im-message 必须知道 superagent 的 dylib 路径。

**下一步（M5：飞书桥接）**

飞书一条消息 → 自动开一个长任务 → 回报。

关键设计：`rpi-im-message` 的 autoReply **已经把消息注入宿主会话**，模型看到后
自己会调 `superagent_task`。所以桥接要做的只是**「这条消息该不该开一个任务」的意图识别**，
而不是重造一条投递链路。

前提已满足：分发完成（M3）、面板可见（M4）。

**仍可读的文件（不是阻塞项）**

1. 读 `rpi-durable-core/src/lib.rs` → 了解**它为什么是单数**（避免重蹈覆辙）
2. 读 `rpi-harness/src/runtime.rs` → 定跨重启恢复边界
5. 阶段 A 实现

**明确不在接下来的计划里**：Eval、Mandate、Policy、Subscription、自建 Source、自建 Panel。

---

## 7. 附：本文档所有数字的来源

| 数字 | 来源 | 核实方式 |
| --- | --- | --- |
| 12,390 行生产代码 | DeepSeek 原型 | 本机 `git clone` + `wc -l` 实测（审计方数字**完全准确**） |
| `core/` 2,088 / `app/` 5,292 / `executor/` 838 / `sources/` 2,059 / `infra/` 347 / `client/` 1,222 | 同上 | 同上，逐目录实测一致 |
| 12,981 行测试 | 同上 | 实测（审计说 12,156，接近但不完全一致；测试在 `tests/` 不在 `src/`） |
| `policy.ts` 242 行 | 同上 | 实测 |
| `policy.gate` 返回 `explicit`/`inferred` | 同上 | 读 `src/policy.ts:171` 实测 |
| `rpi-im-message` autoReply 走 `SendUserMessage` | rpi-package | 读 `src/lib.rs:1102` |
| autoReply 兜底用 `--no-extensions` | rpi-package | 读 `src/lib.rs:726` |
| orchestrator 图在进程内存 | rpi-package | 读 `src/lib.rs` 的 `OnceLock<Mutex<GraphState>>` |
| Dash `Bucket` 8 种 | pi-dingo-dash | 读 `crates/core/src/dingo.rs:26` |
| `rpi-codegraph` 8 工具 ≈1588 tokens | rpi-package | 读 schema 实测字符数 |

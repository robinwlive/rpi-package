# 第三次审计的落实说明（供第四次审计）

> 对应审计报告：最终审计——目标 · 方案 · 路径。
>
> 这一轮审计在**战略层**，前三轮在代码层。审计的核心批评是：
> **这个项目还没有一个能检验成败的目标。**
>
> 本轮的落实：**只改文档，不改代码。**

---

## 0. 结论先行

审计的**六条批评我全部接受**，并逐条落实。

其中两条我**核实后确认成立**，一条**部分成立**，两条**在落实过程中发现比审计说的更严重**。

新增文档：[`00-GOALS-AND-DECISIONS.md`](00-GOALS-AND-DECISIONS.md)（325 行）。

**代码零改动**（本轮）。54 个单测仍全绿，clippy 仍干净——因为本轮只碰文档。

---

## 1. 审计的六条批评，逐条落实

### 1.1 🔴「没有目标陈述」→ ✅ 已补

**审计的批评**：

> 当前文档的「动机」无法检验。……没有目标陈述，接下来的三个阶段（A~G）就没有**验收标准**。

**成立。** 已补 `00-GOALS-AND-DECISIONS.md` §1：

**§1.1 一句话目标**

> 把「一个长任务」从「人盯着它跑」变成「人只在需要裁决时才出现」。

**§1.2 五条可检验陈述**（照 DeepSeek 的写法，每条都能被计数）：

| 陈述 | 检验方式 | 目标值 |
| --- | --- | --- |
| 只在被要求时行动 | 未经请求自行发起的动作数 | 0 |
| 只在人必须裁决或能用上结果时才出声 | 无人等待时的通知条数 | 0 |
| 一次只问一个具体问题 | 一次交付里需要人回答的问题数 | ≤ 1 |
| 工作跨重启不丢 | 重启后仍在进行的长任务数 | 不降 |
| 人不在时也能把活干完 | 无人值守跑完一个完整任务的次数 | 达成 |

**§1.4 四条成功标准**（可判定，做到了就是成功）：

1. 飞书发一条消息 → agent 自主拆 DAG、并行执行、回报
2. 任务进行到一半重启 rpi → 任务能继续
3. 任务卡住 → Dash 上能看到卡在哪一步、为什么卡
4. 人离开 30 分钟回来 → agent 没发打扰性通知，但活干完了

### 1.2 🔴「规模严重低估」→ ✅ 已重估

**审计的论据**（我独立复核，**数字完全准确**）：

我把 DeepSeek 原型 clone 到本地实测：

| 目录 | 审计说的 | 我实测 | |
| --- | --- | --- | --- |
| `src/core/` | 2,088 | 2,088 | ✅ |
| `src/app/` | 5,292 | 5,292 | ✅ |
| `src/executor/` | 838 | 838 | ✅ |
| `src/sources/` | 2,059 | 2,059 | ✅ |
| `src/infra/` | 347 | 347 | ✅ |
| `src/client/` | — | 1,222 | 审计漏列 |
| **生产代码合计** | **12,390** | **12,390** | ✅ |

**测试代码**：审计说 12,156，我实测 **12,981**（测试在 `tests/` 目录，不在 `src/`，用 `*.test.ts` 找不到——这是数字差异的原因）。

**§3.2 重估我们的规模**：**1,500~2,500 行**，不是 12,000 行。

逐目录对照表写在 `00-GOALS-AND-DECISIONS.md` §3.2，每个目录都标了「我们要不要」和「为什么不要」。

### 1.3 🔴「阶段 A 的技术路径错了」→ ✅ 已改

**审计的批评**：

> 「改造 `graph.rs` 的 `Node`」是错误的技术路径。……把 `Node` 改造成 Ticket，等于把一个简单的 DAG 节点改造成一个有状态机、有授权、有历史的工作单元——改完之后 `graph.rs` 里的 `ready()`/`propagate_skips()`/`render()` 全部需要重写。

**成立，且审计还主动撤回了自己第一轮写错的那条映射**（它说「这条我作为审计方要收回」）。

**已改**：`DESIGN-SUPERAGENT.md` §1 映射表里，Ticket 那行现在是：

```
| ~~Ticket~~ | ~~`graph.rs` 的 `Node`~~ | — | ❌ **已撤回**：改造 `Node` 是错误技术路径（审计指出，已核实） |
| **Task**（长任务单元） | `task.rs` | — | 🆕 新增，与 `Node` **分离**（1:N，不是继承） |
| Assignment（执行记录） | `graph.rs` 的 `Node` | fork 自 `rpi-orchestrator/src/graph.rs` | ✅ 原样继承（`Node` 本身不变） |
```

`00-GOALS-AND-DECISIONS.md` §3.1 给了理由表：

| | `Node`（现状） | Task（新的） |
| --- | --- | --- |
| 是什么 | 执行 DAG 的调度单元 | 长任务单元 |
| 状态 | 5 种 status，由**退出码和验收**决定 | 由**任务整体**决定 |
| 数量 | 每个 node 一个 | 一个 Task 有**多个** Assignment |
| 持久化 | 进程内存 | **必须落盘** |

### 1.4 🔴「缺决策闸门」→ ✅ 已补

审计建议 G0/G1/G2/G3 四个闸门。**我做了决策，不只是列出来**：

| 闸门 | 审计的疑问 | 我的决策 | 依据 |
| --- | --- | --- | --- |
| **G0** | 真需要 Ticket 域模型吗？ | **要，但只做最小的两个概念** | 见 §1.5：现有组合缺「跨重启状态」和「任务级视图」 |
| **G1** | 持久化用哪个方案？ | ⬜ **待决**（倾向 session-branch custom entry） | 前置是读 `rpi-durable` |
| **G2** | 授权真要在代码里吗？ | **不必要，砍掉** | 见 §1.6 |
| **G3** | 第一个 Source 是飞书还是 GitHub？ | **飞书** | `rpi-im-message` 已可用，增量最小 |

**G2 我核实后做了一处修正**（审计的判断部分成立）：

审计说「Mandate/Policy 是为多人团队设计的，单人场景价值接近零」。

我读了 `src/policy.ts`（242 行）的 `gate()` 实现，发现它有两类东西：

- ✅ **审计说的对**：`allowed list` / `bot list` / `admission table` / `Policy 版本化` / `mandate.strength` 的完整分级——**这些在单人场景确实无价值**
- ⚠️ **审计说得过头**：`strength: 'explicit' | 'inferred'` 这个**权限强度分级**即使单人也有价值（区分「明确要求」和「我猜你想让我做」）

**决策**：不砍掉「显式 vs 推断」这个区分，但**用一个布尔量表达**，不引入 242 行的 Policy 抽象。

### 1.5 🔴「照搬 vs 解决问题」→ ✅ 已明确

**审计的批评**：

> 当前文档的结构是「DeepSeek 有 7 个概念，我们建 7 个模块」。

**成立。** 已补 `00-GOALS-AND-DECISIONS.md` §1.5「不做行不行」——这是审计要求的核心一节：

| 已有能力 | 来源 |
| --- | --- |
| DAG 编排（并行、分波、验收、取消、进程树） | `rpi-orchestrator` |
| 飞书收发、autoReply、工具进度回传 | `rpi-im-message` |
| 多会话监控、按紧急度分桶、飞书远程交互 | `pi-dingo-dash` |

**实测结论**：`rpi-orchestrator` + `rpi-im-message` 的组合，**已经覆盖了「收消息 → 跑 DAG → 回报」的完整链路**。

所以本项目**不是**为了「能收消息跑 DAG」——那已经有了。真正的增量只有三项：

1. **长程状态**：orchestrator 的图在进程内存（`OnceLock<Mutex<GraphState>>`），跨重启不存在
2. **可诊断的失败**：没有「一个任务卡在哪一步」的整体视图
3. **统一的任务面板**：Dash 现在看的是**会话**，不是**任务**

**§1.6 因此砍掉什么**：Eval / Mandate / Policy / Subscription / Outbox / Effects / 自建 Source / 自建 Panel / 自建 Executor。

**这一刀砍掉约 4,000 行。**

### 1.6 🟠「漏读两个文件可能省掉大量工作」→ ✅ 已读，发现比审计预期更激进

审计建议先读 `rpi-im-message` 的会话路由和 Dash 的 `dingo.rs::classify`。

**我读了，结论比审计说的更彻底**（`00-GOALS-AND-DECISIONS.md` §4）：

#### `rpi-im-message`：第一个 Source 可能不需要新代码

审计说「读完可能发现阶段 C+D 可以被大幅简化甚至省略」。

**我实测发现可以完全省略**：

`rpi-im-message` 的 autoReply 走 `RuntimeActionId::SendUserMessage`（`src/lib.rs:1102`）——
把飞书消息**注入宿主正在运行的会话**。模型看到消息后**自己就会调** `superagent_plan` / `superagent_dispatch`。

| DeepSeek Source 提供 | 我们需要吗 |
| --- | --- |
| Trigger 持久化 + 幂等 | ✅ 需要 |
| Thread 聚合 | ⚠️ 复用飞书原生 thread + Dash 的 `router.rs` |
| Policy 过滤 | ❌ 砍掉 |
| Channel（写回） | ✅ `rpi-im-message` 的 `send` 已有 |
| Polling 循环 | ✅ 长连接已有 |

**Source 层只需一个「消息 → Task」桥接，约 200~300 行。**

#### 审计说的「已覆盖 80%」，我实测后需要修正

审计标注这是「从 README 推断，未经实测」。**我实测了**：

| 说法 | 实测 |
| --- | --- |
| 「组合已覆盖 80%」 | ✅ **比这更彻底**——autoReply 走 `SendUserMessage` 注入宿主会话，orchestrator 的工具**本来就可用** |
| 一个审计没提到的坑 | ⚠️ autoReply 的**兜底路径**用 `rpi --print --no-extensions` 起子进程（`src/lib.rs:726`）——**那条路径加载不了 orchestrator 的工具**。兜底路径下只有单轮对话能力 |

**这个坑已写进设计**（`00-GOALS-AND-DECISIONS.md` §4.4，风险表第 9 条）。

#### Dash 的 `classify`：Panel 状态映射不用重造

`crates/core/src/dingo.rs:26` 的 `Bucket`：

```
Error / Review / Question / Answered / Draft / Running / Waiting / Normal
```

与 DeepSeek 的 Ticket stage（person/delivery、person/answer、person/review、
person/blocked、working/queued、idle、working）**高度同构**——直接映射即可。

已列入下一步的阅读清单（`00-GOALS-AND-DECISIONS.md` §6 第 3 项）。

### 1.7 🟡「`app/` 占 43%，一句『passes 常驻循环』带过」→ ✅ 已逐文件拆解

审计说 `app/` 里有 Outbox / Notices / Effects / Cards / Assessment / Delivery / HTTP / API / Setup / Migration / Cache，**每一个都是独立关注点**。

**成立。** 我列了 `src/app/` 逐文件行数（实测）：

```
migration.ts 616   sessions.ts 352   resources.ts 308   store.ts 291
assessment.ts 282  settings.ts 267   runtime.ts 254     prompts.ts 247
http.ts 247        effects.ts 161    notices.ts 129     setup.ts 99
cards.ts 96        settings-history.ts 91   retry-health.ts 91
delivery.ts 86     store-read.ts 81 source-context.ts 72 links.ts 71
passes.ts 69       ...
```

`00-GOALS-AND-DECISIONS.md` §3.2 的处置表里写明：

> `src/app/` 里我们真正需要的只有：`store`（持久化）、`passes`（常驻循环）、
> `outbox`（若需送达保证）、`cards`（Panel 投影）。

**Outbox / Effects 已列入「明确不做」**（§3.4），理由：无「必须保证送达」的场景。

---

## 2. 审计说「不要立即进入阶段 A，先补三件事」——已补

| 审计的建议 | 状态 |
| --- | --- |
| 1. 写目标陈述（半天） | ✅ `00-GOALS-AND-DECISIONS.md` §1 |
| 2. 做 G0/G2 决策（半天） | ✅ §2（G0/G2/G3 已决，G1 待决并已指明前置） |
| 3. 修正阶段 A 的技术路径（10 分钟） | ✅ §1.3 + DESIGN §1 映射表 |

**代码一行未改。**

---

## 3. 新的阶段划分（7 阶段 → 4 阶段）

| 原阶段 | 新阶段 | 变化 |
| --- | --- | --- |
| A Ticket 域模型（改造 `Node`） | **A Task 模型 + 持久化**（`Node` 不动） | 技术路径改了 |
| B Policy + Eval gate | — | **砍掉** |
| C passes + Source 抽象 | **C 常驻循环 + 飞书桥接** | Source 变成「消息→Task 桥接」，200~300 行 |
| D 第一个 Source | — | **并入 C**（`rpi-im-message` 已覆盖） |
| E Panel | **B 任务级视图** | 复用 Dash `Bucket`，不重造 |
| F worktree 隔离 | **D worktree + 打磨** | 保留 |
| G Eval assessment | — | **砍掉** |
| — | **阶段 0**（已完成的 fork + 3 轮审计修复） | 新增，标记现状 |

**总计约 2,000 行，且每阶段独立交付可用价值。**

---

## 4. 仍待第四轮审计确认 / 未做的事

### 4.1 本轮**未做**的（按审计要求「先不改代码」）

| 项 | 状态 |
| --- | --- |
| 代码改动 | **零**（本轮只改文档） |
| G1 持久化方案 | ⬜ 待决——**前置是读 `rpi-durable/src/lib.rs`（2958 行）**，我没读 |
| `rpi-harness/runtime.rs` 跨重启边界 | ⬜ 未读 |
| Dash `dingo.rs` 全文 + `router.rs` | ⬜ 未读（只读了 `Bucket` 枚举和 `classify` 签名） |
| `task.rs` 接口草案 | ⬜ 未写 |
| `catalog/packages.json` | ⬜ 未加（`rpi install rpi-superagent` 仍装不了） |
| Domain 层代码 | ⬜ 一行没有 |

### 4.2 需要第四轮审计判断的设计取舍

| # | 取舍 | 我的选择 | 需要判断的 |
| --- | --- | --- | --- |
| 1 | **Task 的持久化位置** | 倾向 session-branch custom entry | 若 Task 活得比 Session 长，这个选择就不对 |
| 2 | **让位是静默的**（同装 orchestrator 时 superagent 不注入引导也不提示） | 静默 | 是否应在 `superagent_status` 里提示 |
| 3 | **砍掉 Eval 后，「显式 vs 推断」用一个布尔量** | 是 | 单人场景是否真的需要这个区分 |
| 4 | **`rpi-im-message` 兜底路径无 DAG 能力** | 文档说明 | 是否要让它在兜底路径也能用编排工具 |
| 5 | **阶段 A 之前是否还要先读 `pi-subagents` 的 worktree 逻辑** | 暂不 | 阶段 D 才需要 |

### 4.3 我对审计的一处保留

审计在 §四说「如果砍掉 Eval 和 Mandate，工作量减少约 4,000 行（core 的 2,088 + 相关的 admission/policy）」。

**我核实后认为这个数字偏大**：

- `core/` 的 2,088 行里，`tickets.ts` 423 + `model.ts` 353 + `assignments.ts` 140
  + `engine.ts` 270 + `stage.ts` 26 **我们仍然需要**（Task + Assignment + 引擎 + 阶段）
- 真正砍掉的是 `evals.ts` 340 + `intake.ts` 63 + `replies.ts` 95 + `policy.ts` 242
  + `sources/` 里的 admission 部分 ≈ **900~1,100 行**，不是 4,000

**这不影响决策**（砍 Eval/Mandate 的结论不变），但**影响工作量预估**——
我的 §3.2 说「`core/` 砍掉后约 300 行」可能也偏乐观。

**这一处请第四轮审计复核**：`core/` 里到底哪些行是「Eval/Mandate 专属」、
哪些是「Task/Assignment 也需要」。这是 §3.2 规模估算的唯一薄弱点。

---

## 5. 本轮改动的文件清单

| 文件 | 改动 |
| --- | --- |
| `00-GOALS-AND-DECISIONS.md` | **新增**（325 行）：目标陈述、用户场景、成功标准、不做行不行、砍掉什么、四个决策闸门、修正后的方案、规模重估、新阶段划分、风险清单、数字来源表 |
| `DESIGN-SUPERAGENT.md` | §0 加目标指向与「§6 是参考不是计划」的警告；§1 映射表撤回 Ticket 行、加 Task/Assignment 行、砍掉 7 个概念行；§6 标题改为「参考，非我们的计划」；§6.6 换成 4 阶段新表；§11 下一步换成 3 个待读文件 + 明确「不再计划内」清单 |
| `README.md` | 开头改为目标陈述；「What this is」改为「long-task layer」并说明只有两个概念 |

**代码：零改动。** 54 个单测仍全绿，clippy 仍干净。

---

## 6. 请第四轮审计重点看

1. **§1.2 的五条可检验陈述**——是否真的可检验？目标值定得对不对？
2. **§1.6 砍掉的清单**——有没有砍掉真正必需的？有没有该砍没砍的？
3. **§3.2 的规模重估**（1,500~2,500 行）——尤其 §4.3 我提出的那处保留，请复核
4. **§4 的「第一个 Source 不需要新代码」**——我的推理链是否成立？
5. **§2 的 G0/G2 决策**——特别是 G2「用一个布尔量替代 Policy」是否够用
6. **§3.3 的 4 阶段划分**——依赖关系是否还有隐含的前置没写出来？
7. **文档之间是否一致**——`00-GOALS-AND-DECISIONS.md` 与 `DESIGN-SUPERAGENT.md` 与 `README.md` 三者的目标陈述、阶段划分、砍掉清单

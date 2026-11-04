# rpi-superagent 实施设计

> **状态**：设计已定，代码骨架已落地（fork + 改名完成），Domain 层待建。
> **来源**：本包 fork 自 `pi-rust/rpi-package` 的 `packages/rpi-orchestrator`
> （MIT，Copyright (c) 2026 BigFish），基线 commit `da0a044`（upstream/main #186）。
> **动机**：把 DeepSeek《Exploring Super-agent Design》的架构落到 rpi 上。

---

## 0. 一句话

**rpi-superagent = fork 自 rpi-orchestrator 的执行内核 + 新增的 Ticket 领域层。**

执行内核（graph / dispatch / spawn / 验收 / 取消）已 e2e 验证，不重写；
领域层（Source / Trigger / Thread / Eval / Mandate / Ticket / Subscription）全部新建。

---

## 1. Fork 关系：哪些继承、哪些新增、哪些重写

维护者需要的唯一一张表。三类分明：

| DeepSeek 概念 | rpi-superagent 模块 | 来源 | 状态 |
| --- | --- | --- | --- |
| Executor | `spawn.rs` | fork 自 `rpi-orchestrator/src/spawn.rs` | ✅ 原样继承 |
| Assignment | `graph.rs` 的 `Node.status` | fork 自 `rpi-orchestrator/src/graph.rs` | ✅ 原样继承 |
| Ticket | `graph.rs` 的 `Node` | fork 自 `rpi-orchestrator/src/graph.rs` | ⚠️ **改造**（加 Mandate / Subscription / stage） |
| dispatch（跑一波） | `dispatch.rs` | fork 自 `rpi-orchestrator/src/dispatch.rs` | ⚠️ **改造**（写 Assignment 心跳、写失败原因） |
| plan（改图） | `plan.rs` | fork 自 `rpi-orchestrator/src/plan.rs` | ✅ 原样继承 |
| delegate（单任务） | `subagent.rs` | fork 自 `rpi-orchestrator/src/subagent.rs` | ✅ 原样继承 |
| 工具注册骨架 | `kit.rs` / `tool_schema.rs` / `control.rs` / `host.rs` | fork 自同名前述 | ✅ 原样继承 |
| 插件入口 | `lib.rs` | fork 自 `rpi-orchestrator/src/lib.rs` | ⚠️ **重写**（注册新工具集 + 去掉 orchestrator 的 goal 耦合） |
| Source | `source.rs` | — | 🆕 新增 |
| Trigger | `trigger.rs` | — | 🆕 新增 |
| Thread | `thread.rs` | — | 🆕 新增 |
| Eval | `eval.rs` | — | 🆕 新增 |
| Mandate | `mandate.rs` | — | 🆕 新增 |
| Ticket（领域） | `ticket.rs` | — | 🆕 新增 |
| Subscription | `subscription.rs` | — | 🆕 新增 |
| Policy（白名单/规则） | `policy.rs` | — | 🆕 新增 |
| 常驻 passes 循环 | `passes.rs` | — | 🆕 新增（仿 `rpi-im-message`） |
| 持久化 | `store.rs` | 参照 `rpi-durable` | 🆕 新增 |

### 1.1 为什么只能 fork，不能「依赖它再加」

实测（`/tmp/dupsym`）：两个 crate 都导出 `rpi_plugin_register` 时，后链接者**符号劫持**前者。

```
cratea::rpi_plugin_register called -> 7   ← 期望 42
```

`rpi-orchestrator` 自己就导出该符号（`lib.rs:244`），所以新 crate 依赖它再自己导出会顶掉它的注册入口，
且**宿主不会报错**（`register_tool` 注释："a name collision is not a hard failure — always return 0"）。

另外 `rpi-orchestrator` 的内部模块全是私有 `mod`，外部 crate 无法访问（实测 `error[E0603]: module 'dispatch' is private`）。

**结论：fork 是唯一可行解。**

### 1.2 Fork 的维护成本

上游 `pi-rust/rpi-package` 仍在活跃开发（fetch 时 upstream/main 已到 #186）。
fork 后需定期 rebase。**缓解**：Domain 层新增文件与执行内核文件分离，
rebase 冲突只可能出现在 `lib.rs` 和 `graph.rs` 两个文件。

---

## 2. 命名（已落地）

| 项 | 值 | 依据 |
| --- | --- | --- |
| crate 名 / 目录 | `rpi-superagent` | rpi 生态 13 个包全部带 `rpi-` 前缀；crates.io / npm / GitHub 均未占用 |
| 插件用户可见名 | `superagent` | — |
| 工具前缀 | `superagent_` | 同 `rpi-codegraph` → `codegraph_*`、`rpi-mcp-adapter` → `mcp_*` |
| 子代理标记环境变量 | `RPI_SUPERAGENT_SUBAGENT` | 原 `RPI_ORCHESTRATOR_SUBAGENT`，避免与 orchestrator 混淆 |

### 2.1 工具名冲突如何避免

宿主 `registry.rs:456` 的 `register_tool` 是 **first-wins 且静默**：

```rust
if self.tools.iter().any(|t| t.tool.name == tool.name) {
    return true;  // 保留先到的，插件仍返回成功
}
```

所以**工具名必须与 `rpi-orchestrator` 的 `plan` / `dispatch` / `subagent` 全部不同**。

已改名（`packages/rpi-superagent/src/tool_schema.rs`）：

| 原 | 新 |
| --- | --- |
| `subagent` | `superagent_delegate` |
| `plan` | `superagent_plan` |
| `dispatch` | `superagent_dispatch` |

### 2.2 与 `rpi-orchestrator` 的共存

改名后**工具层面零冲突**，两插件可同时安装。

残留问题：语义重叠，模型可能在「派活」时混用。处理层次：

1. **文档**：README 明确「装 superagent 时不要装 orchestrator；superagent 是其超集」
2. **Prompt**：`superagent` 的引导里写明「多步任务用 `superagent_*`，不要用 `plan`/`dispatch`」
3. **工具描述**：`superagent_plan` 的 description 里点名 `plan`/`dispatch` 是别的插件的

> `superagent_delegate` 而非 `superagent_subagent`：`rpi-subagents` 已有 `delegate_task` 工具，
> 两个语义不同（in-process 子会话 vs 子进程），改名区分。

---

## 3. 架构

```
┌──────────────────────────────────────────────────────────────┐
│ rpi 宿主进程（插件 rpi-superagent 加载其中）                    │
│                                                              │
│  ┌── passes.rs（常驻循环）────────────────────────────┐        │
│  │  thread + 独立 tokio runtime（仿 rpi-im-message）    │        │
│  │  每 2s tick + 事件到达时立即触发                     │        │
│  └───────────────┬────────────────────────────────────┘        │
│                  │                                             │
│  ┌── Domain 层 ──▼─────────────────────────────────┐          │
│  │  Source → Trigger → Thread → Eval → Ticket        │          │
│  │                    ↑                │             │          │
│  │              Policy/Mandate        │             │          │
│  │                                  Assignment      │          │
│  └──────────────────────────────────┬───────────────┘          │
│                                     │                          │
│  ┌── 执行内核（fork 自 orchestrator）─▼──────────────┐         │
│  │  graph.rs（Node→Ticket 域模型）                    │         │
│  │  plan.rs    superagent_plan                       │         │
│  │  dispatch.rs superagent_dispatch                  │         │
│  │  subagent.rs superagent_delegate                  │         │
│  │  spawn.rs   → rpi -p --no-session 子进程           │         │
│  └───────────────────────────────────────────────────┘         │
└──────────────────────────────────────────────────────────────┘
```

### 3.1 两条执行路径（已实测）

| 路径 | 用途 | 对应 DeepSeek |
| --- | --- | --- |
| `rpi --mode rpc`（长驻子进程） | router / main Session：持有领域上下文，做裁决 | main Session |
| `rpi -p --no-session`（一次性） | worker / Executor：每 Ticket 一个 | Executor Session |

`--mode rpc` 的 `maxConcurrentPrompts: 1` —— 同一时刻一个 prompt，恰好匹配 main Session 串行语义。

---

## 4. rpi 已验证的能力（不必重造）

**全部为本机实测，非文档推断。**

### 4.1 `rpi --mode rpc` 事件流

```
输入: {"type":"prompt","id":1,"content":"..."}
输出: {"type":"ready", ...}                      ← 握手 + 能力清单
      {"type":"response","command":"prompt","success":true}
      {"type":"agent_start"} / {"type":"turn_start"}
      {"type":"message_update",...}
      {"type":"turn_end"} / {"type":"agent_end"}
      {"type":"agent_settled","outcome":"completed"}   ← ★ 结算钩子
```

### 4.2 `queue_message` 三种投递语义（★ 最重要的发现）

| `deliverAs` | 队列名 | 实测行为 |
| --- | --- | --- |
| `steer` | `steer` | **工具批次之间注入，当前 turn 内模型可见** |
| `followUp` | `followUp` | 当前 turn 结束后进入下一轮 |
| `nextTurn` | `nextRun` | 等新的显式 prompt，不自动触发 |

steer 实测证据：模型跑 `sleep 20`，第 5 秒注入 `STEER_MARKER_123`，
`turn_end` 中模型原话 *"The user injected `STEER_MARKER_123`"* —— **模型真的读到了**。

这正是 DeepSeek 的 Thread 直通语义，**rpi 原生具备**。

### 4.3 `agent_settled` 可靠性

| 路径 | `agent_settled` | outcome |
| --- | --- | --- |
| 正常完成 | ✅ | `completed` |
| abort | ✅ | `aborted`（附 `reclaimed`：被吞掉的排队消息） |
| 宿主被 SIGKILL | ❌ | — |

### 4.4 执行内核 e2e（全部通过）

| 场景 | 结果 |
| --- | --- |
| 2 独立节点并行 | ✅ 一波完成，文件真实产出 |
| 3 节点依赖链分波 | ✅ 三次 dispatch 各跑 1 个，顺序正确 |
| 失败传播 | ✅ `failed` → 下游 `skipped` → 从未运行 |
| provider/model 继承 | ✅ 4 个 workbuddy-api 模型 + volcengine 全部正确继承 |
| 取消杀进程树 | ✅ 单测覆盖（含孙进程） |

---

## 5. 要补的能力与已知坑

### 5.1 已修：失败原因丢失（问题 B）

**症状**：`plan show` 只显示 `[!] boom failed`，不显示为什么失败。
Super-agent 的 Panel 必须能回答「为什么卡住」，否则违背 DeepSeek 的 "It reports honestly"。

**修法**（已落地）：`graph.rs` 的 `Node` 增加 `error: Option<String>`，
`render()` 在失败节点后追加 `— <原因>`；`dispatch.rs` 在失败分支写入原因。

### 5.2 已知坑：宿主崩溃后 Ticket 卡死（问题 C）

`agent_settled` 在 SIGKILL 时不发。宿主重启后 `running` 状态的 Ticket **永远无人推进**。

**修法**：每个 Assignment 必须有**心跳 + 超时**。
参照 DeepSeek 原型：*"After each turn, background work has at most 30 minutes to finish."*

### 5.3 已知坑：图状态在进程内存（问题 A）

`lib.rs` 的 `OnceLock<Mutex<GraphState>>` —— 插件重载 / 宿主重启即丢。

**修法**：Ticket 状态落盘。**跟随 rpi 生态主流做法（JSONL / session-branch custom entry）**，
参照 `rpi-durable`（它已经解决过同类问题），不自建 DB。

> 注：`rpi-harness` 虽有 `SqliteSessionStorage`，但 rpi 运行时**实际用 JSONL**
> （`session.rs:469` `JsonlSessionRepo`），Sqlite 是可选实验性替代。
> 跟随主流，不自造。

### 5.4 已知坑：工作区无隔离

`rpi-orchestrator` 明确不做 worktree（README: *"neither creates a worktree or provides a sandbox"*）。
并行写同一仓库会互相覆盖。

**修法**：给 node 加 `isolation` 选项，抄 `pi-subagents` 的 `worktreeBaseDir` 安全路径拒绝逻辑
（拒绝把 worktree 建在仓库内 / 扩展目录内 / 仓库父目录下）。

DeepSeek 原型也是**每 Ticket 一个目录**（prepare 命令跑 `git worktree`）。

### 5.5 已知坑：无授权层

DeepSeek 的核心是 **Mandate / Policy 在代码里，不在 prompt 里**。
目前 orchestrator 完全没有——模型想派什么活就派什么活。

**这是 Super-agent 与「多 agent 流水线」的本质区别**，必须新建。

### 5.6 已知坑：无常驻循环

`rpi-orchestrator` 是**模型驱动**（模型自己决定何时 dispatch），不是**事件驱动**。
DeepSeek 是事件驱动（外部消息 → Trigger → Eval → 裁决 → 派活）。

**修法**：新增 `passes.rs`，仿 `rpi-im-message` 的常驻线程模式（已验证该模式在 rpi 插件内可行）。

### 5.7 已知坑：单次 spawn 靠 `rpi -p`，无法 resume

DeepSeek 的 worker 是**有状态 Session**，可 resume。
`rpi-orchestrator` 用 `rpi -p --no-session`（一次性、无状态）。

**取舍**：第一版接受不可 resume（Ticket 失败就重跑）。
若要 resume，需改用 `rpi --mode rpc` 长驻 + `--session-id`。

### 5.8 已知坑：跨平台测试路径

`spawn.rs` 的单测硬编码 `/bin/false`，macOS 上不存在（实际在 `/usr/bin/false`）→ **该测试在 macOS 必失败**。
已验证是环境问题不是逻辑问题（改路径后通过，源文件已还原）。
**修法**：用 `std::env::temp_dir()` 写临时脚本替代硬编码路径。

---

## 6. Domain 层设计

### 6.1 概念与规则（引自 DeepSeek `001-domain.md`）

| 概念 | 定义 | 必须守的规则 |
| --- | --- | --- |
| Source | 事件流。持有读位置与自己的 bot 身份 | 重启不重放事件 |
| Trigger | 一个不可变事件 | 属一个或多个 Thread；同一 Trigger 只接受一次 |
| Thread | 按时间排序的 Trigger 序列 | 同一时刻最多一个 Eval 在跑 |
| Correlation | Trigger 之间的关联 | **只加上下文，不路由** |
| Channel | Thread 的写侧 | Ticket 持有一组 Channel |
| Policy | 版本化的评估规则 + 各 Source 的允许名单 | Policy 变更不改已有 Mandate，但送达正在跑的 Assignment |
| Eval | 对一批 Trigger 的判断，**两层：gate + assessment** | 无状态；输入 = Trigger 批 + Thread 快照 + 所有 Ticket + 当前 Policy |
| Mandate | accept 授予的不可变授权 | 不可变；范围变宽 = 新 Ticket |
| Ticket | 一个被授权的工作单元 | **只有 Eval 能创建**；自己定义状态，不跟随 Executor |
| Assignment | Ticket 与一个 Executor 实例的绑定 | 可有多个；只有最终报告能结束 Ticket |
| Subscription | Ticket ↔ Thread 多对多 | 匹配的 Trigger 扇出到所有订阅的 Ticket，**不检查发送者** |

### 6.2 DeepSeek 明确拒绝的概念（我们也不要造）

| 拒绝 | 原因 |
| --- | --- |
| Principal / Membership 实体，跨 Source 身份映射 | 一个人有多个 Source 账号是 prompt 文本，不是领域概念 |
| Eval 自己调度延迟 Trigger | 需要等待的应是一个普通 Source |
| 等待态 / 吊销态 Eval | 等待属于 Source；Eval 超时重排，三次后记 `unavailable` |
| 一个 Thread 至多一个活 Ticket | 一个 Thread 可以承载多件独立的事 |
| Subscription 的准入检查 | Subscription 覆盖整个 Thread，授权由 Mandate 分离 |
| 从报告自由文本推断「等待人」 | 需要结构化事实，由 Executor 用结构化字段问 |
| Policy 变更时取消运行中的 Ticket | 取消变更留下半成品；应记录并送达 |

### 6.3 Ticket 状态机（引自 DeepSeek `003-execution.md`）

从上往下第一个匹配的生效：

| 阶段 | 条件 | Panel 标签 |
| --- | --- | --- |
| person / delivery | 投递 Input 失败（2s 后重试，指数退避到 1min） | Input undelivered |
| person / answer | 有 Attention（待回答） | Awaiting answer |
| person / review | 最新报告是 `ready` | Awaiting review |
| person / blocked, failed | 最新报告是 `blocked`/`failed`，且此后无人的消息 | Blocked · Failed |
| working / queued | 无 Assignment，或 Assignment pending | Queued |
| idle | 最新报告是 `waiting`，或 Assignment 已结束 | Waiting for a Trigger |
| working | 其余 | Working |

`person` 阶段 = 没有人类输入就无法继续。

### 6.4 Eval 的两层

```
Source 记录 Trigger 是否指向 agent：mention（@了 bot）/ direct（私聊）
        │
        ▼
   ┌─ gate（程序规则，快、便宜）────────────────────────┐
   │  · 允许名单里的 sender + 指向 agent → 显式请求      │
   │  · 其它人类消息 → 查 admission 表                   │
   │  · bot 在 bot 名单 → 只能产出 inferred candidate     │
   │  · 其它 bot → 不准入                                │
   └────────────────────┬───────────────────────────────┘
                        ▼
   ┌─ assessment（LLM 判断，准、贵）────────────────────┐
   │  Decision: accept | stop | ignore | unavailable     │
   │  accept → 创建 Ticket + Mandate + 首个 Assignment    │
   └────────────────────────────────────────────────────┘
```

**第一版只做 gate**（DeepSeek 自己也是 "Start narrow"），assessment 留到第二版。

### 6.5 Subscription 直通（rpi 已具备原语）

DeepSeek 规则：*「Every message on a subscribed Thread reaches its subscribed Tickets as context,
including messages from bots and CI. This delivery bypasses the gate and grants no new authority.」*

rpi 侧映射：

| DeepSeek | rpi |
| --- | --- |
| Thread 消息扇出到订阅 Ticket | `queue_message` with `deliverAs: steer`（当前 turn 内可见） |
| 「但不授予新权限」 | Ticket 的 Mandate 不变——权限来自 Mandate，不来自消息 |
| bot/CI 消息也到 Ticket | 同上，`steer` 不区分来源 |

### 6.6 落地顺序

| 阶段 | 内容 | 依赖 |
| --- | --- | --- |
| A | Ticket 域模型（改造 `graph.rs` 的 `Node`）+ 持久化 | `rpi-durable` 范式 |
| B | Policy（白名单 / admission 表）+ Eval 的 gate 层 | A |
| C | passes 常驻循环 + Source 抽象 | B |
| D | 第一个 Source：`rpi-im-message`（飞书） | C |
| E | Panel：接 pi-dingo-dash | A（读 Ticket 状态） |
| F | worktree 隔离 | A |
| G | Eval 的 assessment 层（LLM 判断） | B |

---

## 7. 工具集

第一版（继承 + 少量新增）：

| 工具 | 来源 | 说明 |
| --- | --- | --- |
| `superagent_plan` | fork | 建/改 Ticket 图 |
| `superagent_dispatch` | fork | 跑一波 |
| `superagent_delegate` | fork | 单任务子进程 |
| `superagent_status` | 新增 | 全局状态（Ticket 分布、Assignment 负载） |
| `superagent_ticket` | 新增 | 读单个 Ticket 详情（含失败原因、阶段） |
| `superagent_policy` | 新增 | 读/改白名单与 admission 表 |

第二版：

| 工具 | 说明 |
| --- | --- |
| `superagent_route` | 批量路由（仿 DeepSeek `orchestrator_route` 的 `decisions[]`，**每项独立成败**） |
| `superagent_source` | Source 管理（list / start / stop） |
| `superagent_eval` | Eval 的读与决策 |

### 7.1 token 预算（实测数据）

| 样本 | 工具数 | schema 总长 | 估算 tokens |
| --- | --- | --- | --- |
| `rpi-codegraph` | 8 | 6354 字符 | ≈1588 |
| `rpi-orchestrator` | 3 | 3614 字符 | ≈903 |
| **rpi-superagent 第一版** | **6** | 估 ≈5000 字符 | **≈1250** |

同量级，可接受。

**约束**：DeepSeek 的 `RouteItemSchema` 有 8 个字段（含 `worktree_name`、`acknowledgement` 带字数限制），
因为它同时服务人机交互和 GitHub PR 流程。**我们的 schema 必须更简**，否则单工具可能 2000+ 字符。

### 7.2 工具数量是否影响模型选对

实测：`rpi-orchestrator` 的 3 工具场景下，明确指示「用 DAG 工具，不要用单次 subagent」，
模型正确选了 `plan` → `dispatch`，两个文件都产出，verify 通过。

**结论**：工具多不必然选错，前提是 description 边界写清楚。已有先例（codegraph 8 工具）。

---

## 8. 已完成的改动

| 改动 | 文件 | 状态 |
| --- | --- | --- |
| crate 改名 `rpi-orchestrator` → `rpi-superagent` | `Cargo.toml` | ✅ |
| 注册进 workspace | `../../Cargo.toml` | ✅ |
| 工具改名 `superagent_*` | `src/tool_schema.rs` | ✅ |
| 注册调用改名 | `src/lib.rs` | ✅ |
| 递归守卫名单改名 | `src/spawn.rs` | ✅ |
| 子代理标记环境变量改名 | `src/spawn.rs` | ✅ |
| `Node.error` 字段 + `render()` 显示失败原因 | `src/graph.rs` | ✅（待补 dispatch 写入） |
| 失败原因写入 | `src/dispatch.rs` | ⬜ 待做 |

---

## 9. 风险清单

| # | 风险 | 等级 | 缓解 |
| --- | --- | --- | --- |
| 1 | Domain 层工作量被低估（7 个概念 + 状态机 + 授权） | 高 | 分 6 阶段（A~G），每阶段独立可交付 |
| 2 | fork 后上游 180+ PR 的 rebase 成本 | 中 | 新增文件与内核文件分离；冲突只在 `lib.rs` / `graph.rs` |
| 3 | Ticket 状态丢在内存 → 崩溃即失忆 | 高 | 阶段 A 优先做持久化 |
| 4 | 宿主崩溃 → Ticket 永久 `running` | 高 | Assignment 心跳 + 超时（§5.2） |
| 5 | 无工作区隔离 → 并行写互相覆盖 | 中 | 阶段 F 加 `isolation` 选项 |
| 6 | 单次 spawn 不可 resume → 失败重跑成本高 | 中 | 第一版接受；后续改 rpc 长驻 |
| 7 | `rpi-orchestrator` 仍在演进而 fork 停滞 | 中 | 定期 rebase；内核尽量少改 |
| 8 | 模型混用 `superagent_*` 与 `plan`/`dispatch` | 低 | 工具描述点名；README 说明不要同装 |
| 9 | macOS 上 `/bin/false` 单测失败 | 低 | §5.8 的修法 |
| 10 | 评估层（LLM 判断）成本与延迟 | 中 | 第一版只做 gate；assessment 延后 |

---

## 10. 未验证 / 未读

诚实列出，避免被当成已知。

| 项 | 状态 |
| --- | --- |
| `rpi-durable/src/lib.rs`（2958 行） | **未读**——阶段 A 的持久化范式待确认 |
| `rpi-harness/src/runtime.rs`（1024 行）recovery 机制 | **未读**——阶段 A 的崩溃恢复待确认 |
| DeepSeek `src/app/store.ts`（291 行）/ `src/core/engine.ts`（270 行） | **未读**——事务边界与「唯一写入者」的实现方式待确认 |
| DeepSeek `src/core/parts/*`（Eval/Intake/Tickets/Assignments 实现） | **未读**——路径未探测 |
| `nextTurn` 的确切消费时机 | 未测（只验证了入队且不自动触发） |
| steer 是否**总**被真实 worker 遵守 | 未测（只测了模型会提及 marker） |
| 多进程（非 shell 管道）下的 rpc 行为 | 未测（管道关闭 stdin 的时机可能影响时序） |
| `agent_settled` 在 timeout / provider 异常下的行为 | 未测（只测了正常 / abort / SIGKILL） |
| pi 生态 `pi-orchestrator` 的实现 | 未读（只读 README）——定位不同，未深究 |
| `pi-subagents` 的 worktree 分配逻辑 | 未读——阶段 F 的参考 |
| 全部实测仅用 `volcengine/deepseek-v4.1-flash` + 4 个 `workbuddy-api/*` | `magpie/acctoken/*` 因账户额度不足（¥0.107）不可用 |

---

## 11. 下一步

**立刻可做（不依赖未读项）**

1. `dispatch.rs` 写入 `node.error`（§8 剩的一项）
2. 修 `/bin/false` 跨平台单测（§5.8）
3. 更新 README：fork 说明 + 双向映射表 + 与 orchestrator 的共存说明
4. 重跑 4 个 e2e 场景，确认改名后一切照旧（工具名、递归守卫、环境变量）

**然后读 2 个文件**

5. 读 `rpi-durable/src/lib.rs` → 定 Ticket 持久化范式
6. 读 `rpi-harness/src/runtime.rs` → 定崩溃恢复边界

**然后开工阶段 A**

7. `Node` → Ticket 域模型（加 Mandate / Subscription / stage）
8. 持久化
9. 阶段 A 的 e2e：建 Ticket → 落盘 → 重启 → 读回

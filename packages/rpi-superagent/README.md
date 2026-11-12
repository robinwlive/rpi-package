# rpi-superagent

Long-task orchestration for the `rpi` Rust agent: a bounded DAG execution
kernel plus a small layer that keeps a task alive across restarts.

## 这个插件的目标：一间公司

人和 agent 的关系，是一个**明确的分工**，不是一个聪明的自动化：

| 角色 | 是谁 | 职责 |
| --- | --- | --- |
| **老板** | 你 | 定方向、改主意；项目经理拍不了板的事，问你 |
| **项目经理** | 主会话 | 和你实时对话，同时拆活、派人、看进度、调方向。**所有工作安排和进度调整都由它完成** |
| **员工** | 子会话 | 只做被派的那一件事，做完交报告。彼此异步，可以同时有多个 |

规则只有三条，但每条都是硬约束：

1. **任何工作都扔给员工做。** 主会话不自己动手干活，它派人。
2. **员工异步，且和对话并行。** 派出去就回来，主会话立刻继续和你说话。
   你在聊天的时候，员工在干活；聊到一半冒出新想法，可以马上派人去做。
3. **老板按需查看，不看不打扰。** 你想知道每个员工在做什么、做到什么程度，
   随时能问；你不想知道，就没有汇报来打扰你。

### 这不是项目制

容易想错的一点，是把它当成「立项 → 排期 → 执行 → 交付」。不是。

- **不限于一个项目。** 手上可能同时在办几件互不相干的事，也可能是一件综合的事。
- **一个"项目"内部目标也不必一致。** 老板的思路一直在变，这很正常，不是需求变更。
- **沟通是实时的、持续的。** 老板不断把想法给项目经理，项目经理不断把自己的判断
  报回来。派活和讨论同时进行，不是先讨论完再派活。
- **员工干的活会作废。** 想法变了，某个员工正在做的事可能就没用了，或者被一个
  更新的版本替代。这不是异常，是常态 —— 所以取消和替换必须是一等操作，
  不能要求"等这一轮跑完再说"。

相应地，`plan(action=update)` 在 wave 运行期间**可以改**：

- `cancel: [...]` —— 这活不要了。标记为 `skipped`，依赖它的节点也一并跳过。
  用 `cancel` 而不是 `reset`：`reset` 会把正在跑的节点重新变回待办，
  留下一个没人等待的活子进程，下一波还会再起一份。
- `addNodes` —— 边聊边加活。
- `setDeps` / `removeIds` / `reset` —— 调整结构或重排。

改计划会让**正在跑的那一波的结果作废**（用代次校验），但**不会打断已经在跑的员工**：
它们是照着派活时的快照在干的。结果作废比让老板干等一堆无关的活要划算。

### 多个主会话并存

Dash 里的每个主会话是一个独立的 rpi 进程，各自管自己的员工，互不干扰。但有一处**共享**：
它们用同一个 `RPI_CODING_AGENT_DIR`，因此共用同一个 effect 队列文件。

队列按「会话 id + task id + wave」去重，所以两个会话都用 `T1` 也不会互相覆盖。
（早期版本只按 task id 去重，后一个会话的派活会**顶掉**前一个 —— 那个会话的活永远
不会跑，而它的任务还显示 `working`。）

计划本身是按会话隔离的：切到另一个会话看不到这边的图。所以隔离独立项目，
**开一个独立主会话就够了**，不必靠 task id 前缀避让。

> **Goal** — 把「一个长任务」从「人盯着它跑」变成「人只在需要裁决时才出现」。
> (*Turn "a long job" from "you watch it run" into "you show up only when a
> decision is needed."*)
>
> The five verifiable criteria, the scope we deliberately cut, and the open
> decisions live in [`00-GOALS-AND-DECISIONS.md`](00-GOALS-AND-DECISIONS.md).

## What this is

**This package is a fork of `rpi-orchestrator`** (from `pi-rust/rpi-package`,
MIT, Copyright (c) 2026 BigFish), renamed and re-scoped. The execution kernel is
inherited unchanged; the long-task layer is new work.

- **Execution kernel (inherited).** `superagent_plan` authors a task DAG and
  `superagent_dispatch` queues one ready wave of it in bounded child processes.
  Dispatch is always asynchronous: it returns at once and the wave's report
  arrives later as a message, because a blocking one-shot child is work the
  calling session could have done itself. Turn and wall-clock budgets,
  per-node acceptance checks, process-tree cancellation and the recursion guard
  are all enforced in code, not asked of the model.
- **Long-task layer (new, in progress).** A `Task` survives a restart, so a job
  that outlives the process is not lost. Two concepts only — `Task` and its
  `Assignment` records — rather than the full Source/Trigger/Eval/Mandate/
  Policy/Subscription set, because those exist for team-scale authorisation that
  a single-user setup does not need.

Design, mapping tables and known gaps: [`DESIGN-SUPERAGENT.md`](DESIGN-SUPERAGENT.md).

## Install

```bash
rpi install rpi-superagent
```

Requires `rpi` 0.15.0 or newer (it uses the `ChildAgent` runtime action and the
unified plugin ABI).

## Tools

| Tool | Purpose |
| --- | --- |
| `superagent_plan` | Author or edit the task DAG: `set` replaces nodes, `update` applies `addNodes` / `removeIds` / `setDeps` / `reset` atomically, `show` inspects. A node with `confirmBeforeNext: true` stops the automatic next wave until the project manager decides. |
| `superagent_dispatch` | Queue one wave and return at once: every pending node whose dependencies are done at wave start, in batches up to `maxParallel`. A wave that unblocks more nodes queues the next one itself; the report arrives as a message. |
| `superagent_task` | Track a long task across restarts: `create` it, `update` its state, `list` them. |

## Relationship to `rpi-orchestrator`

> **Do not install both.**
>
> They overlap: `rpi-orchestrator` exposes `plan` / `dispatch` / `subagent` and
> this package exposes `superagent_plan` / `superagent_dispatch` /
> `superagent_dispatch` — the same workflow under two names, and the same
> project goal under one `.rpi/goals.json`.
>
> If both are installed, this package detects its neighbour and **stands down**
> rather than injecting a second, conflicting guidance section. The model then
> sees only `rpi-orchestrator`'s tools and guidance, so the two work sets are
> not available together.
>
> Uninstall one:
> ```bash
> rpi uninstall rpi-orchestrator
> ```

If you only need a task DAG and no domain semantics, `rpi-orchestrator` is the
smaller package. This one is a superset of it.

## The supervisor process

A host that dies leaves its children running: `process_group(0)` puts them in
their own group, so nothing signals them, and `rpi -p` writes nothing until it
finishes. A separate `rpi-superagent-supervisor` process sweeps the session
directory for those leftovers and kills them.

It is a process rather than a thread because a thread dies with its host. It
detaches from the session, and it holds a lock so that only one supervisor runs
per session directory — two in the same directory would race over the same
processes, and two in different directories have nothing to contend for.

A snapshot records both the children and the host that started them, so the
supervisor can tell an orphan from work that is running right now in another
session. It only kills a child once its owner is gone.

The sweep reads very little: a file that has not been touched for hours cannot
be running anything, and a task snapshot is appended when the task changes, so
the newest one is always near the end of the file.

## Isolating parallel work

Two nodes that share one working directory have to be given non-overlapping
write ownership by hand — a rule the model has to remember, and a rule that is
forgotten shows up as two nodes overwriting each other's files mid-run.

A node with `isolation: true` gets a directory of its own under
`.rpi/superagent-work/<run id>/<node id>`. Its child process runs there and
its `expectFile` / `verify` checks run there too, so those paths are relative
to that node's own work. The directories are removed when the wave finishes;
the files a node produced are the run's output and stay.

```json
{"id": "a", "task": "write the report", "isolation": true,
 "expectFile": "report.md", "verify": "test -s report.md"}
```

## If a change seems to have no effect

The host registers a tool **first-wins**: when two extensions offer a tool with
the same name, the first one loaded keeps it and the other is dropped **without
a warning** (`registry.rs:457`).

Extensions are loaded from the agent's extension directory first and from
explicit `-e` paths afterwards — so an **older build left in the extension
directory silently wins** over the one you just compiled.

Before debugging a change that appears to do nothing, check which build is
actually loaded:

```bash
ls -la "${RPI_CODING_AGENT_DIR:-~/.rpi/agent}/extensions"
```

Removing the stale copy, or pointing at a clean agent directory, usually
explains the mystery.

## What was changed from `rpi-orchestrator`

| Change | Why |
| --- | --- |
| Crate renamed to `rpi-superagent`; tools renamed to `superagent_*` | Avoid a silent first-wins collision with `rpi-orchestrator`'s `plan` / `dispatch` / `subagent`. The host keeps the first registration of a given tool name and does not warn. |
| `RPI_ORCHESTRATOR_SUBAGENT` → `RPI_SUPERAGENT_SUBAGENT` | Avoid confusion with the package it was forked from. |
| **`RPI_ORCHESTRATOR_GUIDE` → `RPI_SUPERAGENT_GUIDE`** | The two packages shared this variable, so setting it for one silently drove the other. |
| **Guide heading and body rewritten** | The injected guidance named `plan` and `dispatch` — tools this package does not register — so the model was told to call tools that do not exist. It now names `superagent_plan` and `superagent_dispatch`. The heading also changed (`## Super-agent orchestration`), because the host de-duplicates injected sections by heading: a shared heading made whichever plugin loaded first win silently. |
| Guide logic moved out of `lib.rs` into `guide.rs` | Keeps the execution kernel a thin diff against upstream, so rebase conflicts land in the registration lines rather than in a file of prompt text. |
| `Node` gained an `error` field; `superagent_plan` shows the failure reason | A failed node used to render as `[!] id failed` with no cause, which leaves a human unable to tell why work stopped. `reset` clears it, so a re-armed node never shows a stale reason. |
| Recursion-guard tool list updated to the new names | The child process must not be able to call back into the orchestrator. |
| macOS test used a hardcoded `/bin/false` | That path does not exist on macOS; the test now writes a failing script to a temp file. |

The source is otherwise identical to the upstream `rpi-orchestrator` at the fork
point. See [`DESIGN-SUPERAGENT.md`](DESIGN-SUPERAGENT.md) §1 for the full
inherited / new / rewritten table.

## License

MIT, as inherited from `rpi-orchestrator`. See the workspace `LICENSE` and
`NOTICE`. The upstream copyright notice is retained.

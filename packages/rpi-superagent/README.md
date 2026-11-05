# rpi-superagent

Long-task orchestration for the `rpi` Rust agent: a bounded DAG execution
kernel plus a small layer that keeps a task alive across restarts.

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

- **Execution kernel (inherited).** `superagent_plan` authors a task DAG,
  `superagent_dispatch` runs one ready wave of it in bounded child processes,
  `superagent_delegate` runs a single bounded task. Turn and wall-clock budgets,
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
| `superagent_plan` | Author or edit the task DAG: `set` replaces nodes, `update` applies `addNodes` / `removeIds` / `setDeps` / `reset` atomically, `show` inspects. |
| `superagent_dispatch` | Run one wave: every pending node whose dependencies are done at wave start, in batches up to `maxParallel`. Newly unblocked nodes wait for the next call. |
| `superagent_delegate` | Run one bounded task in a fresh `rpi` subprocess. Not the host-managed in-memory `delegate_task`; use that for in-process delegation on the current execution binding. |
| `superagent_task` | Track a long task across restarts: `create` it, `update` its state, `list` them. |

## Relationship to `rpi-orchestrator`

> **Do not install both.**
>
> They overlap: `rpi-orchestrator` exposes `plan` / `dispatch` / `subagent` and
> this package exposes `superagent_plan` / `superagent_dispatch` /
> `superagent_delegate` — the same workflow under two names, and the same
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

# rpi-superagent

Super-agent orchestration for the `rpi` Rust agent: a bounded DAG execution
kernel plus a Ticket domain layer, modelled on DeepSeek's *Exploring
Super-agent Design*.

## What this is

**This package is a fork of `rpi-orchestrator`** (from `pi-rust/rpi-package`,
MIT, Copyright (c) 2026 BigFish), renamed and re-scoped. The execution kernel is
inherited unchanged; the Ticket domain layer is new work.

- **Execution kernel (inherited).** `superagent_plan` authors a task DAG,
  `superagent_dispatch` runs one ready wave of it in bounded child processes,
  `superagent_delegate` runs a single bounded task. Turn and wall-clock budgets,
  per-node acceptance checks, process-tree cancellation and the recursion guard
  are all enforced in code, not asked of the model.
- **Domain layer (new, in progress).** `Source` → `Trigger` → `Thread` →
  `Eval` → `Mandate` → `Ticket` → `Subscription`, with authorization in code
  rather than in the prompt.

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
| `superagent_status` | Global state: Ticket distribution and Assignment load. |
| `superagent_ticket` | Read one Ticket in detail, including its failure reason and stage. |
| `superagent_policy` | Read or change the allowed list and admission table. |

## Relationship to `rpi-orchestrator`

Both packages can be installed at the same time — their tool names do not
overlap. **Prefer this one.** `rpi-orchestrator` exposes `plan`, `dispatch` and
`subagent`; `rpi-superagent` exposes `superagent_*` and adds the domain layer.
Installing both gives the model two ways to do the same thing, and it will
sometimes pick the wrong one.

If you only need a task DAG and no domain semantics, `rpi-orchestrator` is the
smaller package. This one is a superset of it.

## What was changed from `rpi-orchestrator`

| Change | Why |
| --- | --- |
| Crate renamed to `rpi-superagent`; tools renamed to `superagent_*` | Avoid a silent first-wins collision with `rpi-orchestrator`'s `plan` / `dispatch` / `subagent`. The host keeps the first registration of a given tool name and does not warn. |
| `RPI_ORCHESTRATOR_SUBAGENT` → `RPI_SUPERAGENT_SUBAGENT` | Avoid confusion with the package it was forked from. |
| `Node` gained an `error` field; `superagent_plan` shows the failure reason | A failed node used to render as `[!] id failed` with no cause, which leaves a human unable to tell why work stopped. |
| Recursion-guard tool list updated to the new names | The child process must not be able to call back into the orchestrator. |

The source is otherwise identical to the upstream `rpi-orchestrator` at the fork
point. See [`DESIGN-SUPERAGENT.md`](DESIGN-SUPERAGENT.md) §1 for the full
inherited / new / rewritten table.

## License

MIT, as inherited from `rpi-orchestrator`. See the workspace `LICENSE` and
`NOTICE`. The upstream copyright notice is retained.

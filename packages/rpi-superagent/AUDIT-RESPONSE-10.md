# 第十轮审计的落实说明（供第十一次审计）

> 对应审计报告：第九轮审计（M1.6 落实 + SDK pin 冲突）。
> 本轮做完了 **M2 的接管部分**：心跳 + 僵死任务标记 `abandoned`。
>
> **目标陈述第 4 条**（"工作跨重启不丢"）的验收标准是：
> 「重启后 N 秒内，每个 `running` 任务都被**重新接管**（有存活心跳）或被**明确标记为 `abandoned`**」。
> **本轮实现了后半句，并给出了前半句的基础设施（心跳）。**

---

## 0. 结论先行

| 上一轮的缺口 | 状态 |
| --- | --- |
| **重启后 `running` 任务的接管 / abandoned** | ✅ **本轮完成「abandoned」这半**；「接管」见 §5 |
| Dash 上看不到任务 | ⬜ 未做 |
| `Graph.task_id` 只在内存 | ⬜ 未做 |
| 任务不能被创建以外的入口驱动 | ⬜ 未做 |
| `nodes` 字段没在渲染里显示 | ⬜ 未做 |

**测试**：69 passed（原 64 + 5）、clippy 干净。

---

## 1. 设计：心跳必须跨重启有意义

### 1.1 一个关键的认识

我原本以为 `Task.updated_at` 可以用「读的时候取当前时间」。**那是错的**——
读时间每次都一样，任务永远「新鲜」，心跳就没有意义。

**心跳必须是「宿主最后一次写这个任务的时间」**。它得满足：

- 跨重启后仍然可解释
- 不依赖任何进程内状态

**实现**：entry 自带 `timestamp`（宿主的写入时刻，毫秒）。
读回时用它覆盖 `updatedAt`：

```rust
// 每个快照的心跳取自承载它的那个 entry
let snapshots: Vec<(Value, Option<u64>)> = entries
    .iter()
    .filter(|entry| entry.get("customType").and_then(Value::as_str) == Some(TASK_ENTRY_TYPE))
    .filter_map(|entry| {
        let data = entry.get("data")?.clone();
        let timestamp = entry.get("timestamp").and_then(Value::as_u64);
        Some((data, timestamp))
    })
    .collect();
```

**这与 DeepSeek 的做法一致**：它的 Assignment 也靠心跳判断执行器是否还在。

### 1.2 心跳的三个写入点

| 时机 | 谁写 | 作用 |
|---|---|---|
| **dispatch 开始** | dispatch 线程 | 任务进入 `working`，心跳从这里开始 |
| **dispatch 运行中** | 独立线程，每 60s | 长任务不会被误判成僵死 |
| **dispatch 结束** | dispatch 线程 | 写入最终状态（done / failed） |

**为什么需要运行中的心跳**：子进程最长可跑 1800 秒（30 分钟），
而僵死阈值也是 30 分钟——**边界重合会误判**。每 60 秒一次心跳让任务年龄始终远低于阈值。

### 1.3 阈值

```rust
pub const ABANDON_AFTER_MS: u64 = 30 * 60 * 1000;
```

心跳每 60 秒一次，所以真正的僵死判定延迟是 30~31 分钟。
这个值与子进程最长超时（30 分钟）相当，但**因为有运行中的心跳**，二者不冲突。

### 1.4 reconcile 规则

```rust
pub fn reconcile_stage(task: &Task, now: u64) -> (Stage, Option<String>) {
    if task.stage != Stage::Working {
        return (task.stage, None);      // 只有 working 可能僵死
    }
    if task.updated_at == 0 || now <= task.updated_at {
        return (task.stage, None);      // 没有心跳数据 = 没有证据
    }
    if now - task.updated_at <= ABANDON_AFTER_MS {
        return (task.stage, None);      // 还新鲜
    }
    (Stage::Abandoned, Some(format!("no progress for {} minutes; the process driving it is gone", ...)))
}
```

**三个刻意的保守选择**：

1. **只有 `working` 会被 abandoned**。`needsPerson` 等人可以等很久——那是正常的，
   不是僵死。**等人和卡死必须分开**。
2. **没有时间戳就不动**。「缺信息」不等于「证据表明它死了」。
3. **reconcile 会写回**，不只是显示。所以下次读到的还是 `abandoned`——
   状态不会因为「没人再读它」而变回去。

---

## 2. e2e 验证

### 2.1 核心：僵死任务被标记

```
建 task long1 stage=working
（把 entry 的 timestamp 改成 40 分钟前，模拟进程死掉）
新进程读：
  1 task(s)
  [~] long1 abandoned — Long running (no progress for 40 minutes; the process driving it is gone)
```

### 2.2 reconcile 结果持久化

再读一次仍是 `abandoned`。session 文件里两条快照：

```
stage: working   error: None
stage: abandoned  error: no progress for 40 minutes; the process driving it is gone
```

### 2.3 不误判

```
新建 fresh1 stage=working（新鲜的）→ list:
  2 task(s)
  [~] long1 abandoned — Long running (...)
  [~] fresh1 working — Just started
```

**僵死的被标记，新鲜的保持 working。**

### 2.4 等人的任务永不 abandoned

单测守着：`NeedsPerson` / `Done` / `Failed` / `Pending` 无论多老都不变。

---

## 3. 实现中修掉的两个问题

### 3.1 心跳线程会泄漏（`?` 早退路径）

心跳在 `dispatch` 中途启动，但 `lock_graph_for_wave(params, generation)?`
（session 切换时失败）会**在停止心跳之前**返回 → 线程泄漏，
它会继续往一个已经不服务的 session 写。

**修法**：把停止心跳移到 `?` 之前。

```rust
let wave = lock_graph_for_wave(params, generation);
if let Some((handle, stop)) = heartbeat {
    stop.store(true, Ordering::Release);
    let _ = handle.join();
}
drop(wave?);
```

### 3.2 `from_value` / `fold` 只被测试用

clippy 报 never used。**改成 `#[cfg(test)]`**——生产代码里不该留只有测试用的入口。

### 3.3 心跳线程的停止机制

`JoinHandle` drop 不会停线程。用 `Arc<AtomicBool>` 共享停止标志，
线程每 1 秒检查一次（写心跳仍是 60 秒一次）。

---

## 4. 本轮改动的文件清单

| 文件 | 改动 | 行数 |
|---|---|---|
| `src/task.rs` | `Stage::Abandoned`；`ABANDON_AFTER_MS`；`reconcile_stage`；`from_snapshot`（带 entry timestamp）；`fold_entries`；`list` 里 reconcile + 写回；`now_ms`；5 个测试 | +180 |
| `src/dispatch.rs` | `start_heartbeat`（60s 间隔的独立线程 + 停止标志）；dispatch 开始/结束时记录；心跳的正确停止顺序 | +90 |
| `00-GOALS-AND-DECISIONS.md` | M2 状态 + e2e 记录 | +10 |
| `AUDIT-RESPONSE-10.md` | 本文件 | — |

**测试**：69 passed，clippy 干净。回归：`plan`+`dispatch` 行为不变。

---

## 5. 「接管」那半还没做

目标陈述第 4 条说的是「**重新接管**（有存活心跳）**或**被明确标记为 `abandoned`」。

**本轮做了后半句**。前半句（「接管」）是什么：

> 一个 `working` 任务在重启后，如果**原进程还活着**（比如它跑在另一个 rpi 进程里），
> 谁来继续驱动它？

**当前状态：没人接管。** 重启后：
- 任务被标记 `abandoned`（如果超过阈值）
- 或者保持 `working`（如果还新鲜，但**没有任何进程在驱动它**）——**这是个小的不一致**

**两种可能的解法**：

| 方案 | 做法 | 代价 |
| --- | --- | --- |
| **A. 认了** | 明确「重启后 running 任务一律 abandoned」——因为重启意味着原进程死了 | 需要把阈值降到很小（比如 1 分钟），否则会出现「保持 working 但没人驱动」的状态 |
| **B. 真接管** | 新进程启动时扫 `working` 任务，重新调度它们的 DAG 节点 | 需要 `Graph.task_id` 持久化 + 恢复调度逻辑——**这是 M2 的真正剩余部分** |

**我倾向 B**，但它要求 `Graph.task_id` 也持久化（当前只在内存），
所以 M2 剩下的工作是：**把图也落到 branch 上**。

**但这里有个更根本的问题**：图（`Graph`）是**会话内**的内存结构，
它包含节点的任务文本、验收命令等。如果要恢复调度，这些都得持久化——
**那就基本是把整个 orchestrator 持久化了**。

**这是本项目最大的未决设计问题**，我留给第十一轮审计判断（§7.2 第 1 条）。

---

## 6. 现在的边界

### 6.1 能做的

| 能力 | 状态 |
| --- | --- |
| 任务落盘、跨重启读回 | ✅ |
| 失败原因贯通到任务 | ✅ |
| plan↔task 绑定、dispatch 自动更新 | ✅ |
| 多任务并存、各自独立折叠 | ✅ |
| 工具引导、模型知道何时用 | ✅ |
| **僵死任务被标记 abandoned 且持久化** | ✅ **本轮** |
| **运行中的心跳**（长任务不被误判） | ✅ **本轮** |

### 6.2 还做不到的

| 缺口 | 影响 |
| --- | --- |
| **重启后 `working` 任务没人接管** | 重启期间的任务实际死了，标记 abandoned 是「承认」而非「恢复」 |
| **`Graph.task_id` 只在内存** | 重启后 plan 要重新传 taskId；也阻碍接管 |
| **Dash 上看不到任务** | 目标陈述第 4 条的「你能看到它」只在工具输出里 |
| **任务不能被创建以外的入口驱动** | 飞书消息 → 自动开任务（M3） |
| **`nodes` 字段没渲染** | 「任务进行到第几步」看不到 |
| **整个图不持久化** | 接管的前置 |

---

## 7. 需要第十一轮审计判断的取舍

### 7.1 三个具体取舍

| # | 取舍 | 我的选择 |
| --- | --- | --- |
| 1 | **僵死阈值 30 分钟是否太长** | 倾向合适——子进程最长 30 分钟，加上运行中的心跳，30 分钟能区分「慢」和「死」。**但如果用户希望重启后立刻看到 abandoned，就得靠方案 B 的真接管** |
| 2 | **reconcile 写回会不会造成快照膨胀** | 实测 300 条 append 5ms、读回 1.4ms——**不会**。每个任务最多多一条 reconcile 快照 |
| 3 | **心跳线程用 `std::thread` 而不是 tokio** | 倾向 thread——插件的工具执行本来就跑在独立 worker 线程上（`tool_entry!` 宏），用 tokio 会带来 runtime 依赖 |

### 7.2 一个更大的未决问题（本轮最想被质疑的）

**「接管」要不要做，做的话怎么做到？**

我在 §5 说了：真接管需要**整个图持久化**，那基本是把 orchestrator 持久化了。

**另一个思路**（我还没想透，想听审计意见）：

> 也许**不该接管**。重启后 `running` 的任务，本来就是「进程死了」——
> 它的子进程也被杀了（`kill_and_reap` 杀进程树）。
> 所以「接管」在语义上是错的：**任务真的没在跑了**。
>
> 正确的做法是把它标成 `abandoned`，然后**让人决定要不要重跑**——
> 这恰好是 DeepSeek 的 `Ticket` stage `person/blocked` + 「人的消息让 blocked 任务复活」的语义。
>
> 也就是说：**我们的 `abandoned` 就是 DeepSeek 的 `blocked`**，而「复活」应该由
> **人重新发起**（发消息 / 重新 plan），而不是系统自动接管。

**如果这个推论成立，M2 就已经完成了**——不需要接管，只需要：
1. 阈值缩短（让重启后立刻可见）
2. 提供「重跑一个 abandoned 任务」的路径

**请第十一轮审计判断这个推论。**

### 7.3 需要复核的实现判断

| # | 判断 | 我的依据 |
| --- | --- | --- |
| 1 | **心跳用 entry timestamp 而非自己的时钟** | 读时间每次都一样，没有跨重启意义 |
| 2 | **只有 `working` 会 abandoned** | 等人和卡死必须分开 |
| 3 | **没有时间戳就不动** | 缺信息 ≠ 证据表明它死了 |
| 4 | **reconcile 写回而非只显示** | 否则下次读又变回 working |
| 5 | **心跳线程的停止在 `?` 之前** | 否则 session 切换会泄漏线程 |

---

## 8. 请第十一轮审计重点看

1. **§7.2 的推论**——「重启后不该接管，标 abandoned 就够了」——这是我最想被质疑的一处
2. **§1.2 的三个写入点**——有没有漏的时机？
3. **§1.4 的三条保守规则**——「缺信息就不动」对吗？
4. **§3.1 的线程泄漏修法**——停止顺序对吗？
5. **§5 的「接管」成本分析**——图持久化是不是真的必要？
6. **§6.2 的缺口清单**——下一件该做哪个
7. **§7.1/§7.3 的取舍与判断**

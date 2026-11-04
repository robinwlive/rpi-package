# 第十二轮审计的落实说明（供第十三轮审计）

> 对应审计报告：第十一轮审计（M2 + 孤儿子进程泄漏 + 「定时扫描 vs 越界扫描」的取舍）。
>
> **本轮采纳了「插件内定时扫描」这条路线**——它比「扫所有会话目录」好：
> 不越界、不碰 rpi 本身、只读本会话的数据。
>
> 同时修掉了**我自己引入的一个严重 bug**。

---

## 0. 结论先行

| 上一轮的取舍 | 本轮的选择 |
| --- | --- |
| 「孤儿清理依赖有人读任务」的缺口 | ✅ **插件内后台定时扫描**（每 5 分钟，只读本会话） |
| 方案 B 的越界问题 | ✅ **绕开了**——不枚举别的会话目录 |
| — | 🔴 **修掉一个我自己引入的 bug**：reaper 会杀掉本进程正在跑的子进程 |

**测试**：72 passed（原 71 + 1）、clippy 干净。

---

## 1. 采纳「插件内定时扫描」

审计提的 B（扫所有会话目录）有越界问题；**定时扫描没有这个问题**，因为：

- 它只读**本会话**的任务快照（`GetSessionBranch` 走宿主 API，不是自己读文件）
- 它不碰 rpi 的任何代码或目录
- 插件本来就有后台线程的先例（`rpi-im-message` 的飞书长连接）

### 1.1 实现

```rust
// 插件加载时启动
crate::task::start_sweep();

// session 结束时停止
if let Some(register_event) = api.register_event_handler {
    let _ = register_event(
        rpi_plugin_sdk::EventTag::SessionShutdown,
        on_session_shutdown,
        std::ptr::null_mut(),
    );
}
```

**停止机制**：`Arc<AtomicBool>` 标志 + 每秒检查一次（写快照仍是 5 分钟一次）。
`stop_sweep()` 设标志并 join 线程。

**间隔可覆盖**：`RPI_SUPERAGENT_SWEEP_SECS`（默认 300 秒）——既是测试手段，也是给 impatient 用户用的。

### 1.2 e2e：全程没有人读任务

```
造真孤儿（父退出，子在跑，状态 S）
写入带 pids 的任务快照
起一个带 sweep 的 rpi 进程，不做任何 prompt
  → 扫描后状态: Z（已清理）
```

**关键**：这次**没有调用 `superagent_task action=list`**——
孤儿是被后台线程清理的。这正是之前补不上的缺口。

### 1.3 `register_event_handler` 在 SDK 0.3.20 可用

我核实了：0.3.20 **没有** `register_plugin_lifecycle`，但**有** `register_event_handler`，
且 `rpi-im-message` 已经用 `EventTag::SessionShutdown` 做过同样的事（`stop_all_servers`）。有先例。

---

## 2. 🔴 修掉我自己引入的 bug

**这是本轮最重要的发现**，而且**只有写测试才能发现**。

### 2.1 症状

`reap_orphans` 会杀掉**本进程正在跑的子进程**。

宿主可能**并行**调用 `list` 和 `dispatch`（`agent_loop.rs:988` `execute_tool_calls_parallel`）。
这时 `list` 触发 reaper，读到任务快照里的 pid——那正是**当前这一波正在跑的子进程**——然后把它杀了。

**上一轮我加的 `if pid == own` 只排除了自己，没排除自己启动的子进程。**

### 2.2 修法

维护一个「本进程拥有的 pid」白名单：

```rust
static OWNED_CHILDREN: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

pub(crate) fn owns_child(pid: u32) -> bool {
    owned_children().lock().map(|c| c.contains(&pid)).unwrap_or(false)
}
```

reaper 遇到 owned 的 pid 就跳过。子进程结束后 `release_child` 移除（列表不能无限增长）。

### 2.3 测试抓到了更深的 bug

我写的测试：

```rust
#[test]
fn a_child_this_process_started_is_not_reaped() {
    // ...
    let reaped = reap_orphans(&[task]);
    assert!(reaped.is_empty(), "reaped a live child: {reaped:?}");
}
```

**第一次跑就红了**：

```
thread 'task::tests::a_child_this_process_started_is_not_reaped' panicked:
reaped a live child: [79039]
```

**根因**：`OWNED_CHILDREN.get()` 返回 `None`——因为 `OnceLock` **从未被初始化**。
我用了 `get()` 而不是 `get_or_init()`，所以白名单永远是空的。

**这意味着上一轮我加的保护完全失效**——而且如果没写这个测试，它会一直失效下去。

修法：

```rust
fn owned_children() -> &'static Mutex<Vec<u32>> {
    OWNED_CHILDREN.get_or_init(|| Mutex::new(Vec::new()))
}
```

**这条测试现在守着它。**

---

## 3. 另一个实现中发现的问题

`sweep` 线程调 `read_tasks()` 会走 `PluginApi.user_data`。

我核实过这在后台线程是安全的：`ActionBridge::resolve_handle(user_data)` 从全局 registry 查，
**任何线程都能用**（`actions.rs:851`），且 `CURRENT_INVOCATION` 是 thread-local，
后台线程没有 → 走 `resolve_handle` 路径 → 正确。

另外 `LazyLock` 不能用（MSRV 1.78 vs 需要 1.80），改用 `OnceLock`。

---

## 4. 现在的语义（完整版）

| 场景 | 实际 |
| --- | --- |
| 宿主正常退出 | `kill_and_reap` 杀进程树 → 无孤儿 |
| 宿主崩溃，子进程存活 | 子进程成孤儿 → **后台扫描（5 分钟内）或下次读任务时被杀** |
| 宿主崩溃，任务状态 | 超 30 分钟变 `abandoned`；否则仍是 `working` |
| `list` 与 `dispatch` 并行 | 本进程的子进程**不会被 reaper 误杀** ✅ |
| 插件卸载 / 会话结束 | `SessionShutdown` 事件停掉扫描线程 ✅ |

---

## 5. 本轮改动的文件清单

| 文件 | 改动 | 行数 |
|---|---|---|
| `src/task.rs` | `sweep_orphans` / `start_sweep` / `stop_sweep` / `sweep_interval_secs`；`reap_orphans` 改用 owned 白名单；`_pids` 内部参数；1 个测试 | +90 |
| `src/dispatch.rs` | `OWNED_CHILDREN` + `owned_children` / `remember_child` / `release_child`；`on_started` 注册 owned | +45 |
| `src/lib.rs` | 插件入口启动 sweep；注册 `SessionShutdown` 钩子 | +18 |
| `00-GOALS-AND-DECISIONS.md` | M2 状态 + 定时扫描记录 + bug 修复记录 | +12 |
| `AUDIT-RESPONSE-12.md` | 本文件 | — |

**测试**：72 passed，clippy 干净。回归：`plan`+`dispatch` 行为不变。

---

## 6. 仍未做

| 缺口 | 状态 |
| --- | --- |
| **Dashboard 上看不到任务** | ⬜ 未做 |
| **`Graph.task_id` 只在内存**（重启后 plan 要重传 taskId） | ⬜ 未做 |
| **图（节点、验收命令）不持久化** | ⬜ 未做 |
| **任务不能被创建以外的入口驱动**（飞书 → 自动开任务） | ⬜ 未做（M3） |
| **`nodes` 字段没渲染** | ⬜ 未做 |
| **扫描只覆盖本会话** | ✅ 有意为之（不越界）。代价：**别的会话的孤儿不会被清** |

---

## 7. 需要第十三轮审计判断的取舍

### 7.1 三个具体取舍

| # | 取舍 | 我的选择 |
| --- | --- | --- |
| 1 | **扫描间隔 5 分钟** | 偏长但够用——孤儿的危害是占资源，不是破坏数据。**可被 `RPI_SUPERAGENT_SWEEP_SECS` 覆盖** |
| 2 | **扫描只覆盖本会话** | 有意为之。**别的会话的孤儿要靠那个会话下次被打开时清理**——这是个真实的缺口（见 §7.2） |
| 3 | **owned 白名单只增不删 vs 结束时删** | 结束时删。列表不该无限增长 |

### 7.2 一个仍然存在的缺口（请重点看）

**别的会话的孤儿不会被清理。**

场景：会话 A 崩溃留下孤儿 → 用户再也没打开会话 A → 那个孤儿一直跑。

**可能的解法**：

| 方案 | 做法 | 代价 |
| --- | --- | --- |
| A. 接受 | 孤儿只影响它自己的会话 | 用户不打开就没人知道 |
| B. 插件加载时扫所有会话目录 | 找所有 `.rpi/sessions/**` 里的 task 快照 | **越界**——插件读用户会话目录 |
| C. 系统级的孤儿清理 | 交给 rpi 宿主（它知道所有会话） | 改 rpi，超出本项目范围 |

**我倾向 A**，理由：B 越界，C 超范围。但如果审计认为这个缺口不可接受，
那说明**孤儿清理本身就不该由插件负责**——它该是宿主的能力
（rpi 已经有 `ProcessTree` 的概念，只是没有跨进程的登记）。

**请第十三轮审计判断**：这个缺口可接受吗？还是说孤儿清理应该整个推给宿主？

### 7.3 需要复核的实现判断

| # | 判断 | 依据 |
| --- | --- | --- |
| 1 | **后台线程用 `PluginApi.user_data` 调 action 是安全的** | `resolve_handle` 从全局 registry 查，任何线程可用 |
| 2 | **`SessionShutdown` 是停止扫描的正确钩子** | `rpi-im-message` 用同样的事件停它的服务器 |
| 3 | **owned 白名单用 `get_or_init` 而非 `get`** | 测试抓到 `get()` 导致白名单永远为空 |
| 4 | **reaper 不杀 owned pid** | 宿主可能并行调用 `list` 和 `dispatch` |
| 5 | **`_pids` 用下划线前缀，不进模型 schema** | 模型不该指定进程 id——写错会误杀无关进程 |

---

## 8. 请第十三轮审计重点看

1. **§2.2~2.3 的 bug 与修法**——「只有写测试才能发现」这个判断对吗
2. **§1 的定时扫描**——实现方式合理吗（`register_event_handler` + `AtomicBool` + join）
3. **§1.3 的 SDK 核实**——`register_event_handler` 是不是正确的停止钩子
4. **§4 的语义表**——准确吗
5. **§7.2 的缺口**——可接受吗，还是该推给宿主
6. **§6 的缺口清单**——下一件该做哪个
7. **§7.1/§7.3 的取舍与判断**

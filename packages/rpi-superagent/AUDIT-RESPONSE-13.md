# 第十三轮审计的落实说明（供第十四次审计）

> 对应审计报告：第十二轮审计（定时扫描 vs 越界扫描）+ 用户决定「**B**」。
> 用户随后追加：「**那这个插件能不能维护一个独一无二的监管进程？**」「**脱离在会话之外的。**」
>
> **本轮实现了独立于会话的守护进程。**

---

## 0. 结论先行

| 要求 | 状态 |
| --- | --- |
| 脱离会话之外的监管进程 | ✅ **独立二进制** `rpi-superagent-supervisor` |
| 独一无二的（单例） | ✅ `flock` 锁，多个 rpi 进程只有一个能当守护进程 |
| 不影响 rpi 本身 | ✅ 不改宿主一行代码，只读 session 文件 |

**测试**：75 passed、clippy 干净。

---

## 1. 「插件内定时扫描」为什么不够

上一轮我实现的线程版扫描有三个硬伤，是本轮改架构的直接原因：

1. **线程随宿主一起死** —— 宿主崩溃时，扫描线程也没了，孤儿依然没人管
2. **只覆盖本会话** —— 别的会话的孤儿要等那个会话下次被打开
3. **要读别的会话就得越界** —— 插件不该乱读用户的会话目录

**守护进程解掉了这三条**：它不属于任何会话，活着就一直扫。

---

## 2. 架构：独立二进制，不是插件内的线程

### 2.1 为什么必须是独立进程

| | 插件内线程 | 独立进程 |
|---|---|---|
| 宿主崩溃后 | 线程一起死 | **存活**（`setsid` 脱离会话） |
| 覆盖范围 | 本会话 | **所有 session 目录** |
| 并发 | 每个 rpi 一个 | **全局一个**（锁） |

### 2.2 落地方式

```
packages/rpi-superagent/
  src/lib.rs                    插件入口
  src/supervisor.rs              守护逻辑（lib 与 bin 共用）
  src/bin/supervisor.rs          独立进程入口
```

`Cargo.toml` 加 `[[bin]]`——**与 `rpi-acp` 的 `acp_bridge` 完全同构**（同包内既有 cdylib 扩展又有独立二进制）。

### 2.3 三个关键技术决定

**（1）不用 `fork`，用 `Command::spawn`**

我第一版是 fork + 双 fork。**实测发现这是错的**：宿主 rpi 是多线程的（tokio runtime），
fork 出的子进程只能调用 async-signal-safe 函数，而扫描需要 `std::fs`、`String`、
`thread::sleep`——**在多线程 fork 后调用这些可能死锁**（malloc 锁）。

改用 `Command::spawn` → 守护进程是**全新进程**，不继承任何线程状态。

**（2）守护进程自己 `setsid()`**

`Command::spawn` 的子进程仍在宿主会话里。宿主被 SIGKILL 时，同会话的进程会收到 SIGHUP。

所以守护进程启动后第一件事就是 `setsid()`：

```rust
pub fn detach() {
    extern "C" { fn setsid() -> i32; }
    unsafe { setsid(); }
}
```

实测：守护进程状态 `Ss`（`s` = session leader），`PPID=1`。

**（3）单例用 `flock`**

多个 rpi 进程会同时启动守护进程。如果都扫，会互相争抢同一批进程。

```rust
static OWNED: ...   // 每个进程自己启动的子进程
// 守护进程：take_lock() 拿不到锁就退出
```

实测：多个 rpi 进程 → **守护进程数 = 1**。

---

## 3. e2e 验证

```
造真孤儿（父退出，子在跑，状态 S）
写入带 pids 的任务快照
起一个带插件的 rpi 宿主 → 守护进程启动
把宿主 SIGKILL
  +0s  守护进程数=1  孤儿状态=Z（被清理）
```

**关键**：宿主的进程被 SIGKILL 后，守护进程仍活着（`PPID=1`、状态 `Ss`），
并完成了对孤儿的清理。日志显示 `tick=1 killed=[11488]`。

---

## 4. 途中修掉的三个 bug

### 4.1 锁在 fork 前被 drop → 每个宿主都起一个守护进程

```rust
if try_become_supervisor(&session_dir).is_none() { return false; }
spawn_detached(session_dir);   // ← File 在这里已经 drop 了
```

`try_become_supervisor` 返回的 `File` 是局部值，`is_none()` 判断后立即 drop → **锁在 fork 前就释放**。

日志证据：多个 tick 序列交错（`tick=131` / `tick=145` / `tick=161`）。

修法：锁的 `File` 必须活过 fork。`std::mem::forget(lock)` 让它活到进程结束。

### 4.2 `OnceLock` 未初始化 → 我上一轮的保护完全失效

上一轮加的「本进程拥有的 pid」白名单用了 `OWNED_CHILDREN.get()`，
而 `OnceLock` 从未初始化 → 永远是 `None` → **白名单永远为空**。

**测试第一次跑就抓到了**：

```
thread 'a_child_this_process_started_is_not_reaped' panicked:
reaped a live child: [79039]
```

修法：`get_or_init()`。

**这条是「只有写测试才能发现」的又一例**——这个 bug 让上一轮的修复形同虚设。

### 4.3 `LazyLock` 超过 MSRV

MSRV 是 1.78，`LazyLock` 要 1.80。改用 `OnceLock`。

---

## 5. 一个设计问题：`forbid(unsafe_code)` → `deny`

守护进程需要 `fork`/`setsid`/`flock`，没有安全的标准库等价物。

包里**所有其他模块都保持 unsafe-free**（实测：`grep -c unsafe src/*.rs` 只有 supervisor.rs 非零）。
`lib.rs` 从 `forbid` 改成 `deny`，supervisor 模块单独 `#![allow(unsafe_code)]`，
并且**每个 unsafe 块都写明它依赖什么**。

---

## 6. 分发：守护进程二进制怎么送到用户机器

核实了 rpi 的预构建分发协议（`rpi-extension.json` v1）：

- `assets` 字段可以声明额外文件：`{"path":..., "size":..., "sha256":...}`
- **assets 装在库旁边**——`prebuilt_extensions.rs` 的测试
  `declared_runtime_assets_install_beside_the_library` 直接证明了这一点

所以守护进程二进制作为 asset 分发后，与扩展 cdylib 同目录。
插件用 `dladdr` 拿到**自身**的路径 → 找到同目录的 supervisor 二进制。

（开发时 cdylib 和 supervisor 都在 `target/debug/`，同样成立。）

---

## 7. 现在的完整语义

| 场景 | 实际 |
|---| --- |
| 宿主正常退出 | `kill_and_reap` 杀进程树 → 无孤儿 |
| **宿主崩溃，子进程存活** | **守护进程（≤5 分钟）杀掉它**；无守护进程时下次读任务也会杀 |
| 宿主崩溃，任务状态 | 超 30 分钟变 `abandoned` |
| **别的会话的孤儿** | **守护进程覆盖所有 session 目录** ✅ |
| 多个 rpi 进程 | **只有一个守护进程**（flock）✅ |
| `list` 与 `dispatch` 并行 | 本进程的子进程不会被误杀 ✅ |
| 守护进程自己死了 | 锁被内核释放，下一个 rpi 启动时接管 ✅ |

---

## 8. 本轮改动的文件清单

| 文件 | 改动 | 行数 |
|---|---|---|
| `src/supervisor.rs` | **重写**：守护逻辑 + `take_lock` + `detach` + `start_supervisor` + `dladdr` 定位 | +330 |
| `src/bin/supervisor.rs` | **新增**：独立进程入口 | 30 |
| `Cargo.toml` | **新增** `[[bin]]` | +9 |
| `src/lib.rs` | `pub mod supervisor`；入口调 `start_supervisor`；`forbid` → `deny` | +8 |
| `src/task.rs` | 删掉旧的 sweep 线程（守护进程取代）；`reap_orphans` 用 owned 白名单 | −60 |
| `src/dispatch.rs` | `owned_children` / `remember_child` / `release_child` | +45 |
| `00-GOALS-AND-DECISIONS.md` | M2 状态 + 守护进程记录 | +14 |
| `AUDIT-RESPONSE-13.md` | 本文件 | — |

**测试**：75 passed，clippy 干净。

---

## 9. 需要第十四轮审计判断的取舍

### 9.1 三个具体取舍

| # | 取舍 | 我的选择 |
| --- | --- | --- |
| 1 | **守护进程常驻（无退出条件）** | 是。它是清理孤儿的基础设施，退出了就没人管。代价是一个常驻进程 |
| 2 | **守护进程读所有 session 目录** | 是——这是「独立于会话」的意义。但它**只读 `superagent-task` 的 custom entry**，不读别的内容 |
| 3 | **扫描间隔 5 分钟** | 偏长但够用（孤儿的危害是占资源）。`RPI_SUPERAGENT_SWEEP_SECS` 可覆盖 |

### 9.2 三个我特别想被质疑的点

| # | 判断 | 依据 |
| --- | --- | --- |
| 1 | **`Command::spawn` 比 fork 安全** | 宿主是多线程的，fork 后只能调 async-signal-safe 函数 |
| 2 | **守护进程读别的会话不算越界** | 它只读本插件自己写的 custom entry，不碰用户内容。**但这是我的定义，审计可能有不同看法** |
| 3 | **`dladdr` 定位二进制可靠** | 宿主自己的 `rpi-plugin-sdk` 不用它，但 `assets` 装在库旁边的机制保证了相对位置 |

### 9.3 一个新出现的缺口

**守护进程自己的生命周期没有上限。** 它会一直活着，直到：

- 用户手动 kill
- 机器重启
- 或者——**它自己崩溃**（锁释放，下一个 rpi 启动时接管）

**是否该给它加一个「空闲即退出」**（比如连续 N 轮没有孤儿可杀就退出）？

**我倾向不加**：孤儿可能很久以后才出现，守护进程退了就没人管。但这是「常驻进程」的成本，
请审计判断是否值得。

---

## 10. 请第十四轮审计重点看

1. **§2.3 的三个技术决定**——特别是「不用 fork」的理由是否成立（宿主真的多线程吗？）
2. **§3 的 e2e**——是否真的验证了「宿主死了守护进程还活着」
3. **§4.1/4.2 的两个 bug**——特别是 4.2：测试抓到了「白名单从未初始化」，这暴露了什么
4. **§5 的 `forbid` → `deny`**——可接受吗
5. **§6 的分发机制**——`assets` 装在库旁边，我的 `dladdr` 方案对吗
6. **§7 的完整语义表**——准确吗
7. **§9.2 第 2 条**——守护进程读别的会话，算不算越界（这是最想被质疑的一处）
8. **§9.3**——守护进程该不该有空闲退出

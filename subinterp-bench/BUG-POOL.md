# 全局 ReferencePool 的跨解释器 decref

> **状态:未修复。** 提交 [`73f61bb`](#四修复尝试-73f61bb--无效) 声称修了它,**没修**。
> 那条提交的保守分支在真实复现里**一次都没有执行过**,是死代码。
> 别看到它就以为这块处理过了 —— 这份文档存在的一半理由就是防止那个误会。

| | |
|---|---|
| 分支 | `subinterp-per-interpreter-state` |
| 命中文件 | `src/internal/state.rs`(`ReferencePool`) |
| 引入者 | **上游**,不是本分支。本分支只是把它逼到了会崩的场景 |
| 复现脚本 | [`pool_soak.py`](pool_soak.py) |
| 失败的修复 | `73f61bbe884d709c62e0dc7bd7efe598e38ed4ce`(父提交 `669dead`) |
| 平台 | macOS ARM64 / Python 3.14.6 / polars 1.43.2。**Linux 上不 abort**,见第六节 |

---

## 一、它是什么

`Py<T>` 的 `Drop` 在线程**没有** attach 时不能直接 decref —— 动 refcount 要持有那个
解释器的锁。于是它把这次 decref 塞进一个队列,等下一次有人 attach 时再补上:

```rust
static POOL: OnceLock<ReferencePool> = OnceLock::new();     // ★ 进程级,一个
```

这个池子是**进程级**的,而 PEP 684 之后一个进程里有 N 个解释器。冲刷发生在
`SuspendAttach::drop`(`state.rs:413`)—— **谁先回来谁就把整池子倒干净**,不问每条
引用属于谁:

```
解释器 A 的线程        Py<Foo> 掉了 → 进池子
解释器 B 的线程        detach 回来 → 冲刷 → 把 A 的 Foo 在 B 里 Py_DECREF
                                             ↓
                       归零 → dealloc → free 到【B 的 arena】
                       而这块内存是【A 的 arena】分配的
```

两件坏事,一起发生:

1. **内存损坏** —— 指针不在这个 arena 里。macOS 的 libmalloc 当场 abort,
   glibc 不 abort,静默损坏堆。
2. **无同步的 refcount 写** —— A 正持着自己的 GIL 在跑,B 在改 A 的对象的
   refcount。abort 是走运的那一面,不崩的时候它也是错的。

这个隐患在本分支的 `AssumeAttached` 文档注释里被点过一句("池子里的引用属于当初
把它们塞进去的那个解释器"),**在那一处绕开了,没有顺着追到根**。压测追到了。

## 二、为什么是 polars 把它逼出来的

polars 的每一次 `collect` / `write_*` 都走 `enter_polars`,它的第一件事就是
`Python::detach` —— 也就是**每次运算都冲刷一遍全局池子**。同时 rayon / tokio 的
worker 线程在往池子里塞。生产者和消费者都是满速的。

崩溃现场(本机 lldb,`bt 20`,截取):

```
frame #5   ___BUG_IN_CLIENT_OF_LIBMALLOC_POINTER_BEING_FREED_WAS_NOT_ALLOCATED
frame #6   Python`_PyObject_Free
frame #7   Python`PyObject_ClearManagedDict
frame #8   Python`subtype_dealloc
frame #9   Python`_Py_Dealloc
frame #11  drop_deferred_references          at state.rs:308      ← 池子在这儿倒
frame #12  drop                              at state.rs:413      ← SuspendAttach::drop
frame #14  detach<...enter_polars...>        at marker.rs:572
frame #15  enter_polars                      at utils.rs:147
frame #17  collect                           at general.rs:616
```

**这条栈里没有 polars 的 bug。** polars 只是调用频率足够高的第一个真实使用者。

---

## 三、怎么复现

### 前置:两个 polars 包

编法见 [`POLARS.md`](POLARS.md)「怎么编」一节 —— 关键是 `cargo update -p pyo3` 之后
确认 `Cargo.lock` 里 pyo3 那条**没有 `source =` 行**,否则 patch 被判为 unused,
构建照样成功但用的还是上游。

本文档的数据来自两份包:

```
/tmp/pl_pkg     本分支 669dead(修复尝试之前)
/tmp/pl_fixed   本分支 73f61bb(修复尝试之后)
```

### 跑

```bash
# 被测:8 个 own-GIL 子解释器,各自 write_csv / write_ipc 到自定义 file-like,20 秒
python3.14 subinterp-bench/pool_soak.py /tmp/pl_fixed 8 20 write ; echo "exit=$?"

# 对照:同一个包、同一份负载,只把解释器数改成 1
python3.14 subinterp-bench/pool_soak.py /tmp/pl_fixed 1 20 write ; echo "exit=$?"
```

拿栈:

```bash
printf 'run\ncontinue\nbt 20\nquit\n' > /tmp/lldb.txt
lldb -b -s /tmp/lldb.txt -- python3.14 subinterp-bench/pool_soak.py /tmp/pl_fixed 8 20 write
```

### 判据是怎么设计的 —— 三个都是为了排除一种"第二解释"

| 做法 | 排掉的另一种解释 |
|---|---|
| **不销毁任何子解释器** | "是不是 teardown 的问题" —— 全程没有一个解释器被关 |
| **干完立刻 `os._exit(0)`** | "是不是 finalize 期的问题" —— 根本不进 finalize。崩就一定是运行中崩的 |
| **N=1 同包同负载对照** | "是不是 use-after-free / polars 自己的线程模型" —— 那些在 N=1 也该犯 |

第三条是最硬的那一条:**唯一的变量是解释器数量。**

### 结果(本机,firsthand)

| 包 | N | 模式 | 结果 |
|---|---:|---|---|
| `/tmp/pl_pkg`(修复前) | 8 | write | **6/6 SIGABRT**,1.0–4.1 秒内 |
| `/tmp/pl_fixed`(修复后) | 8 | write | **3/3 SIGABRT**(我跑的)+ **10/10**(验证 agent 跑的) |
| `/tmp/pl_fixed` | 1 | write | **2/2 干净**,89,272 次写,结果与解析解全部相符 |
| 任意 | 8 | collect | 3/3 存活 —— 触发要**回调 Python**,纯计算不够 |
| 任意 | 2 | write | 3/3 存活 —— 触发下界在 **N≥4** |

**触发条件比最初报告的窄:≥4 个解释器 + 有 Python file-like 回调。**
这一条自己就说明为什么它躲过了所有已有测试。

---

## 四、修复尝试 `73f61bb` —— 无效

```
73f61bbe884d709c62e0dc7bd7efe598e38ed4ce
  fix(sync): tag deferred decrefs by interpreter and only release matching ones
  src/internal/state.rs          +87 -7
  src/sync/interpreter_handle.rs  +1 -32
```

它做的事:池子每条目从 `NonNull<PyObject>` 变成
`(*mut PyInterpreterState, NonNull<PyObject>)`,归属在 `register_decref` 里读;
冲刷时只放掉属于当前解释器的,其余放回队列。为了不给单解释器进程加钱,加了
`first_interp` / `multiple_seen` 两个原子 —— 只有真见到第二个解释器才切到分拣模式。

### 它为什么没用

崩溃瞬间读 `POOL` 的内存(lldb,符号 `..._pyo3_internal_state_POOL`,本机 firsthand):

```
POOL 载入地址 = 0x127b41728
  +0x30  first_interp    = 0x0000000000000000
  +0x38  dirty           = 0
         multiple_seen   = 0        <<<<<<
```

`multiple_seen` **是 0**。分拣分支一次都没进过 —— 也就是说,打了这个补丁的二进制,
在这条路径上和没打**逐字节等价**。5 次独立采样,5 次都是这个。

### 根因:探针取不到值

```rust
pub(crate) fn current_interpreter_or_null() -> *mut ffi::PyInterpreterState {
    let tstate = unsafe { ffi::PyGILState_GetThisThreadState() };   // ← 这里返回 NULL
    ...
}
```

`PyGILState_GetThisThreadState` 读的是 **GILState 的 TSS**,而那个 TSS **只有**
经 `PyGILState_Ensure` 或由 Python 自己创建的线程才会登记。

`register_decref` 触发在哪儿?607 次断点采样,**全部**落在 `tokio-rt-worker` 上
(192 次来自 `register_decref`,415 次来自 `InterpreterHandle::attach`)。那些线程
是 tokio 建的,没经过 `PyGILState_Ensure`,**TSS 是空的**。于是每次
`note_interpreter` 拿到的都是 NULL,`first_interp` 永远填不上,`multiple_seen`
永远翻不开。

### 讽刺的地方

那些 worker 线程是**我自己的 `InterpreterHandle::attach` 挂上去的**,它走的是
`PyThreadState_New` + `PyEval_RestoreThread` —— **不写 GILState TSS**(那正是它
相对 `PyGILState_Ensure` 的全部意义:不绑主解释器)。

**我加的原语让线程 attach 得"GILState 看不见",然后我拿 GILState 去问这个线程在
哪个解释器。** 旁证:`interpreter_handle.rs` 里那句
`assert!(current.is_null(), "...attached to a different interpreter...")`
在这些线程上从来没炸过 —— 因为它问到的一直是 NULL。

### 现在分支里留着什么

| | |
|---|---|
| `note_interpreter` / `first_interp` / `multiple_seen` | 有代码,**0 条执行数据** |
| `drop_deferred_references_slow` 的分拣分支 | 有代码,**0 条执行数据** |
| 上游 855 个单测 | 全过 —— 但它们几乎全是单解释器,**碰不到这条分支** |

**未验证 + 未覆盖 + 提交信息声称已修。** 三样凑齐,比没有这段代码更糟。

---

## 五、为什么这个方向答不出来

> 归属信息在 drop 那一刻是取不到的,**因为那一刻线程根本没有 thread state ——
> 而那正是这个对象进池子的原因**。

`PyInterpreterState_Get` 更不行:没有 tstate 时它是 fatal,不能拿来"问"。
所以任何**线程侧的探针**在这里都是死路,换一个 API 不解决问题。

剩下两条路,都要求归属在**入池那一刻之前**就已经在手上:

```
① Py<T> 自己带解释器      Py<T> 从 1 个指针变 2 个,全生态每个对象 +8 字节
② 池子按解释器分开        入池时仍然要知道归属 → 还是回到 ①
```

**没有便宜的修法。** `InterpreterHandle` 已经在做①的事情(在**诞生**那一刻记),
只是它现在只覆盖用户显式调用的路径,不覆盖 `Py<T>::drop`。

---

## 六、两条别踩的坑

**Linux 上"看崩不崩"不是判据。** glibc 的 `free` 不会因为指针不属于本 arena 而
abort —— 它静默损坏堆。同一份复现在 Linux 上要挂 ASAN 或 valgrind 才看得见,
裸跑是"通过"。

**`PYTHONMALLOC=malloc` 会让 abort 消失,而 bug 还在。** 这和 [`POLARS.md`](POLARS.md)
里那条一样:判据不是"跑完没崩",是"这次 decref 发生在哪个解释器"。

---

## 七、它为什么躲过了已有的全部测试

按 [`METHOD.md`](METHOD.md) 的规矩,修完(或修不动)都要问这一句:

```
leak.py / reclaim.py    串行,一次只活一个解释器      → 没有并发冲刷
stress.py               并发,但纯计算,不回调 Python  → 池子里没有别人的对象
上游 855 个单测          单解释器                     → multiple_seen 天然为 0
matrix.py               只问"能不能加载/隔离到位"      → 不跑长负载
```

**这套基准的每一项都在"把机制用起来",而这个 bug 在"两个解释器同时把它用坏"里。**
它需要:≥4 个解释器 + 各自持续跑 + 有 Python 回调制造 detach 风暴 + 不销毁不退出。
这四个条件的交集,已有的脚本一个都不覆盖。`pool_soak.py` 就是补这个交集的。

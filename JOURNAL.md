# 定制版 PyO3 · 工作日志

> 分支 `subinterp-per-interpreter-state` · 基于 PyO3 main 的 `dfdbc46`(0.29.2 之后)
> 远端 `github.com/leocaolab/pyo3`
> 这份文档记录**这个定制版本身**:改了什么、为什么、量到了什么、还差什么。
> 那次调试过程的复盘在 [`RETRO-subinterp.md`](RETRO-subinterp.md),基准在 [`subinterp-bench/`](subinterp-bench/)。

## 一句话

**让 PyO3 扩展能在 own-GIL 子解释器(PEP 684)里真正并行**,而不是只是"能加载"。

上游的问题不是缺功能,是**存储位置错了**:`#[pyclass]` 的类型对象、`create_exception!` 的
类型对象、以及模块对象本身,都缓存在**进程级** static 里。于是 N 个子解释器共用一份,
引用计数被并发改写。上游的应对是加一道守卫,直接拒绝第二个解释器。

这个分支把这三处改成**每解释器一份**,守卫随之失去存在理由。

---

## 怎么用

```toml
# 你的 Cargo.toml
[patch.crates-io]
pyo3 = { path = "/path/to/leocaolab-pyo3" }   # 或 git = "...", branch = "subinterp-per-interpreter-state"
```

crate 名仍是 `pyo3`,所以 `#[pyclass]` 宏、maturin、以及依赖链上任何用 pyo3 的第三方
crate 都原样工作。**不要改名** —— 改名会丢掉整个生态兼容性,换不来任何东西。

### ⚠️ 一个必踩的坑:`patch.unused`

如果你的 `Cargo.lock` 把 pyo3 钉在比本分支**更低**的补丁版本(比如 0.29.0),cargo
不会为了用 patch 去升级已锁定的版本,而是把它标成:

```
[[patch.unused]]
name = "pyo3"
version = "0.29.2"
```

**构建照常成功,但用的还是上游。** 症状是"打了 patch 但行为没变",而且第二次
`cargo build` 会在零点几秒内 `Finished`。加 patch 之后必须:

```bash
cargo update -p pyo3
```

然后确认 `Cargo.lock` 里 pyo3 那条**没有 `source =` 行**(有 source 就是 registry 版)。

### macOS 上裸 `cargo build`

maturin 会带 `-undefined dynamic_lookup`,`cargo build` 不带,会报一大串
`Undefined symbols: _PyBaseObject_Type ...`。手动编时:

```bash
RUSTFLAGS="-C link-arg=-undefined -C link-arg=dynamic_lookup" cargo build --release
```

---

## 改了什么

只有五个文件,其余全是 `subinterp-bench/`。

### 1. `src/sync/per_interpreter.rs`(新增)

`PerInterpreterCell<T>` —— 每解释器一份的存储。

每个 cell 从全局计数器领一个下标;每个解释器持有一个扁平 `Vec<Option<Slot>>`,
放在该解释器 dict 里的一个 `PyCapsule` 中,CPython 在 finalize 时清掉它,所以值
不可能活过创建它的解释器。

**热路径**是"一次 FFI 拿解释器指针 + 一次线程局部比较 + 一次数组下标":

```rust
let interp = ffi::PyInterpreterState_Get();
let generation = GENERATION.load(Acquire);
let (c_interp, c_gen, base, len) = CACHE.with(Cell::get);
if c_interp == interp && c_gen == generation { return Some((base, len)); }
```

用**指针**而不是解释器 id 当 tag,省掉一次 `PyInterpreterState_GetID`。指针会被
CPython 复用,所以配了个全局代号计数器:registry 被 drop、或它的 Vec 扩容时递增,
把所有线程的缓存一次作废。

### 2. `src/impl_/pyclass/lazy_type_object.rs`

`value` 和 `fully_initialized_type` 两个缓存从 `PyOnceLock` 换成 `PerInterpreterCell`。

### 3. `src/exceptions.rs`

`create_exception!` 宏里那个 `static TYPE_OBJECT` 同样换掉(8 行)。异常类型也是堆类型,
共享同样会竞争引用计数。

`src/types/` 里剩下的 `PyOnceLock<Py<PyType>>` 缓存**没动** —— 它们持有的是 CPython 的
内建类型(list / float / int / range / memoryview / super / code),实测跨 4 个子解释器
全部共享且**永生**,引用计数饱和不动,共享是安全的。

**判据(可执行)**:持有 PyObject 吗?→ 是永生的吗?→ 是堆类型吗?**只有堆类型要改。**

### 4. `src/impl_/pymodule.rs`

三处:

- `module: PyOnceLock<Py<PyModule>>` → `PerInterpreterCell`。一个 `Py<PyModule>` 属于
  创建它的解释器,把它缓存在进程级正是守卫存在的原因。
- 删掉 `interpreter: AtomicI64` 字段和那段 `ImportError: PyO3 modules do not yet
  support subinterpreters`(pyo3#576)。
- 模块 slots 里声明 `Py_mod_multiple_interpreters = Py_MOD_PER_INTERPRETER_GIL_SUPPORTED`。

**第三条只有在前两条成立时才是诚实的。** 少改一处,这个声明就是在骗 CPython。

Windows stable API on 3.9 那条分支保留 —— `PerInterpreterCell` 依赖
`PyInterpreterState_Get`,而它不在那个配置的 python3.dll 里。

### 5. `src/internal/state.rs`

两处,一处修正确性,一处修性能。

**`AssumeAttached` guard(正确性)。** `Py<T>` 的 `Drop` 只在 `thread_is_attached()` 为真时
decref,否则把 decref 塞进一个**进程级**延迟队列。而 `thread_is_attached()` 读的是 PyO3
自己的线程局部计数,只有 PyO3 主动 attach 时才 +1。registry 是被 CPython 直接调的
(capsule 析构 / atexit 钩子),计数是 0,于是每个 `Py<PyType>` 的 decref 都进了队列,
**永远没落到对象上** —— 2.6 MB/解释器,线性不收敛。

这个 guard 只加 attach 计数,**不冲刷延迟队列**。不能用现成的 `AttachGuard::assume()`,
它会顺手冲刷 —— 队列里的引用属于别的解释器,在子解释器 finalize 时释放它们等于
跨解释器改引用计数,修一个泄漏换来一个 use-after-free。

**TLS 地址缓存(性能)。** `ATTACH_COUNT` 原本每次 FFI 调用被访问两次(进 +1、出 −1),
dylib 里每次都走 `tlv_get_addr`。改成进来时取一次地址存进 guard,`Drop` 直接用。

---

## 量到了什么

两台机器:MacBook(M 系列 6P+12E,ARM64)、bluewhale(Ryzen 7840HS 8核16线程,
x86_64,Ubuntu 26.04)。Python 3.14。**对照一律是本分支的父提交 `dfdbc46`,不是发行版。**

### 能力

| | 父提交 | 本分支 |
|---|---|---|
| 6 个子解释器 import(带 override) | 6/6,但**类型对象只有 1 个地址** | 6/6,**6 个地址** |
| 6 个子解释器 import(**不带 override**) | 0/6 `ImportError` | **6/6** |
| 子模块(`wrap_pymodule!`) | 1/6,其余抛 pyo3#576 | **6/6** |
| 并发建对象 | **SIGSEGV / SIGABRT** | 正常 |

### 扩展性(12 worker 建对象)

| | 父提交 | 本分支 |
|---|---:|---:|
| MacBook | 1.20× | **9.54×** |
| bluewhale | 1.48× | **6.70×**(8 worker 时 6.95×,= 物理核数) |

机制:上游所有子解释器共用一个类型对象,每次实例化都动同一个引用计数,所有核抢
同一条缓存行。不走类型对象的路径(纯调用、detach 计算)两版差 <1%,该没差别的地方没差别。

### 内存

| | 父提交 | 本分支 |
|---|---:|---:|
| 泄漏(1000 个解释器串行) | +2.7 MB/千个 | **+0.3 MB/千个** |
| 回收率 · macOS | 48.1–48.3% | **51.3–51.5%** |
| 回收率 · Linux | 63.1–63.5% | **66.4–66.8%** |

回收率的上界是"根本不加载扩展"(macOS 51.4% / Linux 68.0%),本分支贴着它。

### 单次调用开销

| | 父提交 | 本分支 |
|---|---:|---:|
| FFI 边界税 · macOS | 2.06 ns | **1.48 ns** |
| FFI 边界税 · Linux | 4.23 ns | **2.75 ns** |
| 类型对象查找 · macOS | 2.22 ns※ | **1.35 ns** |
| 类型对象查找 · Linux | 3.42 ns※ | **2.04 ns** |

※ 这两格的"父提交"是本分支优化前的自己;上游那一格是 0.20–0.23 ns(一次原子读),
每解释器查找必然比它贵,这是隔离的代价。

单线程端到端**无回归**:裸调用 −2.9%(抖动 ±1.5%),建对象和方法调用在噪声内。

### 上游测试

`cargo test --lib --release`:**850 passed, 0 failed**,两台机器。

---

## 在 pyre 上的实测

`~/projects/pyre`(pyronova-engine 2.7.0,hyper + tokio + PyO3),6 个 own-GIL 子解释器:

```
上游 0.29.0  override  6/6   模块 6 个地址   ★ Request 类型 1 个地址
             strict    0/6   ImportError
★ 本分支     override  6/6   模块 6 个地址   Request 类型 6 个地址
             strict    6/6   模块 6 个地址   Request 类型 6 个地址
```

两条值得记的:

**① pyre 文档里"8 个子解释器 = 8 个不同 module 地址(真隔离)"只对了一半。**
模块确实隔离,**类型对象共用一个** —— 而 `PyronovaRequest` 是每请求都实例化的那个类型。

**② pyre 那张 kernel 表里"最硬"的一格,现在 PyO3 直接给。**
他们为了"严格模式直接过、无需 override"只能手写 raw C-API(`pyronova_request_type.rs`)。
`strict 6/6` 说明 PyO3 高层写法就能拿到那一格。

> 注意:这里的基线是 pyo3 **0.29.0**(pyre 锁定的版本),本分支基于 main。
> 版本有漂移,所以上面只能当**能力**判据,**不能当性能对比**。

---

## 没解决的

### 第二堵墙:进程级 C 全局态

```
ImportError: cannot load module more than once per process
```

这层**没有开关**,不是策略而是物理事实。numpy / orjson / lxml 属于这类,只能每 worker
一份物理副本。**不在 PyO3 这一层**,这个分支碰不到。

(pyre 的 `pyproject.toml` 依赖 `orjson>=3`,而 pyre 自己的文档记着它在第二个子解释器里
会 segfault —— 这条对 pyre 是实际阻塞。)

### 分支自身还欠的

- 79 处 cache site 里约 **20 处未做运行时判定**(`Py<PyAny>` / `Py<PyTzInfo>` / `Py<PyModule>` 等)
- 只验过 **Python 3.14**;Windows stable API on 3.9 走的是回退分支,**没测过**
- 上游测试套件里**没有一个用例能抓到**那个延迟 decref 泄漏 —— 我自己的压测也漏了它
- `subinterp-bench/stress.py` 有两个已知问题(断言冻结了旧探针的行为;泄漏斜率判据和
  它自己的总量互相矛盾),**未修,待定**
- **没有向上游提过** —— 提之前至少要补上"能防住这类回归的测试"

---

## 维护

### rebase 到上游新版

改动集中在五个文件,冲突面小:

```
src/sync/per_interpreter.rs             新增,不冲突
src/sync.rs                             两行导出
src/impl_/pyclass/lazy_type_object.rs   两个字段类型
src/exceptions.rs                       宏里 8 行
src/impl_/pymodule.rs                   字段 + 守卫 + slot
src/internal/state.rs                   guard + TLS 地址
```

rebase 之后**必跑**:`cargo test --lib`、`subinterp-bench/leak.py`、
`subinterp-bench/no_override.py`。前者防语义回归,后两者防这个分支特有的两类回归。

### 重跑基准时的硬要求

这些不是风格,每一条都对应一次量出假结论的经历(细节见 `subinterp-bench/README.md`
和各脚本的 docstring):

```
对照 = 本分支的【父提交】,不是 crates.io 发行版
    差一个上游提交就能造出 92% 的假回归

计时循环不留活对象
    [f() for _ in range(400_000)] 量的是 GC 遍历,不是构造

子解释器的计时窗口只包被测部分
    建解释器 + import 算进分母,会系统性低估 MI

每格重复取中位数并印跨度
    同一格五次能给出 0.91× 到 0.99×;拿一次当结论,
    就会为一个不存在的差异去找解释

免 GIL 下,harness 自己不能用闭包变量
    12 个线程共读一个闭包 cell,量到的是 harness 自己的争用,差 16 倍

崩溃如实上报,不吞
    对照组建对象会 SIGSEGV —— 那是结果的一部分
```

---

## 提交序

```
07b0178  feat(sync): per-interpreter storage for pyclass type objects
f6a86a4  fix(sync): give PerInterpreterCell a non-zero size
cc09cbc  fix(sync): release per-interpreter values at interpreter teardown
de6c9a3  fix(exceptions): per-interpreter storage for create_exception! type objects
b69abc8  docs: retrospective on the sub-interpreter isolation work
799ab51  perf(sync): native array + tagged thread-local base
a37a2bf  docs: restore the status section dropped while rewriting the perf one
7bf6212  bench: reproducible sub-interpreter benchmarks
7b90c8b  fix(sync): decref per-interpreter values instead of deferring them
033a009  bench: compare four parallel topologies
0310b03  bench: run every cell in its own process
4cc8c55  bench: find the free-threaded interpreter instead of hardcoding a path
d8d1dd2  bench: stop charging sub-interpreter setup to its throughput
3a51989  bench: repeat every cell and print the spread
eea2ce3  perf: one thread-local lookup per FFI call, one FFI call per type lookup
cd020f3  fix(pymodule): per-interpreter module object, and drop the guard it forced
3840e20  feat(pymodule): declare Py_MOD_PER_INTERPRETER_GIL_SUPPORTED
```

# 子解释器隔离:调试复盘

> 分支 `subinterp-per-interpreter-state` · 环境 Python 3.14.6 / macOS ARM64 / PyO3 0.29.2
> 所有数字都是本机实测。**八个假设被数据推翻,五个结论靠测量得出** —— 比例本身是这份复盘的主要内容。

---

## 起点

问题很小:**pyarrow 能不能在 own-GIL 子解释器里跑?** 因为一个"Rust 数据平面 + 进程内 Python UDF"的设计,需要 Python 侧能零拷贝地看到 Arrow 内存。

答案是不能,而顺着"为什么不能"往下走,最后落到了 PyO3 的一个可复现的段错误上。

---

## 一、三堵墙,而它们不是同一堵

实测 4 个 own-GIL 子解释器并发 import(override 已开):

| 库 | 第 1 个 | 第 2 个 | 报错 | 性质 |
|---|---|---|---|---|
| pyarrow 25.0.1 | ✅ | ❌ | `Interpreter change detected` | **Cython 守卫** |
| numpy 2.5.2 | ✅ | ❌ | `cannot load module more than once per process` | **进程级 C 全局态**(`m_size=0`) |
| polars 1.43.2 | ✅ | ❌ | `PyO3 modules do not yet support subinterpreters` | **PyO3 的 `wrap_pymodule!`** |
| json / 已适配的 stdlib | ✅ | ✅ | — | — |

第一条的意义比 pyarrow 本身大:**`Interpreter change detected` 是 Cython 生成代码里的 `__Pyx_check_single_interpreter()`,所有 Cython 编译的扩展共享同一命运** —— pandas、scipy、sklearn、lxml 都在这一类。

规模数据:pyarrow 126 MB / 21 个 `.so`,numpy 34 MB。副本方案的代价差 4 倍。

### ❌ 假设 1:硬链接可以替代物理副本

思路:`dlopen` 按路径去重 → 硬链接路径不同 → 独立加载;而页缓存按 inode → `.text` 共享。**磁盘 ≈ 0,内存只多 data 段。**

磁盘那一半是对的(4 份硬链接 34 MB,4 份真副本 136 MB)。但:

```
硬链接   1/4 加载成功    ImportError: cannot load module more than once per process
真副本   4/4 加载成功    ndarray 类型地址 4/4 个不同 ✅
```

**CPython/dyld 按真实路径或 inode 判定,硬链接被认成同一模块。** 假设作废。

顺带一个正面发现:**真副本给的是真隔离**(4 个不同的 `ndarray` 类型地址),比后面发现的 PyO3 原生路径还干净。内存代价也比预期小:1 份 numpy 58 MB → 4 份 100 MB,每多一份只 +14 MB。

---

## 二、polars 为什么挂 —— 连错两次

### ❌ 假设 2:polars 用的 PyO3 版本旧

`strings` 二进制:**`pyo3-0.29.0`**,和已知能跑的 kernel 同版本。作废。

### ❌ 假设 3:是 abi3 导致的

polars 的二进制叫 `_polars_runtime.abi3.so`,而能跑的 kernel 不是 abi3。看起来很有说服力。

编了两个最小模块对照,**abi3 和非 abi3 都 4/4 通过**。作废。

### ❌ 方法错误:用 `strings | grep` 当判据

我一直在 grep 二进制里有没有那句拒绝消息。**这个方法本身是无效的** —— 小模块里那段代码被 `-dead_strip` 剥掉了,而 polars 那个 184 MB 的没剥。**字符串在不在,和检查生不生效是两回事。**

**只有实际加载才是判据。**

### ✅ 真正的原因:`wrap_pymodule!`

报错里的行号(`polars-python/src/c_api/mod.rs:133`)指的是运行时注册子模块,不是 import。12 行复现:

```rust
#[pymodule] fn child(m: &Bound<'_, PyModule>) -> PyResult<()> { ... }

#[pymodule] fn mymod(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_wrapped(wrap_pymodule!(child))?;   // ★ 第 2 个子解释器起 panic
    Ok(())
}
```

```
主解释器          ✅
第 1 个子解释器   ✅        ← 触发条件不是"在子解释器里"
第 2 个子解释器起 ❌ panic  ← 是"进程内第二次注册同名子模块"
```

而且 **panic 跨 FFI 会直接杀掉进程**,不是可捕获的 Python 异常。

---

## 三、那个守卫是不是过时了 —— 数据说不是

PyO3 源码里的注释自认是权宜之计:

```rust
// Check the interpreter ID has not changed, since we currently have no way to guarantee
// that static data is not reused across interpreters.
// TODO: it should be possible to use the Py_mod_multiple_interpreters slot ...
```

把检查 patch 掉,跑起来**功能全对**。差点就下结论说"守卫多余"。

但多测了一列:

```
8 个 own-GIL 子解释器:
  module 地址        8/8 个不同   ✅ 隔离
  ★ #[pyclass] 类型  1/8         ★★ 共享
  refcount           8 → 10 → 12 → 14   (每加一个解释器 +2)
  tp_flags           HEAPTYPE=是,IMMORTAL=否,IMMUTABLETYPE=否
```

**8 个持有独立 GIL 的解释器,在并发改同一个非不朽堆类型的引用计数。** 而且这**不是 patch 出来的** —— 未经修改的生产二进制就是这样。

> **"能跑 + 结果对" ≠ "隔离了"。** 这一列不测,整个方向的结论会是反的。

### 后果是可复现的崩溃

```
上游 PyO3 0.29.2:
  8 并发 + 2000 次方法调用,无 queue         →  Abort trap: 6  (134)
  8 并发 + queue + id(Counter) + 2000 次    →  ★ SIGSEGV (139)
  8 并发 + queue + id,但 0 次调用           →  0   ← 不碰就不崩
```

碰得越多越容易炸 —— 和 refcount 裸奔完全吻合。

### 根因在 PyO3 的架构里

```rust
pub(crate) struct GILOnceCell<T> {
    once: Once,                        // std::sync::Once
    data: UnsafeCell<MaybeUninit<T>>,  // 一个槽
}
```

**没有任何解释器维度。** 而这不是疏忽:PyO3 的 `#[pyclass]` 独立于任何 module 声明,一个 type 可以加到多个 module —— **没有地方可以挂"每解释器"的状态**,只能挂 static。

---

## 四、修法,以及我写出的三个 bug

### 方案

`PerInterpreterCell<T>`:值存进 `PyInterpreterState_GetDict()` 下的一个私有子字典,用 `PyCapsule` 包住 `Box<T>`。CPython 在解释器 finalize 时清空那个字典 —— **不需要任何 teardown hook**。

### 🐛 Bug 1:悬垂引用 —— 注释和代码互相矛盾

```rust
if registry.is_null() || key.is_null() {
    ffi::Py_DECREF(capsule);   // ← 触发析构器,drop 掉 box
    return &*boxed;            // ← ★ 返回指向已释放内存的引用
}
```

注释写的是 `// leaked`,代码做的是 drop。**我写下了我的意图,而不是我写的东西。**

**修法:所有可失败的步骤都放在创建 capsule 之前**;唯一一处 capsule 已存在时的失败,先 `PyCapsule_SetDestructor(NULL)` 再 decref,让它真的泄漏而不是被 drop。

### 🐛 Bug 2:ZST 地址碰撞 —— 850 个测试全部 abort

```rust
pub struct PerInterpreterCell<T> {
    _marker: PhantomData<fn() -> T>,   // ★ 零大小类型
}
```

我拿 `self as *const Self as usize` 当存储 key。而 `LazyTypeObject` 里有两个 cell:

```rust
value: PerInterpreterCell<PyClassTypeObject>,
fully_initialized_type: PerInterpreterCell<Py<PyType>>,
```

**同一个结构体里的两个 ZST 字段可以有相同的地址。** 两个 cell 用同一个 key,互相覆盖,读出来类型混淆 → 空指针 → `NonNull::new_unchecked` → 非展开 panic → SIGABRT。

**怎么发现的:** 猜不出来。插桩打印 key,发现**只出现一个 key**,而应该有两个。

**修法:** 加一个 `AtomicU8` 字段,让每个 cell 有独立地址(顺便防止链接器合并相同的 static)。

### 🐛 Bug 3:内存 0% 回收 —— 排除了五个假设才找到

```
16 个解释器:上游回收 48.0%,我的 fork 回收 0.0%
800 个解释器:RSS 涨 2.5 GB,线性不收敛
```

#### ❌ 测量错误:用了 `ru_maxrss`

那是**峰值** RSS,单调不减 —— 它必然增长,测不出释放。换成 `ps -o rss=` 的当前 RSS。

(换完之后数字几乎一样,泄漏是真的。但方法错了就是错了。)

#### ❌ 假设 4:capsule 析构器没跑

插桩:3 轮 × 2 解释器 = **12 次触发**,正好是 6 个解释器 × 2 个 cell。一次不少。

#### ❌ 假设 5:析构时没有正确的 GIL

插桩:`gil=1`、`tstate` 非空、`drop 完成`。释放是真的发生了。

#### ❌ 假设 6:是 `create_type_object` 里那三处故意泄漏

```rust
create_type_object.rs:173   Box::into_raw(data.into_boxed_slice())
create_type_object.rs:195   def.into_raw()
create_type_object.rs:556   core::mem::forget(class_name);
```

看起来很对 —— PyO3 的前提就是"一个进程建一次 type,活到进程结束,泄漏无所谓",而我打破了这个前提。

**测:1 个类 vs 11 个类。58.5 MB → 59.5 MB。** 10 个额外的类只多 1 MB,每类 6 KB —— 和要解释的 1.76 MB **差 30 倍**。作废。

#### ❌ 假设 7:malloc 碎片化,不是真泄漏

串行 100 轮(建 8 个 → 全关 → 再建 8 个),内存应该能复用。**实测线性涨到 2.5 GB 不收敛 → 是真泄漏。** 作废。

#### ✅ 真正的原因:堆类型是循环垃圾

```
每个 type 的 refcount = 5
  能核销 3 个:module dict 1 + value cell 1 + fully_initialized_type cell 1
  ★ 剩下 2 个核销不掉 —— type 对象的 __mro__ 里含它自己
```

**释放最后一个计数引用之后,它还留给循环收集器。** 而如果收集器没跑到,type 就不死;它通过 `ht_module` 持有 module,module 持有 module dict,**整个解释器的 import 状态被钉住 —— 每个 1.76 MB。**

上游一个进程只建一次 type、泄漏一次,看不见。我改成每解释器一次,泄漏就乘以解释器数。

**修法:** 给每个解释器的 `atexit` 注册一个回调 —— `Py_EndInterpreter` 会在拆解之前调用它,而那时收集器还活着。回调清空 registry,然后 `gc.collect()`。

**一轮不够:**

```
1 轮 GC   →  回收 25.2%
3 轮 GC   →  回收 48.0%   (上游 48.1%)
```

**收掉一个 type 会让它引用的东西变成不可达,那要下一轮才看得见。**

---

## 五、"为什么不改剩下的 141 处"

这个问题问出了第四个 bug。

我的答案本来是:**只修量到坏的那一处**。141 处 `PyOnceLock` 里很多存的是 `bool` / 版本号 / 配置,而 165 处 `intern!` 存的驻留字符串在 3.12+ 多数是不朽的(不朽 = refcount 饱和 = 无竞争)。

**但"我以为安全"刚被证明是不可靠的。** 去测了异常类型:

```rust
// create_exception! 宏里内联的
static TYPE_OBJECT: PyOnceLock<Py<PyType>> = PyOnceLock::new();
```

```
#[pyclass] Counter   4/4 个不同   ✅ 已修
异常类型 MyError     1/4 个不同   ★★ 仍共享
```

**同一个失效形状,藏在宏里,另一条缓存路径。** 改一行修好,8/8。

然后把剩下的 79 处 cache site 静态分类,并对最大的一类(42 处 `src/types/` 下缓存 CPython 内建类型)做了运行时判定:

```
list / float / int / range / memoryview / super / code
  → 4 个子解释器里全部共享同一地址,【全部不朽】
  → refcount 饱和,不存在竞争 → ✅ 无需修改
```

### 判据(可执行)

```
持有 PyObject 吗?   否 → 安全(bool / 版本号 / 配置)
是不朽对象吗?       是 → 安全(CPython 静态类型、3.12+ 驻留字符串)
是堆类型吗?         是 → ★ 危险 —— 只有这一类要改
```

已知的堆类型缓存有两处(`#[pyclass]`、`create_exception!`),都改完了。

---

## 六、性能:回归了 2.75×,又消掉了

PyO3 自带的 `bench_pyclass`,同机背靠背:

| | 上游 | 第一版(dict) | 变化 |
|---|---|---|---|
| `pyclass_create` | 25.87 ns | 71.25 ns | **慢 2.75×** |
| `bench_call` | 53.70 ns | 52.70 ns | 持平 |
| `bench_fast` | 40.42 ns | 32.46 ns | 略快(方差内) |

**每创建一个实例多付 45 ns。** 而根因又是实现偷懒,不是设计的固有代价:

```rust
let key = ffi::PyLong_FromSize_t(self.key());   // ★ 每次查找都堆分配一个 Python 整数
let capsule = ffi::PyDict_GetItem(registry, key);
```

**每次 `get()` 分配一个 `PyLong`,再做两次 dict 哈希查找。**

### ❌ 假设 8:线程只绑定一个解释器,所以 TLS 缓存可以不带标签

一个很有诱惑力的方案是把缓存放进 `thread_local!`,只按 cell 下标索引 —— 那样连解释器 id 都不用读,理论上 <1.5 ns,甚至"超越上游"。

**前提是错的。** `Interpreter.exec()` 做的就是让**调用线程 attach 到目标解释器**,同一个线程会在解释器之间来回切。不带解释器标签的 TLS 缓存会返回另一个解释器的 type 对象 —— **正是这份复盘要修的那个 bug,换了个更隐蔽的形式。**

所以标签必须每次检查,`current_interpreter_id()` 那次 FFI 省不掉。**"零 FFI"做不到,"比上游快"也不可信** —— 正确性的下限就已经接近上游的总成本。

### ✅ 最终设计:原生数组 + 单个带标签的 TLS 缓存

三点:

1. **registry 是原生 `Vec<Option<Slot>>`,不是 `PyDict` 也不是 `PyList`。**
   每个 cell 首次使用时从进程级计数器领一个下标,查找变成**指针偏移** —— 零分配、零引用计数、零 GC 追踪。`Slot` 存 `(erased ptr, drop fn)`,所以一个 registry 能装不同类型的值。

2. **TLS 缓存的是"当前解释器的数组基址",而且全 crate 只有一个。**
   ```rust
   static CACHE: Cell<(i64, *const Option<Slot>, usize)>
   ```
   不是每个 cell 一个。N 个 worker 各缓存自己解释器的基址 → **零争用、零颠簸**。

3. **解释器 id 标签每次检查 —— 这是正确性,不是优化。**
   id 单调不复用,所以陈旧的标签永远匹配不上后来的解释器。

热路径 = 一次 FFI(读 tstate → id)+ 一次 TLS 读 + 一次整数比较 + 一次索引。

| | 上游 | 第一版(dict) | **最终版** |
|---|---|---|---|
| `pyclass_create` | 25.87 ns | 71.25 ns | **25.17 ns** |
| `bench_call` | 53.70 ns | 52.70 ns | **50.18 ns** |
| `bench_fast` | 40.42 ns | 32.46 ns | **31.77 ns** |

**回归归零。** 快出来的那 0.7 ns 在噪声里,不当作收益声称。

### ⚠️ 刻意没做的四项优化

freelist / `#[pyclass(frozen)]` 零开销借用 / `METH_FASTCALL` / `likely`-`cold` 冷热分离 —— 这四项都是真实的、可观的优化,但**它们和这个改动完全正交**:上游加上它们会同样变快。

**把它们混进这个分支再和没加的上游比,是不诚实的基准,而且会让 PR 没法评审。** 一个 PR 只改一件事。

## 七、当前状态

> 这一节的数字在第九节被重测过一次 —— 对照组选错了。以第九节末尾的表为准。

| | 上游 PyO3 0.29.2 | 本分支 |
|---|---|---|
| `#[pyclass]` 类型(8 个解释器) | 1 个,共享 | **8 个,独立** |
| 异常类型(8 个解释器) | 1 个,共享 | **8 个,独立** |
| 并发压测(8 × 2000 次调用) | **SIGSEGV / abort** | 正常 |
| 内存回收(16 个解释器) | 48.0% | **51.1%** |
| 长跑(4000 个解释器) | — | +24.8 MB,斜率 +3.4 MB/千轮,收敛 |
| `pyclass_create` | 25.87 ns | **25.17 ns** |
| 上游测试套件 | 850 passed | **850 passed, 0 failed** |

**没有已知回归** —— 这句话当时是错的。第九节里那个 2.6 MB/解释器的泄漏,写下这张表的时候
已经在分支里了,而这张表的每一行都测不到它。

### 还没做

- ~~`wrap_pymodule!` 的守卫仍在~~ → 第九节拆了
- ~~仍需 `_override_multi_interp_extensions_check(-1)`~~ → 第九节声明了 slot
- 只在 macOS ARM64 / Python 3.14.6 验证过
- 剩余 79 处 cache site 里,`Py<PyAny>` / `Py<PyTzInfo>` / `Py<PyModule>` 等约 20 处未做运行时判定
- ~~多解释器**并发**下的 bench 没做~~ → 第九节做了

### 和这个修复无关的

numpy / pandas / scipy / pyarrow 仍然需要物理副本 —— 那是 Cython 守卫和进程级 C 全局态,不在 PyO3 这一层。

---

## 八、方法上的教训

1. **"能跑 + 结果对" ≠ "隔离了"。** 功能测试全绿的同时,8 个解释器在共享一个 type 对象。**测对象身份(地址、refcount、tp_flags),不要只测输出。**

2. **grep 二进制不是测试,加载才是。** `-dead_strip` 会让小模块里的字符串消失,大模块里留着 —— 字符串的有无和行为无关。

3. **`ru_maxrss` 是峰值,不是当前值。** 它单调不减,测不出释放。

4. **对照组要和被测组同构。** 我第一次比较时对照用的是旧的 `.so`(没有 `Counter`),那一行数据是废的。

5. **注释写的是意图,代码写的是行为。** Bug 1 里两者矛盾,而我读了很多遍都没看出来 —— 因为我读的是注释。

6. **读代码形成的假设,八个错了八个;测量得出的结论,五个对了五个。** 这不是运气,是这类问题的性质:进程级状态、生命周期、引用计数,都不在源码的字面里。

7. **性能提案里最诱人的那条,前提是错的。** "一个线程只绑定一个解释器"听起来显然成立,而 `Interpreter.exec()` 恰恰违反它。**越是省掉一次检查的优化,越要先问那次检查在防什么。**

8. **"我以为它安全"是最贵的一句话。** 异常类型那个 bug,是被"为什么不改剩下的"这个问题逼出来的 —— 如果没人问,它会一直在那。

---

## 九、第二轮:基准抓到的那个泄漏,和我自己的四次测量错误

第七节写"没有已知回归"的时候,分支里已经躺着一个 **2.6 MB/解释器、线性不收敛**的泄漏。
它是被后来那套基准抓到的 —— 而在抓到它之前,**我先用同一套工具量出了三个假结论**。

这一节记的是那四次测量错误,和那个泄漏的定位过程。**每一个错都是因果设计错误,不是编程错误。**

### ❌ 测量错误 1:对照组用了发行版,不是父提交

单线程基准说建对象慢了 **92%**:

```
noop 空调用       13.44 → 13.87 ns    +3.2%
Row(...) 建对象   45.41 → 87.29 ns   +92.2%   ★
Counter.bump      31.55 → 31.64 ns    +0.3%
```

我没去优化它,先把它拆开 —— 我的改动在这条路上只多做两件事:

```
PyInterpreterState_Get + GetID    1.5 ns
类型对象查找本身                   0.39 → 2.22 ns
                                 ────────────
                                 合计差 1.6 ns,账面差 42 ns
```

**差 26 倍。钱对不上,说明账本有问题,不是花销有问题。**

真因:这个分支基于 PyO3 **main** 的 `dfdbc46`,而我拿 crates.io 的 **0.29.2** 当对照。
两者之间隔着上游自己的一个改动:

```
crates.io 0.29.2   gc.is_tracked(Row(1.0,2)) = False
dfdbc46 父提交      gc.is_tracked(Row(1.0,2)) = True    ← 上游把 #[pyclass] 改成 GC 跟踪
```

**我量到的是上游的改动,记在了自己头上。**

### ❌ 测量错误 2:计时循环把结果留活了

```python
best = min(... [f() for _ in range(400_000)] ...)   # 40 万个对象全留着
```

量到的是 GC 遍历,不是构造。**在两个 GC 跟踪状态不同的版本之间,单这一条就能凭空造出 2 倍差距。**

两处都修掉之后:`noop −0.6%`、`Row −0.7%`、`bump +0.9%`,全在噪声内。**92% 是我自己造的。**

### ❌ 测量错误 3:harness 自相矛盾时,先修 harness

中途出现过这样一批数:

```
不 import 扩展     存活 +57.9MB
只 import          存活 +39.4MB   ← 什么都不加载反而更占内存?
```

**被测物可以反直觉,harness 不能自相矛盾。** 原因是五个用例跑在同一个进程里,
前面把堆撑大、后面复用。改成一个用例一个子进程,数字立刻自洽 —— 那批数据全部作废重测。

### ❌ 测量错误 4:免 GIL 下,harness 自己成了被测现象

同一个纯 Python 负载,单线程都是 20.6 M/s,12 线程差 **16 倍**:

```
闭包变量 (LOAD_DEREF,12 线程共用一个 cell)      8.9 M/s
模块全局 (LOAD_GLOBAL,有优化路径)            141.3 M/s
```

12 个线程读同一个闭包 cell,每次读都 incref 它持有的对象 —— **正是这套基准要测的那个
争用,在计时循环里被我复现了一遍。** 这在因果里叫 measurement reactivity,在这里是字面意思。

### 泄漏:四个假设,四个都不是

线头是一个**太整齐的数字**:回收率 `0.0%`。真实测量给 0.3% 或 −0.7%,不给正好零。
重复三次:0.0 / 0.0 / −0.1。

#### ❌ 假设 9:teardown 钩子没跑
埋点:16 个解释器**全部**进了 atexit 回调,每个 registry 恰好 1 个已填槽位。跑了。

#### ❌ 假设 10:gc 收得不够(堆类型是循环垃圾)
把遍数做成环境变量扫 0 / 1 / 3 / 10:`−0.6 / −0.6 / −0.5 / −0.6`。**一模一样。**
不是收得不够,是根本没被回收。

#### ❌ 假设 11:是钩子本身在留住东西
`atexit._clear()` 掉,不变;加开关彻底跳过注册(连 `atexit` 都不 import),还是不变。

#### ❌ 假设 12:是 `模块 → dict → type → ht_module → 模块` 引用环
在解释器里 `del sys.modules[...]; gc.collect()` —— 0.0%,类型的 refcount **纹丝不动,还是 3**。
补刀:只含函数的模块里那个异常类型是 `PyErr_NewException` 建的,**根本没有 ht_module**,
钉不住任何模块,却漏一样多。

### ✅ 转折:我一直在问错的问题

到这里所有嫌疑人都排除了,内存却确实没回来。然后意识到:

**"建 16 个,一起关,立刻收回多少" 的答案可以是 0%,而系统没有泄漏** —— 也许只是延迟释放。
**我拿一个有歧义的指标追了两个小时。**

换个问法 —— 一次只活一个解释器,建了关、建了关,做 400 次:

```
对照   50→+4.8MB   200→+4.9   400→+5.6MB
fork   50→+128.9   200→+518.8  400→+1038.5MB     斜率 +2598 MB/千个
```

完美线性。**不是延迟释放,是真泄漏,而且严重一个量级。**

### ✅ 定案:一行埋点

四个假设全错之后,继续想第五个是负收益。给 `Registry::drop` 加了几行,
在调用 drop 函数**前后各读一次**引用计数:

```
slot refcnt 3 -> 3
```

**drop 调了,返回了,而它本该减掉的那个引用计数没有动。**

只打"后"只会得到一个数(3),然后继续猜谁持有那三个引用 —— 那是个查不完的问题。
**打了两侧,问题当场从"谁持有它"变成"为什么放手没生效",而后者能在源码里查到答案。**

### ✅ 根因

```rust
fn drop(&mut self) {
    if thread_is_attached() { Py_DECREF(obj) }
    else { register_decref(obj) }        // ★ 进【进程级】延迟队列
}
fn thread_is_attached() -> bool {
    ATTACH_COUNT.try_with(|c| c.get() > 0).unwrap_or(false)   // PyO3 自己的线程局部计数
}
```

registry 是被 CPython **直接**调的(capsule 析构 / 裸 `PyMethodDef` 的 atexit 钩子),
两处 PyO3 都没参与,计数是 0。于是每个 `Py<PyType>` 的 decref 都进了那个进程级队列,
**对一个正在消亡的子解释器,冲刷它的那一刻永远不会到来。**

而"有 pyclass 实例活着就没事"的谜,同一句话解释:那条路上类型对象是被 CPython 自己的
`subtype_dealloc` 在清 `__main__` 时 decref 的,**压根不经过 registry**。

修法是一个只加 attach 计数、**不冲刷队列**的 guard。不能用现成的 `AttachGuard::assume()` ——
它会顺手冲刷,而队列里的引用属于别的解释器,**修一个泄漏换来一个 use-after-free**。

### 它为什么藏了七个提交

泄漏是在**第一个提交**里引入的。此后我写了压力测试,跑了 **4000 个解释器**,报告
"+24.8 MB,收敛"。基于那个数,我在第七节写下"没有已知回归"。

那个压测的解释器体是:

```python
c = abi3t.Counter(_seed)
for _ in range(2000): tot = c.bump(1)
```

`c` 是模块级名字,**活到解释器关闭** —— 四种形状里三种漏 519 MB,一种不漏,
**我的压测恰好用的就是不漏的那一种。**

不是运气差。写压测时我想的是"怎么把这套机制**用起来**" —— 一个自然、顺手、
看起来最有代表性的写法。**而 bug 藏在"怎么把它用坏"里。**

### 后续:第二个盲区,被真实使用者收费

补完泄漏、拆完守卫、声明完 slot 之后,拿 polars 一试,又冒出三个:

```
① 限定 API 下编不过      PyObject_CallMethodNoArgs / OneArg 不在里面
② 限定 API 下 slot 发不出  abi3 的编译期 cfg 取【最低】版本,gate 直接成空操作
③ 占位符泄漏到子模块      ②的修法自己造的,当场被 polars 抓到
```

**这套基准一个都抓不到 —— 它全是默认 ABI、全是顶层模块。**
行为维度测得再密,补不上构建维度的零覆盖。现在 `build.sh` 编 8 份探针
({对照,本分支} × {默认,abi3} × {顶层,子模块}),`matrix.py` 跑矩阵。

同样的事在缓存审计上又发生一次:把判据做成脚本(`audit_caches.py`)全量跑,
翻出 **11 处从没被检查过的** —— `collections.abc.Sequence`、`decimal.Decimal`、
`pathlib.Path`、`uuid.UUID`、`zoneinfo.ZoneInfo` 这些**转换层缓存的 Python 层类**,
每个解释器各有一份,却被缓存在进程级。**测试套件永远碰不到,因为转换在单解释器里跑。**

---

## 十、第二轮的教训

9. **整齐的数字是可疑的。** 真实测量给 −0.4%,不给 0.0%。正好是零,通常意味着某条路径压根没执行。

10. **对照组要和被测组同源。** 差一个上游提交就能造出 92% 的假回归 —— 方向和量级都足够逼真,足以让人开始优化一个不存在的问题。

11. **先把账算平,再怀疑代码。** 42 纳秒的差,拆开只有 1.6 纳秒有出处。对不上的时候,错的是账本。

12. **确认你问的是不是那个问题。** "一批一起关收回多少"的 0% 有歧义;"一次只活一个,斜率多少"没有。我在歧义指标上追了两个小时。

13. **埋点打在转换的两侧。** 只打一侧得到一个数字,打两侧得到一个事实。`3 → 3` 把一个查不完的问题换成了一个能查的问题。

14. **二分的答案取决于探针。** 同一批提交,换个探针再分一次,答案从第五个提交变成第一个 —— 探针照不到的路径,二分会把责任推给第一个碰到它的提交。

15. **harness 也是被测物。** 它自相矛盾时先修它;免 GIL 下它甚至会自己产生被测现象。

16. **测试的形状决定了它能看见什么。** 压测覆盖 4000 个解释器却漏掉了 bug,因为它的写法恰好是唯一安全的那种。**问"这个 bug 为什么躲过了我已有的测试",答案通常比 bug 本身值钱。**

17. **行为维度和构建维度是两个维度。** 三个 bug 全从 abi3 / 子模块这个零覆盖区来,全由第一个真实使用者免费发现。**判据做成脚本全量跑,比逐个想更可靠** —— 缓存审计那 11 处就是这么翻出来的。

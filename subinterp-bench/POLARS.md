# polars 在 own-GIL 子解释器里

> 实测记录 · Python 3.14.6 / macOS ARM64 (6 P-core + 12 E-core)
> polars Python 1.43.2 / Rust crates 0.55.1 · PyO3 0.29 · `abi3-py310`
> 对照 = 本分支的父提交 `dfdbc46`(PyO3 main),不是 crates.io 发行版
> 相关:[`JOURNAL.md`](../JOURNAL.md) · [`METHOD.md`](METHOD.md)

## ⚠️ 先读这一段

**这份文档下面的结论,是在没有打到几种关键形状的负载上得到的。** 后来的压测
(见[压测:四个新问题](#压测四个新问题))在 **1–4 秒内**就把 8 个子解释器的进程打死了。

现在诚实的说法是:

```
✅ 能加载、能隔离、单解释器下功能正确、稳态不泄漏
★ 多解释器 + 高频写出   1–4 秒内 SIGABRT(pyo3 的全局延迟释放池跨解释器 free)
★ 多解释器 + map_elements  50% 概率静默拿到【别的解释器的类对象】,给出错误结果
★ 每个子解释器生命周期泄漏 ~60KB,且 import 慢 3.7 倍
```

**它现在不能上生产。** 下面的数字仍然有效 —— 它们量的是"能不能进去、隔不隔离、
扩不扩展",那部分是真的;但"能用"这个结论下得太早了。

---

## 一句话

**上游 PyO3 上,polars 进不了子解释器 —— 开了 `_override` 开关也只进得去 1 个。**
本分支之后,4 个 own-GIL 子解释器**不需要任何进程级开关**就能各自加载完整的 polars,
类型对象各自独立,内存比"每 worker 一份物理副本"的路线少 35%,冷启动快 80 倍。

代价有两层:**只对你自己从源码编译的扩展有效**(pip 装的 wheel 是拿上游 PyO3 编的,
`[patch.crates-io]` 管不到),**以及上面那四个还没解决的问题**。

---

## 为什么上游进不去

polars 是**纯 Rust**(`py-polars` 里没有 `.pyx`,没有 `.c`),所以它撞的**不是**
numpy/lxml 那堵"进程级 C 全局态"的墙。它撞的三样全在 PyO3 这一层:

```
① 模块 slot 没声明        没有 Py_mod_multiple_interpreters
                          → 严格模式直接 ImportError
② 子模块守卫              polars 用 wrap_pymodule! 注册 _ir_nodes / _expr_nodes
                          → 第 2 个解释器起抛 pyo3#576,★ 这条【没有开关】
③ 类型对象共享             77 个 #[pyclass] 全部缓存在进程级 static
                          → N 个解释器共用一个 type,引用计数并发裸奔
```

第 ② 条最要命:**`_override_multi_interp_extensions_check(-1)` 救不了它。**
那个开关只放行第 ① 条(策略检查),而 `wrap_pymodule!` 的守卫在 PyO3 自己的
`make_module` 里,没有任何开关。

## 能力对比

4 个 own-GIL 子解释器,各自 `import` 并建对象:

| | override(带进程级开关) | strict(不带任何开关) |
|---|---|---|
| 上游 PyO3 0.29.0 | **1/4** — 其余抛 pyo3#576 | **0/4** — `does not support loading in subinterpreters` |
| **本分支** | **4/4** — 4 个模块地址,4 个 `PyDataFrame` 类型地址 | **4/4** — 同上 |

---

## 怎么编

polars 必须**从源码编**,而且它的 `Cargo.toml` 已经有 `[patch.crates-io]` 段,
要**并进去**而不是再加一段(否则 `duplicate key`):

```bash
git clone --depth 1 https://github.com/pola-rs/polars.git
cd polars

# 把 pyo3 加进【已有的】 patch 段
#   [patch.crates-io]
#   pyo3 = { path = "/path/to/leocaolab-pyo3" }     ← 加这一行
$EDITOR Cargo.toml

# ★ 必须 update,否则 cargo 把 patch 判为 [[patch.unused]],构建照常成功但用的还是上游
cargo update -p pyo3
grep -A3 'name = "pyo3"$' Cargo.lock      # 确认那条【没有 source = 行】

# macOS 上裸 cargo build 缺 maturin 的链接参数
export RUSTFLAGS="-C link-arg=-undefined -C link-arg=dynamic_lookup"
cargo build --release -p polars-runtime-64      # 约 6–10 分钟
```

拼出 Python 包(polars 的 `_plr.py` 会去找 `_polars_runtime_64` 这个包):

```
<pkgdir>/
  polars/                        ← 从 py-polars/src/polars 复制
  _polars_runtime_64/
    __init__.py                  ← 从 py-polars/runtime/polars-runtime-64/_polars_runtime_64/
    build_feature_flags.py       ← 同上
    _polars_runtime.so           ← target/release/lib_polars_runtime.dylib
```

然后 `sys.path.insert(0, "<pkgdir>")` 就能 `import polars`,子解释器里也一样。

---

## API 面:22 项(默认分配器 21/22,`PYTHONMALLOC=malloc` 下 22/22)

在一个子解释器里(strict,不带开关)逐个跑:

| ✅ 可用 | |
|---|---|
| `DataFrame` / `Series` | `lazy()` + `collect()` |
| `group_by` + `agg` | `join`(left) |
| `over`(窗口) | `when` / `then` / `otherwise` |
| `fill_null(strategy=...)` | `cast` |
| `.str` 表达式 | `sort` / `unique` |
| `pl.selectors` | `read_csv`(StringIO / BytesIO / bytes / 真文件) |
| `scan_csv().collect()` | `write_parquet(BytesIO)` |
| `read_json` / `read_ndjson` | `to_dicts` |
| `concat` | `pivot` |
| `rolling_mean` | `.dt` 日期表达式 |
| `collect(engine="streaming")` | **`map_elements`(Rust 回调 Python UDF)** |

异常路径也正常 —— `ColumnNotFoundError` 等作为正常 Python 异常传播,
是 `pl.exceptions.PolarsError` 的子类,不越界、不崩。

### ★ 写出到 Python file-like 会中止进程 —— 而且根因比崩溃更严重

```
df.write_csv()          Abort trap: 6 (exit 134),stderr 一个字都没有
```

**不是本分支造成的**(上游 `.so` + override 一样崩),也**不是 CSV 特有的**。

#### 根因

`faulthandler` 抓到的栈,里面没有一行 polars 或 PyO3 的代码:

```
_io_BytesIO_getvalue_impl → _PyBytes_Resize → realloc
  → ___BUG_IN_CLIENT_OF_LIBMALLOC_POINTER_BEING_FREED_WAS_NOT_ALLOCATED → abort
```

跨分配器 free。为什么会跨,用一个记录**每次 `write` 发生在哪个解释器**的
自定义 file-like 问出来了:

```
子解释器 id = 1
两次 write 都发生在【解释器 id = 0】,也就是主解释器,而且在两个不是调用线程的线程上
```

polars 用 rayon worker 线程回调 Python 写数据(`crates/polars-python/src/file.rs`
的 `impl Write for PyFileLikeObject` 里 `Python::attach(|py| ... call_method(py, "write", ...))`)。
**`Python::attach` 在一个 PyO3 从未 attach 过的线程上会走 `PyGILState_Ensure`,
而那个 API 按设计绑定主解释器**(PEP 684 的已知禁忌)。于是:

```
子解释器 1 建了 BytesIO
   ↓  rayon 线程 attach 到【主解释器 0】,往那个 BytesIO 里写
      → 缓冲区在主解释器的 arena 里长
   ↓  回到子解释器 1 调 getvalue()
      → _PyBytes_Resize 用子解释器的 arena realloc
      → 指针不属于这个 zone → abort
```

**崩溃只是表象。真正发生的是跨解释器操作对象** —— 一个解释器的线程在改另一个
解释器拥有的 `bytes`。不崩的时候它也是错的。

#### 范围:凡是回调 Python `write()` 的路径

给一个**自定义 file-like**(不是 `BytesIO`)时:

| 写出方式 | 回调发生在 | 结果 |
|---|---|---|
| `write_json` | 调用线程,解释器 1(正确) | ✅ OK |
| `write_csv` | ★ 主解释器 0 | 中止 |
| `write_ndjson` | ★ 主解释器 0 | 中止 |
| `write_parquet` | ★ 主解释器 0 | 中止 |
| `write_ipc` | ★ 主解释器 0 | 中止 |

给 `BytesIO` 时 `write_parquet` / `write_ipc` 不崩 —— 那是因为 polars 对 BytesIO 有
**快路径**(`file.rs:415` "handle BytesIO specially"),压根不回调 Python。
**规则不是"CSV 有问题",是"任何回调 Python `write()` 的路径都有问题"。**

#### 三条出路,代价明确

```
① PYTHONMALLOC=malloc     22/22 全绿。所有解释器共用系统 malloc,arena 边界消失。
                          代价:这个分配密集的负载上 +23%(0.235s → 0.290s)
                          ★ 只是让崩溃消失,【跨解释器写对象这件事本身还在】
② 避开回调路径             零代价。write_csv 给真文件路径;parquet/ipc 给 BytesIO
                          (走快路径);write_json 本来就安全
③ 修 polars               ★ 已做,见 POLARS-PATCH.md —— 一个文件,12 行加 5 行改。
                          让 PyFileLikeObject 记住收到文件对象时所在的解释器,
                          worker 线程 attach 回【那个】解释器。
                          实测:四条写出路径全部从「写在主解释器 0 → 中止」
                          变成「写在解释器 1 → OK」,且不需要 PYTHONMALLOC=malloc
```

**① 只是止血。**它让 realloc 不再跨 zone,但那个 rayon 线程仍然在主解释器里改
子解释器的对象。**真修是 ③,已经做了** —— 见 [`POLARS-PATCH.md`](POLARS-PATCH.md)。

#### 这是 PyO3 层面的通用陷阱,不是 polars 特有

**任何 PyO3 扩展,只要从一个 PyO3 没 attach 过的线程调 `Python::attach` 回调 Python,
在子解释器下就会静默地落到主解释器。** rayon、tokio 的 `spawn_blocking`、
自建线程池都是这个形状。用子解释器的项目值得把这类调用点排查一遍。

本分支为此加了 `pyo3::sync::InterpreterHandle`:在拿到 Python 对象的地方
`InterpreterHandle::current(py)` 捕获解释器,在 worker 线程 `handle.attach(|py| ...)`
attach 回那一个。单解释器下走 fast path,行为和 `Python::attach` 一致。

> `to_arrow` 在这台机器上报 `ModuleNotFoundError: pyarrow` —— 是环境没装,不是缺陷。

---

## 真的跑起来

每个 worker 拿**自己那份种子**的数据,答案互不相同 —— 串号会被抓到。
所有结果都和解析解**精确比对**过,不是"跑完没报错"。

### 扩展性

```
worker × 20 万行,filter → group_by → agg
 1 解释器   墙钟 0.08s    结果 ✅ 精确匹配
 2 解释器   墙钟 0.09s    ✅
 4 解释器   墙钟 0.10s    ✅   类型 4 个地址
 8 解释器   墙钟 0.10s    ✅   类型 8 个地址
```

**8 倍的活,1.25 倍的墙钟。**

### SkyTrade 型策略负载

表达式照抄 SkyTrade 代码里频次最高的那几个:
`rolling_mean(20).over("sym")` / `rolling_std` / `shift(1).over` / `fill_null` /
两级 filter / `group_by`-`agg`。

固定每 worker 负载,加 worker:

| worker | 行数 | 墙钟 | 吞吐 | 类型独立 | 常驻 |
|---:|---:|---:|---:|:---:|---:|
| 1 | 100,000 | 0.10s | 1.0 M行/秒 | 1/1 | +93 MB |
| 8 | 800,000 | 0.12s | 6.9 M行/秒 | 8/8 | +389 MB |
| 24 | 2,400,000 | 0.20s | 12.0 M行/秒 | 24/24 | +938 MB |

固定 8 worker,加数据量:

| 行数 | 墙钟 | 吞吐 | 常驻 |
|---:|---:|---:|---:|
| 3,200,000 | 0.23s | 14.0 M行/秒 | +752 MB |
| 8,000,000 | 0.44s | 18.3 M行/秒 | +1,314 MB |
| 16,000,000 | 0.88s | 18.1 M行/秒 | +1,715 MB |
| 32,000,000 | 1.86s | 17.2 M行/秒 | +2,614 MB |

吞吐在 **~18 M行/秒**封顶(内存带宽),从 3.2M 行到 32M 行只掉 8% —— 不随规模崩。
24 worker 仍在涨,说明超过 6 个 P-core 之后 E-core 还在贡献。

---

## 和"每 worker 一份物理副本"的对比

这是这条分支对 polars 的**实际收益**。两条路线的隔离质量一样
(都是 8 个不同的 `DataFrame` 类型地址):

| 8 个 own-GIL worker | 常驻 | 磁盘 | 冷启动 |
|---|---:|---:|---:|
| 每 worker 一份物理副本 + override | +388 MB | 2,791 MB | 8.02s |
| **本分支:一份 `.so`,strict** | **+252 MB** | **349 MB** | **0.10s** |

按 worker 数展开:

| worker | 副本路线 常驻 | 本分支 常驻 | 副本路线 启动 | 本分支 启动 |
|---:|---:|---:|---:|---:|
| 1 | +56 MB | +57 MB | 2.09s | 0.09s |
| 4 | +198 MB | +141 MB | 5.95s | 0.08s |
| 8 | +388 MB | +252 MB | 8.02s | 0.10s |

**1 个 worker 时两者持平,差距随 worker 数拉开** —— 副本路线每多一个 worker 就多
一份 `.so` 的 data 段,本分支只多一个解释器的堆。

冷启动那 80 倍差距是因为**根本没有克隆**:副本路线每个 worker 都要 `cp -R` 一份
349 MB 的包,而 macOS 会对每个新出现的 `.dylib` 在首次 `dlopen` 时验签。

---

## 边界:这条分支帮不到谁

```
✅ polars / pydantic_core / tokenizers 等纯 PyO3 库   —— 但必须【自己从源码编】
❌ numpy / scipy / pandas / sklearn                  —— C 扩展,进程级 C 全局态,不在 PyO3 这一层
❌ lxml                                              —— libxml2 自带 interpreter 检查
❌ torch                                             —— 多 GB + 进程级 CUDA 上下文
❌ 任何 pip 装的 wheel                                —— 拿上游 PyO3 编的,patch 管不到
```

对 numpy 那一类,**每 worker 一份物理副本仍然是唯一办法**,这条分支没有改变它。

---

## polars 反过来帮我们抓到的三个 bug

polars 是第一个真实消费者,而它当场逼出三个**本仓库基准一个都抓不到**的 bug ——
因为那些基准全是默认 ABI、全是顶层模块:

```
① 限定 API 下编不过     PyObject_CallMethodNoArgs / OneArg 不在限定 API 里
② 限定 API 下 slot 发不出  abi3 的编译期 cfg 取【最低】版本(polars 用 abi3-py310),
                        gate 在 Py_3_12 上的方法直接编译成空操作
③ 占位符泄漏到子模块     ②的修法自己造的:顶层走 init_multi_phase 补上了,
                        子模块走 make_module 没补 → SystemError: unknown slot ID
```

已经补上:`build.sh` 现在编 8 份探针
({对照, 本分支} × {默认 ABI, abi3-py310} × {顶层, 含子模块}),
`matrix.py` 跑能力矩阵 —— polars 的每个症状现在几秒复现,不用等 7 分钟的构建。

**教训写在 [`METHOD.md`](METHOD.md):行为维度测得再密,补不上构建维度的零覆盖。**


---

## 压测:四个新问题

345,600 次写、113 万次操作、跨 4/8/12 个子解释器,全程不开 `PYTHONMALLOC=malloc`。
每个可能崩的用例独占子进程,被信号打死时报信号名。

**先说好的部分,因为它是真的:**

```
并发正确性   9/9 通过,345,600 次写
             写发生在错误解释器 0 次
             与"纯 Rust 直写文件"的解析解逐字节比对,不一致 0 次
             读回校验和不一致 0 次
稳态内存     8 个解释器持续跑 120 秒,113 万次操作,零错误
             RSS 每千次操作 −0.010 MB(整体)/ −0.033(后半)—— 平,甚至微降
```

**然后是四个问题。**

### ★ A. pyo3 的全局延迟释放池跨解释器 free —— 运行中崩

不销毁任何解释器、干完直接 `os._exit(0)` 跳过 finalize,所以崩的一定是**运行期**:

```
write N=8   6/6 崩,用时 1.0 / 1.0 / 1.2 / 1.5 / 2.7 / 4.1 秒
N=1         0/3 崩(16,446+ 次 collect 全对)   ← 证实是跨解释器,不是单纯 UAF
```

栈每次都一样:

```
ReferencePool::drop_deferred_references_slow
  → subtype_dealloc → _PyObject_Free
  → ___BUG_IN_CLIENT_OF_LIBMALLOC_POINTER_BEING_FREED_WAS_NOT_ALLOCATED
```

根因在 `src/internal/state.rs`:

```rust
static POOL: OnceLock<ReferencePool> = OnceLock::new();     // ★ 进程级
```

`register_decref` 把"detach 状态下 drop 的对象"**不分解释器**塞进同一个 `Vec`;
`drop_deferred_references` 随后在**当前恰好附着的那个解释器**上对池里所有对象
`Py_DECREF`。解释器 A 的对象被解释器 B free 掉,指针不在 B 的 arena 里 → abort。
同时这还是一次对 A 的对象的**无同步 refcount 修改**,而 A 正拿着自己的 GIL 在跑。

**这个分支把 `#[pyclass]` 类型对象、异常类型、模块对象、以及 file-like 回调都做成了
每解释器一份,唯独这个池没有。** 而 `AssumeAttached` 的注释里写过这个危险 ——
识别了陷阱、在一个地方绕开了、没有追到根上。

### ★ B. 跨解释器对象串号 —— 不崩,给错数据

`map_elements` 的 `return_dtype` 经 Rust 回到 Python 时,拿到的是**别的解释器的
`Int64` 类对象**:

```
解释器 1 的回调里   id(type(拿到的 Int64)) == 解释器 2 的 pl.Int64 的 id
N=2 时 28 个 worker 里 14 个中招(50%)
用户看到:TypeError: cannot parse input of type 'Int64' into Polars data type (given: Int64)
```

那条错误消息自指("Int64 不能被解析成 Int64")本身就是串号的指纹。

**对照特别有说服力:上游版在同样的配置下大声拒绝**(`ImportError: PyO3 modules do
not yet support subinterpreters`),而这个分支**默默给你别的解释器的对象**。
把"不支持就报错"换成了"静默给错数据",这比崩溃更糟。

### C. 销毁子解释器时段错误

`≥2` 个解释器做过 polars 并行计算之后销毁,`writefile`(**完全不经 Python 回调**)
和 `compute`(**完全不写**)都崩,而 `import` / 建 `DataFrame` 不崩:

```
N=2  1/3 崩    N=3  3/3    N=4  3/3    N=8  3/3(0.1–0.2s,第一轮就崩)
```

栈之一点名了本分支的 `per_interpreter::teardown` 在 `Py_EndInterpreter` 期间做
`PyImport_ImportModule` —— finalize 中途 import 本身就危险。

**但这条归因最弱**:它只在 own-GIL 下出现,而 own-GIL 只有本分支进得去,
所以做不了 A/B。它也可能是 CPython 3.14 自身在多 own-GIL + 原生线程池下的问题。

### D. 每个子解释器生命周期泄漏 ~60KB,import 慢 3.7 倍

一次只活一个解释器(建 → import → 写 → 关),1200 轮零崩溃,但 RSS 线性涨:

```
own-GIL N=1 1200 轮   +51 KB/轮,前半 56 / 后半 46 —— 略减速,但 1200 轮内无平台
分离来源(400 轮):
  纯 CPython           +4 KB/轮    ← 基线
  只 import polars     +50 KB/轮   ← ★ 泄漏在这
  再写 1 次            +62 KB/轮
  写 200 次            +49 KB/轮   ← 和写次数无关
```

**legacy(shared-GIL)配置下的 A/B 是干净的,归因明确:**

```
上游 .so    330 轮 RSS 92.9 → 93.1 MB   +0.6 KB/轮(平)   用时  7.1s
本分支 .so  330 轮 RSS 93.7 → 114.3 MB  +62 KB/轮(线性) 用时 26.5s   ← 慢 3.7 倍
```

### 没覆盖到的面

```
· own-GIL 下【没有对照组】—— 上游根本进不去,A/C 只能证明"在本分支唯一能用的配置里存在"
· 只测了 write 路径,没测 read(InterpreterHandle 同样挂在 PyFileLikeObject::read 上)
· 没测 streaming engine / map_batches / 其它 Rust→Python 回调
· 只测了 macOS。这里的 abort 来自 libmalloc 的指针检查;
  ★ Linux glibc 大概率【不 abort】而是静默堆破坏 —— Linux 上更难发现,不是更少
· 没测 free-threaded build
· Bug A 在低并发长时间下的累积概率没量化
```

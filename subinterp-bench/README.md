# 子解释器基准

四个可复现的测量,支撑 `subinterp-per-interpreter-state` 分支上的改动。
全部数字来自 Python 3.14.6 / macOS ARM64 / 6 P-core + 12 E-core。

```
probe/        被测扩展:noop / sum_buf / Row / Counter —— 每条路径隔离一种开销
build.sh      编 8 份探针:{对照,本分支} × {默认 ABI, abi3-py310} × {顶层,含子模块}
matrix.py     能力矩阵 —— 能不能加载、隔离到没到位   ← ★ 端到端判据
audit_caches.py  把「持有PyObject→永生吗→堆类型吗」这条判据全量跑一遍
no_override.py   不带任何进程级开关直接 import
submodule.py     wrap_pymodule! 子模块能不能进子解释器
scaling.py    多解释器并发扩展性,对照 vs fork 背靠背   ← ★ 最强的那个证据
single.py     单线程热路径:确认改动没有在无争用时收费
leak.py       串行建/关 N 个解释器,看 RSS 斜率        ← ★ 抓到过一个 2.6MB/解释器 的泄漏
reclaim.py    一批解释器关闭后立刻收回多少
stress.py     长跑:隔离 / 结果正确性 / 存活

METHOD.md     测量纪律 —— 改基准前先看
POLARS.md     polars 在子解释器里的完整实测记录
POLARS-PATCH.md  给 polars 的补丁:回调回正确的解释器
```

`leak.py` 和 `reclaim.py` 问的**不是同一件事**,两个都要跑:前者一次只活一个解释器、
做上千次,斜率不为零就是真泄漏;后者一次建 16 个再一起关,只能看出立刻收回了多少,
0% 也可能只是延迟释放。第一版只有 `reclaim.py`,于是一个线性不收敛的泄漏被
当成"可疑但不致命"放过了。

> **改基准或重跑基准之前先看 [`METHOD.md`](METHOD.md)** —— 那里每一条都对应一次
> 量出假结论的经历,包括下面这条。

## 对照组必须是 fork 的父提交

这个分支基于 `dfdbc46`(PyO3 **main** 上的提交),不是 crates.io 上的 **0.29.2 发行版**。
两者之间隔着上游自己的改动 —— 比如 `#[pyclass]` 在 main 上变成了 GC 跟踪而 0.29.2 上不是:

```
crates.io 0.29.2   gc.is_tracked(Row(1.0,2)) = False
dfdbc46 (父提交)    gc.is_tracked(Row(1.0,2)) = True     ← 上游改的
本分支              gc.is_tracked(Row(1.0,2)) = True
```

拿发行版当对照,量到的是上游的改动,却会记在本分支头上。
**第一次就是这么量出一个不存在的 92% 回归的。** 所以脚本里的对照是 `so_base`。

## 跑法

```bash
# 对照组 = 本分支的父提交
git worktree add /tmp/pyo3-base $(git rev-parse HEAD~7)

# 两个版本各编一份。模块名必须是 abi3t.so —— 初始化符号是 PyInit_abi3t。
cd probe
cargo build --release --config 'patch.crates-io.pyo3.path="/tmp/pyo3-base"'   # 对照
mkdir -p ../so_base && cp target/release/libabi3t.dylib ../so_base/abi3t.so
cargo build --release --config 'patch.crates-io.pyo3.path="../.."'            # ★ 本分支
mkdir -p ../so_fork && cp target/release/libabi3t.dylib ../so_fork/abi3t.so

cd ..
python3.14 scaling.py                    # 并发扩展性,自动跑两边
python3.14 single.py                     # 单线程热路径,自动跑两边
python3.14 leak.py 300                   # 泄漏斜率,自动跑两边
python3.14 reclaim.py                    # 关闭后立刻收回多少,自动跑两边
python3.14 stress.py so_fork 500         # 长跑:隔离 / 结果正确性 / 存活
```

`probe/` 是独立 workspace,不参与 pyo3 自己的 workspace。
macOS 上编扩展需要 `.cargo/config.toml` 里的 `-undefined dynamic_lookup`(已附)。

---

## 结果

### 1. 并发扩展性 — 这是核心结论

N 个 OS 线程,每个绑一个独立的 own-GIL 子解释器,同时敲同一个扩展。

**`Row(...)` 对象创建:**

| worker | 父提交 次/秒 | 扩展 | fork 次/秒 | 扩展 |
|---:|---:|---:|---:|---:|
| 1 | 18,010,547 | 1.00× | 18,561,468 | 1.00× |
| 2 | 25,123,660 | 1.39× | 36,997,603 | 1.99× |
| 4 | 25,354,818 | 1.41× | 69,889,456 | 3.77× |
| 6 | 25,245,295 | 1.40× | 95,106,787 | 5.12× |
| 8 | 22,016,761 | 1.22× | 122,073,240 | 6.58× |
| 12 | 21,762,649 | **1.21×** | 174,995,964 | **9.43×** |

**父提交在 2 个 worker 之后就不再增长,封死在 1.2× 上下;fork 到 9.43×。12 worker 下快 8.0 倍。**

机制:上游把 `#[pyclass]` 的 type 对象缓存在进程级 static 里,所有子解释器共享同一个。
每次实例化都要动那一个 refcount,于是所有核抢同一条缓存行 —— MESI 失效风暴,扩展归零。
本分支给每个解释器一个独立 type 对象,各自的缓存行。

**不走 type 对象的路径没有差别**,这是应该的 —— 出现差别就说明改动泄漏到了不该碰的地方:

| worker | 路径 | 父提交 | fork | 差 |
|---:|---|---:|---:|---:|
| 12 | `noop()` 纯调用 | 444,057,529 | 441,027,889 | 0.7% |
| 12 | `sum_buf()` kernel | 5,087,619 | 5,044,565 | 0.8% |

### 2. 单线程热路径 (`single.py`)

无争用时,每解释器查找要不要钱。15 轮交替测量,取中位数:

| 路径 | 父提交 | fork | 差 | 判定 |
|---|---:|---:|---:|---|
| `noop()` 空调用 | 13.06 ns | 12.98 ns | −0.6% | 噪声内 (±2.1%) |
| `Row(...)` 建对象 | 51.26 ns | 50.89 ns | −0.7% | 噪声内 (±1.8%) |
| `Counter.bump` | 28.83 ns | 29.07 ns | +0.9% | 噪声内 (±1.4%) |

拆开看,单次类型对象查找 0.39 ns → 2.22 ns(多出一次 `PyInterpreterState_Get` +
`GetID` 共 1.5 ns,加一次 TLS 比较)。**在 50 ns 的建对象里淹没在噪声中,所以总量测不出来。**

### 3. 泄漏 (`leak.py`) — 这套基准抓到的那个真 bug

串行建/关 300 个解释器,一次只活一个:

| 解释器里做了什么 | 对照 总增长 | 斜率/千个 | fork 总增长 | 斜率/千个 |
|---|---:|---:|---:|---:|
| 只 `import` | 6.9 MB | 14.2 MB | 2.4 MB | 0.2 MB |
| 建一个 `Row` 就扔 | 4.8 MB | 0.0 MB | 2.5 MB | 0.3 MB |
| `Counter` 实例活到关闭 | 4.8 MB | 0.0 MB | 2.4 MB | 0.2 MB |
| 同上但 `del` 掉 | 6.7 MB | 0.0 MB | 2.4 MB | 0.2 MB |

**修之前**,除了"实例活到关闭"那一行,其余三行都是 **2.6 MB/解释器、线性不收敛**
(200 个解释器 +519 MB)。根因:

```
Py<T> 的 Drop 只在 thread_is_attached() 为真时 decref,否则把 decref
塞进一个【进程级】延迟队列。而 thread_is_attached() 读的是 PyO3 自己的
线程局部计数 ATTACH_COUNT,只有 PyO3 主动 attach 时才 +1。

registry 是被 CPython 直接调的 —— capsule 析构 / atexit 钩子,两处
PyO3 都没参与,ATTACH_COUNT 都是 0。于是 registry 里每一个 Py<PyType>
被 drop 时都走了延迟队列,decref 永远没落到对象上,类型对象不死,
连带钉住那个解释器的全部 import 状态。
```

修法是在 `Registry::drop` 外面套一个 `AssumeAttached` guard(`src/internal/state.rs`),
它只加 attach 计数,**不**冲刷延迟队列 —— 队列里的引用属于别的解释器,在子解释器
finalize 的时候去释放它们,等于跨解释器改引用计数。

诊断路上被证伪的三条:teardown 钩子跑了(16/16 都进了 atexit);gc 遍数无关
(0/1/3/10 完全一样);`atexit._clear()` 掉钩子也不变。真正指认它的是一行
`slot refcnt 3 -> 3` —— drop 调了,refcount 没动。

为什么"有实例活着"就没事:那条路上类型对象是被 CPython 自己的 `subtype_dealloc`
decref 的,压根不经过 registry,所以躲开了这个坑。原来的 `stress.py` 用的正好
是这个形状,所以 4000 个解释器都没抓到。

### 4. 内存回收 (`reclaim.py`)

16 个解释器建好再全部关闭,每个用例独占一个进程(同进程里连着跑多个用例,
前面的堆增长会被后面复用,横向不可比):

| 解释器里做了什么 | 对照 回收 | fork 回收 |
|---|---:|---:|
| 不 import 扩展 | 51.5% | 51.5% |
| 只 `import` | 48.0% | **51.3%** |
| `import` + 调函数 | 48.2% | **51.5%** |
| `import` + 建一个 `Row` 就扔 | 48.2% | **51.5%** |
| `import` + `Counter` 实例活到关闭 | 48.2% | **51.6%** |

fork 每一行都高于对照,而且和"根本不加载扩展"持平 —— 每解释器的类型对象
现在是真的随解释器一起没了。

### 5. 长跑 (`stress.py`)

500 轮 × 8 解释器 = 4,000 个解释器,每个 2,000 次方法调用:

```
结果正确性    全部正确(每个解释器算自己那份种子的答案)
类型独立      每轮都是 N 个不同的 type 地址
存活          未崩溃
```

它的 RSS 数字**不要单独当泄漏判据看** —— 它的解释器体是 `c = Counter(...)` 留活,
恰好是唯一躲开延迟 decref 那条路的形状。泄漏看 `leak.py`。

### 6. 上游测试

`cargo test --lib --release`:850 通过,0 失败。

---

## 三个反直觉的地方

**① 上游不崩,但也不扩展。** 这个基准里对照组全程存活 —— 短生命周期对象的 refcount
争用没有撞上致命竞态。**但它一个核也没多用上。** 崩溃是间歇的,不扩展是必然的,
后者更容易被误当成"正常"。

**② 性能核数不是扩展上界。** 这台机器 6 P-core,而 `noop` 在 12 worker 上扩展到 9.3×,
`sum_buf` 到 11.0× —— 12 个 E-core 贡献了实打实的吞吐。对计算密度高的负载
(比如 BPE 分词)E-core 帮不上,但对调用密集的负载帮得上。**别按 P-core 数拍线程数,要按负载测。**

**③ 循环里别留活对象。** `[f() for _ in range(400_000)]` 攒下 40 万个活对象,
量到的是 GC 遍历而不是构造 —— 在 GC 跟踪状态不同的两个版本之间,这一条就能凭空
造出一个 2 倍的假回归。`single.py` 的内层循环用完就扔。

**④ "一批一起关"和"一个一个关"抓的不是同一个 bug。** 那个 2.6 MB/解释器的泄漏,
在 16 个一起关的表里只表现为"回收 0%",看着像延迟释放;换成一次只活一个、做 300 次,
才现出线性不收敛的原形。`reclaim.py` 和 `leak.py` 都得跑。

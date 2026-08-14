# 子解释器基准

四个可复现的测量,支撑 `subinterp-per-interpreter-state` 分支上的改动。
全部数字来自 Python 3.14.6 / macOS ARM64 / 6 P-core + 12 E-core。

```
probe/        被测扩展:noop / sum_buf / Row / Counter —— 每条路径隔离一种开销
scaling.py    多解释器并发扩展性,对照 vs fork 背靠背   ← ★ 最强的那个证据
single.py     单线程热路径:确认改动没有在无争用时收费
stress.py     长跑:泄漏 / 隔离 / 结果正确性 / 存活
reclaim.py    解释器关闭后的内存回收率
```

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
python3.14 stress.py  so_fork 500        # 长跑:泄漏 / 隔离 / 类型独立 / 存活
python3.14 reclaim.py so_fork 16 cls     # 内存回收率(第三参数 cls|fn 切换是否用 pyclass)
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

### 3. 长跑 (`stress.py`)

500 轮 × 8 解释器 = 4,000 个解释器,每个 2,000 次方法调用:

```
RSS 增长      +24.8 MB,后半程斜率 +3.4 MB/千轮 —— 收敛
结果正确性    全部正确(每个解释器算自己那份种子的答案)
类型独立      每轮都是 N 个不同的 type 地址
存活          未崩溃
```

改动前同样的测试:800 个解释器涨 2.5 GB,线性不收敛。

### 4. 内存回收 (`reclaim.py`) — ★ 这里有一个未修的回归

16 个解释器建好再全部关闭,每个用例独占一个进程(同进程里跑多个用例,
前面的堆增长会被后面复用,横向不可比):

| 解释器里做了什么 | 对照 回收 | fork 回收 |
|---|---:|---:|
| 不 import 扩展 | 51.7% | 51.8% |
| 只 `import`  | 48.3% | **−0.4%** |
| `import` + 调函数 | 48.5% | **−0.1%** |
| `import` + 建一个 `Row` 就扔 | 48.5% | **0.0%** |
| `import` + `Counter` 实例**活到关闭** | 48.5% | 51.9% |

**只要没有 pyclass 实例活到解释器关闭,fork 一点都收不回来** —— 每解释器多留
约 1.76 MB(16 个 = 28 MB)。

已经查清的部分:

```
二分到的提交   de6c9a3 fix(exceptions): per-interpreter storage for create_exception!
               它之前的每一个提交都是 48.4%,它自己和之后都是 −0.5%
复现最小面      一个【完全不含 pyclass】的模块也复现 —— 所以和 #[pyclass] 无关
teardown 跑了   16 个解释器全部进了 atexit 回调,每个 registry 恰好 1 个已填槽位
                (就是 PanicException,create_exception! 建的堆类型)
gc 遍数无关      0 / 1 / 3 / 10 遍结果完全一样 —— 不是"收得不够",是根本没被回收
teardown 无罪    atexit._clear() 掉钩子,结果不变
```

**没查清的**:为什么"有一个 pyclass 实例活到关闭"就能让它收回来。这与直觉相反 ——
活着的实例本该让类型对象更难回收才对。在解释清楚之前,这个回归不算修完。

顺带一课:原来的 `stress.py` 之所以没抓到它,是因为它的解释器体正好是
`c = Counter(...)` 留活 —— **恰好是唯一能回收的那种形状**。

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

#!/usr/bin/env python3
"""同一台机器、同一份扩展、同一批负载,四种并行拓扑背靠背。

  MT           常规 GIL build + 线程
  free-thr     免 GIL build (python3.1Xt) + 线程
  MI 对照      own-GIL 子解释器,扩展编自本分支的父提交
  MI 本分支    own-GIL 子解释器,扩展编自本分支

五条负载各自隔离一种开销,其中两条是对照,不是被测物:

  raw_noop     手写的裸 PyMethodDef,零 PyO3 参与
               —— 用来判断某个现象是 PyO3 的还是 CPython 的
  PyRow        纯 Python 类
               —— 用来判断某个现象是不是扩展特有的

没有这两条对照,免 GIL 下那个 0.08× 会被记在 PyO3 头上。实际上裸 C ABI
一样塌,而纯 Python 类扩展到 6.9× —— 那是 CPython 对【扩展在运行时创建的
PyCFunction】没上延迟引用计数,和 PyO3 无关。

用法: python3.14 topology.py              跑全部四列
      FT_PYTHON=/path/to/python3.14t python3.14 topology.py
"""
import os, subprocess, sys, threading, time, warnings

warnings.filterwarnings("ignore")

SECS = 1.0
WORKERS = 12
HERE = os.path.dirname(os.path.abspath(__file__))
FT_PYTHON = os.environ.get("FT_PYTHON", "/opt/homebrew/bin/python3.14t")

# (标签, 表达式) —— 表达式在两种 harness 里都得能求值
CASES = [
    ("raw_noop()   裸 C ABI 对照", "abi3t.raw_noop()"),
    ("noop()       PyO3 pyfunction", "abi3t.noop()"),
    ("Row(1.0,2)   PyO3 建对象", "abi3t.Row(1.0, 2)"),
    ("sum_buf(mv)  内部 detach 真计算", "abi3t.sum_buf(mv)"),
    ("PyRow(1,2)   纯 Python 类对照", "PyRow(1.0, 2)"),
]

PRELUDE = """
import array
class PyRow:
    __slots__ = ('a', 'b')
    def __init__(s, a, b): s.a = a; s.b = b
_buf = array.array('d', [1.0] * 4096)
mv = memoryview(_buf)
"""


# ── 拓扑一/二:线程 ────────────────────────────────────────────────
def bench_threads(so_dir, stmt, n):
    """N 个 OS 线程共享一个解释器。返回总吞吐(次/秒)。"""
    sys.path.insert(0, so_dir)
    import abi3t  # noqa: F401

    # ── 这个 harness 的两条硬约束,都是被量出来的 ──────────────────────
    #
    # 免 GIL 下,任何"每次访问都要 incref 一个共享对象"的名字查找都会让 12 个
    # 线程抢同一条缓存行,于是 harness 自己变成瓶颈。同一个纯 Python 负载:
    #
    #   闭包变量 (LOAD_DEREF,12 线程共用一个 cell)    n=12   8.9 M/s   0.44×
    #   模块全局 (LOAD_GLOBAL,有专门优化路径)         n=12 141.3 M/s   6.78×
    #
    # 单线程两者都是 20.6 M/s —— 差别只在多线程,而且是 16 倍。所以:
    #   ① 被测表达式和循环体都必须 exec 进【真正的模块全局】,不能用新建的 dict;
    #   ② worker 函数本身也得在那里定义,否则它对 fn 的引用又变回闭包 cell。
    ns = globals()
    ns["abi3t"] = abi3t
    exec(PRELUDE, ns)
    ns["_stop"] = [False]
    ns["_counts"] = [0] * n
    exec(f"""
def _worker(i):
    k = 0
    while not _stop[0]:
        for _ in range(200):
            {stmt}
        k += 200
    _counts[i] = k
""", ns)
    work, stop, counts = ns["_worker"], ns["_stop"], ns["_counts"]

    ts = [threading.Thread(target=work, args=(i,)) for i in range(n)]
    t0 = time.perf_counter()
    for t in ts:
        t.start()
    time.sleep(SECS)
    stop[0] = True
    for t in ts:
        t.join()
    return sum(counts) / (time.perf_counter() - t0)


# ── 拓扑三/四:own-GIL 子解释器 ────────────────────────────────────
SUB_BODY = """
import sys, _imp, time
try: _imp._override_multi_interp_extensions_check(-1)
except Exception: pass
sys.path.insert(0, {so!r})
import abi3t
{prelude}
for _ in range(500):
    {stmt}
n = 0
deadline = time.perf_counter() + {secs}
while time.perf_counter() < deadline:
    for _ in range(200):
        {stmt}
    n += 200
_q.put(n)
"""


def bench_interps(so_dir, stmt, n):
    """N 个 OS 线程,每个绑一个独立 own-GIL 子解释器。"""
    from concurrent import interpreters

    body = SUB_BODY.format(so=so_dir, prelude=PRELUDE, stmt=stmt, secs=SECS)
    got, errs, keep = [], [], []
    lock = threading.Lock()

    def work(_i):
        try:
            it = interpreters.create()
            with lock:
                keep.append(it)
            q = interpreters.create_queue()
            it.prepare_main(_q=q)
            it.exec(body)
            with lock:
                got.append(q.get())
        except Exception as e:
            with lock:
                errs.append(f"{type(e).__name__}: {str(e).splitlines()[-1][:70]}")

    ts = [threading.Thread(target=work, args=(i,)) for i in range(n)]
    t0 = time.perf_counter()
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    el = time.perf_counter() - t0
    for it in keep:
        try:
            it.close()
        except Exception:
            pass
    if len(got) != n:
        raise RuntimeError(errs[0] if errs else "worker 未全部完成")
    return sum(got) / el


# ── 每个单元格独占一个子进程 ──────────────────────────────────────
#
# 必须这样。`MI 对照` 跑 Row(...) 会 SIGSEGV —— 那正是这个分支要修的东西。
# 跑在主进程里,它会把整张表一起带走;而且崩溃本身是结果,不该被吞掉。

RUNNERS = {
    "threads": bench_threads,
    "interps": bench_interps,
}

if len(sys.argv) == 5 and sys.argv[1] in RUNNERS:
    _kind, _so, _stmt = sys.argv[1], sys.argv[2], sys.argv[3]
    print(RUNNERS[_kind](_so, _stmt, int(sys.argv[4])))
    raise SystemExit


def cell(python, kind, so_dir, stmt, n):
    """返回吞吐。子进程被信号打死时抛出带信号名的异常。"""
    r = subprocess.run([python, __file__, kind, so_dir, stmt, str(n)],
                       capture_output=True, text=True)
    if r.returncode < 0:
        import signal
        raise RuntimeError(f"崩溃 {signal.Signals(-r.returncode).name}")
    if r.returncode != 0:
        tail = (r.stderr.strip().splitlines() or ["无 stderr"])[-1]
        raise RuntimeError(tail[:60])
    return float(r.stdout.strip())


# ── 主表 ──────────────────────────────────────────────────────────
def have_ft():
    if not os.path.exists(FT_PYTHON):
        return f"未找到 {FT_PYTHON}"
    r = subprocess.run([FT_PYTHON, "-c",
                        "import sysconfig;print(sysconfig.get_config_var('Py_GIL_DISABLED'))"],
                       capture_output=True, text=True)
    if r.stdout.strip() != "1":
        return f"{FT_PYTHON} 不是免 GIL build"
    if not os.path.exists("/tmp/ft_base/abi3t.so"):
        return "缺 /tmp/ft_base/abi3t.so(需用 PYO3_PYTHON=<免GIL解释器> 另编一份)"
    return None


ft_err = have_ft()
COLS = [
    ("MT (GIL)",  sys.executable, "threads", f"{HERE}/so_base"),
    ("free-thr",  FT_PYTHON,      "threads", "/tmp/ft_base"),
    ("MI 对照",    sys.executable, "interps", f"{HERE}/so_base"),
    ("MI 本分支",  sys.executable, "interps", f"{HERE}/so_fork"),
]

W = 96
print("=" * W)
print(f"并行拓扑对比   {WORKERS} worker / 每点 {SECS}s   Python {sys.version.split()[0]}   "
      f"{os.cpu_count()} 逻辑核")
print("=" * W)
if ft_err:
    print(f"  free-thr 列跳过:{ft_err}\n")
print(f"  {'负载':32}" + "".join(f"{c:>14}" for c, *_ in COLS) + f"{'单 worker':>12}")
print("  " + "-" * (W - 4))

for label, stmt in CASES:
    cells, base_rate = [], None
    for name, py, kind, so in COLS:
        if name == "free-thr" and ft_err:
            cells.append("—")
            continue
        try:
            many = cell(py, kind, so, stmt, WORKERS)
            one = cell(py, kind, so, stmt, 1)
            if base_rate is None:
                base_rate = one
            cells.append(f"{many / one:.2f}×")
        except Exception as e:
            cells.append(str(e)[:13])          # 崩溃/报错原样带出,不用哨兵词
    rate = f"{base_rate / 1e6:.1f}M/s" if base_rate else "—"
    print(f"  {label:32}" + "".join(f"{c:>14}" for c in cells) + f"{rate:>12}")

print("=" * W)
print("  倍数 = 该拓扑下 12 worker 吞吐 ÷ 同拓扑 1 worker 吞吐;单 worker 列取第一个成功的拓扑。")
print("  每个单元格独占一个子进程 —— MI 对照跑建对象时会 SIGSEGV,那是结果的一部分。")
print("=" * W)

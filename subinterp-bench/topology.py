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
    ns = {"abi3t": abi3t}
    exec(PRELUDE, ns)
    fn = eval(f"lambda: {stmt}", ns)

    stop = [False]
    counts = [0] * n

    def work(i):
        k = 0
        while not stop[0]:
            for _ in range(200):
                fn()
            k += 200
        counts[i] = k

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


# ── 子进程入口:免 GIL 那一列得换解释器跑 ──────────────────────────
if len(sys.argv) == 4 and sys.argv[1] == "--threads":
    print(bench_threads(sys.argv[2], sys.argv[3], WORKERS))
    print(bench_threads(sys.argv[2], sys.argv[3], 1))
    raise SystemExit


def ft_column(stmt):
    r = subprocess.run([FT_PYTHON, __file__, "--threads", "/tmp/ft_base", stmt],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(r.stderr.strip().splitlines()[-1][:80])
    many, one = (float(x) for x in r.stdout.split())
    return many, one


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


COLS = [
    ("MT (GIL)",   lambda s, n: bench_threads(f"{HERE}/so_base", s, n)),
    ("free-thr",   None),                                    # 走子进程
    ("MI 对照",     lambda s, n: bench_interps(f"{HERE}/so_base", s, n)),
    ("MI 本分支",   lambda s, n: bench_interps(f"{HERE}/so_fork", s, n)),
]

W = 92
ft_err = have_ft()
print("=" * W)
print(f"并行拓扑对比   {WORKERS} worker / 每点 {SECS}s   Python {sys.version.split()[0]}")
print("=" * W)
if ft_err:
    print(f"  free-thr 列跳过:{ft_err}\n")
print(f"  {'负载':32}" + "".join(f"{c:>13}" for c, _ in COLS) + f"{'单线程':>12}")
print("  " + "-" * (W - 4))

for label, stmt in CASES:
    cells, base_rate = [], None
    for name, fn in COLS:
        try:
            if fn is None:
                if ft_err:
                    cells.append("—")
                    continue
                many, one = ft_column(stmt)
            else:
                many, one = fn(stmt, WORKERS), fn(stmt, 1)
            if base_rate is None:
                base_rate = one
            cells.append(f"{many / one:.2f}×")
        except Exception as e:
            # 失败要带出真实原因,不要一个哨兵词
            cells.append(f"失败({type(e).__name__})")
    rate = f"{base_rate / 1e6:.1f}M/s" if base_rate else "—"
    print(f"  {label:32}" + "".join(f"{c:>13}" for c in cells) + f"{rate:>12}")

print("=" * W)
print("  倍数 = 该拓扑下 12 worker 吞吐 ÷ 同拓扑 1 worker 吞吐。单线程列取第一个成功的拓扑。")
print("=" * W)

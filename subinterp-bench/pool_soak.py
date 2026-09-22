"""隔离 pyo3 全局 ReferencePool 的跨解释器 decref。

背景、判据为什么这么设计、以及一次失败的修复,见 BUG-POOL.md。

只做 detach/attach 密集的 polars 计算(每次 enter_polars 都会 drain 全局 POOL),
【不销毁任何子解释器】,干完活立刻 os._exit(0) 跳过解释器 finalize —— 于是
一旦崩,就一定是【运行中】崩的,和销毁/退出期的问题无关。

用法: pool_soak.py <polars 包目录> <解释器数 N> <秒数> [collect|write] [shared|copies]

  shared(默认)= 所有解释器 import 同一份包;copies = 每个解释器一份物理拷贝
  (Pyronova 的 isolate 路线)。M1.3 之后,shared 下工作线程回调 Python 会大声失败
  (AmbiguousInterpreter),所以池子的正确性要在 copies 下验;shared 验的是"只许大声失败,
  不许 SIGABRT / 静默损坏"。

  N=1 是对照组:同一个包、同一份负载,只改解释器数。它不崩而 N>=4 崩,
  这个差就是"跨解释器"本身 —— 不是 use-after-free,也不是 polars 的线程模型。

退出码 134 (SIGABRT) 是一种结果,不是"脚本跑挂了" —— 见 METHOD.md「崩溃是结果」。
"""
import json, os, sys, threading, time, warnings
warnings.filterwarnings("ignore")
from concurrent import interpreters

PKG, N, SECS = sys.argv[1], int(sys.argv[2]), float(sys.argv[3])
MODE = sys.argv[4] if len(sys.argv) > 4 else "collect"
DEPLOY = sys.argv[5] if len(sys.argv) > 5 else "shared"
if DEPLOY == "copies":
    import shutil, tempfile
    _tmp = tempfile.mkdtemp(prefix="pool-soak-")
    PKGS = [shutil.copytree(PKG, os.path.join(_tmp, "c%d" % i), symlinks=True) for i in range(N)]
else:
    PKGS = [PKG] * N

BODY = r'''
import sys, time, json
sys.path.insert(0, _pkg)
import polars as pl
from concurrent import interpreters

me = interpreters.get_current().id
seed = _seed
deadline = _deadline
n = 20000
keys = [f"k{(i * 7 + seed * 3) % 61:02d}" for i in range(n)]
vals = [(i * 13 + seed * 29) % 1000 for i in range(n)]
df = pl.DataFrame({"k": keys, "v": vals})
exp = {}
for k, v in zip(keys, vals):
    exp[k] = exp.get(k, 0) + v
exp_sorted = [(k, exp[k]) for k in sorted(exp)]

ops = 0
bad = 0
first_bad = None
MODE = "__MODE__"

class Sink:
    def __init__(s): s.buf = bytearray(); s.interps = set()
    def write(s, b):
        s.interps.add(interpreters.get_current().id)
        s.buf += (b.encode() if isinstance(b, str) else b); return len(b)
    def flush(s): pass
    def seek(s, *a): return 0
    def tell(s): return len(s.buf)
    def close(s): pass

import os as _os
gold = {}
if MODE == "write":
    _p = "/tmp/st_pool_g_%d" % seed
    for _n, _w in (("csv", lambda d, t: d.write_csv(t)), ("ipc", lambda d, t: d.write_ipc(t))):
        _w(df, _p); gold[_n] = open(_p, "rb").read()
    _os.unlink(_p)

while time.time() < deadline:
    if MODE == "collect":
        got = df.lazy().group_by("k").agg(pl.col("v").sum()).sort("k").collect().rows()
        got = [(r[0], int(r[1])) for r in got]
        ops += 1
        if got != exp_sorted:
            bad += 1
            if first_bad is None:
                first_bad = str([(a, b) for a, b in zip(got, exp_sorted) if a != b][:2])
    else:
        for _n, _w in (("csv", lambda d, t: d.write_csv(t)), ("ipc", lambda d, t: d.write_ipc(t))):
            sk = Sink(); _w(df, sk); ops += 1
            if bytes(sk.buf) != gold[_n] or sk.interps != {me}:
                bad += 1
                if first_bad is None:
                    first_bad = "fmt=%s len=%d/%d 写在解释器 %s (我是 %d)" % (_n, len(sk.buf), len(gold[_n]), sorted(sk.interps), me)
_q.put(json.dumps([seed, me, ops, bad, first_bad]))
'''.replace("__MODE__", MODE)

deadline = time.time() + SECS
res, errs = [], []
lock = threading.Lock()


def w(i):
    try:
        it = interpreters.create()
        q = interpreters.create_queue()
        it.prepare_main(_q=q, _seed=i + 1, _deadline=deadline, _pkg=PKGS[i])
        it.exec(BODY)
        with lock:
            res.append(json.loads(q.get()))
    except BaseException as e:
        import traceback
        with lock:
            errs.append("%s: %s" % (type(e).__name__,
                                    traceback.format_exc().strip().splitlines()[-1][:200]))


print("pkg=%s N=%d %.0fs mode=%s deploy=%s" % (PKG, N, SECS, MODE, DEPLOY))
sys.stdout.flush()
t0 = time.time()
ts = [threading.Thread(target=w, args=(i,)) for i in range(N)]
[t.start() for t in ts]
[t.join() for t in ts]
wall = time.time() - t0
for e in errs:
    print("  线程异常: " + e)
tot = sum(r[2] for r in res)
bad = sum(r[3] for r in res)
print("WORKLOAD-DONE worker %d/%d  collect 次数 %d  结果与解析解不符 %d 次  墙钟 %.1fs"
      % (len(res), N, tot, bad, wall))
for r in res:
    if r[4]:
        print("  seed=%d 首个差异 %s" % (r[0], r[4]))
sys.stdout.flush()
sys.stderr.flush()
if DEPLOY == "copies":
    shutil.rmtree(_tmp, ignore_errors=True)
os._exit(0 if (not errs and bad == 0 and len(res) == N) else 3)

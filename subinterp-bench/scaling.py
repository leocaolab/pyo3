#!/usr/bin/env python3
"""多解释器并发扩展性:父提交 PyO3 vs fork,背靠背。

拓扑:N 个 OS 线程,每个绑定一个独立的 own-GIL 子解释器,同时敲同一个扩展。
三条路径分别隔离不同的开销:

  noop      纯调用开销 —— 框架每次调用的固定成本
  sum_buf   有工作量的 kernel,内部 detach 释放 GIL —— 真正的并行计算
  Row(...)  对象创建 —— 上游共享 type 对象的那条路

前两条不碰 type 对象,两个版本应当【没有差别】;差别只应出现在第三条上。
如果 noop 上也出现了差别,那说明改动泄漏到了不该碰的路径,是回归。
"""
import os, sys, threading, time, warnings
from concurrent import interpreters

warnings.filterwarnings("ignore")

BUF_ELEMS = 4096          # 32 KB,驻留 L1/L2
SECONDS = 1.2             # 每个点跑多久

BODY = """
import sys, _imp, time, array
try: _imp._override_multi_interp_extensions_check(-1)
except Exception: pass
sys.path.insert(0, {path!r})
import abi3t

buf = array.array('d', [1.0] * {elems})
mv = memoryview(buf)

# 预热,把首次初始化排除掉
for _ in range(1000):
    {warm}

n = 0
deadline = time.perf_counter() + {secs}
while time.perf_counter() < deadline:
    for _ in range(200):
        {stmt}
    n += 200
_q.put(n)
"""

PATHS = {
    "noop 纯调用":   ("abi3t.noop()",          "abi3t.noop()"),
    "sum_buf kernel": ("abi3t.sum_buf(mv)",     "abi3t.sum_buf(mv)"),
    "Row(...) 建对象": ("abi3t.Row(1.0, 2)",     "abi3t.Row(1.0, 2)"),
}


def run(so_dir, stmt, warm, n_workers):
    """返回总吞吐(次/秒),或 None 表示崩溃/失败。"""
    got, errs, keep = [], [], []
    lock = threading.Lock()

    def work(_i):
        it = None
        try:
            it = interpreters.create()
            with lock:
                keep.append(it)
            q = interpreters.create_queue()
            it.prepare_main(_q=q)
            it.exec(BODY.format(path=os.path.abspath(so_dir), elems=BUF_ELEMS,
                                secs=SECONDS, stmt=stmt, warm=warm))
            with lock:
                got.append(q.get())
        except Exception as e:
            with lock:
                errs.append(f"{type(e).__name__}: {str(e).splitlines()[-1][:70]}")

    ts = [threading.Thread(target=work, args=(i,)) for i in range(n_workers)]
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
    if len(got) != n_workers:
        return None, errs[0] if errs else "worker 未全部完成"
    return sum(got) / el, None


W = 84
print("=" * W)
print(f"多解释器并发扩展性   {os.cpu_count()} 逻辑核 "
      f"(P={os.popen('sysctl -n hw.perflevel0.logicalcpu').read().strip()} "
      f"E={os.popen('sysctl -n hw.perflevel1.logicalcpu').read().strip()})")
print("=" * W)

WORKERS = [1, 2, 4, 6, 8, 12]

for label, (stmt, warm) in PATHS.items():
    print(f"\n── {label} " + "─" * (W - len(label) - 4))
    print(f"  {'worker':>7} │ {'父提交 PyO3':>22} │ {'★ fork':>22}")
    print(f"  {'':>7} │ {'次/秒':>11} {'扩展':>9} │ {'次/秒':>11} {'扩展':>9}")
    print("  " + "─" * (W - 2))
    base = {}
    for n in WORKERS:
        cells = []
        for tag, d in (("up", "so_base"), ("fk", "so_fork")):
            r, err = run(d, stmt, warm, n)
            if r is None:
                cells.append((None, err))
            else:
                base.setdefault(tag, r if n == 1 else None)
                if base.get(tag) is None and n == 1:
                    base[tag] = r
                cells.append((r, None))
        row = f"  {n:>7} │"
        for tag, (r, err) in zip(("up", "fk"), cells):
            if r is None:
                row += f" {'★ 失败':>22} │" if tag == "up" else f" {'★ 失败':>22}"
                if err:
                    row += f"  {err[:40]}"
            else:
                b = base.get(tag) or r
                sc = r / b
                cell = f"{r:11,.0f} {sc:8.2f}×"
                row += f" {cell} │" if tag == "up" else f" {cell}"
        print(row)

print(f"\n{'=' * W}")
print("  只有 Row(...) 那条路该有差别。noop / sum_buf 上出现差别 = 回归。")
print("=" * W)

"""M3.2 (#10): cost of a #[pyfunction] call (every call drains the reference pool on entry).

  idle      — no deferred decrefs anywhere: upstream checks one pool's flag, the
              fork one counter
  dirty     — another interpreter has a decref queued (its pool stays dirty):
              the fork looks up this interpreter's pool on every call

Usage:  python3.14 subinterp-bench/attach_cost.py
"""
import os, statistics, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
CHILD = r'''
import sys, timeit
from concurrent import interpreters
d, override, dirty = sys.argv[1], sys.argv[2] == "1", sys.argv[3] == "1"
pre = "import _imp; _imp._override_multi_interp_extensions_check(-1)\n" if override else ""
A = interpreters.create()
A.exec(pre + f"import sys; sys.path.insert(0, {d!r}); import abi3t\nclass P: pass\nkeep = P()\n")
if dirty:
    # keep a decref queued in B's pool for the whole measurement: B drops inside py.detach and
    # stays detached for 15 s (upstream: A's first call drains it, wrongly, and the pool is clean)
    import threading, time
    B = interpreters.create()
    B.exec(pre + f"import sys; sys.path.insert(0, {d!r}); import abi3t\nclass P: pass\no = P()\n")
    threading.Thread(target=B.exec, args=("abi3t.detach_race(o, 15000)",), daemon=True).start()
    time.sleep(0.2)
q = interpreters.create_queue(); A.prepare_main(q=q)
A.exec("import timeit\nn=2_000_000\n"
       "q.put(repr(sorted(timeit.timeit('f()', globals={'f': abi3t.noop}, number=n) / n * 1e9 for _ in range(7))[3]))")
print(q.get(), flush=True)
import os; os._exit(0)
'''
print("| build | pools | ns per noop() call (median of 7) |")
print("|---|---|---:|")
for build in ("so_base", "so_fork"):
    for dirty in ("0", "1"):
        vals = []
        for _ in range(3):
            p = subprocess.run([sys.executable, "-c", CHILD, os.path.join(HERE, build), "1" if build == "so_base" else "0", dirty],
                               capture_output=True, text=True, timeout=300)
            if p.returncode != 0:
                vals = None; err = p.stderr.strip().splitlines()[-1][:80]; break
            vals.append(float(eval(p.stdout.strip().splitlines()[-1])))
        label = "idle" if dirty == "0" else "another interp dirty"
        print(f"| {build} | {label} | " + (f"{statistics.median(vals):.2f}" if vals else f"error: {err}") + " |")

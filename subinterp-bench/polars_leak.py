"""M4.2 (#13): per-interpreter-lifetime leak and import cost with polars (POLARS.md "D").

One interpreter alive at a time: create → import polars → write one CSV to memory →
close, ROUNDS times. Reports RSS growth per round (second half, after warm-up) and
wall time. The same polars source built against upstream PyO3 and against this
fork gives the A/B. Upstream only loads in the legacy config (shared GIL, shared
obmalloc), so that is the A/B config; `isolated` runs the fork alone.

Usage:
    python3.14 subinterp-bench/polars_leak.py <polars pkg> [ROUNDS] [legacy|isolated]
"""

import os, resource, sys, time

PKG = sys.argv[1]
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 300
CONFIG = sys.argv[3] if len(sys.argv) > 3 else "legacy"

import _interpreters

BODY = f"""
import sys, io
sys.path.insert(0, {PKG!r})
import polars as pl
pl.DataFrame({{'a': [1, 2, 3]}}).write_csv(io.BytesIO())
"""


def rss_kb():
    # macOS: ru_maxrss is bytes and a peak; read the current RSS instead.
    import subprocess
    return int(subprocess.run(["ps", "-o", "rss=", "-p", str(os.getpid())], capture_output=True, text=True).stdout)


def main():
    os.environ["POLARS_FORCE_PKG"] = "64"
    samples = []
    t0 = time.time()
    import_s = []
    for i in range(ROUNDS):
        it = _interpreters.create(CONFIG)
        if CONFIG == "legacy":
            _interpreters.exec(it, "import _imp; _imp._override_multi_interp_extensions_check(-1)")
        t = time.time()
        err = _interpreters.exec(it, BODY)
        import_s.append(time.time() - t)
        if err is not None:
            print("exec failed:", err)
            return 1
        _interpreters.destroy(it)
        if i % 10 == 0:
            samples.append((i, rss_kb()))
    wall = time.time() - t0
    half = [s for s in samples if s[0] >= ROUNDS // 2]
    slope = (half[-1][1] - half[0][1]) / max(1, half[-1][0] - half[0][0])
    first, last = samples[0][1], samples[-1][1]
    med = sorted(import_s)[len(import_s) // 2]
    print(f"{os.path.basename(PKG.rstrip('/'))} {CONFIG} {ROUNDS} rounds: RSS {first/1024:.1f} → {last/1024:.1f} MB, "
          f"second-half slope {slope:+.1f} KB/round, median import+write {med*1000:.1f} ms, wall {wall:.1f} s")
    return 0


if __name__ == "__main__":
    sys.exit(main())

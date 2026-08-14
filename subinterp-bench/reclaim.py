#!/usr/bin/env python3
"""解释器关闭后收回了多少内存 —— 对照 vs fork。

一个用例一个进程。同一个进程里连着跑多个用例是量不准的:前面的用例把堆撑大,
后面的用例复用那块堆,于是"存活占用"会随顺序变化,横向没法比。
(第一版就是这么量出"不 import 比 import 还占内存"这种自相矛盾的数的。)

用 RSS,不是 ru_maxrss —— 后者是峰值,单调不减,测不出释放。

用法: python3.14 reclaim.py            跑完整对比表
      python3.14 reclaim.py <so目录> <用例>   跑单格(内部用)
"""
import gc, os, subprocess, sys, time, warnings

warnings.filterwarnings("ignore")

N = 16
CASES = {
    "none": ("不 import 扩展", ""),
    "imp": ("只 import", "import abi3t"),
    "fn": ("import + 调函数", "import abi3t\nabi3t.noop()"),
    "row": ("import + 建一个 Row 就扔", "import abi3t\nabi3t.Row(1.0, 2)"),
    "live": ("import + Counter 实例活到关闭",
             "import abi3t\nc = abi3t.Counter(1)\nfor _ in range(100): c.bump(1)"),
}


def rss_mb():
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(os.getpid())],
                         capture_output=True, text=True).stdout.strip()
    return int(out) / 1024 if out else 0.0


def run_one(so_dir, case):
    """建 N 个解释器 → 各自跑一遍 body → 全部关掉。打印 存活MB 关闭后MB 回收%。"""
    from concurrent import interpreters
    head = (f"import sys, _imp\n"
            f"try: _imp._override_multi_interp_extensions_check(-1)\n"
            f"except Exception: pass\n"
            f"sys.path.insert(0, {os.path.abspath(so_dir)!r})\n")
    body = head + CASES[case][1]

    gc.collect()
    before = rss_mb()
    its = []
    for _ in range(N):
        it = interpreters.create()
        its.append(it)
        it.exec(body)
    gc.collect()
    live = rss_mb()
    for it in its:
        it.close()
    gc.collect()
    time.sleep(0.3)
    gc.collect()
    after = rss_mb()
    print(f"{live - before:.1f} {after - before:.1f} "
          f"{100 * (live - after) / max(live - before, 0.01):.1f}")


if len(sys.argv) == 3:
    run_one(sys.argv[1], sys.argv[2])
    raise SystemExit

BUILDS = [("so_base", "对照"), ("so_fork", "fork")]
W = 78
print("=" * W)
print(f"解释器关闭后的内存回收   {N} 个解释器,每个用例独占一个进程")
print("=" * W)
print(f"  {'解释器里做了什么':28} " +
      " ".join(f"{'│ ' + tag + ' 存活':>13}{'关闭后':>9}{'回收':>8}" for _, tag in BUILDS))
print("  " + "-" * (W - 4))

for case, (label, _) in CASES.items():
    cells = []
    for so_dir, _tag in BUILDS:
        r = subprocess.run([sys.executable, __file__, so_dir, case],
                           capture_output=True, text=True)
        if r.returncode != 0:
            cells.append(f"{'│ 失败':>30}  {r.stderr.strip().splitlines()[-1][:24]}")
            continue
        live, after, pct = r.stdout.split()
        cells.append(f"│ {live:>10}MB {after:>7}MB {pct:>6}%")
    print(f"  {label:28} " + " ".join(cells))

print("=" * W)
print("  每行都该和「不 import 扩展」那行持平。修复前,除了「实例活到关闭」以外全是 0%。")
print("=" * W)

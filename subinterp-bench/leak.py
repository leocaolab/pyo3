#!/usr/bin/env python3
"""串行建/关 N 个解释器,看 RSS 涨不涨。

这是四个脚本里唯一能抓到无界泄漏的那个,原因值得写下来:

`reclaim.py` 问的是"16 个一起建、一起关,立刻收回多少",答案可以是 0% 而仍然
没有泄漏 —— 也许只是延迟释放。这个脚本问的是"一次只活一个,做 1000 次",
斜率不为零就是真泄漏,没有别的解释。

它们量的不是同一件事。第一版只有 reclaim.py,于是"0% 回收"被当成一个可疑但
不致命的现象;换成这个脚本才看出是 2.6 MB/解释器、线性不收敛。

解释器体分四种形状,因为泄漏只在其中三种出现 —— 差别是关闭时有没有 pyclass
实例还活着,那决定了类型对象是被 CPython 自己的 dealloc 释放,还是被
PerInterpreterCell 的 registry 释放。走后一条路时才踩得到坑。

用法: python3.14 leak.py [轮数]
"""
import gc, os, subprocess, sys, warnings

warnings.filterwarnings("ignore")

# argv 有两种形状:顶层 `leak.py [轮数]`,和它自己派生的子进程 `leak.py <so目录> <用例> <轮数>`
ROUNDS = int(sys.argv[1]) if len(sys.argv) == 2 else 400
BUILDS = [("so_base", "对照"), ("so_fork", "fork")]
CASES = {
    "imp": ("只 import", ""),
    "row": ("建一个 Row 就扔", "abi3t.Row(1.0, 2)"),
    "live": ("Counter 实例活到关闭", "c = abi3t.Counter(1)\nfor _ in range(100): c.bump(1)"),
    "live-del": ("同上但 del 掉", "c = abi3t.Counter(1)\nfor _ in range(100): c.bump(1)\ndel c"),
}


def rss_mb():
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(os.getpid())],
                         capture_output=True, text=True).stdout.strip()
    return int(out) / 1024 if out else 0.0


def run_one(so_dir, case, rounds):
    from concurrent import interpreters
    body = (f"import sys, _imp\n"
            f"try: _imp._override_multi_interp_extensions_check(-1)\n"
            f"except Exception: pass\n"
            f"sys.path.insert(0, {os.path.abspath(so_dir)!r})\n"
            f"import abi3t\n") + CASES[case][1]
    gc.collect()
    base = rss_mb()
    half = None
    for i in range(1, rounds + 1):
        it = interpreters.create()          # ★ 一次只活一个
        it.exec(body)
        it.close()
        if i == rounds // 2:
            gc.collect()
            half = rss_mb() - base
    gc.collect()
    total = rss_mb() - base
    # 后半程斜率:前半程还在填堆,只有后半程能说明有没有收敛
    slope = (total - half) / (rounds - rounds // 2) * 1000
    print(f"{total:.1f} {slope:.1f}")


if len(sys.argv) == 4:
    run_one(sys.argv[1], sys.argv[2], int(sys.argv[3]))
    raise SystemExit

W = 76
print("=" * W)
print(f"串行建/关 {ROUNDS} 个解释器 —— 一次只活一个,斜率不为零就是泄漏")
print("=" * W)
print(f"  {'解释器里做了什么':26} " +
      " ".join(f"│ {tag:>6} 总增长  斜率/千个" for _, tag in BUILDS))
print("  " + "-" * (W - 4))
for case, (label, _) in CASES.items():
    cells = []
    for so_dir, _tag in BUILDS:
        r = subprocess.run([sys.executable, __file__, so_dir, case, str(ROUNDS)],
                           capture_output=True, text=True)
        if r.returncode != 0:
            cells.append(f"│ {'失败':>20}")
            continue
        total, slope = r.stdout.split()
        cells.append(f"│ {total:>9}MB {slope:>8}MB")
    print(f"  {label:26} " + " ".join(cells))
print("=" * W)

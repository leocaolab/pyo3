#!/usr/bin/env python3
"""单线程热路径:fork 的每解释器查找到底要不要钱。

两个坑,这个脚本都躲开了 —— 它们各自都足以量出一个假的 2 倍回归:

  ① 对照组必须是 fork 的【父提交】,不是 crates.io 上的发行版。
     两者之间隔着上游自己的改动(比如 pyclass 变成 GC 跟踪),
     拿发行版当对照,量到的是上游的改动,记在 fork 头上。

  ② 内层循环不能把对象留活。`[f() for _ in range(400_000)]` 会攒下
     40 万个活对象,量到的是 GC 遍历,不是构造。用完就扔。

交替(ABAB)测量:顺序跑完 A 再跑 B,机器的热状态会系统性地偏袒其中一个。

用法: python3.14 single.py [轮数]
"""
import os, statistics, subprocess, sys, textwrap

ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 15
INNER = 400_000

BUILDS = [("so_base", "父提交"), ("so_fork", "fork")]

CHILD = textwrap.dedent("""
    import sys, time
    sys.path.insert(0, sys.argv[1])
    import abi3t
    what, N = sys.argv[2], {inner}
    if what == "row":
        R = abi3t.Row; f = lambda: R(1.0, 2)
    elif what == "noop":
        f = abi3t.noop
    else:
        c = abi3t.Counter(3); f = lambda: c.bump(1)

    def once():
        t0 = time.perf_counter()
        for _ in range(N):
            f()                      # ★ 不收集 —— 收集就是在测 GC
        return time.perf_counter() - t0

    for _ in range(3): once()        # 预热
    print(min(once() for _ in range(5)) / N * 1e9)
""").format(inner=INNER)


def measure(so_dir, what):
    r = subprocess.run([sys.executable, "-c", CHILD, os.path.abspath(so_dir), what],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"{so_dir}/{what} 失败:\n{r.stderr.strip()[-600:]}")
    return float(r.stdout.strip())


W = 74
print("=" * W)
print(f"单线程热路径   对照 = fork 的父提交   {ROUNDS} 轮交替,{INNER:,} 次/轮,取中位数")
print("=" * W)
print(f"  {'路径':16} {'父提交 ns':>11} {'fork ns':>10} {'差':>9}   判定")
print("  " + "-" * (W - 4))

for what, label in (("noop", "noop 空调用"), ("row", "Row(...) 建对象"),
                    ("ctr", "Counter.bump")):
    a, b = [], []
    for _ in range(ROUNDS):                    # ★ 交替,不是先跑完一边
        a.append(measure(BUILDS[0][0], what))
        b.append(measure(BUILDS[1][0], what))
    u, f = statistics.median(a), statistics.median(b)
    d = 100 * (f - u) / u
    # 判定门槛取两边各自的离散度,而不是拍一个百分比
    spread = max(statistics.pstdev(a) / u, statistics.pstdev(b) / f) * 100
    verdict = "噪声内" if abs(d) <= spread else ("★ fork 更慢" if d > 0 else "★ fork 更快")
    print(f"  {label:16} {u:11.2f} {f:10.2f} {d:+8.1f}%   {verdict}  (抖动 ±{spread:.1f}%)")

print("=" * W)

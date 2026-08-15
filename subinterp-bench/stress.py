#!/usr/bin/env python3
"""PerInterpreterCell 压测 —— 目标是【证伪】,不是演示。

四个断言,每个都能独立地把这个改动判死:
  A. 泄漏      反复建毁解释器,RSS 不能持续增长(capsule 必须随解释器一起没)
  B. 隔离      并发下每个解释器算自己那份数据,结果必须各自正确
  C. 类型独立  N 个解释器必须有 N 个不同的 type 对象地址
  D. 存活      长时间高并发不崩、不 abort

用法: ./pav/bin/python stress.py <so目录> [轮数]
"""
import gc, os, resource, sys, threading, time, warnings
from concurrent import interpreters

warnings.filterwarnings("ignore")

SO_DIR = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "so_fork")
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 2000
WORKERS = 8
BUMPS = 2_000

BODY = """
import sys, _imp
try: _imp._override_multi_interp_extensions_check(-1)
except Exception: pass
sys.path.insert(0, {d!r})
import abi3t
c = abi3t.Counter(_seed)
for _ in range({bumps}): tot = c.bump(1)
_q.put((_seed, tot, id(abi3t.Counter)))
""".format(d=SO_DIR, bumps=BUMPS)


def rss_mb():
    """当前 RSS(不是 ru_maxrss —— 那是峰值,单调不减,测不出释放)。"""
    import subprocess
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(os.getpid())],
                         capture_output=True, text=True).stdout.strip()
    return int(out) / 1024 if out else 0.0


def one_round(seed_base, n=WORKERS):
    """建 n 个解释器 → 各自算 → 全部关掉。返回 (结果, 错误)。"""
    out, err, interps = [], [], []
    lock = threading.Lock()

    def work(i):
        it = None
        try:
            it = interpreters.create()
            with lock:
                interps.append(it)
            q = interpreters.create_queue()
            it.prepare_main(_q=q, _seed=seed_base + i)
            it.exec(BODY)
            with lock:
                out.append(q.get())
        except Exception as e:
            with lock:
                err.append(f"{type(e).__name__}: {str(e).splitlines()[-1][:90]}")

    ts = [threading.Thread(target=work, args=(i,)) for i in range(n)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    for it in interps:                      # ★ 必须全部关掉,否则测不到释放
        try:
            it.close()
        except Exception as e:
            err.append(f"close: {type(e).__name__}")
    return out, err


W = 72
print("=" * W)
print(f"PerInterpreterCell 压测   so={SO_DIR}")
print(f"{ROUNDS} 轮 × {WORKERS} 解释器 × {BUMPS} 次调用 "
      f"= {ROUNDS*WORKERS:,} 个解释器, {ROUNDS*WORKERS*BUMPS:,} 次方法调用")
print("=" * W)

# 预热:让第一次加载的固定开销不算进泄漏曲线
one_round(0)
gc.collect()
base_rss = rss_mb()
print(f"预热后 RSS {base_rss:.1f} MB\n")

bad_result = bad_isolation = 0
errors = []
t0 = time.time()
marks = {int(ROUNDS * f) for f in (0.05, 0.25, 0.5, 0.75, 1.0)}
samples = []

for r in range(1, ROUNDS + 1):
    seed = r * 1_000_000
    out, err = one_round(seed)
    errors += err[:2]

    # B. 每个解释器的结果必须是【它自己那份种子】的正确答案
    # probe 里 Counter 是 total += seed*k,起始 0 —— bump(1) 做 BUMPS 次就是 seed*BUMPS。
    # (旧断言写的是 seed + BUMPS,冻结的是并入共用探针【之前】那个 Counter 的算法。)
    for s, tot, _tid in out:
        if tot != s * BUMPS:
            bad_result += 1
    # C. N 个解释器 = N 个不同 type 地址
    if out and len(set(t for _, _, t in out)) != len(out):
        bad_isolation += 1

    if r in marks:
        gc.collect()
        cur = rss_mb()
        samples.append((r, cur))
        el = time.time() - t0
        print(f"  第 {r:>5} 轮  RSS {cur:>6.1f} MB  (基线 +{cur-base_rss:>5.1f})  "
              f"{r*WORKERS/el:>6.0f} 解释器/秒  错误 {len(errors)}")

el = time.time() - t0
gc.collect()
final = rss_mb()

print("\n" + "-" * W)
print(f"A. 泄漏      基线 {base_rss:.1f} MB → 结束 {final:.1f} MB  "
      f"(增长 {final-base_rss:+.1f} MB / {ROUNDS*WORKERS:,} 个解释器)")
if len(samples) >= 3:
    # 按【解释器】算,不按轮 —— 一轮建 WORKERS 个,按轮算出来的数没法和 leak.py 比,
    # 也没法和上面那行总量对账。旧版就是这么给出「总量在减速、斜率却在上升」的。
    mid = samples[len(samples) // 2]
    d_mb = final - mid[1]
    d_interp = max((ROUNDS - mid[0]) * WORKERS, 1)
    slope = d_mb / d_interp * 1000
    overall = (final - base_rss) / (ROUNDS * WORKERS) * 1000
    verdict = "✅ 平稳" if slope <= overall + 0.5 else "★ 后半程比整体还快,疑似泄漏"
    print(f"             后半程 {slope:+.2f} MB/千个解释器   整体 {overall:+.2f}   {verdict}")
    print(f"             (判据是后半程不快于整体 —— 收敛的过程整体会被前期抬高,"
          f"后半程必然更低)")
print(f"B. 结果正确  {'✅ 全部正确' if bad_result == 0 else f'★ {bad_result} 次错误'}")
print(f"C. 类型独立  {'✅ 每轮都是 N 个不同 type' if bad_isolation == 0 else f'★ {bad_isolation} 轮出现共享'}")
print(f"D. 存活      ✅ 跑完未崩溃   用时 {el:.0f}s")
if errors:
    print(f"\n错误样本 ({len(errors)} 条):")
    for e in errors[:5]:
        print("   ", e)
print("-" * W)

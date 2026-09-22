"""M3.2 (#10): whose pool does a deferred decref land in, and who applies it?

Two own-GIL sub-interpreters A and B load the probe extension, either as one shared
.so or as a copy each. A drops one of its own objects while not attached, then
B makes a PyO3 call through its module (upstream: that drains the process-wide
pool), then A looks at the refcount with no PyO3 call in between:

  A, on return      — applied by A itself when its call re-attached (before B ran)
  applied by B      — the decref ran in the wrong interpreter (bug A)
  pending, then A   — still queued after B's call; A's own next PyO3 call applies it
  LOUD              — PyO3 refused (unknown owner), with its message

Two ways to drop while not attached:
  foreign  — on a fresh std::thread with no Python thread state (rayon/tokio-like)
  detach   — inside `py.detach` on A's own thread (thread state kept, detached)
  race     — inside `py.detach`, and A stays detached for 300 ms while B makes its
             PyO3 call; A reads the refcount before re-attaching (a drop means B
             applied A's decref)

Usage:  python3.14 subinterp-bench/pool_probe.py [ROUNDS]
"""

import json, os, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 5

CHILD = r'''
import json, sys
from concurrent import interpreters
dirs, override, how = json.loads(sys.argv[1]), sys.argv[2] == "1", sys.argv[3]
pre = "import _imp; _imp._override_multi_interp_extensions_check(-1)\n"
A, B = interpreters.create(), interpreters.create()
for it, d in ((A, dirs[0]), (B, dirs[1])):
    it.exec((pre if override else "") + f"import sys; sys.path.insert(0, {d!r}); import abi3t\n"
            "class P: pass\nobj = P()\n")
qa = interpreters.create_queue(); A.prepare_main(q=qa)
if how == "race":
    import threading, time
    def run_a():
        A.exec("b, d = abi3t.detach_race(obj, 300)\nq.put(repr(('ok', d - b, d - b, 0)))")
    t = threading.Thread(target=run_a); t.start()
    time.sleep(0.1); B.exec("abi3t.noop()\n"); t.join()
    print(json.dumps(eval(qa.get()))); raise SystemExit
A.exec("before = sys.getrefcount(obj)\n"
       + ("r = abi3t.drop_on_foreign_thread(obj)\n" if how == "foreign" else "abi3t.drop_during_detach(obj); r = 'ok'\n")
       + "after_call = sys.getrefcount(obj) - before\n")
B.exec("abi3t.noop()\n")
A.exec("after_b = sys.getrefcount(obj) - before\n"
       "abi3t.noop()\n"
       "after_a = sys.getrefcount(obj) - before\n"
       "q.put(repr((r, after_call, after_b, after_a)))")
print(json.dumps(eval(qa.get())))
'''

def verdict(r, after_call, after_b, after_a, how=None):
    if r != "ok":
        return "LOUD"
    if how == "race":
        return "applied by B (wrong interp)" if after_call < 0 else "still pending (correct)"
    if after_call == 0:
        return "A, on return"
    if after_b == 0:
        return "applied by B (wrong interp)"
    if after_b == 1 and after_a == 0:
        return "pending, then A"
    return f"other (after B {after_b:+d}, after A {after_a:+d})"

def run(build, deploy, how):
    src = os.path.join(HERE, build)
    tmp = tempfile.mkdtemp()
    dirs = [src, src] if deploy == "shared" else [shutil.copytree(src, os.path.join(tmp, f"c{i}")) for i in range(2)]
    p = subprocess.run([sys.executable, "-c", CHILD, json.dumps(dirs), "1" if build.startswith("so_base") else "0", how],
                       capture_output=True, text=True, timeout=120)
    shutil.rmtree(tmp, ignore_errors=True)
    if p.returncode != 0:
        errs = [l for l in p.stderr.strip().splitlines() if "remaining subinterpreters" not in l]
        return f"exit {p.returncode}: {(errs or [''])[-1][:90]}", None
    r = json.loads(p.stdout.strip().splitlines()[-1])
    return verdict(*r, how=how), r[0]

print(f"python {sys.version.split()[0]}, {ROUNDS} rounds per cell\n")
print("| build | deploy | drop on | verdict (rounds) | message |")
print("|---|---|---|---|---|")
for build in ("so_base", "so_fork"):
    for deploy in ("shared", "copies"):
        for how in ("foreign", "detach", "race"):
            seen = {}
            msg = ""
            for _ in range(ROUNDS):
                v, m = run(build, deploy, how)
                seen[v] = seen.get(v, 0) + 1
                if m and m != "ok":
                    msg = m
            cell = ", ".join(f"{k} ({n}×)" for k, n in seen.items())
            print(f"| {build} | {deploy} | {how} | {cell} | {msg} |")

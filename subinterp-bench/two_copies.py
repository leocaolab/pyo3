"""Ledger #21: two PyO3 extension copies in one interpreter.

Every extension `.so` carries its own copy of PyO3's statics. Before the fix, the fork kept the
per-interpreter registry under ONE fixed key in the interpreter dict, so the second PyO3 extension
loaded into an interpreter found the first one's registry and read its slots by its own indices.
Measured: `import pyronova.engine` then a fork-built `import polars` segfaulted, main or sub,
either order; two copies of this probe's `abi3t.so` segfault the same way.

For each build and each place, this loads two physically separate copies of `abi3t.so` into the
same interpreter and checks that each copy has its own module and classes, and that both work:
  main — both copies in the main interpreter
  sub  — both copies in each of N own-GIL sub-interpreters (four files per pair of interpreters)

Verdict: correct / SHARED (a class object came back from the other copy) / crash (exit code).

The upstream control has no per-interpreter registry, so it is correct here too: this defect is
fork-only. The regression is proven by mutation (revert the fix → so_fork rows crash).

Usage:  python3.14 subinterp-bench/two_copies.py [N]
Needs `build.sh` to have produced so_base/ and so_fork/.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import textwrap

HERE = os.path.dirname(os.path.abspath(__file__))
N = int(sys.argv[1]) if len(sys.argv) > 1 else 4

CHECK = textwrap.dedent(
    r'''
    import importlib.util
    def _load(d):
        spec = importlib.util.spec_from_file_location("abi3t", d + "/abi3t.so")
        m = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(m)
        return m
    A, B = _load(DIR_A), _load(DIR_B)
    ok = (A is not B and A.Row is not B.Row and A.Counter is not B.Counter
          and type(B.Row(1.5, 2)) is B.Row and type(A.Row(1.5, 2)) is A.Row
          and B.Counter(3).bump(2) == 6 and A.Counter(5).bump(2) == 10)
    result = "correct" if ok else "SHARED"
    '''
)

CHILD = textwrap.dedent(
    r'''
    import sys, json
    place, dirs, n = sys.argv[1], json.loads(sys.argv[2]), int(sys.argv[3])
    check = sys.argv[4]
    if place == "main":
        g = {"DIR_A": dirs[0], "DIR_B": dirs[1]}
        exec(check, g)
        print(g["result"])
    else:
        from concurrent import interpreters
        out = []
        for i in range(n):
            it = interpreters.create()
            q = interpreters.create_queue()
            it.prepare_main(q=q, DIR_A=dirs[2 * i], DIR_B=dirs[2 * i + 1])
            it.exec("import _imp; _imp._override_multi_interp_extensions_check(-1)\n"
                    + check + "\nq.put(result)")
            out.append(q.get())
        print("correct" if all(r == "correct" for r in out) else "SHARED " + ",".join(out))
    '''
)


def run(build, place):
    src = os.path.join(HERE, build, "abi3t.so")
    tmp = tempfile.mkdtemp(prefix=f"two_copies_{build}_")
    try:
        dirs = []
        for i in range(2 if place == "main" else 2 * N):
            d = os.path.join(tmp, f"c{i}")
            os.makedirs(d)
            shutil.copy(src, os.path.join(d, "abi3t.so"))
            dirs.append(d)
        import json
        p = subprocess.run(
            [sys.executable, "-c", CHILD, place, json.dumps(dirs), str(N), CHECK],
            capture_output=True, text=True, timeout=600,
        )
        if p.returncode != 0:
            return f"crash (exit {p.returncode})"
        return p.stdout.strip().splitlines()[-1]
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


print(f"two copies of abi3t.so in one interpreter; N={N} sub-interpreters\n")
print("| build | place | verdict |")
print("|---|---|---|")
for build in ("so_base", "so_fork"):
    if not os.path.exists(os.path.join(HERE, build, "abi3t.so")):
        print(f"| {build} | — | missing (run build.sh) |")
        continue
    for place in ("main", "sub"):
        print(f"| {build} | {place} | {run(build, place)} |", flush=True)

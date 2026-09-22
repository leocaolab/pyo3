"""M2.2 (#6) acceptance: polars bug B (POLARS.md "B. 跨解释器对象串号").

polars caches `polars` module objects in process-global `PyOnceLock` statics
(`py_modules.rs`). With one polars package shared by several interpreters,
upstream PyO3 hands every interpreter the first one's objects:
`map_elements(..., return_dtype=pl.Int64)` then fails with
"cannot parse input of type 'Int64' into Polars data type (given: Int64)" —
another interpreter's `Int64` class.

With M2.2 `PyOnceLock` is per interpreter, so the same unpatched polars must
give every interpreter its own objects.

Eager `Series.map_elements` runs the UDF on the calling thread, so this check
does not depend on the foreign-thread attach (M1.3).

Usage:
    POLARS_PKG=<dir containing polars/ and _polars_runtime_64/> \\
        python3.14 subinterp-bench/polars_bugb_check.py [N] [ROUNDS]
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap

N = int(sys.argv[1]) if len(sys.argv) > 1 else 4
ROUNDS = int(sys.argv[2]) if len(sys.argv) > 2 else 20
PKG = os.environ["POLARS_PKG"]

WORKER = textwrap.dedent(
    r'''
    import sys
    from concurrent import interpreters
    sys.path.insert(0, PKGDIR)
    import polars as pl

    me = interpreters.get_current().id
    ok = wrong = 0
    errors = []
    for i in range(ROUNDS):
        try:
            s = pl.Series("a", list(range(1000))).map_elements(lambda v: v * 2, return_dtype=pl.Int64)
            if s.dtype == pl.Int64 and s[999] == 1998:
                ok += 1
            else:
                wrong += 1
                errors.append(f"dtype={s.dtype!r}")
        except BaseException as e:
            wrong += 1
            errors.append(f"{type(e).__name__}: {str(e)[:120]}")
    result = (me, ok, wrong, errors[:2])
    '''
)

CHILD = textwrap.dedent(
    r'''
    import json, sys, threading
    from concurrent import interpreters
    dirs = json.loads(sys.argv[1]); worker = sys.argv[2]; rounds = sys.argv[3]
    results = [None] * len(dirs)
    interps = [interpreters.create() for _ in dirs]
    def run(i):
        q = interpreters.create_queue()
        interps[i].prepare_main(q=q)
        try:
            interps[i].exec(worker.replace("PKGDIR", repr(dirs[i])).replace("ROUNDS", rounds) + "\nq.put(repr(result))")
            results[i] = q.get()
        except Exception as e:
            results[i] = repr(("exec-failed", str(e).strip().splitlines()[-1][:200]))
    ts = [threading.Thread(target=run, args=(i,)) for i in range(len(dirs))]
    [t.start() for t in ts]; [t.join() for t in ts]
    print(json.dumps(results))
    '''
)


def run(deploy):
    tmp = tempfile.mkdtemp(prefix=f"plb-{deploy}-")
    if deploy == "shared":
        dirs = [PKG] * N
    else:
        dirs = [shutil.copytree(PKG, os.path.join(tmp, f"c{i}"), symlinks=True) for i in range(N)]
    env = dict(os.environ, POLARS_FORCE_PKG="64")
    p = subprocess.run([sys.executable, "-c", CHILD, json.dumps(dirs), WORKER, str(ROUNDS)],
                       capture_output=True, text=True, timeout=900, env=env)
    shutil.rmtree(tmp, ignore_errors=True)
    if p.returncode != 0:
        tail = p.stderr.strip().splitlines()[-3:] if p.stderr.strip() else []
        return f"process exit {p.returncode}: " + " | ".join(tail)
    return [eval(x) for x in json.loads(p.stdout.strip().splitlines()[-1])]


def main():
    print(f"N = {N}, rounds = {ROUNDS}, python {sys.version.split()[0]}, package {PKG}\n")
    for deploy in ("shared", "copies"):
        res = run(deploy)
        print(f"== {deploy}")
        if isinstance(res, str):
            print("  " + res)
            continue
        for item in res:
            if item[0] == "exec-failed":
                print(f"  exec failed: {item[1]}")
                continue
            me, ok, wrong, errs = item
            print(f"  interp {me}: ok {ok}, wrong {wrong}" + (f"  e.g. {errs}" if errs else ""))


if __name__ == "__main__":
    main()

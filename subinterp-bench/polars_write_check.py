"""M1.3 (#4) acceptance: polars' writes to a Python file-like object land in the
owning interpreter through the home interpreter alone, with no InterpreterHandle
patch in polars.

polars writes csv / ndjson / parquet / ipc to a Python file-like object from its
rayon worker threads (`PyFileLikeObject::write`, `Python::attach`). Before M1.3
those attaches landed in the MAIN interpreter (POLARS.md). This check:

  copies — each interpreter imports its own copy of the polars package
           (Pyronova isolate): every write must land in the owning interpreter;
  shared — one polars package imported by every interpreter: a foreign attach
           must fail loudly (AmbiguousInterpreter), never land in main.

For each write, the file-like records the interpreter id it runs in; the bytes
are read back and compared with the source frame.

Usage:
    POLARS_PKG=<dir containing polars/ and _polars_runtime_64/> \\
        python3.14 subinterp-bench/polars_write_check.py [N]
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap

N = int(sys.argv[1]) if len(sys.argv) > 1 else 4
PKG = os.environ["POLARS_PKG"]

WORKER = textwrap.dedent(
    r'''
    import io, random, sys, threading
    from concurrent import interpreters
    sys.path.insert(0, PKGDIR)
    import polars as pl

    me = interpreters.get_current().id
    r = random.Random(SEED)
    n = 200_000
    df = pl.DataFrame({
        "id": list(range(n)),
        "x": [r.random() for _ in range(n)],
        "s": [f"w{SEED}-{r.randrange(1000)}" for _ in range(n)],
    })

    class Sink:
        def __init__(self):
            self.buf = io.BytesIO()
            self.seen = set()
        def write(self, b):
            self.seen.add(interpreters.get_current().id)
            return self.buf.write(b if isinstance(b, (bytes, bytearray)) else b.encode())
        def flush(self):
            pass

    out = {}
    for name, write, read in (
        ("csv", lambda s: df.write_csv(s), lambda b: pl.read_csv(io.BytesIO(b))),
        ("ndjson", lambda s: df.write_ndjson(s), lambda b: pl.read_ndjson(io.BytesIO(b))),
        ("parquet", lambda s: df.write_parquet(s), lambda b: pl.read_parquet(io.BytesIO(b))),
        ("ipc", lambda s: df.write_ipc(s), lambda b: pl.read_ipc(io.BytesIO(b))),
    ):
        s = Sink()
        try:
            write(s)
            back = read(s.buf.getvalue())
            same = back.equals(df)
            where = sorted(s.seen)
            out[name] = ("ok" if same and where == [me] else "WRONG", where, same)
        except BaseException as e:
            out[name] = ("RAISED", type(e).__name__, str(e)[:160])
    result = (me, out)
    '''
)

CHILD = textwrap.dedent(
    r'''
    import json, sys, threading
    from concurrent import interpreters
    dirs = json.loads(sys.argv[1]); worker = sys.argv[2]
    results = [None] * len(dirs)
    interps = [interpreters.create() for _ in dirs]
    def run(i):
        q = interpreters.create_queue()
        interps[i].prepare_main(q=q)
        try:
            interps[i].exec(worker.replace("PKGDIR", repr(dirs[i])).replace("SEED", str(i + 1)) + "\nq.put(repr(result))")
            results[i] = q.get()
        except Exception as e:
            results[i] = repr(("exec-failed", str(e).strip().splitlines()[-1][:200]))
    ts = [threading.Thread(target=run, args=(i,)) for i in range(len(dirs))]
    [t.start() for t in ts]; [t.join() for t in ts]
    print(json.dumps(results))
    '''
)


def run(deploy):
    tmp = tempfile.mkdtemp(prefix=f"plw-{deploy}-")
    if deploy == "shared":
        dirs = [PKG] * N
    else:
        dirs = []
        for i in range(N):
            d = os.path.join(tmp, f"c{i}")
            shutil.copytree(PKG, d, symlinks=True)
            dirs.append(d)
    env = dict(os.environ, POLARS_FORCE_PKG="64")
    p = subprocess.run([sys.executable, "-c", CHILD, json.dumps(dirs), WORKER],
                       capture_output=True, text=True, timeout=900, env=env)
    shutil.rmtree(tmp, ignore_errors=True)
    if p.returncode != 0:
        tail = p.stderr.strip().splitlines()[-3:] if p.stderr.strip() else []
        return f"process exit {p.returncode}: " + " | ".join(tail)
    return [eval(x) for x in json.loads(p.stdout.strip().splitlines()[-1])]


def main():
    print(f"N = {N}, python {sys.version.split()[0]}, package {PKG}\n")
    for deploy in ("copies", "shared"):
        res = run(deploy)
        print(f"== {deploy}")
        if isinstance(res, str):
            print("  " + res)
            continue
        for item in res:
            if item[0] == "exec-failed":
                print(f"  exec failed: {item[1]}")
                continue
            me, out = item
            print(f"  interp {me}: " + ", ".join(f"{k}={v[0]}" + ("" if v[0] == "ok" else f" {v[1:]}") for k, v in out.items()))


if __name__ == "__main__":
    main()

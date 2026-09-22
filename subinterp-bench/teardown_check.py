"""M4.1 (#12): destroying sub-interpreters after polars did parallel work (POLARS.md "C").

N own-GIL sub-interpreters each import polars and do one kind of work, then every
interpreter is closed (`Py_EndInterpreter`) while the process keeps running. The
process must survive the close and exit 0.

  import    — import only
  frame     — build a DataFrame
  compute   — lazy group_by / agg on the rayon pool (no Python callbacks)
  writefile — write_csv / write_ipc to a file path (no Python callbacks)

Usage:
    POLARS_PKG=<dir> python3.14 subinterp-bench/teardown_check.py [ROUNDS] [shared|copies]
"""

import os, shutil, subprocess, sys, tempfile, textwrap

ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 3
DEPLOY = sys.argv[2] if len(sys.argv) > 2 else "copies"
PKG = os.environ["POLARS_PKG"]

WORK = {
    "import": "",
    "frame": "df = pl.DataFrame({'k': [i % 61 for i in range(200_000)], 'v': list(range(200_000))})",
    "compute": "df = pl.DataFrame({'k': [i % 61 for i in range(200_000)], 'v': list(range(200_000))})\n"
               "for _ in range(20): df.lazy().group_by('k').agg(pl.col('v').sum()).collect()",
    "writefile": "df = pl.DataFrame({'k': [i % 61 for i in range(200_000)], 'v': list(range(200_000))})\n"
                 "p = f'/tmp/teardown_check_{interpreters.get_current().id}_{os.getpid()}'\n"
                 "for _ in range(5): df.write_csv(p + '.csv'); df.write_ipc(p + '.ipc')\n"
                 "os.unlink(p + '.csv'); os.unlink(p + '.ipc')",
}

CHILD = textwrap.dedent(r'''
    import json, sys, threading
    from concurrent import interpreters
    dirs, work = json.loads(sys.argv[1]), sys.argv[2]
    its = [interpreters.create() for _ in dirs]
    def run(i):
        its[i].exec(f"import os, sys\nsys.path.insert(0, {dirs[i]!r})\nimport polars as pl\n"
                    "from concurrent import interpreters\n" + work)
    ts = [threading.Thread(target=run, args=(i,)) for i in range(len(dirs))]
    [t.start() for t in ts]; [t.join() for t in ts]
    for it in its:
        it.close()
    print("closed", len(its), flush=True)
''')


def main():
    import json
    print(f"python {sys.version.split()[0]}, deploy {DEPLOY}, {ROUNDS} rounds, package {PKG}\n")
    print("| work | N | clean / rounds | failures |")
    print("|---|---:|---|---|")
    only = os.environ.get("TD_WORK")
    for work, code in WORK.items():
        if only and work not in only.split(","):
            continue
        for n in [int(x) for x in os.environ.get("TD_N", "2,4,8").split(",")]:
            tmp = tempfile.mkdtemp(prefix="td-")
            dirs = [PKG] * n if DEPLOY == "shared" else [shutil.copytree(PKG, os.path.join(tmp, f"c{i}"), symlinks=True) for i in range(n)]
            ok, fails = 0, []
            for _ in range(ROUNDS):
                p = subprocess.run([sys.executable, "-c", CHILD, json.dumps(dirs), code], capture_output=True,
                                   text=True, timeout=600, env=dict(os.environ, POLARS_FORCE_PKG="64"))
                if p.returncode == 0 and "closed" in p.stdout:
                    ok += 1
                else:
                    tail = [l for l in p.stderr.strip().splitlines() if l.strip()][-1:] or [""]
                    fails.append(f"exit {p.returncode} {tail[0][:80]}")
            shutil.rmtree(tmp, ignore_errors=True)
            print(f"| {work} | {n} | {ok}/{ROUNDS} | {'; '.join(sorted(set(fails)))} |", flush=True)


if __name__ == "__main__":
    main()

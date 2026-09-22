"""M1.1 (#2): where does a `Python::attach` land?

For each build (upstream parent vs this branch) and each deployment:
  shared  — one .so loaded by every interpreter (a shared extension)
  copies  — each interpreter loads its own copy of the .so (Pyronova isolate)
and each thread origin:
  pythread — a `threading.Thread` of the main interpreter that runs the
             sub-interpreter (its gilstate slot is bound to MAIN)
  native   — a native thread whose FIRST thread state is the sub-interpreter's
             (what Pyronova's workers are)
it records which interpreter two calls land in:
  after_detach — `Python::attach` nested inside `py.detach` on the same thread
  new_thread   — `Python::attach` on a fresh OS thread

Verdict per cell: correct / MAIN / other / LOUD (attach panicked) / crash.

Usage:
    python3.14 subinterp-bench/attach_probe.py [N]
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

CHILD = textwrap.dedent(
    r'''
    import json, os, sys, threading
    from concurrent import interpreters

    build, deploy, dirs, override = sys.argv[1], sys.argv[2], json.loads(sys.argv[3]), sys.argv[4] == "1"

    def code_for(d):
        pre = "import _imp; _imp._override_multi_interp_extensions_check(-1)\n" if override else ""
        return pre + f"import sys; sys.path.insert(0, {d!r}); import abi3t\n"

    rows = []
    interps = []
    for i in range(len(dirs)):
        it = interpreters.create()
        it.exec(code_for(dirs[i]))
        interps.append(it)

    def run_in(it, src):
        box = {}
        q = interpreters.create_queue()
        it.prepare_main(q=q)
        it.exec(src + "\nq.put(repr(result))")
        return eval(q.get())

    # pythread origin: a main-interpreter threading.Thread runs the sub-interpreter
    def pythread_cell(it):
        out = {}
        def body():
            try:
                out["r"] = run_in(it, "result = (abi3t.current_interp_id(), abi3t.attach_after_detach(), abi3t.attach_on_new_thread())")
            except Exception as e:
                out["r"] = ("crash", repr(e)[:120])
        t = threading.Thread(target=body)
        t.start(); t.join()
        return out["r"]

    # native origin: a native thread whose first thread state is the sub-interpreter's
    def native_cell(it):
        try:
            return run_in(it, "result = abi3t.probe_on_native_thread(abi3t.interp_ptr())")
        except Exception as e:
            return ("crash", repr(e)[:120])

    for it in interps:
        for origin, fn in (("pythread", pythread_cell), ("native", native_cell)):
            r = fn(it)
            rows.append({"origin": origin, "result": r})
    print(json.dumps(rows))
    '''
)


def verdict(expected, got):
    if got == -2:
        return "LOUD"
    if got == expected:
        return "correct"
    if got == 0:
        return "MAIN"
    return "other"


def run(build, deploy):
    src = os.path.join(HERE, build)
    tmp = tempfile.mkdtemp(prefix=f"probe-{build}-{deploy}-")
    if deploy == "shared":
        dirs = [src] * N
    else:
        dirs = []
        for i in range(N):
            d = os.path.join(tmp, f"copy{i}")
            shutil.copytree(src, d)
            dirs.append(d)
    override = "1" if build == "so_base" else "0"  # upstream cannot load strict
    import json
    p = subprocess.run(
        [sys.executable, "-c", CHILD, build, deploy, json.dumps(dirs), override],
        capture_output=True, text=True, timeout=300,
    )
    shutil.rmtree(tmp, ignore_errors=True)
    if p.returncode != 0:
        return [("process", f"exit {p.returncode}: {p.stderr.strip().splitlines()[-1] if p.stderr.strip() else ''}")]
    rows = json.loads(p.stdout.strip().splitlines()[-1])
    out = []
    for row in rows:
        r = row["result"]
        if r and r[0] == "crash":
            out.append((row["origin"], "crash", r[1]))
            continue
        exp, after, fresh = r
        out.append((row["origin"], verdict(exp, after), verdict(exp, fresh)))
    return out


def main():
    print(f"N = {N} sub-interpreters, python {sys.version.split()[0]}\n")
    print("| build | deploy | origin | after_detach | new_thread |")
    print("|---|---|---|---|---|")
    for build in ("so_base", "so_fork"):
        if not os.path.isdir(os.path.join(HERE, build)):
            print(f"| {build} | — | — | missing build | |")
            continue
        for deploy in ("shared", "copies"):
            res = run(build, deploy)
            if res and res[0][0] == "process":
                print(f"| {build} | {deploy} | — | {res[0][1]} | |")
                continue
            agg = {}
            for origin, a, b in res:
                agg.setdefault(origin, []).append((a, b))
            for origin, cells in agg.items():
                a = ", ".join(sorted({c[0] for c in cells}))
                b = ", ".join(sorted({c[1] for c in cells}))
                print(f"| {build} | {deploy} | {origin} | {a} ({len(cells)}×) | {b} ({len(cells)}×) |")


if __name__ == "__main__":
    main()

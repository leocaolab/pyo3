"""M2 (#5, #6): per-interpreter caches.

For each build (upstream parent vs this branch) and each deployment (one .so
shared by N own-GIL sub-interpreters, or a copy per interpreter), check that a
cached Python object is per interpreter:

  intern!  — `interned_ptr()` must be the object this interpreter's own
             `sys.intern` returns (interned strings are mortal on 3.12-3.14, so
             another interpreter's object means a shared live refcount, and a
             dangling pointer once that interpreter is destroyed).

Also reports the per-lookup cost of intern!.

Usage:  python3.14 subinterp-bench/cache_probe.py [N]
"""

import json, os, shutil, statistics, subprocess, sys, tempfile, textwrap

HERE = os.path.dirname(os.path.abspath(__file__))
N = int(sys.argv[1]) if len(sys.argv) > 1 else 4

CHILD = textwrap.dedent(r'''
    import json, sys
    from concurrent import interpreters
    dirs, override = json.loads(sys.argv[1]), sys.argv[2] == "1"
    pre = "import _imp; _imp._override_multi_interp_extensions_check(-1)\n" if override else ""
    rows = []
    alive = []  # keep every interpreter alive: a destroyed one frees its memory,
                # and a later interpreter can reuse the same addresses
    for d in dirs:
        it = interpreters.create()
        alive.append(it)
        q = interpreters.create_queue(); it.prepare_main(q=q)
        it.exec(pre + f"import sys; sys.path.insert(0, {d!r}); import abi3t\n"
                "own = sys.intern(''.join(['subinterp_probe_', 'interned_key']))\n"
                "q.put(repr((abi3t.this_interp_id(), abi3t.interned_ptr(), id(own), "
                "sorted(abi3t.intern_ns(200000) for _ in range(7))[3])))")
        rows.append(eval(q.get()))
    print(json.dumps(rows))
''')

def run(build, deploy):
    src = os.path.join(HERE, build)
    tmp = tempfile.mkdtemp()
    dirs = [src] * N if deploy == "shared" else [shutil.copytree(src, os.path.join(tmp, f"c{i}")) for i in range(N)]
    p = subprocess.run([sys.executable, "-c", CHILD, json.dumps(dirs), "1" if build.startswith("so_base") else "0"],
                       capture_output=True, text=True, timeout=300)
    shutil.rmtree(tmp, ignore_errors=True)
    if p.returncode != 0:
        return None, p.stderr.strip().splitlines()[-1] if p.stderr.strip() else f"exit {p.returncode}"
    return json.loads(p.stdout.strip().splitlines()[-1]), None

print(f"N = {N}, python {sys.version.split()[0]}\n")
print("| build | deploy | intern! returns this interpreter's own interned str | verdict | intern! ns/lookup (median) |")
print("|---|---|---:|---|---:|")
for build in ("so_base", "so_fork"):
    for deploy in ("shared", "copies"):
        rows, err = run(build, deploy)
        if err:
            print(f"| {build} | {deploy} | — | error: {err[:80]} | |"); continue
        own = sum(1 for r in rows if r[1] == r[2])
        ok = own == N
        print(f"| {build} | {deploy} | {own}/{N} | {'per-interpreter' if ok else 'WRONG interpreter'} | {statistics.median(r[3] for r in rows):.2f} |")

"""M2.3 (#7): the PyO3 cache sites that are not `PyOnceLock` / `PerInterpreterCell`,
checked end to end in N own-GIL sub-interpreters.

  conversions — `PathBuf`, `Ipv4Addr` (Python classes cached in `PyOnceLock`),
                `Duration`, `PyTzInfo::utc` (datetime C-API table, a process-wide
                `AtomicPtr` in pyo3-ffi): the result must be this interpreter's own
                class / object.
  datetime C-API — PyO3's cached table vs a fresh `PyCapsule_Import` here.
  ffi statics — CPython's static builtin types and exceptions that pyo3-ffi
                links against must be immortal (shared by CPython by design).

Usage:  python3.14 subinterp-bench/conv_probe.py [N]
        PROBE_SUFFIX=_312 python3.12 subinterp-bench/conv_probe.py [N]   (and _313 / 3.13)
"""

import json, os, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
N = int(sys.argv[1]) if len(sys.argv) > 1 else 4

CHILD = r'''
import json, os, sys, tempfile
dirs, override = json.loads(sys.argv[1]), sys.argv[2] == "1"
try:
    from concurrent import interpreters
except ImportError:  # 3.12 / 3.13: private modules; results come back through a file
    interpreters = None
    if sys.version_info[:2] == (3, 12):
        import _xxsubinterpreters as _xi
        create = lambda: _xi.create(isolated=True)
        def run_src(i, src): _xi.run_string(i, src)
    else:
        import _interpreters as _xi
        create = lambda: _xi.create("isolated")
        def run_src(i, src):
            err = _xi.exec(i, src)
            if err is not None: raise RuntimeError(str(err))
pre = "import _imp; _imp._override_multi_interp_extensions_check(-1)\n" if override else ""
body = """
import sys, pathlib, ipaddress, datetime
sys.path.insert(0, D)
import abi3t
def chk(f):
    try: return f()
    except BaseException as e: return f"{type(e).__name__}: {e}"[:80]
def immortal(o):
    f = getattr(sys, "_is_immortal", None)
    return f(o) if f else sys.getrefcount(o) >= (1 << 30)  # 3.12/3.13: saturated refcount
r = {
  "path": chk(lambda: type(abi3t.conv_path()) is type(pathlib.Path())),
  "ip": chk(lambda: type(abi3t.conv_ip()) is ipaddress.IPv4Address),
  "delta": chk(lambda: type(abi3t.conv_delta()) is datetime.timedelta),
  "utc": chk(lambda: abi3t.conv_utc() is datetime.timezone.utc),
  "capi": chk(lambda: abi3t.datetime_capi()) if hasattr(abi3t, "datetime_capi") else None,
  "immortal": all(immortal(t) for t in (int, str, list, dict, type, range, ValueError, KeyError, StopIteration, BaseException)),
  "id_timedelta": id(datetime.timedelta), "id_utc": id(datetime.timezone.utc),
  "utc_immortal": immortal(datetime.timezone.utc),
}
"""
rows, alive = [], []
for d in dirs:
    src = pre + body.replace("sys.path.insert(0, D)", f"sys.path.insert(0, {d!r})")
    if interpreters is not None:
        it = interpreters.create(); alive.append(it)
        q = interpreters.create_queue(); it.prepare_main(q=q)
        it.exec(src + "\nq.put(repr(r))")
        rows.append(eval(q.get()))
    else:
        it = create(); alive.append(it)
        fd, path = tempfile.mkstemp(); os.close(fd)
        run_src(it, src + f"\nopen({path!r}, 'w').write(repr(r))")
        rows.append(eval(open(path).read())); os.unlink(path)
print(json.dumps(rows))
'''

def run(build, deploy):
    src = os.path.join(HERE, build)
    tmp = tempfile.mkdtemp()
    dirs = [src] * N if deploy == "shared" else [shutil.copytree(src, os.path.join(tmp, f"c{i}")) for i in range(N)]
    p = subprocess.run([sys.executable, "-c", CHILD, json.dumps(dirs), "1" if build.startswith("so_base") else "0"],
                       capture_output=True, text=True, timeout=300)
    shutil.rmtree(tmp, ignore_errors=True)
    if p.returncode != 0:
        errs = [l for l in p.stderr.strip().splitlines() if "remaining subinterpreters" not in l]
        return None, (errs or [f"exit {p.returncode}"])[-1]
    return json.loads(p.stdout.strip().splitlines()[-1]), None

def frac(rows, key):
    ok = sum(1 for r in rows if r[key] is True)
    bad = [r[key] for r in rows if r[key] is not True]
    return f"{ok}/{len(rows)}" + (f" ({bad[0]})" if bad else "")

print(f"N = {N}, python {sys.version.split()[0]}\n")
print("| build | deploy | Path own class | IPv4Address own class | timedelta own class | utc own object | datetime C-API: cached == this interp's | distinct timedelta / utc across interps | ffi statics + utc immortal |")
print("|---|---|---|---|---|---|---|---|---|")
SUFFIX = os.environ.get("PROBE_SUFFIX", "")
builds = [b + SUFFIX for b in ("so_base", "so_fork")] + ([] if SUFFIX else ["so_base_abi3", "so_fork_abi3"])
for build in [b for b in builds if os.path.isdir(os.path.join(HERE, b))]:
    for deploy in ("shared", "copies"):
        rows, err = run(build, deploy)
        if err:
            print(f"| {build} | {deploy} | error: {err[:100]} |"); continue
        capi = "n/a (abi3)" if rows[0]["capi"] is None else f"{sum(1 for r in rows if isinstance(r['capi'], list) and r['capi'][0] == r['capi'][1])}/{N}"
        dist = f"{len({r['id_timedelta'] for r in rows})} / {len({r['id_utc'] for r in rows})}"
        imm = f"{sum(1 for r in rows if r['immortal'])}/{N}, utc {sum(1 for r in rows if r['utc_immortal'])}/{N}"
        print(f"| {build} | {deploy} | {frac(rows,'path')} | {frac(rows,'ip')} | {frac(rows,'delta')} | {frac(rows,'utc')} | {capi} | {dist} | {imm} |")

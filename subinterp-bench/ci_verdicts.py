"""CI gate (M5.1, #14): run the sub-interpreter probes and fail on any wrong verdict.

Every probe is checked in both directions:
  - the fork rows must show the correct behaviour;
  - the upstream control rows must still show the bug. If a control stops
    showing it, the probe no longer tells the two builds apart, which is also a
    failure (a probe that passes on upstream proves nothing).

Needs `build.sh` to have produced so_base*/so_fork*. Python 3.14 (concurrent.interpreters).

Usage:  python3.14 subinterp-bench/ci_verdicts.py
"""

import os, re, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
failures = []


def run(script, *args):
    p = subprocess.run([sys.executable, os.path.join(HERE, script), *args], capture_output=True, text=True, timeout=1800)
    out = p.stdout
    print(f"\n### {script} {' '.join(args)} (exit {p.returncode})\n{out}")
    if p.returncode != 0:
        failures.append(f"{script}: exit {p.returncode}: {p.stderr.strip().splitlines()[-1:] }")
    return [l for l in out.splitlines() if l.startswith("| so_")]


def cells(line):
    return [c.strip() for c in line.strip().strip("|").split("|")]


def expect(script, rows, build, deploy, col, pattern, extra=None):
    """Some row with this build/deploy (and `extra` in its third column, if given) has
    column `col` matching `pattern`, and no such row does not."""
    hit = [r for r in rows if cells(r)[0] == build and cells(r)[1] == deploy and (extra is None or cells(r)[2] == extra)]
    if not hit:
        failures.append(f"{script}: no row for {build}/{deploy}{'/' + extra if extra else ''}")
        return
    for r in hit:
        c = cells(r)
        if col >= len(c) or not re.search(pattern, c[col]):
            failures.append(f"{script}: {build}/{deploy}{'/' + extra if extra else ''} column {col} = {c[col] if col < len(c) else '—'!r}, expected /{pattern}/")


# M1.3: where a foreign-thread attach lands.  columns: build, deploy, origin, after_detach, new_thread
rows = run("attach_probe.py", "4")
for d in ("shared", "copies"):
    expect("attach_probe", rows, "so_base", d, 4, r"^MAIN \(")          # control: upstream lands in main
    expect("attach_probe", rows, "so_fork", d, 3, r"^correct \(")
expect("attach_probe", rows, "so_fork", "shared", 4, r"^LOUD \(")
expect("attach_probe", rows, "so_fork", "copies", 4, r"^correct \(")

# M2.1 / M2.2: per-interpreter intern! and PyOnceLock.
# columns: build, deploy, intern own, PyOnceLock own sys, heap drop/reuse, ns, ns
rows = run("cache_probe.py", "4")
expect("cache_probe", rows, "so_base", "shared", 2, r"^1/4$")            # control
expect("cache_probe", rows, "so_base", "shared", 3, r"^1/4$")            # control
for d in ("shared", "copies"):
    for col in (2, 3, 4):
        expect("cache_probe", rows, "so_fork", d, col, r"^4/4$")

# M2.3: conversion-layer caches (and immortal CPython statics).
# columns: build, deploy, Path, IPv4Address, timedelta, utc, capi, distinct, immortal
rows = run("conv_probe.py", "4")
for b in ("so_base", "so_base_abi3"):
    expect("conv_probe", rows, b, "shared", 2, r"^1/4")                  # control
for b in ("so_fork", "so_fork_abi3"):
    for d in ("shared", "copies"):
        for col in (2, 3, 4, 5):
            expect("conv_probe", rows, b, d, col, r"^4/4$")
        expect("conv_probe", rows, b, d, 8, r"^4/4, utc 4/4$")

# M3.2: who applies a deferred decref.  columns: build, deploy, drop on, verdict, message
rows = run("pool_probe.py", "3")
expect("pool_probe", rows, "so_base", "shared", 3, r"applied by B", "foreign")   # control: bug A
expect("pool_probe", rows, "so_base", "shared", 3, r"applied by B", "race")      # control: bug A
expect("pool_probe", rows, "so_fork", "shared", 3, r"^LOUD \(\d+×\)$", "foreign")
for d in ("shared", "copies"):
    expect("pool_probe", rows, "so_fork", d, 3, r"^still pending \(correct\) \(\d+×\)$", "race")
    expect("pool_probe", rows, "so_fork", d, 3, r"^A, on return \(\d+×\)$", "detach")
expect("pool_probe", rows, "so_fork", "copies", 3, r"^pending, then A \(\d+×\)$", "foreign")

# Ledger #7 / #10: wrap_pymodule! submodules load in every interpreter, with their own module
# and class objects; on abi3 that also proves the slot placeholder does not leak into them.
for b in ("sm_base", "sm_fork", "sm_base_abi3", "sm_fork_abi3"):
    p = subprocess.run([sys.executable, os.path.join(HERE, "submodule.py"), os.path.join(HERE, b)],
                       capture_output=True, text=True, timeout=600)
    print(f"\n### submodule.py {b} (exit {p.returncode})\n{p.stdout}")
    if b.startswith("sm_base"):
        # control: upstream refuses 5 of 6 with the pyo3#576 ImportError, or (seen on macOS CI)
        # aborts outright. A signal death counts; an ordinary error exit (a missing build, a
        # Python traceback) does not.
        if "成功 1/6" not in p.stdout and p.returncode >= 0:
            failures.append(f"submodule {b}: control no longer shows the pyo3#576 refusal (exit {p.returncode})")
    else:
        for want in ("成功 6/6", "子模块对象     6 个不同地址", "子模块里的类    6 个不同地址"):
            if want not in p.stdout:
                failures.append(f"submodule {b}: missing {want!r}")

print("\n" + ("\n".join("FAIL " + f for f in failures) if failures else "all verdicts as expected"))
sys.exit(1 if failures else 0)

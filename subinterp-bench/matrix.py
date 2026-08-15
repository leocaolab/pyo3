#!/usr/bin/env python3
"""能力矩阵:{对照, 本分支} × {默认 ABI, abi3-py310} × {顶层模块, 含子模块}。

这个脚本存在的理由是一次教训。原来的基准把**行为**测得很密,把**构建配置**测成了零 ——
全是默认 ABI、全是顶层模块。于是三个 bug 一个都没抓到,全由第一个真实消费者
(polars,用 abi3-py310 + wrap_pymodule!)免费发现:

  ① 限定 API 下编不过    PyObject_CallMethodNoArgs / OneArg 不在里面
  ② 限定 API 下 slot 发不出  abi3 的编译期 cfg 取【最低】版本,gate 在 Py_3_12 上直接成空操作
  ③ 占位符泄漏到子模块    顶层走 init_multi_phase,子模块走 make_module,只补了前者

判据是**能不能加载 + 隔离到没到位**,不是性能:

  override   带 _imp._override_multi_interp_extensions_check(-1) —— 测隔离
  strict     不带 —— 测 CPython 认不认这个模块

先跑 ./build.sh 编出八份 .so,再跑这个。
"""
import os, subprocess, sys, threading, warnings

warnings.filterwarnings("ignore")

N = 4
HERE = os.path.dirname(os.path.abspath(__file__))

BUILDS = [
    ("对照 · 默认ABI", "so_base", False),
    ("本分支 · 默认ABI", "so_fork", False),
    ("对照 · abi3", "so_base_abi3", False),
    ("本分支 · abi3", "so_fork_abi3", False),
    ("对照 · 默认+子模块", "sm_base", True),
    ("本分支 · 默认+子模块", "sm_fork", True),
    ("对照 · abi3+子模块", "sm_base_abi3", True),
    ("本分支 · abi3+子模块", "sm_fork_abi3", True),
]

HACK = ("import _imp\n"
        "try: _imp._override_multi_interp_extensions_check(-1)\n"
        "except Exception: pass\n")


def body(so_dir, strict, submodule):
    inner = "sub = abi3t.inner\n_sub = (sub.answer, id(sub))\n" if submodule else "_sub = (None, 0)\n"
    return (f"import sys\n"
            f"{'' if strict else HACK}"
            f"sys.path.insert(0, {os.path.abspath(so_dir)!r})\n"
            f"import abi3t\n"
            f"{inner}"
            f"r = abi3t.Row(1.0, 2)\n"
            f"_q.put((id(type(r)), _sub[1], _sub[0]))\n")


def run(so_dir, strict, submodule):
    """返回 (成功数, 类型地址数, 子模块地址数, 首个错误)。"""
    from concurrent import interpreters

    src = body(so_dir, strict, submodule)
    out, err, keep = [], [], []
    lock = threading.Lock()

    def w(_i):
        try:
            it = interpreters.create()
            with lock:
                keep.append(it)
            q = interpreters.create_queue()
            it.prepare_main(_q=q)
            it.exec(src)
            with lock:
                out.append(q.get())
        except BaseException as e:
            with lock:
                err.append(f"{type(e).__name__}: {str(e).splitlines()[-1]}")

    ts = [threading.Thread(target=w, args=(i,)) for i in range(N)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    for it in keep:
        try:
            it.close()
        except Exception:
            pass
    types = len({t for t, _, _ in out})
    subs = len({s for _, s, _ in out}) if submodule else 0
    return len(out), types, subs, (err[0] if err else "")


# 每个格子独占一个子进程:对照建对象会 SIGSEGV,在主进程里跑会把整张表带走
if len(sys.argv) == 4:
    n, types, subs, e = run(sys.argv[1], sys.argv[2] == "strict", sys.argv[3] == "sub")
    print(f"{n}\t{types}\t{subs}\t{e}")
    raise SystemExit


def cell(so_dir, strict, submodule):
    r = subprocess.run([sys.executable, __file__, so_dir,
                        "strict" if strict else "override",
                        "sub" if submodule else "nosub"],
                       capture_output=True, text=True)
    if r.returncode < 0:
        import signal
        return f"崩溃 {signal.Signals(-r.returncode).name}", ""
    if r.returncode != 0:
        return "跑挂", (r.stderr.strip().splitlines() or [""])[-1][:70]
    n, types, subs, e = r.stdout.rstrip("\n").split("\t")
    n, types, subs = int(n), int(types), int(subs)
    if n < N:
        short = e.split(": ", 1)[-1]
        return f"{n}/{N}", short[:70]
    detail = f"类型 {types}" + (f" 子模块 {subs}" if submodule else "")
    ok = types == N and (not submodule or subs == N)
    return (f"{n}/{N} {detail}" if ok else f"{n}/{N} ★{detail}"), ""


W = 108
print("=" * W)
print(f"能力矩阵   {N} 个 own-GIL 子解释器   Python {sys.version.split()[0]}")
print("=" * W)
print(f"  {'构建':26} {'override':>28} {'strict(不带任何开关)':>30}")
print("  " + "-" * (W - 4))

notes = []
for label, so_dir, submodule in BUILDS:
    if not os.path.exists(f"{HERE}/{so_dir}/abi3t.so"):
        print(f"  {label:26} {'缺 ' + so_dir + ',先跑 ./build.sh':>28}")
        continue
    row = []
    for strict in (False, True):
        text, why = cell(f"{HERE}/{so_dir}", strict, submodule)
        row.append(text)
        if why:
            notes.append(f"  {label} / {'strict' if strict else 'override'}: {why}")
    print(f"  {label:26} {row[0]:>28} {row[1]:>30}")

if notes:
    print("\n  失败原因(原样带出,不用哨兵词):")
    for n in dict.fromkeys(notes):
        print(n)

print("=" * W)
print(f"  「类型 {N}」= 每个解释器一个独立的 #[pyclass] 类型对象;少于 {N} 打 ★。")
print("  strict 一列才是端到端判据:override 是个进程级开关,开了就测不出 CPython 认不认。")
print("=" * W)

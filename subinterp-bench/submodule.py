#!/usr/bin/env python3
"""子模块能不能进子解释器 —— wrap_pymodule! 那条路。

父提交在 `ModuleDef::make_module` 里有个守卫:模块对象缓存在【进程级】
PyOnceLock 里,所以第二个解释器 import 时会抛

    ImportError: PyO3 modules do not yet support subinterpreters  (pyo3#576)

本分支把那个缓存改成每解释器一份,守卫就没有存在理由了。

探针里的子模块默认【不编】—— 它一开,父提交连载入都失败,会把其他基准的
对照列一起废掉。所以跑这个脚本前要带 --features submodule 单独编一份:

    cd probe
    cargo build --release --features submodule \
        --config 'patch.crates-io.pyo3.path="/tmp/pyo3-base"'
    mkdir -p /tmp/sm_base && cp target/release/libabi3t.dylib /tmp/sm_base/abi3t.so
    cargo build --release --features submodule \
        --config 'patch.crates-io.pyo3.path="../.."'
    mkdir -p /tmp/sm_fork && cp target/release/libabi3t.dylib /tmp/sm_fork/abi3t.so

用法: python3.14 submodule.py <so目录>
"""
import os, sys, threading, warnings
warnings.filterwarnings("ignore")
from concurrent import interpreters
D = os.path.abspath(sys.argv[1]); N = 6
BODY = f"""
import sys, _imp
try: _imp._override_multi_interp_extensions_check(-1)
except Exception: pass
sys.path.insert(0, {D!r})
import abi3t
sub = abi3t.inner
_q.put((sub.answer, id(sub), id(sub.Row)))
"""
out, err, keep = [], [], []
lock = threading.Lock()
def w(i):
    try:
        it = interpreters.create()
        with lock: keep.append(it)
        q = interpreters.create_queue(); it.prepare_main(_q=q); it.exec(BODY)
        with lock: out.append(q.get())
    except Exception as e:
        with lock: err.append(str(e).splitlines()[-1][:100])
ts = [threading.Thread(target=w, args=(i,)) for i in range(N)]
[t.start() for t in ts]; [t.join() for t in ts]
for it in keep:
    try: it.close()
    except Exception: pass
print(f"  成功 {len(out)}/{N}")
if err:
    print(f"  失败原因: {err[0]}")
else:
    print(f"  answer 全对   {all(a == 42 for a, _, _ in out)}")
    print(f"  子模块对象     {len({m for _, m, _ in out})} 个不同地址 (应为 {N})")
    print(f"  子模块里的类    {len({t for _, _, t in out})} 个不同地址 (应为 {N})")

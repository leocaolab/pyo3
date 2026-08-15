"""不带 _override hack 直接 import —— 端到端是否真通的判据。

其他脚本都在解释器体里放了一行

    _imp._override_multi_interp_extensions_check(-1)

那是个【进程级】开关,意思是"别管扩展怎么声明,一律放行"。带着它跑,测的是
隔离做没做对;去掉它,测的才是 CPython 认不认这个模块。

本分支在模块 slots 里声明了 Py_mod_multiple_interpreters =
Py_MOD_PER_INTERPRETER_GIL_SUPPORTED,所以不需要那个开关。父提交不能声明 ——
它的 #[pyclass] 类型、create_exception! 类型和模块对象都缓存在进程级。

用法: python3.14 no_override.py <so目录>
"""
import os, sys, threading, warnings
warnings.filterwarnings("ignore")
from concurrent import interpreters
D = os.path.abspath(sys.argv[1]); N = 6
BODY = f"""
import sys
sys.path.insert(0, {D!r})
import abi3t                      # ← 没有 _override,全靠模块自己声明的 slot
r = abi3t.Row(1.0, 2)
_q.put((r.a, r.b, id(type(r))))
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
        with lock: err.append(str(e).splitlines()[-1][:110])
ts=[threading.Thread(target=w,args=(i,)) for i in range(N)]
[t.start() for t in ts]; [t.join() for t in ts]
for it in keep:
    try: it.close()
    except Exception: pass
print(f"  成功 {len(out)}/{N}")
if err: print(f"  失败: {err[0]}")
else:   print(f"  类型独立: {len({t for _,_,t in out})} 个不同地址 (应为 {N})")

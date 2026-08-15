#!/usr/bin/env python3
"""把 PyO3 里所有进程级缓存过一遍判据,判它们该不该改成每解释器。

判据(可执行,不靠读代码):

    持有 PyObject 吗?  →  是永生的吗?  →  是堆类型吗?
    只有【堆类型且非永生】的要改。

永生对象(内建类型、None/True/False)的引用计数是饱和值 3<<30,并发改写没有后果,
共享是安全的;堆类型不永生,共享它就是在跨解释器竞争一个引用计数。

它抓到过 11 处:PyO3 把 `collections.abc.Sequence`、`decimal.Decimal`、`pathlib.Path`、
`uuid.UUID`、`zoneinfo.ZoneInfo` 这类【Python 层的类】缓存在进程级,而每个子解释器
各有一份(实测 4 个解释器 4 个不同地址)。和 #[pyclass] 那个是同一类 bug,
只是发生在转换层,而且没有任何测试会碰到它。

从 pyo3 仓库根目录跑(它要 grep src/):
    python3.14 subinterp-bench/audit_caches.py
"""
import re, subprocess, warnings, threading, pathlib
warnings.filterwarnings("ignore")

# 抓出每个 static 缓存实际缓存的是什么 —— 靠它们的 import(mod, attr) 调用
src = subprocess.run(["grep","-rn","-B4","-A6",r"PyOnceLock<Py<","--include=*.rs","src/"],
                     capture_output=True, text=True).stdout
targets = sorted(set(re.findall(r'\.import\(\s*py\s*,\s*"([\w.]+)"\s*,\s*"(\w+)"', src)))
print(f"  从源码里抓到 {len(targets)} 个可解析的缓存目标\n")

BODY = '''
import sys, ctypes
IMMORTAL = 3 << 30
res = []
for mod, attr in {targets!r}:
    try:
        m = __import__(mod, fromlist=[attr]); o = getattr(m, attr)
    except Exception as e:
        res.append((mod, attr, None, None, None, f"{{type(e).__name__}}: {{e}}")); continue
    rc = sys.getrefcount(o)
    heap = False
    if isinstance(o, type):
        f = ctypes.c_ulong.from_address(id(o) + ctypes.sizeof(ctypes.c_void_p)*21).value
        heap = bool(f & (1 << 9))          # Py_TPFLAGS_HEAPTYPE
    res.append((mod, attr, id(o), rc, heap, ""))
_q.put(res)
'''
from concurrent import interpreters
out, keep, lock = [], [], threading.Lock()
def w(i):
    it = interpreters.create()
    with lock: keep.append(it)
    q = interpreters.create_queue(); it.prepare_main(_q=q)
    it.exec(BODY.format(targets=targets))
    with lock: out.append(q.get())
ts=[threading.Thread(target=w,args=(i,)) for i in range(4)]
[t.start() for t in ts]; [t.join() for t in ts]
for it in keep:
    try: it.close()
    except Exception: pass

N = len(out)
IMMORTAL = 3 << 30
need, ok, skip = [], 0, []
for idx, (mod, attr, *_ ) in enumerate(out[0]):
    rows = [o[idx] for o in out]
    if rows[0][5]:
        skip.append((f"{mod}.{attr}", rows[0][5][:50])); continue
    ids   = {r[2] for r in rows}
    rcs   = [r[3] for r in rows]
    heap  = rows[0][4]
    immortal = all(r >= IMMORTAL for r in rcs)
    if heap and not immortal:
        need.append((f"{mod}.{attr}", len(ids), rcs[0]))
    else:
        ok += 1
print(f"  {N} 个子解释器,逐个判定:")
print(f"    ✅ 安全(非堆类型 或 永生)      {ok}")
print(f"    ★ 需要改(堆类型且非永生)      {len(need)}")
for name, nid, rc in need:
    print(f"        {name:38} {nid} 个地址  refcount {rc}")
if skip:
    print(f"    ⚠ 取不到(原样带出,不猜)     {len(skip)}")
    for name, why in skip[:8]:
        print(f"        {name:38} {why}")

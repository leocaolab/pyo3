use pyo3::prelude::*;
#[cfg(feature = "submodule")]
use pyo3::wrap_pymodule;
// PyBuffer 不在限定 API 里,abi3 构建下整条 sum_buf 都要关掉。
#[cfg(not(feature = "abi3"))]
use pyo3::buffer::PyBuffer;

/// 纯空调用:隔离框架的每次调用开销
#[pyfunction] fn noop() {}

/// 定长缓冲区求和:有可测的工作量,驻留 L1,零分配
/// 裸指针本身不是 Send;包一层,由调用方保证 buffer 在 detach 期间存活
/// (PyBuffer 持有对该对象的引用,它在整个函数体内存活)。
#[cfg(not(feature = "abi3"))]
struct SendPtr(*const f64);
#[cfg(not(feature = "abi3"))]
unsafe impl Send for SendPtr {}
#[cfg(not(feature = "abi3"))]
impl SendPtr {
    /// 通过方法取值,这样闭包捕获的是整个 `SendPtr` 而不是里面那个裸指针字段
    /// (Rust 2021 的闭包按字段捕获,直接写 `p.0` 会捕获 `*const f64`,它不是 Send)。
    #[inline] fn get(&self) -> *const f64 { self.0 }
}

#[cfg(not(feature = "abi3"))]
#[pyfunction]
fn sum_buf(buf: PyBuffer<f64>, py: Python<'_>) -> f64 {
    let n = buf.item_count();
    let p = SendPtr(buf.buf_ptr() as *const f64);
    py.detach(move || {
        let s = unsafe { std::slice::from_raw_parts(p.get(), n) };
        let mut acc = 0.0f64;
        for &v in s { acc += v * 1.000001; }
        acc
    })
}

/// 对象创建路径:上游把 type 对象缓存在进程级 static 里,所有子解释器共用一个,
/// 于是所有核抢同一条 refcount 缓存行 —— 这条路上上游不扩展(重负载下还会 SIGSEGV)。
#[pyclass] struct Row { #[pyo3(get)] a: f64, #[pyo3(get)] b: i64 }
#[pymethods] impl Row {
    #[new] fn new(a: f64, b: i64) -> Self { Row { a, b } }
}

/// 带状态的 pyclass:每个解释器拿自己那份 seed 各算各的。
/// 隔离一旦破了(两个解释器共用一个实例/一个 type),结果会串,`stress.py` 会抓到。
#[pyclass] struct Counter { seed: i64, total: i64 }
#[pymethods] impl Counter {
    #[new] fn new(seed: i64) -> Self { Counter { seed, total: 0 } }
    fn bump(&mut self, k: i64) -> i64 { self.total += self.seed * k; self.total }
}

/// 隔离 fork 热路径里那两个 FFI 调用的成本(fork 每次查找都要做一次)。
/// 返回每次调用的纳秒数。两个版本都能编,因为它只用 ffi,不用改动过的东西。
#[pyfunction]
fn interp_id_ns(n: u64, py: Python<'_>) -> f64 {
    let t = std::time::Instant::now();
    let mut acc = 0i64;
    for _ in 0..n {
        acc = acc.wrapping_add(unsafe {
            pyo3::ffi::PyInterpreterState_GetID(pyo3::ffi::PyInterpreterState_Get())
        });
    }
    let ns = t.elapsed().as_nanos() as f64 / n as f64;
    let _ = (acc, py);
    ns
}

/// 隔离【类型对象查找】本身:上游是一次原子读,fork 是 FFI + TLS + 数组下标。
/// 建对象路径每次都要做一次这个,所以两版之差应当全部落在这里。
#[pyfunction]
fn type_lookup_ns(n: u64, py: Python<'_>) -> f64 {
    use pyo3::type_object::PyTypeInfo;
    let t = std::time::Instant::now();
    let mut acc = 0usize;
    for _ in 0..n {
        acc = acc.wrapping_add(<Row as PyTypeInfo>::type_object_raw(py) as usize);
    }
    let ns = t.elapsed().as_nanos() as f64 / n as f64;
    let _ = acc;
    ns
}

// ── 裸 C ABI 对照 ──────────────────────────────────────────────────────
// 和 noop() 做同样的事(什么都不做,返回 None),但完全绕开 PyO3 的调用包装:
// 没有 catch_unwind,没有 attach 记账,没有参数/返回值转换。
// 两者的差 = PyO3 每次调用的全部固定成本。

unsafe extern "C" fn raw_noop_impl(
    _slf: *mut pyo3::ffi::PyObject,
    _args: *mut pyo3::ffi::PyObject,
) -> *mut pyo3::ffi::PyObject {
    // Py_None 在 3.12+ 是永生对象,这个 INCREF 不写内存
    pyo3::ffi::Py_INCREF(pyo3::ffi::Py_None());
    pyo3::ffi::Py_None()
}

/// `PyMethodDef` 只被 CPython 读,共享一个 static 是安全的。
struct RawDef(pyo3::ffi::PyMethodDef);
unsafe impl Sync for RawDef {}
static RAW_NOOP: RawDef = RawDef(pyo3::ffi::PyMethodDef {
    ml_name: c"raw_noop".as_ptr(),
    ml_meth: pyo3::ffi::PyMethodDefPointer { PyCFunction: raw_noop_impl },
    ml_flags: pyo3::ffi::METH_NOARGS,
    ml_doc: core::ptr::null(),
});

/// 子模块 —— 走 wrap_pymodule! / ModuleDef::make_module 那条路。
/// 上游在这里有个守卫:第二个子解释器 import 会抛
/// "PyO3 modules do not yet support subinterpreters"。
#[cfg(feature = "submodule")]
#[pymodule] fn inner(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("answer", 42i64)?;
    m.add_class::<Row>()?;
    Ok(())
}


// ── 对象创建的成本阶梯 ────────────────────────────────────────────────
// 手写两个裸 C 类型,除了 GC 标志之外完全一样。和 PyO3 的 #[pyclass] 并排跑,
// 就能把「GC 跟踪的钱」和「PyO3 包装层的钱」分开,而不是笼统说「慢 17%」。

#[repr(C)]
struct RawRow { ob_base: pyo3::ffi::PyObject, a: f64, b: i64 }

unsafe extern "C" fn rawrow_new(
    subtype: *mut pyo3::ffi::PyTypeObject,
    _args: *mut pyo3::ffi::PyObject,
    _kwds: *mut pyo3::ffi::PyObject,
) -> *mut pyo3::ffi::PyObject {
    let alloc = (*subtype).tp_alloc.unwrap();
    let obj = alloc(subtype, 0);
    if !obj.is_null() {
        let r = obj as *mut RawRow;
        (*r).a = 1.0;
        (*r).b = 2;
    }
    obj
}

unsafe extern "C" fn rawrow_dealloc(obj: *mut pyo3::ffi::PyObject) {
    let ty = pyo3::ffi::Py_TYPE(obj);
    if (pyo3::ffi::PyType_GetFlags(ty) & pyo3::ffi::Py_TPFLAGS_HAVE_GC) != 0 {
        pyo3::ffi::PyObject_GC_UnTrack(obj as *mut _);
    }
    let free = (*ty).tp_free.unwrap();
    free(obj as *mut _);
    pyo3::ffi::Py_DECREF(ty as *mut pyo3::ffi::PyObject);
}

unsafe extern "C" fn rawrow_traverse(
    _o: *mut pyo3::ffi::PyObject,
    _v: pyo3::ffi::visitproc,
    _a: *mut core::ffi::c_void,
) -> core::ffi::c_int { 0 }

unsafe fn make_raw_type(name: *const core::ffi::c_char, with_gc: bool) -> *mut pyo3::ffi::PyObject {
    let mut slots = vec![
        pyo3::ffi::PyType_Slot { slot: pyo3::ffi::Py_tp_new, pfunc: rawrow_new as *mut _ },
        pyo3::ffi::PyType_Slot { slot: pyo3::ffi::Py_tp_dealloc, pfunc: rawrow_dealloc as *mut _ },
    ];
    if with_gc {
        slots.push(pyo3::ffi::PyType_Slot { slot: pyo3::ffi::Py_tp_traverse, pfunc: rawrow_traverse as *mut _ });
    }
    slots.push(pyo3::ffi::PyType_Slot { slot: 0, pfunc: core::ptr::null_mut() });
    let mut flags = pyo3::ffi::Py_TPFLAGS_DEFAULT;
    if with_gc { flags |= pyo3::ffi::Py_TPFLAGS_HAVE_GC; }
    let spec = pyo3::ffi::PyType_Spec {
        name,
        basicsize: core::mem::size_of::<RawRow>() as i32,
        itemsize: 0,
        flags: flags as core::ffi::c_uint,
        slots: slots.as_mut_ptr(),
    };
    pyo3::ffi::PyType_FromSpec(&spec as *const _ as *mut _)
}

#[pymodule] fn abi3t(m: &Bound<'_, PyModule>) -> PyResult<()> {
    #[cfg(feature = "submodule")]
    m.add_wrapped(wrap_pymodule!(inner))?;
    unsafe {
        let f = pyo3::ffi::PyCFunction_NewEx(
            &RAW_NOOP.0 as *const _ as *mut _,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
        if f.is_null() { return Err(PyErr::fetch(m.py())); }
        m.add("raw_noop", Bound::from_owned_ptr(m.py(), f))?;
    }
    m.add_function(wrap_pyfunction!(interp_id_ns, m)?)?;
    m.add_function(wrap_pyfunction!(type_lookup_ns, m)?)?;
    m.add_function(wrap_pyfunction!(noop, m)?)?;
    #[cfg(not(feature = "abi3"))]
    m.add_function(wrap_pyfunction!(sum_buf, m)?)?;
    m.add_class::<Row>()?;
    m.add_class::<Counter>()?;
    unsafe {
        let t1 = make_raw_type(c"RawRowNoGC".as_ptr(), false);
        if t1.is_null() { return Err(PyErr::fetch(m.py())); }
        m.add("RawRowNoGC", Bound::from_owned_ptr(m.py(), t1))?;
        let t2 = make_raw_type(c"RawRowGC".as_ptr(), true);
        if t2.is_null() { return Err(PyErr::fetch(m.py())); }
        m.add("RawRowGC", Bound::from_owned_ptr(m.py(), t2))?;
    }
    Ok(())
}

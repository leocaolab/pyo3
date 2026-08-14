use pyo3::prelude::*;
use pyo3::buffer::PyBuffer;

/// 纯空调用:隔离框架的每次调用开销
#[pyfunction] fn noop() {}

/// 定长缓冲区求和:有可测的工作量,驻留 L1,零分配
/// 裸指针本身不是 Send;包一层,由调用方保证 buffer 在 detach 期间存活
/// (PyBuffer 持有对该对象的引用,它在整个函数体内存活)。
struct SendPtr(*const f64);
unsafe impl Send for SendPtr {}
impl SendPtr {
    /// 通过方法取值,这样闭包捕获的是整个 `SendPtr` 而不是里面那个裸指针字段
    /// (Rust 2021 的闭包按字段捕获,直接写 `p.0` 会捕获 `*const f64`,它不是 Send)。
    #[inline] fn get(&self) -> *const f64 { self.0 }
}

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

#[pymodule] fn abi3t(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(interp_id_ns, m)?)?;
    m.add_function(wrap_pyfunction!(type_lookup_ns, m)?)?;
    m.add_function(wrap_pyfunction!(noop, m)?)?;
    m.add_function(wrap_pyfunction!(sum_buf, m)?)?;
    m.add_class::<Row>()?;
    m.add_class::<Counter>()?;
    Ok(())
}

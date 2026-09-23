//! Runs code in real own-GIL sub-interpreters, for the `test_subinterp_*` binaries.
//!
//! Each of those binaries is its own process on purpose: executing a module records the
//! extension's home interpreter (a process-wide fact, M1.3), and these tests need to set it
//! up in a specific way without disturbing any other test.

#![allow(dead_code)] // each test binary uses a subset of these helpers

use pyo3::ffi;
use pyo3::Python;

/// Runs `f(i, py)` in `n` own-GIL sub-interpreters at once, one thread each, and ends each one
/// with `Py_EndInterpreter`. Returns the results in order of `i`.
pub fn in_sub_interpreters<R: Send>(n: usize, f: impl Fn(usize, Python<'_>) -> R + Sync) -> Vec<R> {
    Python::initialize();
    let f = &f;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n)
            .map(|i| {
                // SAFETY: standard sub-interpreter lifecycle on a thread with no thread state:
                // take main's GIL, create the interpreter (which detaches main's thread state
                // and makes the new one current), run, end it, restore main's.
                scope.spawn(move || unsafe {
                    let gstate = ffi::PyGILState_Ensure();
                    let main = ffi::PyThreadState_Get();
                    let config = ffi::_PyInterpreterConfig_INIT;
                    let mut sub: *mut ffi::PyThreadState = std::ptr::null_mut();
                    let status = ffi::Py_NewInterpreterFromConfig(&mut sub, &config);
                    assert!(ffi::PyStatus_Exception(status) == 0 && !sub.is_null());
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // The new thread state is current and bound, so this attach is a
                        // re-entrant one on this interpreter.
                        Python::attach(|py| f(i, py))
                    }));
                    ffi::Py_EndInterpreter(sub);
                    ffi::PyEval_RestoreThread(main);
                    ffi::PyGILState_Release(gstate);
                    out.unwrap_or_else(|e| std::panic::resume_unwind(e))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}

/// `in_sub_interpreters` with one interpreter.
#[expect(
    clippy::disallowed_types,
    reason = "a std Mutex hands the FnOnce to the thread; tests are std"
)]
pub fn in_sub_interpreter<R: Send>(f: impl FnOnce(Python<'_>) -> R + Send) -> R {
    let f = std::sync::Mutex::new(Some(f));
    in_sub_interpreters(1, |_, py| (f.lock().unwrap().take().unwrap())(py))
        .pop()
        .unwrap()
}

/// The id of the interpreter the calling thread is attached to.
pub fn interp_id(_py: Python<'_>) -> i64 {
    // SAFETY: `_py` witnesses that this thread is attached.
    unsafe { ffi::PyInterpreterState_GetID(ffi::PyInterpreterState_Get()) }
}

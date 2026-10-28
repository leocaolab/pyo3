//! Making an interpreter's per-interpreter registry runs no Python code, so an exception set on
//! the thread for later (`PyThreadState_SetAsyncExc`, as an interrupt from another thread does)
//! is still pending afterwards and lands at the next bytecode boundary.
#![cfg(all(Py_3_12, not(Py_LIMITED_API)))]

mod subinterp_util;

use pyo3::exceptions::PyRuntimeError;
use pyo3::ffi;
use pyo3::sync::PerInterpreterCell;
use std::ffi::c_long;
use subinterp_util::in_sub_interpreter;

#[test]
fn an_exception_set_before_the_first_cell_fill_still_lands() {
    let landed = in_sub_interpreter(|py| {
        static CELL: PerInterpreterCell<()> = PerInterpreterCell::new();
        // SAFETY: this thread is attached; the exception is set on its own thread state.
        let set = unsafe {
            ffi::PyThreadState_SetAsyncExc(
                ffi::PyThread_get_thread_ident() as c_long,
                ffi::PyExc_RuntimeError,
            )
        };
        assert_eq!(set, 1, "exactly one thread state should take the exception");

        // the first per-interpreter value in this interpreter: it makes the registry
        CELL.get_or_init(py, || ());

        py.run(c"pass", None, None)
            .err()
            .map(|e| e.is_instance_of::<PyRuntimeError>(py))
    });
    assert_eq!(
        landed,
        Some(true),
        "the pending RuntimeError must land on the next bytecode, not be consumed by the registry"
    );
}

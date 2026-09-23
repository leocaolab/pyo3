//! Ledger #20 (M4.1): the teardown hook imports nothing during `Py_EndInterpreter`.
//!
//! An audit hook records `import` events while an interpreter is being ended. `gc` is removed
//! from `sys.modules` first, so an import of it at teardown would be a real one and raise the
//! event. Before the fix, the hook ran `PyImport_ImportModule("gc")` there.
#![cfg(all(Py_3_12, not(Py_LIMITED_API)))]

mod subinterp_util;

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use subinterp_util::in_sub_interpreter;

extern "C" {
    fn PySys_AddAuditHook(
        hook: unsafe extern "C" fn(*const c_char, *mut ffi::PyObject, *mut c_void) -> c_int,
        user_data: *mut c_void,
    ) -> c_int;
}

static RECORDING: AtomicBool = AtomicBool::new(false);
static IMPORTED_GC: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn audit(event: *const c_char, args: *mut ffi::PyObject, _: *mut c_void) -> c_int {
    if RECORDING.load(Ordering::Acquire) && CStr::from_ptr(event).to_bytes() == b"import" {
        let name = ffi::PyTuple_GetItem(args, 0);
        if !name.is_null() {
            let mut len = 0;
            let s = ffi::PyUnicode_AsUTF8AndSize(name, &mut len);
            if !s.is_null() && std::slice::from_raw_parts(s as *const u8, len as usize) == b"gc" {
                IMPORTED_GC.store(true, Ordering::Release);
            }
            ffi::PyErr_Clear();
        }
    }
    0
}

#[test]
fn teardown_imports_nothing() {
    Python::initialize();
    assert_eq!(unsafe { PySys_AddAuditHook(audit, std::ptr::null_mut()) }, 0);

    static CELL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    in_sub_interpreter(|py| {
        // first per-interpreter value here: creates the registry and registers the hook
        CELL.get_or_init(py, || py.None());
        py.run(c"import sys; sys.modules.pop('gc', None)", None, None).unwrap();
        RECORDING.store(true, Ordering::Release);
    });
    RECORDING.store(false, Ordering::Release);
    assert!(!IMPORTED_GC.load(Ordering::Acquire), "the teardown hook imported gc during Py_EndInterpreter");
}

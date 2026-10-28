//! What happens to an interpreter's per-interpreter values when it is ended.
//!
//! Ledger #20 (M4.1): nothing imports during `Py_EndInterpreter`. An audit hook records `import`
//! events while an interpreter is being ended. `gc` is removed from `sys.modules` first, so an
//! import of it at teardown would be a real one and raise the event. A former teardown hook ran
//! `PyImport_ImportModule("gc")` there.
#![cfg(all(Py_3_12, not(Py_LIMITED_API)))]

mod subinterp_util;

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::{PerInterpreterCell, PyOnceLock};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use subinterp_util::in_sub_interpreter;

extern "C" {
    fn PySys_AddAuditHook(
        hook: unsafe extern "C" fn(*const c_char, *mut ffi::PyObject, *mut c_void) -> c_int,
        user_data: *mut c_void,
    ) -> c_int;
}

static RECORDING: AtomicBool = AtomicBool::new(false);
static IMPORTED_GC: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn audit(
    event: *const c_char,
    args: *mut ffi::PyObject,
    _: *mut c_void,
) -> c_int {
    // SAFETY: CPython calls audit hooks with a valid event name and an argument tuple, with
    // the GIL held.
    unsafe {
        if RECORDING.load(Ordering::Acquire) && CStr::from_ptr(event).to_bytes() == b"import" {
            let name = ffi::PyTuple_GetItem(args, 0);
            if !name.is_null() {
                let mut len = 0;
                let s = ffi::PyUnicode_AsUTF8AndSize(name, &mut len);
                if !s.is_null() && std::slice::from_raw_parts(s as *const u8, len as usize) == b"gc"
                {
                    IMPORTED_GC.store(true, Ordering::Release);
                }
                ffi::PyErr_Clear();
            }
        }
        0
    }
}

#[test]
fn teardown_imports_nothing() {
    Python::initialize();
    assert_eq!(
        // SAFETY: Python is initialized; `audit` matches the hook signature.
        unsafe { PySys_AddAuditHook(audit, std::ptr::null_mut()) },
        0
    );

    static CELL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    in_sub_interpreter(|py| {
        // first per-interpreter value here: creates the registry and registers the hook
        CELL.get_or_init(py, || py.None());
        py.run(c"import sys; sys.modules.pop('gc', None)", None, None)
            .unwrap();
        RECORDING.store(true, Ordering::Release);
    });
    RECORDING.store(false, Ordering::Release);
    assert!(
        !IMPORTED_GC.load(Ordering::Acquire),
        "something imported gc during Py_EndInterpreter"
    );
}

static CYCLE_MEMBERS_FREED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn count_freed(_capsule: *mut ffi::PyObject) {
    CYCLE_MEMBERS_FREED.fetch_add(1, Ordering::SeqCst);
}

/// A value whose only way out is the cyclic collector must still be freed by the end of
/// `Py_EndInterpreter`: the registry releases its reference, and the interpreter's last
/// collection has to come after that. A heap type object is such a value, since its `__mro__`
/// contains it.
#[test]
fn a_cycle_a_cell_held_is_collected_when_the_interpreter_ends() {
    static CELL: PerInterpreterCell<Py<PyAny>> = PerInterpreterCell::new();
    in_sub_interpreter(|py| {
        // SAFETY: attached; the capsule holds no pointer anyone dereferences.
        let member = unsafe {
            Bound::from_owned_ptr(
                py,
                ffi::PyCapsule_New(
                    std::ptr::NonNull::<u8>::dangling().as_ptr().cast(),
                    std::ptr::null(),
                    Some(count_freed),
                ),
            )
        };
        let cycle = pyo3::types::PyList::new(py, [member]).unwrap();
        cycle.append(&cycle).unwrap();
        CELL.get_or_init(py, || cycle.into_any().unbind());
        assert_eq!(CYCLE_MEMBERS_FREED.load(Ordering::SeqCst), 0);
    });
    assert_eq!(
        CYCLE_MEMBERS_FREED.load(Ordering::SeqCst),
        1,
        "the cycle the cell held outlived its interpreter"
    );
}

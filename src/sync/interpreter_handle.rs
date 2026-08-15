//! Attaching a foreign thread back to the interpreter an object came from.
//!
//! # The hazard this exists for
//!
//! [`Python::attach`][crate::Python::attach] on a thread PyO3 has never attached goes through
//! `PyGILState_Ensure`, and that API binds the **main** interpreter by design. Under PEP 684 that
//! is silently wrong: a worker thread ends up executing in interpreter 0 while touching objects
//! owned by interpreter 1.
//!
//! It is not hypothetical, and it is not rare. Any extension that calls back into Python from a
//! thread pool has this shape — rayon, tokio's `spawn_blocking`, a hand-rolled pool. Measured on
//! polars 1.43.2: writing a `DataFrame` to a Python file-like object dispatches through rayon, and
//! a probe that records which interpreter each `write` runs in reports interpreter 0 while the
//! caller is interpreter 1. The visible symptom is an abort inside CPython —
//!
//! ```text
//! _io_BytesIO_getvalue_impl -> _PyBytes_Resize -> realloc
//!   -> ___BUG_IN_CLIENT_OF_LIBMALLOC_POINTER_BEING_FREED_WAS_NOT_ALLOCATED
//! ```
//!
//! — because the buffer grew in one interpreter's arena and is reallocated against another's. But
//! the abort is the lucky case. The actual event is one interpreter mutating another's object, and
//! that is wrong whether or not it crashes.
//!
//! # Using it
//!
//! Capture the interpreter where you receive the Python object, then attach back to *that* one
//! from the worker:
//!
//! ```no_run
//! # use pyo3::prelude::*;
//! # use pyo3::sync::InterpreterHandle;
//! # fn example(py: Python<'_>, obj: Py<PyAny>) {
//! let interp = InterpreterHandle::current(py);
//!
//! std::thread::spawn(move || {
//!     // Without this the callback would run in the main interpreter.
//!     interp.attach(|py| {
//!         let _ = obj.bind(py);
//!     });
//! });
//! # }
//! ```

use crate::ffi;
use crate::internal::state::AssumeAttached;
use crate::Python;

/// The interpreter a value belongs to, so a foreign thread can attach back to it.
///
/// See the [module docs][self] for why `Python::attach` is not enough.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InterpreterHandle(*mut ffi::PyInterpreterState);

// SAFETY: the handle is an opaque interpreter pointer. It is only dereferenced by CPython, under
// that interpreter's own lock, and `attach` re-checks nothing that a raw pointer could invalidate
// — see the safety note on `attach` for the one thing the caller must guarantee.
unsafe impl Send for InterpreterHandle {}
unsafe impl Sync for InterpreterHandle {}

impl InterpreterHandle {
    /// The interpreter this thread is currently attached to.
    pub fn current(_py: Python<'_>) -> Self {
        // SAFETY: `Python<'_>` witnesses that this thread is attached, so there is a current
        // interpreter.
        InterpreterHandle(unsafe { ffi::PyInterpreterState_Get() })
    }

    /// Runs `f` attached to this handle's interpreter, whichever thread calls it.
    ///
    /// If the calling thread is already attached to this same interpreter, `f` runs directly. On
    /// any other thread a fresh thread state is created for this interpreter, used, and destroyed.
    ///
    /// # Panics
    ///
    /// Panics if the thread is already attached to a *different* interpreter. Attaching to two
    /// interpreters at once is not a thing CPython supports, and doing it silently is how the
    /// class of bug this type exists for gets written in the first place.
    ///
    /// # Safety of the handle
    ///
    /// The interpreter must still be alive. Holding a `Py<T>` that belongs to it is enough: the
    /// object cannot outlive its interpreter, so if you have one the interpreter is there. A
    /// handle kept past the interpreter's destruction is a dangling pointer and calling `attach`
    /// on it is undefined behaviour — which is why `current` takes a `Python<'_>` and there is no
    /// constructor from a raw pointer.
    pub fn attach<F, R>(self, f: F) -> R
    where
        F: for<'py> FnOnce(Python<'py>) -> R,
    {
        let current = current_interpreter_or_null();

        if current == self.0 {
            // Already here. Take the fast path, and tell PyO3 the thread is attached so that
            // `Py<T>`'s `Drop` decrefs instead of deferring.
            // SAFETY: a thread state for this interpreter is current.
            let _attached = unsafe { AssumeAttached::new() };
            // SAFETY: as above.
            return f(unsafe { Python::assume_attached() });
        }
        assert!(
            current.is_null(),
            "this thread is attached to a different interpreter; detach from it before attaching \
             to another"
        );

        // SAFETY: the caller guarantees the interpreter is alive. `PyThreadState_New` does not
        // require holding a GIL.
        let tstate = unsafe { ffi::PyThreadState_New(self.0) };
        assert!(!tstate.is_null(), "PyThreadState_New returned null");

        // SAFETY: `tstate` was just created for this interpreter and is not current anywhere.
        unsafe { ffi::PyEval_RestoreThread(tstate) };

        let result = {
            // SAFETY: `tstate` is now current.
            let _attached = unsafe { AssumeAttached::new() };
            // SAFETY: as above.
            f(unsafe { Python::assume_attached() })
        };

        // Order matters and there is only one right one:
        //   Clear   — requires the GIL held and `tstate` current
        //   Save    — detaches it and releases the GIL, returning it
        //   Delete  — requires it to no longer be current
        // Slipping a `PyThreadState_Swap(NULL)` in before `Save` leaves nothing for `Save` to
        // detach, and CPython segfaults.
        // SAFETY: `tstate` is current and this thread holds its interpreter's GIL.
        unsafe {
            ffi::PyThreadState_Clear(tstate);
            let detached = ffi::PyEval_SaveThread();
            debug_assert_eq!(detached, tstate);
            ffi::PyThreadState_Delete(tstate);
        }
        result
    }
}

/// The interpreter this thread is attached to, or null if it is attached to none.
///
/// `PyInterpreterState_Get` cannot answer this — it is fatal when there is no thread state, so it
/// can only be called once you already know.
#[inline]
fn current_interpreter_or_null() -> *mut ffi::PyInterpreterState {
    #[cfg(all(Py_3_13, not(Py_LIMITED_API), not(PyPy), not(GraalPy)))]
    {
        // SAFETY: this one is explicitly null-returning rather than fatal.
        let tstate = unsafe { ffi::PyThreadState_GetUnchecked() };
        if tstate.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: `tstate` is non-null and current.
        return unsafe { ffi::PyThreadState_GetInterpreter(tstate) };
    }
    // Limited API and older versions have no null-returning accessor, so fall back to PyO3's own
    // attach count. It misses a thread that CPython attached without PyO3 seeing it — the same
    // blind spot `AssumeAttached` exists for — so on those builds do not call `attach` from
    // inside a raw CPython callback.
    #[cfg(not(all(Py_3_13, not(Py_LIMITED_API), not(PyPy), not(GraalPy))))]
    {
        if crate::internal::state::thread_is_attached() {
            // SAFETY: PyO3 says this thread is attached, so there is a thread state.
            unsafe { ffi::PyInterpreterState_Get() }
        } else {
            core::ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::any::PyAnyMethods;
    use crate::platform::prelude::String;
    use crate::{Py, PyAny};

    /// The whole point: a foreign thread must land in the handle's interpreter, not the main one.
    ///
    /// Run against the main interpreter this can only show that the value round-trips — proving it
    /// picks the *right* interpreter needs a sub-interpreter, which lives in
    /// `subinterp-bench/callback.py`.
    #[test]
    fn foreign_thread_can_attach_and_use_an_object() {
        let (handle, obj): (InterpreterHandle, Py<PyAny>) = Python::attach(|py| {
            (
                InterpreterHandle::current(py),
                py.eval(c"'from the owning interpreter'", None, None)
                    .unwrap()
                    .unbind(),
            )
        });

        let got = std::thread::spawn(move || {
            handle.attach(|py| obj.bind(py).extract::<String>().unwrap())
        })
        .join()
        .unwrap();

        assert_eq!(got, "from the owning interpreter");
    }

    /// Attaching from a thread that is already there must not create a second thread state.
    #[test]
    fn attaching_from_the_same_interpreter_is_a_no_op() {
        Python::attach(|py| {
            let handle = InterpreterHandle::current(py);
            let n = handle.attach(|py2| {
                assert_eq!(InterpreterHandle::current(py2), handle);
                7
            });
            assert_eq!(n, 7);
        });
    }
}

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
use core::ptr::NonNull;

/// The interpreter a value belongs to, so a foreign thread can attach back to it.
///
/// See the [module docs][self] for why `Python::attach` is not enough.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InterpreterHandle(*mut ffi::PyInterpreterState);

// SAFETY: the handle is an opaque interpreter pointer. It is only dereferenced by CPython, under
// that interpreter's own lock, and `attach` re-checks nothing that a raw pointer could invalidate
// — see the safety note on `attach` for the one thing the caller must guarantee.
unsafe impl Send for InterpreterHandle {}
// SAFETY: as for `Send`; a shared handle offers nothing beyond copying the pointer.
unsafe impl Sync for InterpreterHandle {}

impl InterpreterHandle {
    /// The interpreter this thread is currently attached to.
    pub fn current(_py: Python<'_>) -> Self {
        // SAFETY: `Python<'_>` witnesses that this thread is attached, so there is a current
        // interpreter.
        InterpreterHandle(unsafe { ffi::PyInterpreterState_Get() })
    }

    /// The main interpreter. Needs no token and no attach, but Python must be initialized
    /// (see [`Python::initialize`]) and not yet finalized, as for any other handle.
    ///
    /// Not on the limited API: `PyInterpreterState_Main` is only in the full API.
    #[cfg(not(Py_LIMITED_API))]
    pub fn main() -> Self {
        // SAFETY: `PyInterpreterState_Main` only reads the runtime's main-interpreter slot, which
        // is set once Python is initialized; the caller's precondition is that it is.
        InterpreterHandle(unsafe { ffi::PyInterpreterState_Main() })
    }

    /// The interpreter that the calling thread's own thread state belongs to, or `None` if this
    /// thread has no thread state.
    ///
    /// Unlike [`current`][Self::current] this does not require the thread to be attached: a
    /// thread bound to an interpreter but detached from it (inside `py.detach`, or a worker
    /// between requests) still answers. It is the same binding [`attach`][Self::attach] reads.
    pub fn bound() -> Option<Self> {
        // SAFETY: always safe to call; null when this thread has no thread state.
        let tstate = unsafe { ffi::PyGILState_GetThisThreadState() };
        if tstate.is_null() {
            return None;
        }
        // SAFETY: `tstate` is this thread's own thread state, which keeps its interpreter alive.
        Some(InterpreterHandle(unsafe {
            ffi::PyThreadState_GetInterpreter(tstate)
        }))
    }

    /// Whether this is the main interpreter.
    ///
    /// Not on the limited API, for the same reason as [`main`][Self::main].
    #[cfg(not(Py_LIMITED_API))]
    pub fn is_main(self) -> bool {
        // SAFETY: `PyInterpreterState_Main` has no preconditions once the runtime is initialized.
        self.0 == unsafe { ffi::PyInterpreterState_Main() }
    }

    /// The interpreter's id, as CPython numbers them: the main interpreter is `0`, and each
    /// sub-interpreter gets the next free number.
    ///
    /// Meant for naming an interpreter in errors and logs, and for comparing two of them. Two
    /// handles to the same interpreter always give the same id.
    pub fn id(self) -> i64 {
        // SAFETY: the handle's interpreter is alive (see "Safety of the handle" below), and
        // `PyInterpreterState_GetID` only reads it.
        unsafe { ffi::PyInterpreterState_GetID(self.0) }
    }

    /// A new thread state for this interpreter, made on the calling thread and not current, or
    /// `None` when CPython could not make one (`PyThreadState_New` returned null: out of memory).
    ///
    /// For code that runs this interpreter's thread states by hand: a thread that keeps one
    /// thread state for its whole life, or a visit from another thread that has to hold this
    /// interpreter's GIL for a raw CPython call (`PyThreadState_SetAsyncExc` only reaches the
    /// threads of the interpreter that is current). Code that only wants to run Rust against
    /// the interpreter uses [`attach`][Self::attach], which also frees the thread state.
    ///
    /// The thread state is bound to the calling thread; if the thread had no thread state, it
    /// becomes the thread's own, so [`bound`][Self::bound] names this interpreter from then on
    /// and `Python::attach` there re-attaches it. Needs no GIL. The caller owns it: it is made
    /// current with `PyEval_RestoreThread`, and released, with it current, by
    /// `PyThreadState_Clear`, then `PyEval_SaveThread` and `PyThreadState_Delete` (or
    /// `PyThreadState_DeleteCurrent`). One never released stays in the interpreter's list of
    /// threads until the interpreter ends.
    ///
    /// The handle's lifetime rule applies (see [`attach`][Self::attach]).
    pub fn new_thread_state(self) -> Option<NonNull<ffi::PyThreadState>> {
        // SAFETY: the handle's interpreter is alive (the handle's rule); `PyThreadState_New`
        // needs no GIL.
        NonNull::new(unsafe { ffi::PyThreadState_New(self.0) })
    }

    /// Runs `f` attached to this handle's interpreter, whichever thread calls it.
    ///
    /// If the calling thread is attached to this same interpreter, `f` runs directly. If its thread
    /// state for this interpreter exists but is detached (inside `py.detach`, or a worker between
    /// requests), that thread state is restored for `f` and detached again afterwards. On a thread
    /// with no thread state, a fresh one is created for this interpreter, used, and destroyed.
    ///
    /// # Panics
    ///
    /// Panics if the thread is attached, or bound, to a *different* interpreter. Attaching to two
    /// interpreters at once is not a thing CPython supports, and doing it silently is how the
    /// class of bug this type exists for gets written in the first place.
    ///
    /// # Safety of the handle
    ///
    /// The interpreter must still be alive. Holding a `Py<T>` that belongs to it is enough: the
    /// object cannot outlive its interpreter, so if you have one the interpreter is there. A
    /// handle kept past the interpreter's destruction is a dangling pointer and calling `attach`
    /// on it is undefined behaviour — which is why `current` and `bound` take their interpreter
    /// from the thread and there is no constructor from a raw pointer. The same holds for
    /// [`is_main`][Self::is_main] and [`id`][Self::id], which read the interpreter. [`main`][Self::main]
    /// is no exception: it names the main interpreter, which is alive only until Python finalizes.
    pub fn attach<F, R>(self, f: F) -> R
    where
        F: for<'py> FnOnce(Python<'py>) -> R,
    {
        let current = crate::internal::state::current_tstate_or_null();
        if !current.is_null() {
            // SAFETY: a thread state is current, so this thread holds its interpreter's GIL.
            let interp = unsafe { ffi::PyInterpreterState_Get() };
            assert!(
                interp == self.0,
                "this thread is attached to a different interpreter; detach from it before \
                 attaching to another"
            );
            // Already here, with the GIL. Tell PyO3 the thread is attached so that `Py<T>`'s
            // `Drop` decrefs instead of deferring.
            // SAFETY: a thread state for this interpreter is current.
            let _attached = unsafe { AssumeAttached::new() };
            // SAFETY: as above.
            return f(unsafe { Python::assume_attached() });
        }

        // Not attached. A thread that is *bound* to this interpreter but detached (inside
        // `py.detach`, or a worker between requests) gets its own thread state back rather than
        // a second one: that keeps its thread-locals, and it is what the thread will resume with.
        let bound = crate::internal::state::current_interpreter_or_null();
        if bound == self.0 {
            // SAFETY: the GILState thread state is this thread's, belongs to this interpreter,
            // and is not current (checked above).
            let tstate = unsafe { ffi::PyGILState_GetThisThreadState() };
            // SAFETY: as above.
            unsafe { ffi::PyEval_RestoreThread(tstate) };
            let result = {
                // SAFETY: `tstate` is now current.
                let _attached = unsafe { AssumeAttached::new() };
                // SAFETY: as above.
                f(unsafe { Python::assume_attached() })
            };
            // SAFETY: `tstate` is current; hand it back detached, as it was found.
            let detached = unsafe { ffi::PyEval_SaveThread() };
            debug_assert_eq!(detached, tstate);
            return result;
        }
        assert!(
            bound.is_null(),
            "this thread's thread state belongs to a different interpreter; attach to that one, \
             or use a thread that has none"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::prelude::String;
    use crate::types::any::PyAnyMethods;
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

    /// A thread whose thread state for this interpreter exists but is *detached* — inside
    /// `py.detach`, or a worker that saved its thread state between requests — must get the GIL
    /// back for `f`. The fast path used to fire on the thread's bound thread state alone and ran
    /// `f` without the GIL (SUBINTERP-FIXES.md ledger #22).
    #[test]
    fn attach_inside_detach_holds_the_gil() {
        Python::attach(|py| {
            let handle = InterpreterHandle::current(py);
            let (held, same) = py.detach(|| {
                handle.attach(|py2| {
                    // SAFETY: always safe to call; no sub-interpreter exists in this binary, so
                    // `PyGILState_Check` is live and reports whether this thread holds the GIL.
                    let held = unsafe { ffi::PyGILState_Check() };
                    (held, InterpreterHandle::current(py2) == handle)
                })
            });
            assert_eq!(held, 1, "f ran without the GIL");
            assert!(same);
        });
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

    /// The main interpreter is the one the test process runs in, and it is interpreter 0.
    #[cfg(not(Py_LIMITED_API))]
    #[test]
    fn main_is_the_main_interpreter() {
        Python::attach(|py| {
            let main = InterpreterHandle::main();
            assert!(main.is_main());
            assert_eq!(main.id(), 0);
            assert_eq!(main, InterpreterHandle::current(py));
            assert_eq!(InterpreterHandle::current(py).id(), 0);
        });
    }

    /// A thread with no thread state has no interpreter to be bound to.
    #[test]
    fn bound_is_none_on_a_thread_without_a_thread_state() {
        std::thread::spawn(|| assert_eq!(InterpreterHandle::bound(), None))
            .join()
            .unwrap();
    }

    /// `bound` answers for a thread that is detached, where `current` cannot.
    #[test]
    fn bound_survives_detaching() {
        Python::attach(|py| {
            let handle = InterpreterHandle::current(py);
            assert_eq!(InterpreterHandle::bound(), Some(handle));
            py.detach(|| assert_eq!(InterpreterHandle::bound(), Some(handle)));
        });
    }

    /// On a thread that has none, the new thread state becomes the thread's own, for the
    /// handle's interpreter, and it runs there once made current.
    #[test]
    fn new_thread_state_becomes_the_threads_own() {
        let handle = Python::attach(InterpreterHandle::current);
        let (bound, ran_in) = std::thread::spawn(move || {
            assert_eq!(InterpreterHandle::bound(), None);
            let tstate = handle
                .new_thread_state()
                .expect("PyThreadState_New returned null");
            let bound = InterpreterHandle::bound();
            // SAFETY: `tstate` was just made on this thread and is not current; made current, it
            // is released the documented way (Clear with it current, then Save and Delete).
            let ran_in = unsafe {
                ffi::PyEval_RestoreThread(tstate.as_ptr());
                let ran_in = InterpreterHandle(ffi::PyInterpreterState_Get());
                ffi::PyThreadState_Clear(tstate.as_ptr());
                let detached = ffi::PyEval_SaveThread();
                assert_eq!(detached, tstate.as_ptr());
                ffi::PyThreadState_Delete(detached);
                ran_in
            };
            (bound, ran_in)
        })
        .join()
        .unwrap();
        assert_eq!(
            bound,
            Some(handle),
            "the thread state is not the thread's own"
        );
        assert_eq!(ran_in, handle, "the thread state is in another interpreter");
    }
}

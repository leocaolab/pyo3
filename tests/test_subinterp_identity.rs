//! The interpreter identity methods of `InterpreterHandle` (`main`, `bound`, `is_main`, `id`),
//! `InterpreterHandle::new_thread_state`, `InterpreterHandle::try_attach` and
//! `Python::attach_current`, in real own-GIL
//! sub-interpreters.
//!
//! The lib unit tests cover the main interpreter only. Whether a handle really names the
//! sub-interpreter it came from, and not the main one, needs a sub-interpreter to tell them apart.
//! Each `test_subinterp_*` binary is its own process (see `subinterp_util`).
#![cfg(all(feature = "macros", Py_3_12, not(Py_LIMITED_API)))]

mod subinterp_util;

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::InterpreterHandle;
use subinterp_util::{in_sub_interpreter, interp_id};

#[test]
fn a_sub_interpreter_handle_is_not_main_and_has_its_own_id() {
    let (id, is_main, main_is_main, main_id, main_equal) = in_sub_interpreter(|py| {
        let handle = InterpreterHandle::current(py);
        assert_eq!(
            handle.id(),
            interp_id(py),
            "the handle's id must be the attached interpreter's id"
        );
        let main = InterpreterHandle::main();
        (
            handle.id(),
            handle.is_main(),
            main.is_main(),
            main.id(),
            main == handle,
        )
    });
    assert_ne!(id, 0, "must run in a sub-interpreter");
    assert!(!is_main, "a sub-interpreter handle claims to be main");
    assert!(
        main_is_main,
        "InterpreterHandle::main() is not the main interpreter"
    );
    assert_eq!(main_id, 0, "the main interpreter is id 0");
    assert!(
        !main_equal,
        "main() compared equal to a sub-interpreter handle"
    );
}

#[test]
fn bound_names_the_sub_interpreter_inside_detach() {
    in_sub_interpreter(|py| {
        let handle = InterpreterHandle::current(py);
        assert_eq!(InterpreterHandle::bound(), Some(handle));
        py.detach(|| {
            assert_eq!(
                InterpreterHandle::bound(),
                Some(handle),
                "a detached thread must still report its own sub-interpreter"
            );
        });
    });
}

/// `new_thread_state` makes the thread state in the handle's sub-interpreter, not the main one: a
/// thread with none that takes it is bound to the sub-interpreter, and runs in it once it makes
/// the thread state current.
#[test]
fn new_thread_state_belongs_to_the_handles_sub_interpreter() {
    let (sub, bound, ran_in) = in_sub_interpreter(|py| {
        let handle = InterpreterHandle::current(py);
        let sub = interp_id(py);
        // Detached, so the visiting thread can take the sub-interpreter's GIL.
        let (bound, ran_in) = py.detach(|| {
            std::thread::spawn(move || {
                let tstate = handle
                    .new_thread_state()
                    .expect("PyThreadState_New returned null");
                let bound = InterpreterHandle::bound() == Some(handle);
                // SAFETY: `tstate` was just made on this thread and is not current; made
                // current, it is released the documented way.
                unsafe {
                    ffi::PyEval_RestoreThread(tstate.as_ptr());
                    let ran_in = ffi::PyInterpreterState_GetID(ffi::PyInterpreterState_Get());
                    ffi::PyThreadState_Clear(tstate.as_ptr());
                    ffi::PyThreadState_DeleteCurrent();
                    (bound, ran_in)
                }
            })
            .join()
            .unwrap()
        });
        (sub, bound, ran_in)
    });
    assert_ne!(sub, 0, "must run in a sub-interpreter");
    assert!(
        bound,
        "the new thread state did not make its thread bound to the handle's interpreter"
    );
    assert_eq!(
        ran_in, sub,
        "the new thread state runs in interpreter {ran_in}, not {sub}"
    );
}

/// `try_attach` from a thread with no thread state runs `f` in the handle's sub-interpreter, not
/// the main one, with a thread state current, and frees that thread state afterwards.
#[test]
fn try_attach_runs_in_the_handles_sub_interpreter() {
    let (sub, got, bound_after) = in_sub_interpreter(|py| {
        let handle = InterpreterHandle::current(py);
        let sub = interp_id(py);
        // Detached, so the visiting thread can take the sub-interpreter's GIL.
        let (got, bound_after) = py.detach(|| {
            std::thread::spawn(move || {
                let got = handle.try_attach(|py2| {
                    // SAFETY: always safe to call; null when no thread state is current.
                    let current = !unsafe { ffi::PyThreadState_GetUnchecked() }.is_null();
                    (interp_id(py2), current)
                });
                (got, InterpreterHandle::bound())
            })
            .join()
            .unwrap()
        });
        (sub, got, bound_after)
    });
    assert_ne!(sub, 0, "must run in a sub-interpreter");
    let (ran_in, current) = got.expect("try_attach could not make a thread state");
    assert_eq!(
        ran_in, sub,
        "try_attach ran in interpreter {ran_in}, not {sub}"
    );
    assert!(current, "try_attach ran f with no thread state current");
    assert_eq!(
        bound_after, None,
        "try_attach left its thread state on the visiting thread"
    );
}

/// `attach_current` runs `f` on the thread state that is already current (made so with raw calls
/// PyO3 does not see), in the right interpreter, with the GIL, and leaves that thread state
/// current afterwards.
#[test]
fn attach_current_runs_in_the_current_thread_state() {
    let (sub, got, had_gil, still_current) = in_sub_interpreter(|py| {
        let sub = interp_id(py);
        let (got, had_gil, still_current) = py.detach(|| {
            // SAFETY: the thread state is this thread's and belongs to the sub-interpreter.
            let tstate = unsafe { ffi::PyGILState_GetThisThreadState() };
            // SAFETY: `tstate` is this thread's own thread state, detached by `py.detach`.
            unsafe { ffi::PyEval_RestoreThread(tstate) };
            // SAFETY: a thread state is current on this thread and stays current for `f`.
            let (got, had_gil) = unsafe {
                Python::attach_current(|py2| {
                    // SAFETY: always safe to call; reports whether this thread holds the GIL.
                    (interp_id(py2), ffi::PyGILState_Check() == 1)
                })
            };
            // SAFETY: always safe to call; `attach_current` must not have released `tstate`.
            let still_current = unsafe { ffi::PyThreadState_GetUnchecked() } == tstate;
            // SAFETY: `tstate` is current; hand it back detached, as it was found.
            unsafe { ffi::PyEval_SaveThread() };
            (got, had_gil, still_current)
        });
        (sub, got, had_gil, still_current)
    });
    assert_ne!(sub, 0, "must run in a sub-interpreter");
    assert_eq!(
        got, sub,
        "attach_current ran in interpreter {got}, not {sub}"
    );
    assert!(had_gil, "attach_current ran without the GIL");
    assert!(
        still_current,
        "attach_current released the thread state it was given"
    );
}

/// `attach_current` drains the deferred references of its interpreter on entry, as `attach` does:
/// a decref queued while detached is applied before `f` runs.
#[test]
fn attach_current_applies_the_deferred_decrefs_of_its_interpreter() {
    let (before, during) = in_sub_interpreter(|py| {
        let obj = py.eval(c"object()", None, None).unwrap().unbind();
        let extra = obj.clone_ref(py);
        let raw = obj.as_ptr() as usize;
        // SAFETY: attached; `obj` keeps the object alive.
        let before = unsafe { ffi::Py_REFCNT(raw as *mut ffi::PyObject) };
        let during = py.detach(move || {
            drop(extra); // detached: queued, not applied
                         // SAFETY: the thread state is this thread's, detached by `py.detach`.
            let tstate = unsafe { ffi::PyGILState_GetThisThreadState() };
            // SAFETY: as above.
            unsafe { ffi::PyEval_RestoreThread(tstate) };
            // SAFETY: a thread state is current on this thread and stays current for `f`.
            let during = unsafe {
                Python::attach_current(|_| {
                    // SAFETY: `obj` keeps the object alive; the thread holds the GIL.
                    ffi::Py_REFCNT(raw as *mut ffi::PyObject)
                })
            };
            // SAFETY: `tstate` is current; hand it back detached.
            unsafe { ffi::PyEval_SaveThread() };
            during
        });
        drop(obj);
        (before, during)
    });
    assert_eq!(
        during,
        before - 1,
        "attach_current did not apply the deferred decref of its interpreter"
    );
}

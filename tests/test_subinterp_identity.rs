//! The interpreter identity methods of `InterpreterHandle` (`main`, `bound`, `is_main`, `id`),
//! `InterpreterHandle::new_thread_state`, `InterpreterHandle::try_attach` (also when its `f`
//! panics, and from a thread bound to another interpreter) and `Python::attach_current`, in real
//! own-GIL sub-interpreters.
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

/// A panic in `try_attach`'s `f` on a thread with no thread state, in a sub-interpreter: the
/// thread state made for the visit is freed and the thread is left detached, so the
/// sub-interpreter's GIL is free again (ledger #27).
#[test]
fn a_panic_in_try_attach_frees_the_visits_thread_state() {
    let (sub, panicked, attached_after, bound_after, reentered) = in_sub_interpreter(|py| {
        let handle = InterpreterHandle::current(py);
        let sub = interp_id(py);
        let (panicked, attached_after, bound_after) = py.detach(|| {
            std::thread::spawn(move || {
                let panicked = std::panic::catch_unwind(|| {
                    handle.try_attach(|_py| panic!("f panics inside the visit"))
                })
                .is_err();
                // SAFETY: always safe to call; null when no thread state is current.
                let current = unsafe { ffi::PyThreadState_GetUnchecked() };
                if !current.is_null() {
                    // What the fix does, so a red run does not leave the sub-interpreter's GIL
                    // held by a thread that has ended.
                    // SAFETY: the visit's thread state is current on this thread.
                    unsafe {
                        ffi::PyThreadState_Clear(current);
                        ffi::PyEval_SaveThread();
                        ffi::PyThreadState_Delete(current);
                    }
                }
                (panicked, !current.is_null(), InterpreterHandle::bound())
            })
            .join()
            .unwrap()
        });
        // The GIL is free again: a second visit from another thread runs.
        let reentered = py.detach(|| {
            std::thread::spawn(move || handle.try_attach(interp_id))
                .join()
                .unwrap()
        });
        (sub, panicked, attached_after, bound_after, reentered)
    });
    assert_ne!(sub, 0, "must run in a sub-interpreter");
    assert!(panicked);
    assert!(
        !attached_after,
        "a panic in f left the visiting thread attached"
    );
    assert_eq!(
        bound_after, None,
        "a panic in f left the visit's thread state on the visiting thread"
    );
    assert_eq!(reentered, Some(sub));
}

/// How many thread states `interp` has, read while attached to it (the caller's own included).
fn thread_states_of(_py: Python<'_>) -> usize {
    let mut n = 0;
    // SAFETY: attached (the token), so the current interpreter is alive and its GIL is held; the
    // list is only read.
    unsafe {
        let mut tstate = ffi::PyInterpreterState_ThreadHead(ffi::PyInterpreterState_Get());
        while !tstate.is_null() {
            n += 1;
            tstate = ffi::PyThreadState_Next(tstate);
        }
    }
    n
}

/// What a detached thread's binding looks like from the outside: its bound (`PyGILState`)
/// thread state (as an address, to compare), the interpreter that names, whether a thread state is current, and
/// `PyGILState_Check` (always 1 once a sub-interpreter exists: CPython turns the check off then,
/// so this only shows it did not change).
fn binding() -> (usize, Option<InterpreterHandle>, bool, i32) {
    // SAFETY: the three are always safe to call.
    unsafe {
        (
            ffi::PyGILState_GetThisThreadState() as usize,
            InterpreterHandle::bound(),
            !ffi::PyThreadState_GetUnchecked().is_null(),
            ffi::PyGILState_Check(),
        )
    }
}

/// A thread bound to a sub-interpreter and detached from it (inside `py.detach`, as a Python
/// function that detaches and then interrupts work in another interpreter is) visits the main
/// interpreter with `try_attach`: `f` runs there, the visit's thread state is freed, and the
/// thread is left bound to its own sub-interpreter thread state exactly as before (ledger #28).
#[test]
fn try_attach_from_a_thread_bound_to_another_interpreter_keeps_its_binding() {
    let (sub, own, before, visits, after, back_in) = in_sub_interpreter(|py| {
        let sub = interp_id(py);
        let main = InterpreterHandle::main();
        // SAFETY: always safe to call; attached here, so this is the thread's bound one.
        let own = unsafe { ffi::PyGILState_GetThisThreadState() } as usize;
        let (before, visits, after) = py.detach(|| {
            let before = binding();
            // Twice: the second visit sees the first one's thread state gone, and one that
            // follows another (an interrupting loop does that) is no different.
            let first = main.try_attach(|py2| (interp_id(py2), thread_states_of(py2)));
            let second = main.try_attach(|py2| (interp_id(py2), thread_states_of(py2)));
            (before, (first, second), binding())
        });
        // Back in the sub-interpreter, on the thread's own thread state.
        // SAFETY: attached, so a thread state is current.
        let back_in = (
            interp_id(py),
            unsafe { ffi::PyThreadState_Get() } as usize == own,
        );
        (sub, own, before, visits, after, back_in)
    });
    assert_ne!(sub, 0, "must run in a sub-interpreter");
    assert_ne!(own, 0);
    assert_eq!(
        before.0, own,
        "detached, the thread is bound to its own thread state"
    );
    let (first, second) = visits;
    let first = first.expect("try_attach could not make a thread state");
    let second = second.expect("try_attach could not make a thread state");
    assert_eq!(
        first.0, 0,
        "the visit ran in interpreter {}, not main",
        first.0
    );
    assert_eq!(
        first.1, second.1,
        "the first visit left its thread state in the main interpreter"
    );
    assert_eq!(
        after, before,
        "the visit changed the thread's binding (bound thread state, interpreter, current, check)"
    );
    assert_eq!(back_in, (sub, true));
}

/// The same visit with a panic in `f`: unwinding, the visit's thread state is freed and the
/// thread is bound to its own thread state again (ledger #28).
#[test]
fn a_panic_in_try_attach_from_a_thread_bound_to_another_interpreter_keeps_its_binding() {
    let (sub, panicked, before, after, reentered) = in_sub_interpreter(|py| {
        let sub = interp_id(py);
        let main = InterpreterHandle::main();
        let (panicked, before, after, reentered) = py.detach(|| {
            let before = binding();
            let panicked = std::panic::catch_unwind(|| {
                main.try_attach(|_py| panic!("f panics inside the visit"))
            })
            .is_err();
            let after = binding();
            // The main interpreter's GIL is free again.
            let reentered = std::thread::spawn(move || main.try_attach(interp_id))
                .join()
                .unwrap();
            (panicked, before, after, reentered)
        });
        (sub, panicked, before, after, reentered)
    });
    assert_ne!(sub, 0, "must run in a sub-interpreter");
    assert!(panicked);
    assert_eq!(
        after, before,
        "a panic in f changed the thread's binding (bound thread state, interpreter, current, check)"
    );
    assert_eq!(reentered, Some(0));
}

/// Attached to a sub-interpreter, `try_attach` to another interpreter still panics: a thread
/// cannot be attached to two at once.
#[test]
fn try_attach_while_attached_to_another_interpreter_panics() {
    let (sub, panicked) = in_sub_interpreter(|py| {
        let main = InterpreterHandle::main();
        let panicked = std::panic::catch_unwind(|| main.try_attach(|_py| ())).is_err();
        (interp_id(py), panicked)
    });
    assert_ne!(sub, 0, "must run in a sub-interpreter");
    assert!(panicked, "try_attach ran f attached to two interpreters");
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

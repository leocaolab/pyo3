//! Ledger #13 (M1.3): `Python::attach` on a thread with no Python thread state lands in the
//! extension's home interpreter, not in the main one.
//!
//! A module of this binary is executed in exactly one sub-interpreter, which makes that
//! sub-interpreter the home. Before the fix, `PyGILState_Ensure` sent such a thread to the main
//! interpreter.
//!
//! Also ledger #18 (M3.2): a `Py<T>` dropped on such a thread is queued for the home
//! interpreter, and that interpreter's teardown applies it.
#![cfg(all(feature = "macros", Py_3_12, not(Py_LIMITED_API)))]

mod subinterp_util;

use pyo3::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use subinterp_util::{in_sub_interpreter, interp_id};

static DROPPED: AtomicBool = AtomicBool::new(false);

#[pyclass]
struct DropFlag;

impl Drop for DropFlag {
    fn drop(&mut self) {
        DROPPED.store(true, Ordering::Release);
    }
}

#[pymodule]
mod home_probe {}

#[test]
fn foreign_thread_attach_lands_in_the_home_interpreter() {
    let (home, landed) = in_sub_interpreter(|py| {
        let _module = pyo3::wrap_pymodule!(home_probe)(py); // executes the module here
        let home = interp_id(py);
        // a thread that has never had a Python thread state (rayon/tokio-like)
        let landed = py.detach(|| {
            std::thread::spawn(|| Python::attach(interp_id))
                .join()
                .unwrap()
        });

        // Dropped on a thread with no thread state: queued for this (home) interpreter. Joined
        // without detaching, so nothing in this interpreter drains the queue before teardown.
        let flag = Py::new(py, DropFlag).unwrap();
        std::thread::spawn(move || drop(flag)).join().unwrap();
        assert!(
            !DROPPED.load(Ordering::Acquire),
            "a detached drop must be queued, not applied"
        );
        (home, landed)
    });
    assert_ne!(
        home, 0,
        "the module must have been executed in a sub-interpreter"
    );
    assert_eq!(
        landed, home,
        "a foreign-thread attach landed in interpreter {landed}, not home {home}"
    );
    assert!(
        DROPPED.load(Ordering::Acquire),
        "the interpreter's queued decref was not applied at its teardown"
    );
}

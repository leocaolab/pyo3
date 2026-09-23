//! One extension loaded by several interpreters ("shared"): ledger #7, #13 and #19.
//!
//! Everything is in one test because the order matters: the module is executed in the main
//! interpreter and in a sub-interpreter first, which makes the home ambiguous for the rest of
//! this process.
#![cfg(all(feature = "macros", Py_3_12, not(Py_LIMITED_API)))]

mod subinterp_util;

use pyo3::prelude::*;
use subinterp_util::in_sub_interpreter;

#[pymodule]
mod shared_probe {}

fn panic_message(e: Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_default()
}

#[test]
fn shared_extension() {
    // Ledger #7: the module object is per interpreter. Before, it was cached process-wide and a
    // guard refused every interpreter but the first (`ImportError ... pyo3#576`).
    let main_module = Python::attach(|py| {
        pyo3::wrap_pymodule!(shared_probe)(py).as_ptr() as usize
    });
    let sub_module = in_sub_interpreter(|py| {
        pyo3::wrap_pymodule!(shared_probe)(py).as_ptr() as usize
    });
    assert_ne!(sub_module, main_module, "a sub-interpreter got the main interpreter's module object");

    // Ledger #13, the ambiguous half: two interpreters executed this extension's module, so a
    // thread with no thread state cannot be sent to "home". It must fail, never land in main.
    let attached = std::thread::spawn(|| Python::try_attach(|_| ())).join().unwrap();
    assert!(attached.is_none(), "an ambiguous foreign-thread attach must fail, not land in main");
    let msg = panic_message(std::thread::spawn(|| Python::attach(|_| ())).join().unwrap_err());
    assert!(msg.contains("more than one interpreter"), "unexpected attach panic: {msg}");

    // Ledger #19 (M3.2): a Py<T> dropped on such a thread has no known owner. It panics with
    // the reason; and while the thread is already panicking it is leaked with the reason
    // printed, instead of a second panic that aborts the process.
    let obj = Python::attach(|py| py.None());
    let msg = panic_message(std::thread::spawn(move || drop(obj)).join().unwrap_err());
    assert!(msg.contains("cannot tell which interpreter owns"), "unexpected drop panic: {msg}");

    let obj = Python::attach(|py| py.None());
    let unwound = std::thread::spawn(move || {
        let _held = obj;
        panic!("an unrelated failure while holding a Py<T>");
    })
    .join();
    // reaching this line means the process was not aborted
    assert!(panic_message(unwound.unwrap_err()).contains("unrelated failure"));
}

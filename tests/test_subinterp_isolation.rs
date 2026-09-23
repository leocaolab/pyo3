//! Per-interpreter isolation regressions (SUBINTERP-FIXES.md, "Bug ledger").
//!
//! Each test compares what one interpreter caches with what a sub-interpreter sees, so each one
//! fails on the code before its fix: there, the sub-interpreter gets the other interpreter's
//! object.
//!
//! These live in their own process, not in the lib's unit tests: once a process has created a
//! sub-interpreter, CPython disables `PyGILState_Check` for good (it always returns 1), which
//! breaks the upstream unit test `test_acquire_gil`. No module is executed here, so this
//! binary's home interpreter stays unset.
#![cfg(all(feature = "macros", Py_3_12, not(Py_LIMITED_API)))]

mod subinterp_util;

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::{PerInterpreterCell, PyOnceLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use subinterp_util::{in_sub_interpreter, in_sub_interpreters};

/// Address of a Python object, so it can leave the interpreter it belongs to.
fn addr(obj: &Bound<'_, PyAny>) -> usize {
    obj.as_ptr() as usize
}

// ---------------------------------------------------------------------------------------------
// Ledger #15 (M2.2), #14 (M2.1), #12, #1, #4: caches are per interpreter.

/// Ledger #15: a `static PyOnceLock` holds one value per interpreter. Before, a sub-interpreter
/// got the main interpreter's value (polars bug B: another interpreter's `Int64`; bug C: a
/// segfault once that interpreter was gone).
#[test]
fn once_lock_value_is_per_interpreter() {
    static SYS: PyOnceLock<Py<PyModule>> = PyOnceLock::new();

    let main_sys = Python::attach(|py| {
        addr(
            SYS.get_or_init(py, || py.import("sys").unwrap().unbind())
                .bind(py)
                .as_any(),
        )
    });
    let (seen_before_init, cached, own) = in_sub_interpreter(|py| {
        let before = SYS.get(py).is_some();
        let cached = addr(
            SYS.get_or_init(py, || py.import("sys").unwrap().unbind())
                .bind(py)
                .as_any(),
        );
        (before, cached, addr(py.import("sys").unwrap().as_any()))
    });
    assert!(
        !seen_before_init,
        "a sub-interpreter saw the main interpreter's PyOnceLock value"
    );
    assert_eq!(
        cached, own,
        "PyOnceLock must hold this interpreter's own object"
    );
    assert_ne!(cached, main_sys);
}

/// Ledger #14: `intern!` gives each interpreter its own interned string. Interned strings are
/// mortal on 3.12+, so sharing one means sharing a live refcount.
#[test]
fn intern_is_per_interpreter() {
    fn interned(py: Python<'_>) -> usize {
        pyo3::intern!(py, "pyo3_subinterp_regression_interned").as_ptr() as usize
    }
    fn own(py: Python<'_>) -> usize {
        // this interpreter's own interned object for the same text, built at run time
        let text = String::from("pyo3_subinterp_regression_") + "interned";
        addr(
            &py.import("sys")
                .unwrap()
                .getattr("intern")
                .unwrap()
                .call1((text,))
                .unwrap(),
        )
    }
    let main = Python::attach(|py| (interned(py), own(py)));
    assert_eq!(main.0, main.1);
    let sub = in_sub_interpreter(|py| (interned(py), own(py)));
    assert_eq!(
        sub.0, sub.1,
        "intern! must return this interpreter's own interned str"
    );
    assert_ne!(sub.0, main.0);
}

/// Ledger #12: Python-level classes cached by the conversion layer (here `pathlib.Path`) are per
/// interpreter. The main interpreter converts first, so a process-wide cache would be filled.
#[test]
fn conversion_class_cache_is_per_interpreter() {
    fn converted_is_own_path(py: Python<'_>) -> bool {
        let obj = std::path::PathBuf::from("/tmp/x")
            .into_pyobject(py)
            .unwrap();
        let own = py
            .import("pathlib")
            .unwrap()
            .getattr("Path")
            .unwrap()
            .call1(("/tmp/x",))
            .unwrap();
        obj.get_type().is(own.get_type())
    }
    assert!(Python::attach(converted_is_own_path));
    assert!(
        in_sub_interpreter(converted_is_own_path),
        "PathBuf must convert to this interpreter's own pathlib.Path"
    );
}

#[pyclass]
struct RegressionProbe;

pyo3::create_exception!(
    subinterp_regression,
    RegressionError,
    pyo3::exceptions::PyException
);

/// Ledger #1: a `#[pyclass]` type object is per interpreter. It is a heap type, so sharing one
/// races its refcount across GILs.
#[test]
fn pyclass_type_object_is_per_interpreter() {
    let main = Python::attach(|py| addr(py.get_type::<RegressionProbe>().as_any()));
    let sub = in_sub_interpreter(|py| addr(py.get_type::<RegressionProbe>().as_any()));
    assert_ne!(
        sub, main,
        "a sub-interpreter got the main interpreter's #[pyclass] type"
    );
}

/// Ledger #4: a `create_exception!` type is per interpreter.
#[test]
fn created_exception_type_is_per_interpreter() {
    let main = Python::attach(|py| addr(py.get_type::<RegressionError>().as_any()));
    let sub = in_sub_interpreter(|py| addr(py.get_type::<RegressionError>().as_any()));
    assert_ne!(
        sub, main,
        "a sub-interpreter got the main interpreter's exception type"
    );
}

// ---------------------------------------------------------------------------------------------
// Ledger #16: `LazyTypeObject.initializing_threads` is per interpreter.
//
// Interpreter A starts filling `SlowInit`'s `tp_dict` and blocks inside the class attribute.
// Interpreter B then initializes the same class to completion, which clears its "initializing"
// list. A then asks for the type again, re-entrantly, as an enum variant's `into_pyobject`
// does. With a process-wide list, B's clear removed A's entry, so A lost its reentrancy guard
// and ran the class attribute a second time (in polars that re-entered a PyOnceLock and
// deadlocked). With a per-interpreter list it does not.

static ROLE: AtomicUsize = AtomicUsize::new(0);
static A_STARTED: AtomicBool = AtomicBool::new(false);
static B_DONE: AtomicBool = AtomicBool::new(false);
static A_REENTERED: AtomicBool = AtomicBool::new(false);
static TIMED_OUT: AtomicBool = AtomicBool::new(false);
thread_local! {
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn wait_for(flag: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !flag.load(Ordering::Acquire) {
        if Instant::now() > deadline {
            TIMED_OUT.store(true, Ordering::Release);
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[pyclass]
struct SlowInit;

#[pymethods]
impl SlowInit {
    #[classattr]
    fn marker(py: Python<'_>) -> u32 {
        let depth = DEPTH.with(|d| {
            d.set(d.get() + 1);
            d.get()
        });
        if depth > 1 {
            A_REENTERED.store(true, Ordering::Release);
        } else if ROLE.fetch_add(1, Ordering::AcqRel) == 0 {
            // interpreter A: let B initialize fully, then re-request the type
            A_STARTED.store(true, Ordering::Release);
            wait_for(&B_DONE);
            let _ = py.get_type::<SlowInit>();
        }
        DEPTH.with(|d| d.set(d.get() - 1));
        1
    }
}

#[test]
fn initializing_threads_is_per_interpreter() {
    in_sub_interpreters(2, |i, py| {
        if i == 1 {
            wait_for(&A_STARTED);
            let _ = py.get_type::<SlowInit>();
            B_DONE.store(true, Ordering::Release);
        } else {
            let _ = py.get_type::<SlowInit>();
        }
    });
    assert!(
        !TIMED_OUT.load(Ordering::Acquire),
        "the two interpreters never interleaved"
    );
    assert!(
        !A_REENTERED.load(Ordering::Acquire),
        "another interpreter's finished init cleared this interpreter's reentrancy guard"
    );
}

// ---------------------------------------------------------------------------------------------
// Ledger #18 (M3.2, bug A): a deferred decref is applied only by its own interpreter.

/// Before, the pool was process-wide, and an attach anywhere (here: the main interpreter, while
/// the sub-interpreter is still detached) applied it.
#[test]
fn deferred_decref_is_applied_by_its_own_interpreter_only() {
    use std::sync::mpsc;

    let (dropped_tx, dropped_rx) = mpsc::channel::<()>();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    std::thread::scope(|scope| {
        // another thread attaches to the main interpreter while the sub-interpreter is detached
        // with a decref queued
        let main_attach = scope.spawn(move || {
            dropped_rx.recv().unwrap();
            Python::attach(|_| ());
            go_tx.send(()).unwrap();
        });
        let (before, during) = in_sub_interpreter(move |py| {
            let obj = py.eval(c"object()", None, None).unwrap().unbind();
            let extra = obj.clone_ref(py);
            let raw = obj.as_ptr() as usize;
            // SAFETY: attached; `obj` keeps the object alive.
            let before = unsafe { ffi::Py_REFCNT(raw as *mut ffi::PyObject) };
            let during = py.detach(move || {
                drop(extra); // detached: queued, not applied
                dropped_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                // SAFETY: a racy read of a live object's refcount (`obj` keeps it alive); nothing
                // in this interpreter runs while this thread is detached from it.
                unsafe { ffi::Py_REFCNT(raw as *mut ffi::PyObject) }
            });
            drop(obj);
            (before, during)
        });
        main_attach.join().unwrap();
        assert_eq!(
            during, before,
            "another interpreter applied this interpreter's deferred decref"
        );
    });
}

// ---------------------------------------------------------------------------------------------
// Ledger #2, #5, #6: the per-interpreter registry. Moved from `src/sync/per_interpreter.rs`
// unit tests unchanged in what they assert.

/// The key `PerInterpreterCell` keeps this copy's registry under in the interpreter dict:
/// `_pyo3_per_interpreter.<tag>`, where the tag is per extension copy (`keys()` in
/// `src/sync/per_interpreter.rs`). This test binary is one copy, so there is exactly one.
///
/// # Safety
/// The caller holds the GIL of the interpreter that owns `dict`.
unsafe fn registry_key(dict: *mut ffi::PyObject) -> std::ffi::CString {
    let mut found = Vec::new();
    let (mut pos, mut k, mut v) = (0, std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: the caller holds the GIL; `dict` is a dict.
    while unsafe { ffi::PyDict_Next(dict, &mut pos, &mut k, &mut v) } != 0 {
        // SAFETY: as above; a non-str key yields NULL, which is skipped.
        let name = unsafe { ffi::PyUnicode_AsUTF8(k) };
        if name.is_null() {
            // SAFETY: the caller holds the GIL.
            unsafe { ffi::PyErr_Clear() };
            continue;
        }
        // SAFETY: CPython returns a NUL-terminated UTF-8 buffer owned by the key.
        let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_owned();
        let text = name.to_string_lossy();
        if let Some(tag) = text.strip_prefix("_pyo3_per_interpreter.") {
            if !tag.contains('.') {
                found.push(name);
            }
        }
    }
    assert_eq!(
        found.len(),
        1,
        "expected exactly one registry key, found {found:?}"
    );
    found.pop().unwrap()
}

/// Drops this interpreter's registry the way CPython does at finalization, **including the part
/// that matters**: from a thread PyO3 has never attached.
///
/// Deleting the key from inside `Python::attach` exercises the same code but not the same
/// condition: there PyO3's attach count is already non-zero, so `Py<T>`'s `Drop` decrefs whether
/// or not the guard exists, and the test passes against the bug. The capsule destructor and the
/// `atexit` hook are both entered from CPython directly; this reproduces that by taking the GIL
/// through the raw C API on a fresh thread, where PyO3's thread-local count is zero.
///
/// The caller is attached, so the GIL has to be handed over for the duration, and through the
/// raw C API rather than `Python::detach`: detaching and re-attaching through PyO3 applies the
/// deferred reference pool on the way back in, which would settle the very decrefs this is here
/// to catch and turn the test green against the bug.
fn drop_registry_as_cpython_would(_py: Python<'_>) {
    // SAFETY: the caller is attached, so there is a current interpreter and thread state.
    let interp = unsafe { ffi::PyInterpreterState_Get() } as usize;
    // SAFETY: as above; no Python object is touched until the thread state is restored below.
    let tstate = unsafe { ffi::PyEval_SaveThread() };
    // SAFETY: a fresh thread state for `interp`, which stays alive: its only other thread is
    // the caller, blocked in `join` below.
    let joined = std::thread::spawn(move || unsafe {
        let ts = ffi::PyThreadState_New(interp as *mut ffi::PyInterpreterState);
        assert!(!ts.is_null());
        ffi::PyEval_RestoreThread(ts);
        let dict = ffi::PyInterpreterState_GetDict(ffi::PyInterpreterState_Get());
        assert!(!dict.is_null());
        let key = registry_key(dict);
        assert!(
            !ffi::PyDict_GetItemString(dict, key.as_ptr()).is_null(),
            "no registry under {key:?}"
        );
        if ffi::PyDict_DelItemString(dict, key.as_ptr()) < 0 {
            ffi::PyErr_Clear();
        }
        ffi::PyThreadState_Clear(ts);
        let _ = ffi::PyEval_SaveThread();
        ffi::PyThreadState_Delete(ts);
    })
    .join();
    // runs whether or not the thread panicked, so the caller's `Python` token is valid again
    // SAFETY: pairs with the `PyEval_SaveThread` above.
    unsafe { ffi::PyEval_RestoreThread(tstate) };
    joined.unwrap();
}

fn refcount(_py: Python<'_>, obj: &Py<PyAny>) -> isize {
    // SAFETY: `obj` is alive and the caller is attached.
    unsafe { ffi::Py_REFCNT(obj.as_ptr()) }
}

/// Ledger #5: a value stored in a cell must actually be released when the registry goes away.
///
/// This is the regression that cost 2.6 MB per interpreter, without bound. `Py<T>`'s `Drop`
/// only decrefs when PyO3's own thread-local attach count is above zero; otherwise it hands the
/// reference to a pool. CPython reaches the registry's drop through a capsule destructor without
/// going through PyO3, so the count was zero and every decref was deferred into a pool that
/// nothing ever applied: the drop ran and the refcount did not move.
#[test]
fn registry_teardown_releases_its_values() {
    in_sub_interpreter(|py| {
        static CELL: PerInterpreterCell<Py<PyAny>> = PerInterpreterCell::new();

        // A fresh Python class: a heap type with an ordinary refcount, so a missing decref is
        // visible. A builtin would be immortal and its refcount would not move either way.
        let class: Py<PyAny> = py
            .eval(c"type('PerInterpreterProbe', (), {})", None, None)
            .unwrap()
            .unbind();

        let before = refcount(py, &class);
        CELL.get_or_init(py, || class.clone_ref(py));
        assert_eq!(
            refcount(py, &class),
            before + 1,
            "storing a value should hold one reference"
        );

        drop_registry_as_cpython_would(py);
        assert_eq!(
            refcount(py, &class),
            before,
            "the registry's reference must be released, not deferred into the reference pool"
        );
    });
}

/// Ledger #6: after teardown the cell must read as empty rather than hand back a dangling
/// pointer.
///
/// A stale cached base pointer reads freed memory, and whether that still shows the old value
/// depends on the allocator. A small registry is a small allocation, whose first bytes the
/// allocator reuses at once, so the stale read comes back empty and the test would pass against
/// the bug. 512 other cells are claimed first to make the registry large, as it is in any real
/// extension (and was in the lib's unit tests, where this test used to live).
#[test]
fn cell_is_empty_after_teardown() {
    in_sub_interpreter(|py| {
        for _ in 0..512 {
            let pad: &'static PerInterpreterCell<u64> =
                Box::leak(Box::new(PerInterpreterCell::new()));
            pad.get_or_init(py, || 0);
        }
        static CELL: PerInterpreterCell<Py<PyAny>> = PerInterpreterCell::new();
        let value: Py<PyAny> = py.None();
        CELL.get_or_init(py, || value.clone_ref(py));
        assert!(CELL.get(py).is_some());

        drop_registry_as_cpython_would(py);
        assert!(
            CELL.get(py).is_none(),
            "a stale base pointer must not survive the registry it pointed into"
        );
    });
}

/// Ledger #2: two cells declared next to each other must not share storage.
///
/// They are zero-sized but for the index field, and two zero-sized fields of one struct can
/// share an address, which made both cells claim the same slot, overwrite each other, and abort
/// all 850 tests with a null type pointer.
#[test]
fn neighbouring_cells_have_distinct_slots() {
    in_sub_interpreter(|py| {
        struct Pair {
            a: PerInterpreterCell<Py<PyAny>>,
            b: PerInterpreterCell<Py<PyAny>>,
        }
        static PAIR: Pair = Pair {
            a: PerInterpreterCell::new(),
            b: PerInterpreterCell::new(),
        };

        let first: Py<PyAny> = py.eval(c"'first'", None, None).unwrap().unbind();
        let second: Py<PyAny> = py.eval(c"'second'", None, None).unwrap().unbind();
        PAIR.a.get_or_init(py, || first.clone_ref(py));
        PAIR.b.get_or_init(py, || second.clone_ref(py));

        assert!(PAIR.a.get(py).unwrap().bind(py).eq("first").unwrap());
        assert!(PAIR.b.get(py).unwrap().bind(py).eq("second").unwrap());
    });
}


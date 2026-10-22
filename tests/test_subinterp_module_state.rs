//! PEP 489 module state: each interpreter's module object holds its own state, dropped with the
//! module (ledger #29).
#![cfg(all(feature = "macros", Py_3_12, not(Py_LIMITED_API), not(Py_3_15)))]

mod subinterp_util;

use std::sync::atomic::{AtomicUsize, Ordering};

use pyo3::prelude::*;
use pyo3::types::PyModule;
use subinterp_util::{in_sub_interpreters, interp_id};

#[pymodule]
mod state_probe {}

/// What each interpreter keeps: which interpreter set it. Dropping one counts it.
struct Assignment {
    interpreter: i64,
}

static DROPPED: AtomicUsize = AtomicUsize::new(0);

impl Drop for Assignment {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn each_interpreter_s_module_has_its_own_state_and_drops_it_with_the_module() {
    let seen = in_sub_interpreters(3, |_, py| {
        let module = pyo3::wrap_pymodule!(state_probe)(py).into_bound(py);
        let module = module.cast::<PyModule>().unwrap();
        let mine = interp_id(py);
        assert!(
            module.state::<Assignment>().is_err(),
            "a new module has no state yet"
        );
        module.set_state(Assignment { interpreter: mine }).unwrap();
        // The same module object, found again, holds what this interpreter set.
        let again = pyo3::wrap_pymodule!(state_probe)(py).into_bound(py);
        let again = again.cast::<PyModule>().unwrap();
        let read = again.state::<Assignment>().unwrap().interpreter;
        // Set once: a second state is refused and the first stays.
        assert!(module.set_state(Assignment { interpreter: -1 }).is_err());
        assert!(
            module.state::<String>().is_err(),
            "a state of another type is not handed out"
        );
        (mine, read, again.state::<Assignment>().unwrap().interpreter)
    });
    for (mine, read, after_refusal) in &seen {
        assert_eq!((read, after_refusal), (mine, mine));
    }
    let ids: std::collections::HashSet<i64> = seen.iter().map(|(mine, _, _)| *mine).collect();
    assert_eq!(ids.len(), 3, "three interpreters, three states");
    // Each interpreter's end frees its module, and the module frees its state; the refused
    // second state was dropped on the spot.
    assert_eq!(DROPPED.load(Ordering::SeqCst), 6);
}

#[test]
fn a_module_pyo3_did_not_make_has_no_state_slot() {
    Python::attach(|py| {
        let sys = py.import("sys").unwrap();
        assert!(sys.set_state(String::from("not a PyO3 module")).is_err());
        assert!(sys.state::<String>().is_err());
    });
}

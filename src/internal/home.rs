//! The home interpreter of this extension copy (SUBINTERP-FIXES.md M1.3, #4).
//!
//! `Python::attach` on a thread that has never had a Python thread state goes
//! through `PyGILState_Ensure`, which CPython binds to the **main** interpreter.
//! For a rayon / tokio / `std::thread` worker spawned by an extension that runs
//! in a sub-interpreter, that is the wrong interpreter: the callback then touches
//! the sub-interpreter's objects under the main interpreter's GIL. Measured in
//! `subinterp-bench/attach_probe.py`: a fresh thread lands in main in every
//! configuration, upstream and this branch, shared or copied.
//!
//! Every extension `.so` carries its own copy of PyO3's statics. So this module
//! records, per copy, the interpreter that executed its modules:
//!
//! - exactly one interpreter → a foreign-thread attach goes to that interpreter;
//! - more than one → PyO3 cannot tell which one the caller means, so the attach
//!   fails loudly instead of silently landing in main (use
//!   `pyo3::sync::InterpreterHandle` for that case);
//! - the home interpreter was finalized → fails loudly (never a dangling pointer).
//!
//! Threads that already have a Python thread state are not affected: a re-attach
//! after `py.detach` on the same thread was measured correct everywhere.

use crate::ffi;
use std::sync::Mutex;

/// Where a foreign-thread attach should go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForeignTarget {
    /// No home recorded, or the home is the main interpreter: the old behaviour
    /// (`PyGILState_Ensure`) is correct.
    Gilstate,
    /// Attach to this interpreter with a fresh thread state.
    Home(*mut ffi::PyInterpreterState),
    /// More than one interpreter executed a module of this copy.
    Ambiguous,
    /// The only interpreter that executed a module of this copy was finalized.
    HomeGone,
}

// Flags.
const HOME_IS_MAIN: u8 = 1;
const MULTI: u8 = 2;
const HOME_GONE: u8 = 4;

/// `(home interpreter as usize, flags)`. Written on module exec and interpreter
/// teardown, read on foreign-thread attach; all three are rare or already slow
/// (a foreign attach creates a thread state), so one lock keeps the pair
/// consistent without a clever lock-free encoding.
static STATE: Mutex<(usize, u8)> = Mutex::new((0, 0));

/// The pure state transition for a module exec by `interp`, so it can be tested
/// without real interpreters. Returns the new `(home, flags)`.
fn on_exec(
    home: *mut ffi::PyInterpreterState,
    flags: u8,
    interp: *mut ffi::PyInterpreterState,
    interp_is_main: bool,
) -> (*mut ffi::PyInterpreterState, u8) {
    if home.is_null() {
        // First interpreter, or the first after the previous home was finalized:
        // nothing of the old one survives it, so taking over is safe.
        let flags = (flags & MULTI) | if interp_is_main { HOME_IS_MAIN } else { 0 };
        (interp, flags)
    } else if home == interp {
        (home, flags)
    } else {
        (home, flags | MULTI)
    }
}

/// The pure decision for a foreign-thread attach.
fn target(home: *mut ffi::PyInterpreterState, flags: u8) -> ForeignTarget {
    if flags & MULTI != 0 {
        ForeignTarget::Ambiguous
    } else if home.is_null() {
        if flags & HOME_GONE != 0 {
            ForeignTarget::HomeGone
        } else {
            ForeignTarget::Gilstate
        }
    } else if flags & HOME_IS_MAIN != 0 {
        ForeignTarget::Gilstate
    } else {
        ForeignTarget::Home(home)
    }
}

fn lock() -> std::sync::MutexGuard<'static, (usize, u8)> {
    // A panic while holding this lock cannot leave the pair half-written (both
    // fields are assigned together), so a poisoned lock is still consistent.
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Records that the current interpreter executed a module of this copy.
///
/// # Safety
/// The calling thread must be attached to an interpreter.
pub(crate) unsafe fn note_module_exec() {
    // SAFETY: the caller is attached.
    let interp = unsafe { ffi::PyInterpreterState_Get() };
    // SAFETY: `interp` is the live current interpreter.
    let is_main = unsafe { ffi::PyInterpreterState_GetID(interp) } == 0;
    let mut st = lock();
    let (home, flags) = on_exec(st.0 as *mut _, st.1, interp, is_main);
    *st = (home as usize, flags);
}

/// Called from the per-interpreter teardown hook: forget `interp` if it is home.
pub(crate) fn on_interpreter_teardown(interp: *mut ffi::PyInterpreterState) {
    let mut st = lock();
    if st.0 == interp as usize {
        *st = (0, (st.1 & MULTI) | HOME_GONE);
    }
}

/// Where a `Python::attach` from a thread with no Python thread state should go.
pub(crate) fn foreign_target() -> ForeignTarget {
    let st = lock();
    target(st.0 as *mut _, st.1)
}

/// Attach this thread (which has no thread state) to `interp` with a fresh
/// thread state. Pair with [`detach_home`].
///
/// # Safety
/// `interp` must be alive, and this thread must have no current thread state.
pub(crate) unsafe fn attach_home(interp: *mut ffi::PyInterpreterState) -> *mut ffi::PyThreadState {
    // SAFETY: caller guarantees `interp` is alive; no GIL needed for New.
    let tstate = unsafe { ffi::PyThreadState_New(interp) };
    assert!(!tstate.is_null(), "PyThreadState_New returned null");
    // SAFETY: `tstate` is new and not current anywhere.
    unsafe { ffi::PyEval_RestoreThread(tstate) };
    tstate
}

/// Undo [`attach_home`]. Same order as `InterpreterHandle::attach`:
/// Clear (GIL held, current) → Save (detach + release) → Delete (not current).
///
/// # Safety
/// `tstate` must be the current thread state, created by [`attach_home`].
pub(crate) unsafe fn detach_home(tstate: *mut ffi::PyThreadState) {
    unsafe {
        ffi::PyThreadState_Clear(tstate);
        let detached = ffi::PyEval_SaveThread();
        debug_assert_eq!(detached, tstate);
        ffi::PyThreadState_Delete(tstate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr;

    fn p(n: usize) -> *mut ffi::PyInterpreterState {
        n as *mut ffi::PyInterpreterState
    }

    #[test]
    fn no_exec_keeps_old_behaviour() {
        assert_eq!(target(ptr::null_mut(), 0), ForeignTarget::Gilstate);
    }

    #[test]
    fn single_sub_interpreter_is_home() {
        let (h, f) = on_exec(ptr::null_mut(), 0, p(0x10), false);
        assert_eq!(target(h, f), ForeignTarget::Home(p(0x10)));
        // re-exec by the same interpreter changes nothing
        let (h, f) = on_exec(h, f, p(0x10), false);
        assert_eq!(target(h, f), ForeignTarget::Home(p(0x10)));
    }

    #[test]
    fn main_only_keeps_old_behaviour() {
        let (h, f) = on_exec(ptr::null_mut(), 0, p(0x20), true);
        assert_eq!(target(h, f), ForeignTarget::Gilstate);
    }

    #[test]
    fn second_interpreter_is_ambiguous() {
        let (h, f) = on_exec(ptr::null_mut(), 0, p(0x10), false);
        let (h, f) = on_exec(h, f, p(0x30), false);
        assert_eq!(target(h, f), ForeignTarget::Ambiguous);
        // main + sub is ambiguous too
        let (h, f) = on_exec(ptr::null_mut(), 0, p(0x20), true);
        let (h, f) = on_exec(h, f, p(0x10), false);
        assert_eq!(target(h, f), ForeignTarget::Ambiguous);
    }

    #[test]
    fn finalized_home_is_loud_then_rehomes() {
        // home finalized: pointer cleared, GONE set
        let (h, f) = on_exec(ptr::null_mut(), 0, p(0x10), false);
        let (h, f) = (
            if h == p(0x10) { ptr::null_mut() } else { h },
            (f & MULTI) | HOME_GONE,
        );
        assert_eq!(target(h, f), ForeignTarget::HomeGone);
        // a later interpreter re-homes the copy (nothing of the old one survives)
        let (h, f) = on_exec(h, f & !HOME_GONE, p(0x40), false);
        assert_eq!(target(h, f), ForeignTarget::Home(p(0x40)));
    }
}

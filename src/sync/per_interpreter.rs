//! Per-interpreter storage for values that must not be shared across sub-interpreters.
//!
//! # Why this exists
//!
//! [`PyOnceLock`][crate::sync::PyOnceLock] and the older `GILOnceCell` store a single value
//! per *process*. That is correct while PyO3 refuses to load into sub-interpreters, but it is
//! exactly what makes that refusal necessary: a `#[pyclass]` type object cached in a
//! process-global slot ends up shared by every interpreter, and a heap type object is *not*
//! immortal — its reference count is then mutated concurrently by interpreters that each hold
//! their own GIL.
//!
//! This cell stores one value per interpreter instead, in the dictionary returned by
//! `PyInterpreterState_GetDict`. CPython owns that dictionary and clears it when the
//! interpreter is finalized, so entries do not outlive the interpreter that created them and
//! there is no teardown hook to register.
//!
//! # Storage layout
//!
//! The interpreter dict is a plain `dict` shared with everything else that uses this API, so
//! all PyO3 entries live under one private sub-dict keyed by [`REGISTRY_KEY`]. Within that
//! sub-dict, each cell is keyed by its own address — every `PerInterpreterCell` is a `static`,
//! so its address is stable for the life of the process and unique among cells.
//!
//! Values are boxed and handed to a `PyCapsule`, whose destructor drops the box. That keeps the
//! API generic over `T` rather than restricting it to types that are already `PyObject`s.

use crate::ffi;
use crate::Python;
use alloc::boxed::Box;
use core::ffi::{c_void, CStr};

/// Key under which PyO3's per-interpreter registry is stored in the interpreter dict.
const REGISTRY_KEY: &CStr = c"_pyo3_per_interpreter";

/// Capsule name. CPython compares this pointer-or-string when unwrapping, so it must be stable.
const CAPSULE_NAME: &CStr = c"pyo3.per_interpreter.cell";

/// A cell holding at most one value *per interpreter*.
///
/// Unlike `PyOnceLock`, a value written by one interpreter is invisible to every other
/// interpreter, and is destroyed when the interpreter that created it is finalized.
pub struct PerInterpreterCell<T> {
    _marker: core::marker::PhantomData<fn() -> T>,
}

impl<T> Default for PerInterpreterCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> PerInterpreterCell<T> {
    /// Creates a cell with no value in any interpreter.
    pub const fn new() -> Self {
        PerInterpreterCell {
            _marker: core::marker::PhantomData,
        }
    }

    /// Returns this cell's identity key: its own address.
    ///
    /// Cells are `static`s, so the address is stable and distinct per cell.
    fn key(&self) -> usize {
        self as *const Self as usize
    }

    /// Returns PyO3's registry sub-dict for the current interpreter, creating it if absent.
    ///
    /// Returns a *borrowed* pointer owned by the interpreter dict; the caller must not decref it.
    ///
    /// # Safety
    /// Requires the GIL of the current interpreter, which `Python<'_>` witnesses.
    unsafe fn registry(_py: Python<'_>) -> *mut ffi::PyObject {
        let interp_dict = ffi::PyInterpreterState_GetDict(ffi::PyInterpreterState_Get());
        if interp_dict.is_null() {
            // Only happens if the interpreter is already finalizing.
            return core::ptr::null_mut();
        }
        let existing = ffi::PyDict_GetItemString(interp_dict, REGISTRY_KEY.as_ptr());
        if !existing.is_null() {
            return existing; // borrowed
        }
        let fresh = ffi::PyDict_New();
        if fresh.is_null() {
            return core::ptr::null_mut();
        }
        // The interpreter dict takes its own reference; drop ours so the dict is the sole owner.
        let rc = ffi::PyDict_SetItemString(interp_dict, REGISTRY_KEY.as_ptr(), fresh);
        ffi::Py_DECREF(fresh);
        if rc < 0 {
            ffi::PyErr_Clear();
            return core::ptr::null_mut();
        }
        fresh // borrowed
    }

    /// Returns a reference to this interpreter's value, if it has been initialized here.
    pub fn get(&self, py: Python<'_>) -> Option<&T> {
        unsafe {
            let registry = Self::registry(py);
            if registry.is_null() {
                return None;
            }
            let key = ffi::PyLong_FromSize_t(self.key());
            if key.is_null() {
                ffi::PyErr_Clear();
                return None;
            }
            let capsule = ffi::PyDict_GetItem(registry, key); // borrowed
            ffi::Py_DECREF(key);
            if capsule.is_null() {
                return None;
            }
            let raw = ffi::PyCapsule_GetPointer(capsule, CAPSULE_NAME.as_ptr()) as *const T;
            if raw.is_null() {
                ffi::PyErr_Clear();
                return None;
            }
            // The capsule keeps the box alive for as long as this interpreter's registry does,
            // which outlives any `Python<'_>` token the caller can hold.
            Some(&*raw)
        }
    }

    /// Returns this interpreter's value, initializing it with `f` if this interpreter has not
    /// initialized it yet.
    ///
    /// `f` may run more than once if two threads of the *same* interpreter race; the loser's
    /// value is dropped. It never observes a value created by a *different* interpreter.
    pub fn get_or_init<F>(&self, py: Python<'_>, f: F) -> &T
    where
        F: FnOnce() -> T,
    {
        match self.get_or_try_init(py, || Ok::<T, core::convert::Infallible>(f())) {
            Ok(v) => v,
            Err(never) => match never {},
        }
    }

    /// Fallible [`get_or_init`][Self::get_or_init].
    pub fn get_or_try_init<F, E>(&self, py: Python<'_>, f: F) -> Result<&T, E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        if let Some(v) = self.get(py) {
            return Ok(v);
        }
        let value = f()?;
        Ok(self.set_and_get(py, value))
    }

    /// Stores `value` for this interpreter and returns a reference to whatever is stored
    /// afterwards — which may be a value another thread of this interpreter installed first.
    fn set_and_get(&self, py: Python<'_>, value: T) -> &T {
        unsafe {
            let boxed = Box::into_raw(Box::new(value));
            let capsule =
                ffi::PyCapsule_New(boxed as *mut c_void, CAPSULE_NAME.as_ptr(), Some(destructor::<T>));
            if capsule.is_null() {
                ffi::PyErr_Clear();
                // Cannot store it; hand back the leaked box rather than returning a dangling
                // reference. This only happens under memory exhaustion.
                return &*boxed;
            }
            let registry = Self::registry(py);
            let key = if registry.is_null() {
                core::ptr::null_mut()
            } else {
                ffi::PyLong_FromSize_t(self.key())
            };
            if registry.is_null() || key.is_null() {
                ffi::PyErr_Clear();
                ffi::Py_DECREF(capsule);
                return &*boxed; // leaked, same rationale as above
            }
            // Another thread of this interpreter may have won the race; keep the existing value.
            if let Some(existing) = self.get(py) {
                ffi::Py_DECREF(key);
                ffi::Py_DECREF(capsule); // drops our box via the destructor
                return existing;
            }
            let rc = ffi::PyDict_SetItem(registry, key, capsule);
            ffi::Py_DECREF(key);
            ffi::Py_DECREF(capsule); // the registry holds the surviving reference
            if rc < 0 {
                ffi::PyErr_Clear();
            }
            &*boxed
        }
    }
}

/// Drops the boxed value when CPython destroys the capsule, i.e. when the owning interpreter's
/// dict is cleared during finalization.
unsafe extern "C" fn destructor<T>(capsule: *mut ffi::PyObject) {
    let raw = ffi::PyCapsule_GetPointer(capsule, CAPSULE_NAME.as_ptr()) as *mut T;
    if raw.is_null() {
        ffi::PyErr_Clear();
        return;
    }
    drop(Box::from_raw(raw));
}

// SAFETY: every access goes through a `Python<'_>` token, so it is serialised by the GIL of the
// interpreter that owns the value, and values are never handed across interpreters.
unsafe impl<T: Send> Send for PerInterpreterCell<T> {}
unsafe impl<T: Send> Sync for PerInterpreterCell<T> {}

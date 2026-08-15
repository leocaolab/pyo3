//! Per-interpreter storage for values that must not be shared across sub-interpreters.
//!
//! # Why this exists
//!
//! [`PyOnceLock`][crate::sync::PyOnceLock] stores a single value per *process*. That is what makes
//! PyO3's refusal to load into sub-interpreters necessary: a `#[pyclass]` type object cached in a
//! process-global slot ends up shared by every interpreter, and a heap type object is *not*
//! immortal — its reference count is then mutated concurrently by interpreters that each hold
//! their own GIL. Measured on 0.29.2: eight own-GIL sub-interpreters observe one type object whose
//! refcount rises 8 → 10 → 12 → 14, and touching it concurrently segfaults.
//!
//! # Layout
//!
//! Each cell claims a small index from a process-wide counter the first time it is used. Every
//! interpreter owns one flat `Vec` of erased pointers indexed by that number, so a lookup is an
//! offset rather than a hash. The vector lives in a `PyCapsule` in that interpreter's dict, which
//! CPython clears at finalization, so values cannot outlive the interpreter that created them.
//!
//! # Fast path
//!
//! Reaching the vector through the interpreter dict on every read costs two dict lookups, and the
//! obvious key — the cell's address — is not a `PyObject`, so it has to be boxed into a `PyLong`
//! first. That measured at +45 ns per `#[pyclass]` instantiation.
//!
//! Instead each *thread* caches the base pointer of the vector belonging to the interpreter it is
//! attached to, tagged with that interpreter's id. A thread may attach to a different interpreter
//! at any time — `Interpreter.exec` does exactly that — so the tag is checked on every read and is
//! not optional. There is one such cache for the whole crate rather than one per cell, and being
//! thread-local it neither contends nor thrashes when N workers each drive their own interpreter.

use crate::ffi;
use crate::internal::state::AssumeAttached;
use crate::Python;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::Cell;
use core::ffi::{c_void, CStr};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Key under which an interpreter's registry lives in its interpreter dict.
const REGISTRY_KEY: &CStr = c"_pyo3_per_interpreter";

/// Capsule name for the registry. CPython compares this when unwrapping, so it must be stable.
const CAPSULE_NAME: &CStr = c"pyo3.per_interpreter.registry";

/// Marks that this interpreter's teardown hook has been registered.
const HOOK_KEY: &CStr = c"_pyo3_per_interpreter_hook";

/// Hands out one index per cell — one per `#[pyclass]` or `create_exception!` in the program,
/// assigned in first-use order.
static NEXT_INDEX: AtomicUsize = AtomicUsize::new(0);

const UNCLAIMED: usize = usize::MAX;

/// Bumped whenever a cached base pointer could have become wrong: a registry is dropped, or its
/// backing storage is reallocated.
///
/// This is what lets the thread-local cache be tagged with the *interpreter pointer* rather than
/// its id, which saves an FFI call on every read. A pointer alone would be unsound — CPython can
/// hand the same address to a later interpreter — and it would also miss a reallocation performed
/// by another thread of the same interpreter, which the previous id-tagged version got wrong.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Invalidates every thread's cached base pointer.
#[inline]
fn invalidate_caches() {
    GENERATION.fetch_add(1, Ordering::Release);
}

/// A value plus the function that drops it, so one registry can own values of mixed types.
type Slot = (*mut c_void, unsafe fn(*mut c_void));

/// One interpreter's values, indexed by cell.
struct Registry {
    slots: Vec<Option<Slot>>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        // This registry's storage is about to go, and the interpreter's address may be reused.
        invalidate_caches();
        // CPython reaches this from a capsule destructor, which it calls without going through
        // PyO3's attach machinery. `Py<T>`'s `Drop` would then see an unattached thread and defer
        // its decref into the process-wide reference pool, where nothing ever applies it: measured
        // at 2.6 MB retained per interpreter, growing without bound.
        //
        // SAFETY: a capsule destructor runs with this interpreter's thread state current.
        let _attached = unsafe { AssumeAttached::new() };
        for slot in self.slots.drain(..).flatten() {
            // SAFETY: each slot records the drop function for the type stored in it.
            unsafe { (slot.1)(slot.0) }
        }
    }
}

std::thread_local! {
    /// This thread's view of the registry of the interpreter it is attached to:
    /// `(interpreter id, base pointer, length)`.
    ///
    /// The id is checked on every read. A thread can move between interpreters, and interpreter
    /// ids are monotonic, so a stale tag can never match a later interpreter.
    static CACHE: Cell<(*mut ffi::PyInterpreterState, u64, *const Option<Slot>, usize)> =
        const { Cell::new((core::ptr::null_mut(), 0, core::ptr::null(), 0)) };
}

/// A cell holding at most one value *per interpreter*.
///
/// A value written by one interpreter is invisible to every other, and is dropped when the
/// interpreter that created it is finalized.
pub struct PerInterpreterCell<T> {
    /// This cell's index into each interpreter's registry, claimed on first use.
    ///
    /// Also gives the cell a non-zero size. Without that, two cells declared in one struct could
    /// share an address — they are `static`s, and a zero-sized field has no address of its own.
    index: AtomicUsize,
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
            index: AtomicUsize::new(UNCLAIMED),
            _marker: core::marker::PhantomData,
        }
    }

    /// This cell's registry index, claiming one if it has none.
    #[inline]
    fn index(&self) -> usize {
        match self.index.load(Ordering::Relaxed) {
            UNCLAIMED => self.claim_index(),
            i => i,
        }
    }

    #[cold]
    fn claim_index(&self) -> usize {
        let fresh = NEXT_INDEX.fetch_add(1, Ordering::Relaxed);
        // If another thread claimed one first, use theirs and leave ours unused. A gap costs one
        // pointer in each registry.
        match self
            .index
            .compare_exchange(UNCLAIMED, fresh, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => fresh,
            Err(existing) => existing,
        }
    }

    /// Returns a reference to this interpreter's value, if it has been initialized here.
    #[inline]
    pub fn get(&self, py: Python<'_>) -> Option<&T> {
        let idx = self.index();
        // SAFETY: `Python<'_>` witnesses that this thread is attached to an interpreter, so the
        // base pointer belongs to that interpreter's live registry.
        unsafe {
            let (base, len) = current_registry(py)?;
            if idx >= len {
                return None;
            }
            let slot = (*base.add(idx))?;
            Some(&*(slot.0 as *const T))
        }
    }

    /// Returns this interpreter's value, initializing it with `f` if this interpreter has not
    /// initialized it yet.
    ///
    /// `f` may run more than once if two threads of the *same* interpreter race; the loser's value
    /// is dropped. It never observes a value created by a *different* interpreter.
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

    #[cold]
    fn set_and_get(&self, py: Python<'_>, value: T) -> &T {
        let idx = self.index();
        let boxed = Box::into_raw(Box::new(value));
        unsafe {
            let registry = ensure_registry(py);
            if registry.is_null() {
                // The interpreter is finalizing, or out of memory. Leak rather than drop: the
                // caller receives a `&T` derived from this box.
                return &*boxed;
            }
            let registry = &mut *registry;
            if registry.slots.len() <= idx {
                registry.slots.resize(idx + 1, None);
            }
            if let Some(existing) = registry.slots[idx] {
                // Another thread of this interpreter won the race.
                drop(Box::from_raw(boxed));
                return &*(existing.0 as *const T);
            }
            registry.slots[idx] = Some((boxed as *mut c_void, drop_boxed::<T>));
            // `resize` may have moved the backing storage, so every thread's cached base is
            // stale — not just this one's.
            invalidate_caches();
            &*boxed
        }
    }
}

/// Drops a value stored as an erased pointer.
///
/// # Safety
/// `ptr` must have come from `Box::into_raw` on a `Box<T>`.
unsafe fn drop_boxed<T>(ptr: *mut c_void) {
    drop(Box::from_raw(ptr as *mut T));
}

/// Returns `(base, len)` of the current interpreter's registry, or `None` if it has none yet.
///
/// Tagging by interpreter *pointer* rather than id costs one FFI call instead of two:
/// `PyInterpreterState_GetID` was 82% of the 1.83 ns this lookup added over upstream's plain
/// atomic load. [`GENERATION`] is what makes the pointer safe to compare.
#[inline]
unsafe fn current_registry(py: Python<'_>) -> Option<(*const Option<Slot>, usize)> {
    let interp = ffi::PyInterpreterState_Get();
    let generation = GENERATION.load(Ordering::Acquire);
    let (cached_interp, cached_generation, base, len) = CACHE.with(Cell::get);
    if cached_interp == interp && cached_generation == generation {
        return Some((base, len));
    }
    lookup_registry_slow(py, interp, generation)
}

#[cold]
unsafe fn lookup_registry_slow(
    _py: Python<'_>,
    interp: *mut ffi::PyInterpreterState,
    generation: u64,
) -> Option<(*const Option<Slot>, usize)> {
    let registry = find_registry();
    if registry.is_null() {
        return None;
    }
    let registry = &*registry;
    let entry = (interp, generation, registry.slots.as_ptr(), registry.slots.len());
    CACHE.with(|c| c.set(entry));
    Some((entry.2, entry.3))
}

/// Returns this interpreter's registry, or null if it has none.
unsafe fn find_registry() -> *mut Registry {
    let interp_dict = ffi::PyInterpreterState_GetDict(ffi::PyInterpreterState_Get());
    if interp_dict.is_null() {
        return core::ptr::null_mut();
    }
    let capsule = ffi::PyDict_GetItemString(interp_dict, REGISTRY_KEY.as_ptr());
    if capsule.is_null() {
        return core::ptr::null_mut();
    }
    let raw = ffi::PyCapsule_GetPointer(capsule, CAPSULE_NAME.as_ptr());
    if raw.is_null() {
        ffi::PyErr_Clear();
        return core::ptr::null_mut();
    }
    raw as *mut Registry
}

/// Returns this interpreter's registry, creating it if absent.
unsafe fn ensure_registry(_py: Python<'_>) -> *mut Registry {
    let existing = find_registry();
    if !existing.is_null() {
        return existing;
    }
    let interp_dict = ffi::PyInterpreterState_GetDict(ffi::PyInterpreterState_Get());
    if interp_dict.is_null() {
        return core::ptr::null_mut();
    }
    let registry = Box::into_raw(Box::new(Registry { slots: Vec::new() }));
    let capsule = ffi::PyCapsule_New(
        registry as *mut c_void,
        CAPSULE_NAME.as_ptr(),
        Some(registry_destructor),
    );
    if capsule.is_null() {
        ffi::PyErr_Clear();
        drop(Box::from_raw(registry));
        return core::ptr::null_mut();
    }
    let rc = ffi::PyDict_SetItemString(interp_dict, REGISTRY_KEY.as_ptr(), capsule);
    ffi::Py_DECREF(capsule); // the dict holds the surviving reference
    if rc < 0 {
        ffi::PyErr_Clear();
        // Releasing the capsule destroyed the registry with it.
        return core::ptr::null_mut();
    }
    register_teardown_hook(interp_dict);
    registry
}

/// Drops an interpreter's registry when CPython destroys its capsule.
unsafe extern "C" fn registry_destructor(capsule: *mut ffi::PyObject) {
    let raw = ffi::PyCapsule_GetPointer(capsule, CAPSULE_NAME.as_ptr()) as *mut Registry;
    if raw.is_null() {
        ffi::PyErr_Clear();
        return;
    }
    // A thread still caching this registry's base holds a stale interpreter id, and the id is
    // checked before the pointer is used.
    drop(Box::from_raw(raw));
}

// ---------------------------------------------------------------------------
// Interpreter teardown
// ---------------------------------------------------------------------------
//
// Dropping the registry is not enough on its own. A heap type object is part of a reference cycle
// — its own `__mro__` contains it — so releasing the last *counted* reference still leaves it for
// the cyclic collector. If that never runs, the type stays alive, and through `ht_module` it pins
// the module, the module dict, and with it most of the interpreter's imported state: measured at
// 1.76 MB per interpreter, none of it reclaimed.
//
// `Py_EndInterpreter` runs this interpreter's `atexit` callbacks before tearing it down, and the
// collector still works there.

/// Empties this interpreter's registry and collects the cycles that releases.
unsafe extern "C" fn teardown(
    _self: *mut ffi::PyObject,
    _args: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    let interp_dict = ffi::PyInterpreterState_GetDict(ffi::PyInterpreterState_Get());
    if !interp_dict.is_null() {
        if ffi::PyDict_DelItemString(interp_dict, REGISTRY_KEY.as_ptr()) < 0 {
            ffi::PyErr_Clear();
        }
        // Three passes, not one: collecting a type object makes what it referenced unreachable in
        // turn, and that is only seen on the next pass. One pass recovered 25% of the excess,
        // three recover all of it.
        let gc = ffi::PyImport_ImportModule(c"gc".as_ptr());
        if !gc.is_null() {
            let name = ffi::PyUnicode_FromString(c"collect".as_ptr());
            for _ in 0..3 {
                let r = ffi::PyObject_CallMethodNoArgs(gc, name);
                if r.is_null() {
                    ffi::PyErr_Clear();
                    break;
                }
                ffi::Py_DECREF(r);
            }
            if !name.is_null() {
                ffi::Py_DECREF(name);
            }
            ffi::Py_DECREF(gc);
        } else {
            ffi::PyErr_Clear();
        }
    }
    ffi::Py_INCREF(ffi::Py_None());
    ffi::Py_None()
}

/// `PyMethodDef` is only read by CPython, so a shared static is sound.
struct TeardownDef(ffi::PyMethodDef);
unsafe impl Sync for TeardownDef {}

static TEARDOWN_DEF: TeardownDef = TeardownDef(ffi::PyMethodDef {
    ml_name: HOOK_KEY.as_ptr(),
    ml_meth: ffi::PyMethodDefPointer {
        PyCFunction: teardown,
    },
    ml_flags: ffi::METH_NOARGS,
    ml_doc: core::ptr::null(),
});

/// Registers [`teardown`] with this interpreter's `atexit`, once per interpreter.
///
/// Failure is not fatal: without the hook the registry is reclaimed later, or not at all, which is
/// the behaviour this exists to improve on.
unsafe fn register_teardown_hook(interp_dict: *mut ffi::PyObject) {
    if !ffi::PyDict_GetItemString(interp_dict, HOOK_KEY.as_ptr()).is_null() {
        return;
    }
    let atexit = ffi::PyImport_ImportModule(c"atexit".as_ptr());
    if atexit.is_null() {
        ffi::PyErr_Clear();
        return;
    }
    let callable = ffi::PyCFunction_NewEx(
        &TEARDOWN_DEF.0 as *const ffi::PyMethodDef as *mut ffi::PyMethodDef,
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
    if callable.is_null() {
        ffi::PyErr_Clear();
        ffi::Py_DECREF(atexit);
        return;
    }
    let name = ffi::PyUnicode_FromString(c"register".as_ptr());
    let res = ffi::PyObject_CallMethodOneArg(atexit, name, callable);
    if res.is_null() {
        ffi::PyErr_Clear();
    } else {
        ffi::Py_DECREF(res);
    }
    if !name.is_null() {
        ffi::Py_DECREF(name);
    }
    if ffi::PyDict_SetItemString(interp_dict, HOOK_KEY.as_ptr(), callable) < 0 {
        ffi::PyErr_Clear();
    }
    ffi::Py_DECREF(callable);
    ffi::Py_DECREF(atexit);
}

// SAFETY: every access goes through a `Python<'_>` token, so it is serialised by the GIL of the
// interpreter that owns the value, and values are never handed across interpreters.
unsafe impl<T: Send> Send for PerInterpreterCell<T> {}
unsafe impl<T: Send> Sync for PerInterpreterCell<T> {}

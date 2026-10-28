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

use crate::exceptions::PyRuntimeError;
use crate::ffi;
use crate::internal::state::AssumeAttached;
use crate::platform::sync::non_poison::Mutex;
use crate::{PyErr, Python};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::Cell;
use core::ffi::{c_void, CStr};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Capsule name for the registry. CPython compares this when unwrapping, so it must be stable.
const CAPSULE_NAME: &CStr = c"pyo3.per_interpreter.registry";

/// The interpreter-dict key this extension copy keeps its registry under.
///
/// Every extension `.so` carries its own copy of PyO3's statics: its own [`NEXT_INDEX`], its own
/// [`REGISTRIES`] and reference pools. The key must therefore be per copy too. With one fixed key,
/// the second PyO3 extension loaded into an interpreter found the first one's registry and read
/// its slots by its own indices: the first extension's type objects came back as the second's —
/// measured as a segfault on importing two fork-built extensions in one interpreter, main or sub,
/// in either order.
///
/// The suffix is the address of a static inside this copy. A physically separate copy of the
/// same library (a per-worker clone) is mapped at another address and so gets its own key; the
/// same copy loaded by several interpreters uses the same key in each, which is what it is for.
fn registry_key() -> &'static CStr {
    static KEY: std::sync::OnceLock<alloc::ffi::CString> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        let tag = &NEXT_INDEX as *const AtomicUsize as usize;
        alloc::ffi::CString::new(alloc::format!("_pyo3_per_interpreter.{tag:x}"))
            .expect("a formatted hexadecimal number has no interior NUL")
    })
}

/// Hands out one index per cell — one per `#[pyclass]` or `create_exception!` in the program,
/// assigned in first-use order.
static NEXT_INDEX: AtomicUsize = AtomicUsize::new(0);

/// Indices released by dropped cells (a `PyOnceLock` that was a field of a heap value), ready
/// for reuse. An index is pushed here only after it has been cleared from every registry, so a
/// new cell that reuses it can never observe an old value.
static FREE_INDICES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Every live registry, so a dropped cell can clear its index from all interpreters.
/// Lock order: `REGISTRIES`, then a registry's `lock`.
static REGISTRIES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

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
///
/// Its drop is the end of this copy's state in that interpreter. CPython reaches it when
/// `interpreter_clear` clears the interpreter dict, right before the interpreter's last garbage
/// collection, which then collects the cycles the released values were part of (a heap type
/// object is one: its own `__mro__` contains it). That is later than `atexit`, after
/// `finalize_modules`, and it needs no hook: registering one takes `import atexit`, which runs
/// bytecode, and bytecode raises whatever exception is pending on the thread.
struct Registry {
    slots: Vec<Option<Slot>>,
    /// Serialises structural changes: growth in `set_and_get`, and clearing an index from
    /// another thread when a cell is dropped. Reads never take it: a reader holds `&cell`, and a
    /// cell cannot be dropped while it is borrowed.
    lock: Mutex<()>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        // Unregister first, so no dropped cell on another thread reaches into this registry
        // while it is being torn down. A registry that was never registered never held this
        // interpreter's state: it is one whose dict insert failed.
        let me = self as *mut Registry as usize;
        let was_live = {
            let mut regs = REGISTRIES.lock();
            let before = regs.len();
            regs.retain(|&r| r != me);
            regs.len() != before
        };
        // This registry's storage is about to go, and the interpreter's address may be reused.
        invalidate_caches();
        // CPython reaches this from a capsule destructor, which it calls without going through
        // PyO3's attach machinery. `Py<T>`'s `Drop` would then see an unattached thread and defer
        // its decref into the process-wide reference pool, where nothing ever applies it: measured
        // at 2.6 MB retained per interpreter, growing without bound.
        //
        // SAFETY: a capsule destructor runs with this interpreter's thread state current.
        let _attached = unsafe { AssumeAttached::new() };
        if was_live {
            // SAFETY: as above, this interpreter's thread state is current.
            let interp = unsafe { ffi::PyInterpreterState_Get() };
            // Stop sending foreign-thread attaches to an interpreter that is going away.
            crate::internal::home::on_interpreter_teardown(interp);
            // Apply the decrefs still queued for it, while its objects exist.
            // SAFETY: attached through the guard above.
            crate::internal::state::drain_pool_on_teardown(unsafe { Python::assume_attached() });
        }
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
        let fresh = FREE_INDICES
            .lock()
            .pop()
            .unwrap_or_else(|| NEXT_INDEX.fetch_add(1, Ordering::Relaxed));
        // If another thread claimed one first, use theirs and leave ours unused. A gap costs one
        // pointer in each registry.
        match self
            .index
            .compare_exchange(UNCLAIMED, fresh, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => fresh,
            Err(existing) => {
                // Lost the race: give the index back.
                FREE_INDICES.lock().push(fresh);
                existing
            }
        }
    }

    /// Clears this cell's index from every interpreter and makes the index reusable.
    ///
    /// The current interpreter's value (if this thread is attached through PyO3) is dropped here.
    /// Another interpreter's value is **leaked**, never dropped from this thread: that would
    /// decref its objects outside their interpreter, which is the defect this module exists to
    /// prevent, and deferring the drop would outlive a non-`'static` `T`.
    ///
    /// Only for cells that are dropped (`PyOnceLock` fields of heap values); a `static` never is.
    pub(crate) fn release(&mut self) {
        let idx = *self.index.get_mut();
        if idx == UNCLAIMED {
            return;
        }
        *self.index.get_mut() = UNCLAIMED;
        let current = if crate::internal::state::thread_is_attached() {
            // SAFETY: PyO3 has this thread attached, so there is a current interpreter.
            unsafe { find_registry() as usize }
        } else {
            0
        };
        let mut own: Option<Slot> = None;
        {
            let regs = REGISTRIES.lock();
            for &r in regs.iter() {
                // SAFETY: a registry stays in `REGISTRIES` until its `Drop` removes it, which
                // takes the `REGISTRIES` lock held here.
                let reg = unsafe { &mut *(r as *mut Registry) };
                let _structural = reg.lock.lock();
                if let Some(slot) = reg.slots.get_mut(idx).and_then(Option::take) {
                    if r == current {
                        own = Some(slot);
                    }
                    // else: another interpreter's value, leaked (see above).
                }
            }
        }
        // Dropped outside every lock: the drop can run Python code that uses other cells.
        if let Some(slot) = own {
            // SAFETY: the slot records its own drop function; this thread is attached to the
            // interpreter that owns it.
            unsafe { (slot.1)(slot.0) }
        }
        FREE_INDICES.lock().push(idx);
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

    /// Returns a mutable reference to this interpreter's value, if it has been initialized here.
    ///
    /// `&mut self` rules out every other borrow of this cell; other interpreters' values are
    /// separate allocations.
    #[inline]
    pub fn get_mut(&mut self, py: Python<'_>) -> Option<&mut T> {
        let idx = self.index();
        // SAFETY: as in `get`; the slot pointer came from `Box::into_raw`, so it is a unique,
        // mutable pointer to a live `T`, and `&mut self` makes this the only borrow of it.
        unsafe {
            let (base, len) = current_registry(py)?;
            if idx >= len {
                return None;
            }
            let slot = (*base.add(idx))?;
            Some(&mut *(slot.0 as *mut T))
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
        // SAFETY: `py` witnesses that this thread is attached. The registry belongs to this
        // interpreter and is only restructured under its `lock`; `boxed` is either stored in the
        // registry (which then owns it) or freed here, never both.
        unsafe {
            let mut registry = match ensure_registry(py) {
                Ok(registry) => registry,
                Err(unavailable) => {
                    // The value is handed out without being stored, so the next call makes it
                    // again. Leak rather than drop: the caller receives a `&T` derived from this
                    // box. `get_or_init` has no error channel, so the failure goes to
                    // `sys.unraisablehook`.
                    unavailable.into_pyerr().write_unraisable(py, None);
                    return &*boxed;
                }
            };
            let registry = registry.as_mut();
            let _structural = registry.lock.lock();
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
    // SAFETY: the caller guarantees `ptr` came from `Box::into_raw` on a `Box<T>`.
    unsafe {
        drop(Box::from_raw(ptr as *mut T));
    }
}

/// Returns `(base, len)` of the current interpreter's registry, or `None` if it has none yet.
///
/// Tagging by interpreter *pointer* rather than id costs one FFI call instead of two:
/// `PyInterpreterState_GetID` was 82% of the 1.83 ns this lookup added over upstream's plain
/// atomic load. [`GENERATION`] is what makes the pointer safe to compare.
#[inline]
unsafe fn current_registry(py: Python<'_>) -> Option<(*const Option<Slot>, usize)> {
    // SAFETY: `py` witnesses that this thread is attached, so there is a current interpreter.
    unsafe {
        let interp = ffi::PyInterpreterState_Get();
        let generation = GENERATION.load(Ordering::Acquire);
        let (cached_interp, cached_generation, base, len) = CACHE.with(Cell::get);
        if cached_interp == interp && cached_generation == generation {
            return Some((base, len));
        }
        lookup_registry_slow(py, interp, generation)
    }
}

#[cold]
unsafe fn lookup_registry_slow(
    _py: Python<'_>,
    interp: *mut ffi::PyInterpreterState,
    generation: u64,
) -> Option<(*const Option<Slot>, usize)> {
    // SAFETY: the caller is attached (it passed `_py`), and a non-null registry is alive for as
    // long as its interpreter, which this thread is attached to.
    let registry = unsafe { find_registry() };
    if registry.is_null() {
        return None;
    }
    // SAFETY: non-null, and alive for as long as its interpreter, which this thread is attached to.
    let registry = unsafe { &*registry };
    let entry = (
        interp,
        generation,
        registry.slots.as_ptr(),
        registry.slots.len(),
    );
    CACHE.with(|c| c.set(entry));
    Some((entry.2, entry.3))
}

/// Returns this interpreter's registry, or null if it has none.
///
/// Raises nothing: an object under this copy's key that is not its registry reads as none.
unsafe fn find_registry() -> *mut Registry {
    // SAFETY: the caller is attached to an interpreter (every caller holds a `Python` token or runs inside a CPython callback of that interpreter).
    unsafe {
        let interp_dict = ffi::PyInterpreterState_GetDict(ffi::PyInterpreterState_Get());
        if interp_dict.is_null() {
            return core::ptr::null_mut();
        }
        let capsule = ffi::PyDict_GetItemString(interp_dict, registry_key().as_ptr());
        if capsule.is_null() || ffi::PyCapsule_IsValid(capsule, CAPSULE_NAME.as_ptr()) == 0 {
            return core::ptr::null_mut();
        }
        ffi::PyCapsule_GetPointer(capsule, CAPSULE_NAME.as_ptr()) as *mut Registry
    }
}

/// Why this interpreter's registry could not be made.
enum RegistryUnavailable {
    /// `PyInterpreterState_GetDict` returned null. It does so only when it cannot allocate the
    /// dict, and it clears the `MemoryError` itself.
    NoInterpreterDict,
    /// Making the capsule or storing it in the interpreter dict raised this exception.
    Raised(PyErr),
}

impl RegistryUnavailable {
    fn into_pyerr(self) -> PyErr {
        match self {
            RegistryUnavailable::NoInterpreterDict => PyRuntimeError::new_err(
                "PyO3 could not store a per-interpreter value: \
                 PyInterpreterState_GetDict returned NULL",
            ),
            RegistryUnavailable::Raised(err) => err,
        }
    }
}

/// Returns this interpreter's registry, creating it if absent.
///
/// Makes it with C calls only and runs no Python code, so an exception pending on this thread
/// (`PyThreadState_SetAsyncExc`) stays pending for the caller's next bytecode. An `Err` carries
/// only the exception one of those calls raised.
unsafe fn ensure_registry(py: Python<'_>) -> Result<NonNull<Registry>, RegistryUnavailable> {
    // SAFETY: `py` witnesses that this thread is attached; the capsule takes ownership of the box, and the box is only freed here if the capsule was never created.
    unsafe {
        if let Some(existing) = NonNull::new(find_registry()) {
            return Ok(existing);
        }
        let interp_dict = ffi::PyInterpreterState_GetDict(ffi::PyInterpreterState_Get());
        if interp_dict.is_null() {
            return Err(RegistryUnavailable::NoInterpreterDict);
        }
        let registry = NonNull::from(Box::leak(Box::new(Registry {
            slots: Vec::new(),
            lock: Mutex::new(()),
        })));
        let capsule = ffi::PyCapsule_New(
            registry.as_ptr() as *mut c_void,
            CAPSULE_NAME.as_ptr(),
            Some(registry_destructor),
        );
        if capsule.is_null() {
            drop(Box::from_raw(registry.as_ptr()));
            return Err(RegistryUnavailable::Raised(PyErr::fetch(py)));
        }
        let rc = ffi::PyDict_SetItemString(interp_dict, registry_key().as_ptr(), capsule);
        if rc < 0 {
            let raised = PyErr::fetch(py);
            // Releasing the capsule destroys the registry with it, still unregistered.
            ffi::Py_DECREF(capsule);
            return Err(RegistryUnavailable::Raised(raised));
        }
        ffi::Py_DECREF(capsule); // the dict holds the surviving reference
        REGISTRIES.lock().push(registry.as_ptr() as usize);
        Ok(registry)
    }
}

/// Drops an interpreter's registry when CPython destroys its capsule.
unsafe extern "C" fn registry_destructor(capsule: *mut ffi::PyObject) {
    // SAFETY: CPython calls this with the capsule's interpreter current; the pointer came from `Box::into_raw` in `ensure_registry`.
    unsafe {
        let raw = ffi::PyCapsule_GetPointer(capsule, CAPSULE_NAME.as_ptr()) as *mut Registry;
        if raw.is_null() {
            ffi::PyErr_Clear();
            return;
        }
        // A thread still caching this registry's base holds a stale interpreter id, and the id is
        // checked before the pointer is used.
        drop(Box::from_raw(raw));
    }
}

// SAFETY: every access goes through a `Python<'_>` token, so it is serialised by the GIL of the
// interpreter that owns the value, and values are never handed across interpreters.
unsafe impl<T: Send> Send for PerInterpreterCell<T> {}
// SAFETY: as for `Send`: a shared reference only reaches values through a `Python<'_>` token.
unsafe impl<T: Send> Sync for PerInterpreterCell<T> {}

// Tests: `tests/test_subinterp_isolation.rs`. They create sub-interpreters, which disables
// `PyGILState_Check` for the rest of the process, so they cannot run in the lib's unit tests.

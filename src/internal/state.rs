// TODO https://github.com/PyO3/pyo3/issues/5487
#![allow(clippy::undocumented_unsafe_blocks)]

//! Interaction with attachment of the current thread to the Python interpreter.

#[cfg(pyo3_disable_reference_pool)]
use crate::impl_::panic::PanicTrap;
use crate::platform::prelude::*;
use crate::{ffi, Python};

#[cfg(not(pyo3_disable_reference_pool))]
use alloc::sync::Arc;
use core::cell::Cell;
#[cfg(not(pyo3_disable_reference_pool))]
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg_attr(pyo3_disable_reference_pool, allow(unused_imports))]
use core::{mem, ptr::NonNull};
#[cfg(not(pyo3_disable_reference_pool))]
use std::sync::Mutex;

std::thread_local! {
    /// This is an internal counter in pyo3 monitoring whether this thread is attached to the interpreter.
    ///
    /// It will be incremented whenever an AttachGuard is created, and decremented whenever
    /// they are dropped.
    ///
    /// As a result, if this thread is attached to the interpreter, ATTACH_COUNT is greater than zero.
    ///
    /// Additionally, we sometimes need to prevent safe access to the Python interpreter,
    /// e.g. when implementing `__traverse__`, which is represented by a negative value.
    static ATTACH_COUNT: Cell<isize> = const { Cell::new(0) };
}

const ATTACH_FORBIDDEN_DURING_TRAVERSE: isize = -1;

/// Checks whether the thread is attached to the Python interpreter.
///
/// Note: This uses pyo3's internal count rather than PyGILState_Check for two reasons:
///  1) for performance
///  2) PyGILState_Check always returns 1 if the sub-interpreter APIs have ever been called,
///     which could lead to incorrect conclusions that the thread is attached.
#[inline(always)]
pub(crate) fn thread_is_attached() -> bool {
    ATTACH_COUNT.try_with(|c| c.get() > 0).unwrap_or(false)
}

/// RAII type that represents thread attachment to the interpreter.
pub(crate) enum AttachGuard {
    /// Indicates the thread was already attached when this AttachGuard was acquired.
    ///
    /// Carries the address of this thread's `ATTACH_COUNT`, resolved once on the way in so
    /// that `Drop` does not pay for a second thread-local lookup. Null if TLS was already
    /// gone (this can be reached from `atexit`), in which case neither side touches it.
    ///
    /// Kept as an opaque pointer: a `*const Cell<isize>` would make `AttachGuard` not
    /// `RefUnwindSafe`, and `catch_unwind` in the trampoline borrows the guard.
    Assumed { cell: *const () },
    /// Indicates that we attached when this AttachGuard was acquired
    Ensured { gstate: ffi::PyGILState_STATE },
    /// A thread with no Python thread state, attached to this extension copy's
    /// home interpreter with a fresh thread state (see `internal::home`).
    Home { tstate: *mut ffi::PyThreadState },
}

/// Possible error when calling `try_attach()`
pub(crate) enum AttachError {
    /// Forbidden during GC traversal.
    ForbiddenDuringTraverse,
    /// The interpreter is not initialized.
    NotInitialized,
    #[cfg(Py_3_13)]
    /// The interpreter is finalizing.
    Finalizing,
    /// A thread with no Python thread state tried to attach, and this extension
    /// copy was loaded by more than one interpreter: which one is meant is unknown.
    AmbiguousInterpreter,
    /// A thread with no Python thread state tried to attach, and the only
    /// interpreter that loaded this extension copy has been finalized.
    HomeGone,
}

impl AttachGuard {
    /// PyO3 internal API for attaching to the Python interpreter. The public API is Python::attach.
    ///
    /// If the thread was already attached via PyO3, this returns
    /// `AttachGuard::Assumed`. Otherwise, the thread will attach now and
    /// `AttachGuard::Ensured` will be returned.
    pub(crate) fn attach() -> Self {
        match Self::try_attach() {
            Ok(guard) => guard,
            Err(AttachError::ForbiddenDuringTraverse) => {
                panic!("{}", ForbidAttaching::FORBIDDEN_DURING_TRAVERSE)
            }
            Err(AttachError::NotInitialized) => {
                // try to initialize the interpreter and try again
                crate::interpreter_lifecycle::ensure_initialized();
                unsafe { Self::do_attach_unchecked() }
            }
            #[cfg(Py_3_13)]
            Err(AttachError::Finalizing) => {
                panic!("Cannot attach to the Python interpreter while it is finalizing.");
            }
            Err(AttachError::AmbiguousInterpreter) => {
                panic!(
                    "Python::attach was called on a thread that has no Python thread state \
                     (e.g. a rayon, tokio or std::thread worker), and this extension is loaded \
                     by more than one interpreter, so PyO3 cannot tell which one is meant. \
                     Attaching to the main interpreter would run another interpreter's objects \
                     under the wrong GIL. Capture a pyo3::sync::InterpreterHandle where the \
                     Python object is obtained and call handle.attach(...) on this thread."
                );
            }
            Err(AttachError::HomeGone) => {
                panic!(
                    "Python::attach was called on a thread that has no Python thread state, \
                     after the only interpreter that loaded this extension was finalized."
                );
            }
        }
    }

    /// Variant of the above which will will return gracefully if the interpreter cannot be attached to.
    pub(crate) fn try_attach() -> Result<Self, AttachError> {
        match ATTACH_COUNT.try_with(|c| c.get()) {
            Ok(i) if i > 0 => {
                // SAFETY: We just checked that the thread is already attached.
                return Ok(unsafe { Self::assume() });
            }
            // Cannot attach during GC traversal.
            Ok(ATTACH_FORBIDDEN_DURING_TRAVERSE) => {
                return Err(AttachError::ForbiddenDuringTraverse)
            }
            // other cases handled below
            _ => {}
        }

        // SAFETY: always safe to call this
        if unsafe { ffi::Py_IsInitialized() } == 0 {
            return Err(AttachError::NotInitialized);
        }

        // Py_IsInitialized() can return 1 while Py_InitializeEx is still
        // running (e.g. importing site.py). Block until any in-progress PyO3
        // initialization has fully completed.
        crate::interpreter_lifecycle::wait_for_initialization();

        // Calling `PyGILState_Ensure` while finalizing may crash CPython in unpredictable
        // ways, we'll make a best effort attempt here to avoid that. (There's a time of
        // check to time-of-use issue, but it's better than nothing.)
        //
        // SAFETY: always safe to call this
        #[cfg(Py_3_13)]
        if unsafe { ffi::Py_IsFinalizing() } != 0 {
            // If the interpreter is not initialized, we cannot attach.
            return Err(AttachError::Finalizing);
        }

        // A thread with no Python thread state at all (rayon, tokio, std::thread):
        // `PyGILState_Ensure` would bind it to the MAIN interpreter. Send it to this
        // extension copy's home interpreter instead, or fail loudly (M1.3, #4).
        // Threads that already have a thread state keep the old path; re-attach
        // after `py.detach` was measured correct (subinterp-bench/attach_probe.py).
        // SAFETY: always safe to call.
        if unsafe { ffi::PyGILState_GetThisThreadState() }.is_null() {
            match crate::internal::home::foreign_target() {
                crate::internal::home::ForeignTarget::Gilstate => {}
                crate::internal::home::ForeignTarget::Home(interp) => {
                    // SAFETY: the home is alive (cleared by its teardown hook) and this
                    // thread has no thread state.
                    let tstate = unsafe { crate::internal::home::attach_home(interp) };
                    increment_attach_count();
                    // SAFETY: just attached to the home interpreter.
                    drop_deferred_references(unsafe { Python::assume_attached() });
                    return Ok(AttachGuard::Home { tstate });
                }
                crate::internal::home::ForeignTarget::Ambiguous => {
                    return Err(AttachError::AmbiguousInterpreter)
                }
                crate::internal::home::ForeignTarget::HomeGone => {
                    return Err(AttachError::HomeGone)
                }
            }
        }

        // SAFETY: We have done everything reasonable to ensure we're in a safe state to
        // attach to the Python interpreter.
        Ok(unsafe { Self::do_attach_unchecked() })
    }

    /// Acquires the `AttachGuard` without performing any state checking.
    ///
    /// This can be called in "unsafe" contexts where the normal interpreter state
    /// checking performed by `AttachGuard::try_attach` may fail. This includes calling
    /// as part of multi-phase interpreter initialization.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the Python interpreter is sufficiently initialized
    /// for a thread to be able to attach to it.
    pub(crate) unsafe fn attach_unchecked() -> Self {
        if thread_is_attached() {
            return unsafe { Self::assume() };
        }

        unsafe { Self::do_attach_unchecked() }
    }

    /// Attach to the interpreter, without a fast-path to check if the thread is already attached.
    #[cold]
    unsafe fn do_attach_unchecked() -> Self {
        // SAFETY: interpreter is sufficiently initialized to attach a thread.
        let gstate = unsafe { ffi::PyGILState_Ensure() };
        increment_attach_count();
        // SAFETY: just attached to the interpreter
        drop_deferred_references(unsafe { Python::assume_attached() });
        AttachGuard::Ensured { gstate }
    }

    /// Acquires the `AttachGuard` while assuming that the thread is already attached
    /// to the interpreter.
    pub(crate) unsafe fn assume() -> Self {
        // Look up the TLS address once and use it for both the increment here and the decrement
        // in `Drop`. In a dylib every `thread_local` access goes through `tlv_get_addr` (macOS) /
        // `__tls_get_addr`; doing it twice was most of the measured FFI boundary cost (2.24 ns per
        // call).
        let cell = attach_count_cell();
        if !cell.is_null() {
            // SAFETY: non-null means this thread's TLS is alive; the guard cannot outlive
            // the call it was created in, and only this thread writes this cell.
            unsafe {
                let c = &*(cell as *const Cell<isize>);
                let current = c.get();
                if current < 0 {
                    ForbidAttaching::bail(current);
                }
                c.set(current + 1);
            }
        }
        // SAFETY: invariant of calling this function
        drop_deferred_references(unsafe { Python::assume_attached() });
        AttachGuard::Assumed { cell }
    }

    /// Gets the Python token associated with this [`AttachGuard`].
    #[inline]
    pub(crate) fn python(&self) -> Python<'_> {
        // SAFETY: this guard guarantees the thread is attached
        unsafe { Python::assume_attached() }
    }
}

/// The Drop implementation for `AttachGuard` will decrement the attach count (and potentially detach).
impl Drop for AttachGuard {
    fn drop(&mut self) {
        match self {
            AttachGuard::Assumed { cell } => {
                if !cell.is_null() {
                    // SAFETY: same thread as `assume`, and the cell outlives this guard.
                    unsafe {
                        let c = &*(*cell as *const Cell<isize>);
                        let current = c.get();
                        debug_assert!(current > 0, "Negative attach count detected.");
                        c.set(current - 1);
                    }
                }
                return;
            }
            AttachGuard::Ensured { gstate } => unsafe {
                // Drop the objects in the pool before attempting to release the thread state
                ffi::PyGILState_Release(*gstate);
            },
            AttachGuard::Home { tstate } => unsafe {
                // SAFETY: created by `attach_home` in `try_attach` and still current.
                crate::internal::home::detach_home(*tstate);
            },
        }
        decrement_attach_count();
    }
}

/// The interpreter this thread's GILState thread state belongs to, or null. Used by
/// `InterpreterHandle::attach`.
///
/// Not `PyInterpreterState_Get`: that is fatal without a thread state, so it cannot be used to
/// ask. `PyGILState_GetThisThreadState` is in the stable ABI and returns null instead.
#[inline]
pub(crate) fn current_interpreter_or_null() -> *mut ffi::PyInterpreterState {
    // SAFETY: this one is explicitly null-returning.
    let tstate = unsafe { ffi::PyGILState_GetThisThreadState() };
    if tstate.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `tstate` is non-null.
    #[cfg(any(not(Py_LIMITED_API), Py_3_10))]
    unsafe {
        ffi::PyThreadState_GetInterpreter(tstate)
    }
    // The 3.9 limited API has no `PyThreadState_GetInterpreter`: report "unknown".
    #[cfg(all(Py_LIMITED_API, not(Py_3_10)))]
    core::ptr::null_mut()
}

#[cfg(not(pyo3_disable_reference_pool))]
type PyObjVec = Vec<NonNull<ffi::PyObject>>;

#[cfg(not(pyo3_disable_reference_pool))]
/// Thread-safe storage for objects which were dec_ref while not attached.
struct ReferencePool {
    // Whether any decrefs are (or may be) pending. The `Mutex` performs
    // synchronization so we can use `Relaxed` ordering for all operations
    // on this flag.
    dirty: AtomicBool,
    pending_decrefs: Mutex<PyObjVec>,
}

#[cfg(not(pyo3_disable_reference_pool))]
impl ReferencePool {
    const fn new() -> Self {
        Self {
            dirty: AtomicBool::new(false),
            pending_decrefs: Mutex::new(Vec::new()),
        }
    }

    fn register_decref(&self, obj: NonNull<ffi::PyObject>) {
        self.pending_decrefs.lock().unwrap().push(obj);
        if !self.dirty.swap(true, Ordering::Relaxed) {
            DIRTY_POOLS.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn drop_deferred_references(&self, py: Python<'_>) {
        // Check the dirty flag first to avoid any possible contention from atomic
        // RMW operation to update the dirty flag on a hit.
        if !self.dirty.load(Ordering::Relaxed) {
            return;
        }

        // dirty flag is set, we _probably_ need to drop references (the flag is
        // not updated under the mutex so false positives are possible but rare)
        self.drop_deferred_references_slow(py);
    }

    #[cold]
    fn drop_deferred_references_slow(&self, _py: Python<'_>) {
        // Compare and swap the dirty flag to false avoids multiple threads from having
        // contention on the mutex.
        if self
            .dirty
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            // Another thread is already dropping the references, so we can return early.
            return;
        }
        DIRTY_POOLS.fetch_sub(1, Ordering::Relaxed);

        let mut pending_decrefs = self.pending_decrefs.lock().unwrap();
        if pending_decrefs.is_empty() {
            // We don't set the dirty flag under the mutex so it's possible to reach
            // this case as a false positive. Returning early avoids a store on false
            // positives.
            return;
        }
        let decrefs = mem::take(&mut *pending_decrefs);
        drop(pending_decrefs);

        for ptr in decrefs {
            unsafe { ffi::Py_DECREF(ptr.as_ptr()) };
        }
    }
}

#[cfg(not(pyo3_disable_reference_pool))]
unsafe impl Send for ReferencePool {}

#[cfg(not(pyo3_disable_reference_pool))]
unsafe impl Sync for ReferencePool {}

// One pool per interpreter (M3.2, #10).
//
// A `Py<T>` dropped while detached is queued and decref'd on the next attach. A single
// process-wide queue let interpreter B decref interpreter A's objects: A's pointer freed into
// B's allocator (SIGABRT on macOS, silent heap corruption on glibc) and an unsynchronised write
// to A's refcount while A ran under its own GIL (`subinterp-bench/BUG-POOL.md`).
//
// So each deferred decref goes to its owner's pool, and a pool is drained only by an attach to
// its own interpreter, or emptied by that interpreter's teardown hook. The owner has to be known
// when the object is queued (`owner_now`): at drain time it can no longer be recovered.

/// Which pool a deferred decref belongs to: an interpreter's address, or [`MAIN_POOL`].
#[cfg(not(pyo3_disable_reference_pool))]
type PoolKey = usize;

/// The main interpreter's pool. A constant rather than its address: a thread with no thread
/// state learns "main" from `home::foreign_target` without ever seeing the pointer, and the
/// limited API has no `PyInterpreterState_Main`.
#[cfg(not(pyo3_disable_reference_pool))]
const MAIN_POOL: PoolKey = 1;

#[cfg(not(pyo3_disable_reference_pool))]
static POOLS: Mutex<Vec<(PoolKey, Arc<ReferencePool>)>> = Mutex::new(Vec::new());

/// Number of pools whose `dirty` flag is set. Zero is the common case, and then an attach costs
/// one atomic load, as upstream's single pool did.
#[cfg(not(pyo3_disable_reference_pool))]
static DIRTY_POOLS: AtomicUsize = AtomicUsize::new(0);

/// Bumped when a pool is removed, invalidating every thread's cached pool pointer.
#[cfg(not(pyo3_disable_reference_pool))]
static POOLS_GENERATION: AtomicU64 = AtomicU64::new(0);

#[cfg(not(pyo3_disable_reference_pool))]
std::thread_local! {
    /// `(key, generation, pool)`: this thread's last drained pool. The pointer stays valid while
    /// the generation matches, because only a teardown removes a pool, and it bumps the
    /// generation first.
    static POOL_CACHE: Cell<(PoolKey, u64, *const ReferencePool)> =
        const { Cell::new((0, 0, core::ptr::null())) };
}

#[cfg(not(pyo3_disable_reference_pool))]
fn pool_key(interp: *mut ffi::PyInterpreterState) -> PoolKey {
    // SAFETY: `interp` is a live interpreter; reading its id needs no GIL.
    if unsafe { ffi::PyInterpreterState_GetID(interp) } == 0 {
        MAIN_POOL
    } else {
        interp as PoolKey
    }
}

/// The interpreter that owns an object dropped on this thread right now, while not attached.
///
/// 1. The thread has a thread state (it detached, e.g. inside `py.detach`): that thread
///    state's interpreter. Re-attach on such threads was measured to land correctly
///    (`subinterp-bench/attach_probe.py`), and the drop follows the same rule.
/// 2. No thread state (a rayon / tokio / `std::thread` worker): this extension copy's home
///    interpreter (M1.3), which is the main interpreter when the copy has only ever run there.
/// 3. Otherwise (a copy loaded by several interpreters, or whose home is gone): unknown.
#[cfg(not(pyo3_disable_reference_pool))]
fn owner_now() -> Option<PoolKey> {
    // SAFETY: always safe to call; returns null when there is no thread state.
    let tstate = unsafe { ffi::PyGILState_GetThisThreadState() };
    #[cfg(any(not(Py_LIMITED_API), Py_3_10))]
    if !tstate.is_null() {
        // SAFETY: `tstate` is non-null.
        return Some(pool_key(unsafe {
            ffi::PyThreadState_GetInterpreter(tstate)
        }));
    }
    #[cfg(all(Py_LIMITED_API, not(Py_3_10)))]
    let _ = tstate;
    match crate::internal::home::foreign_target() {
        crate::internal::home::ForeignTarget::Gilstate => Some(MAIN_POOL),
        crate::internal::home::ForeignTarget::Home(interp) => Some(pool_key(interp)),
        crate::internal::home::ForeignTarget::Ambiguous
        | crate::internal::home::ForeignTarget::HomeGone => None,
    }
}

/// The pool for `key`, created on first use.
#[cfg(not(pyo3_disable_reference_pool))]
fn pool_for(key: PoolKey) -> Arc<ReferencePool> {
    let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, pool)) = pools.iter().find(|(k, _)| *k == key) {
        return pool.clone();
    }
    let pool = Arc::new(ReferencePool::new());
    pools.push((key, pool.clone()));
    pool
}

/// The pool of the thread that is dropping an object right now. Used by the unit tests, which
/// call it on an attached main-interpreter thread.
#[cfg(all(test, not(pyo3_disable_reference_pool)))]
fn get_pool() -> Arc<ReferencePool> {
    pool_for(owner_now().expect("no owning interpreter for this thread"))
}

/// Queues a decref for an object dropped while not attached.
#[cfg(not(pyo3_disable_reference_pool))]
fn defer_decref(obj: NonNull<ffi::PyObject>) {
    match owner_now() {
        Some(key) => pool_for(key).register_decref(obj),
        None => unknown_owner(),
    }
}

/// A `Py<T>` dropped on a thread with no thread state, in an extension copy that several
/// interpreters loaded (or whose home interpreter is gone): PyO3 cannot tell whose object it is.
/// Decref'ing it in the wrong interpreter corrupts memory, and leaking it silently hides the
/// bug, so this fails loudly, like the attach in the same situation (`AttachError`).
///
/// Except while this thread is already panicking: then the drop is fallout of a failure that
/// is already being reported (typically that very attach panicking, and unwinding dropping the
/// objects the closure held). Panicking again would abort the process, turning one loud error
/// into a crash (measured: `pool_soak.py` shared, exit 134). So the object is leaked and the
/// reason printed.
#[cfg(not(pyo3_disable_reference_pool))]
#[cold]
fn unknown_owner() {
    const MSG: &str = "a Py<T> was dropped on a thread that has no Python thread state (e.g. a \
        rayon, tokio or std::thread worker), and this extension is loaded by more than one \
        interpreter (or its interpreter is gone), so PyO3 cannot tell which interpreter owns \
        the object. Decref'ing it in the wrong interpreter would corrupt memory. Drop it while \
        attached, e.g. inside pyo3::sync::InterpreterHandle::attach, or give each interpreter \
        its own copy of the extension";
    if std::thread::panicking() {
        std::eprintln!("{MSG} (this thread is already panicking, so the object is leaked)");
        return;
    }
    panic!("{MSG}");
}

#[cfg_attr(pyo3_disable_reference_pool, inline(always))]
#[cfg_attr(pyo3_disable_reference_pool, allow(unused_variables))]
fn drop_deferred_references(py: Python<'_>) {
    #[cfg(not(pyo3_disable_reference_pool))]
    if DIRTY_POOLS.load(Ordering::Relaxed) != 0 {
        drop_deferred_references_slow(py);
    }
}

/// Drains the current interpreter's pool, if it has one.
#[cfg(not(pyo3_disable_reference_pool))]
#[cold]
fn drop_deferred_references_slow(py: Python<'_>) {
    // SAFETY: attached (`py`), so there is a current interpreter.
    let key = pool_key(unsafe { ffi::PyInterpreterState_Get() });
    let generation = POOLS_GENERATION.load(Ordering::Acquire);
    let (cached_key, cached_generation, cached) = POOL_CACHE.with(Cell::get);
    if cached_key == key && cached_generation == generation && !cached.is_null() {
        // SAFETY: see `POOL_CACHE`; this thread is attached to the pool's interpreter, so its
        // teardown cannot be running.
        unsafe { &*cached }.drop_deferred_references(py);
        return;
    }
    // Created if absent, so that "this interpreter has nothing queued" is cached too: otherwise
    // every call here takes the `POOLS` lock for as long as some other interpreter's pool is dirty
    // (measured +13 ns per `#[pyfunction]` call).
    let pool = pool_for(key);
    POOL_CACHE.with(|c| c.set((key, generation, Arc::as_ptr(&pool))));
    pool.drop_deferred_references(py);
}

/// Called from the per-interpreter teardown hook, attached to the interpreter being destroyed:
/// removes its pool and applies the decrefs still queued in it.
pub(crate) fn drain_pool_on_teardown(_py: Python<'_>) {
    #[cfg(not(pyo3_disable_reference_pool))]
    {
        // SAFETY: attached (`_py`).
        let key = pool_key(unsafe { ffi::PyInterpreterState_Get() });
        // Loop: a decref can run a destructor that drops another `Py<T>` of this interpreter.
        loop {
            let pool = {
                let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
                match pools.iter().position(|(k, _)| *k == key) {
                    Some(i) => pools.swap_remove(i).1,
                    None => break,
                }
            };
            POOLS_GENERATION.fetch_add(1, Ordering::Release);
            if pool.dirty.swap(false, Ordering::Relaxed) {
                DIRTY_POOLS.fetch_sub(1, Ordering::Relaxed);
            }
            let decrefs = mem::take(&mut *pool.pending_decrefs.lock().unwrap());
            for ptr in decrefs {
                // SAFETY: queued for this interpreter, which this thread is attached to.
                unsafe { ffi::Py_DECREF(ptr.as_ptr()) };
            }
        }
    }
}

/// A guard which can be used to temporarily detach from the interpreter and restore on `Drop`.
pub(crate) struct SuspendAttach {
    count: isize,
    tstate: *mut ffi::PyThreadState,
}

impl SuspendAttach {
    pub(crate) unsafe fn new() -> Self {
        let count = ATTACH_COUNT.with(|c| c.replace(0));
        let tstate = unsafe { ffi::PyEval_SaveThread() };

        Self { count, tstate }
    }
}

impl Drop for SuspendAttach {
    fn drop(&mut self) {
        ATTACH_COUNT.with(|c| c.set(self.count));
        unsafe {
            ffi::PyEval_RestoreThread(self.tstate);

            // Update counts of `Py<T>` that were dropped while not attached.
            drop_deferred_references(Python::assume_attached());
        }
    }
}

/// Used to lock safe access to the interpreter
pub(crate) struct ForbidAttaching {
    count: isize,
}

impl ForbidAttaching {
    const FORBIDDEN_DURING_TRAVERSE: &'static str = "Attaching a thread to the interpreter is prohibited while a __traverse__ implementation is running.";

    /// Lock access to the interpreter while an implementation of `__traverse__` is running
    pub fn during_traverse() -> Self {
        Self::new(ATTACH_FORBIDDEN_DURING_TRAVERSE)
    }

    fn new(reason: isize) -> Self {
        let count = ATTACH_COUNT.with(|c| c.replace(reason));

        Self { count }
    }

    #[cold]
    fn bail(current: isize) {
        match current {
            ATTACH_FORBIDDEN_DURING_TRAVERSE => panic!("{}", Self::FORBIDDEN_DURING_TRAVERSE),
            _ => panic!("Attaching a thread to the interpreter is currently prohibited."),
        }
    }
}

impl Drop for ForbidAttaching {
    fn drop(&mut self) {
        ATTACH_COUNT.with(|c| c.set(self.count));
    }
}

/// Registers a Python object pointer inside the release pool, to have its reference count decreased
/// the next time the thread is attached in pyo3.
///
/// If the thread is attached, the reference count will be decreased immediately instead of being queued
/// for later.
///
/// # Safety
/// - The object must be an owned Python reference.
/// - The reference must not be used after calling this function.
#[inline]
pub unsafe fn register_decref(obj: NonNull<ffi::PyObject>) {
    #[cfg(not(pyo3_disable_reference_pool))]
    {
        defer_decref(obj);
    }
    #[cfg(all(
        pyo3_disable_reference_pool,
        not(pyo3_leak_on_drop_without_reference_pool)
    ))]
    {
        let _trap = PanicTrap::new("Aborting the process to avoid panic-from-drop.");
        panic!("Cannot drop pointer into Python heap without the thread being attached.");
    }
}

/// Private helper function to check if we are currently in a GC traversal (as detected by PyO3).
#[cfg(any(not(Py_LIMITED_API), Py_3_11))]
pub(crate) fn is_in_gc_traversal() -> bool {
    ATTACH_COUNT
        .try_with(|c| c.get() == ATTACH_FORBIDDEN_DURING_TRAVERSE)
        .unwrap_or(false)
}

/// Marks the thread as attached for the duration of a callback that CPython makes into Rust
/// *without* going through PyO3 — a capsule destructor, or an `atexit` hook installed as a raw
/// `PyMethodDef`.
///
/// Such a callback runs with a thread state current, but PyO3 never saw the transition, so
/// [`thread_is_attached`] reports `false` and every `Py<T>` dropped inside it defers its decref
/// into the process-wide reference pool instead of applying it.
///
/// Unlike [`AttachGuard::assume`] this deliberately does **not** flush that pool. It is used while
/// a *sub*-interpreter is finalizing, and the pool holds references belonging to whichever
/// interpreter queued them; releasing those from here would decrement refcounts in a different
/// interpreter's heap.
pub(crate) struct AssumeAttached(());

impl AssumeAttached {
    /// # Safety
    ///
    /// A thread state must be current for the whole lifetime of the returned guard.
    pub(crate) unsafe fn new() -> Self {
        increment_attach_count();
        AssumeAttached(())
    }
}

impl Drop for AssumeAttached {
    fn drop(&mut self) {
        decrement_attach_count();
    }
}

/// This thread's `ATTACH_COUNT` cell, or null if its TLS is already gone.
///
/// Resolving the address once and reusing it removes one thread-local lookup per FFI call.
#[inline(always)]
fn attach_count_cell() -> *const () {
    ATTACH_COUNT
        .try_with(|c| c as *const Cell<isize> as *const ())
        .unwrap_or(core::ptr::null())
}

/// Increments pyo3's internal attach count - to be called whenever an AttachGuard is created.
#[inline(always)]
fn increment_attach_count() {
    // Ignores the error in case this function called from `atexit`.
    let _ = ATTACH_COUNT.try_with(|c| {
        let current = c.get();
        if current < 0 {
            ForbidAttaching::bail(current);
        }
        c.set(current + 1);
    });
}

/// Decrements pyo3's internal attach count - to be called whenever AttachGuard is dropped.
#[inline(always)]
fn decrement_attach_count() {
    // Ignores the error in case this function called from `atexit`.
    let _ = ATTACH_COUNT.try_with(|c| {
        let current = c.get();
        debug_assert!(
            current > 0,
            "Negative attach count detected. Please report this error to the PyO3 repo as a bug."
        );
        c.set(current - 1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{Py, PyAny, Python};

    fn get_object(py: Python<'_>) -> Py<PyAny> {
        py.eval(c"object()", None, None).unwrap().unbind()
    }

    #[cfg(not(pyo3_disable_reference_pool))]
    fn pool_dec_refs_does_not_contain(obj: &Py<PyAny>) -> bool {
        !get_pool()
            .pending_decrefs
            .lock()
            .unwrap()
            .contains(&unsafe { NonNull::new_unchecked(obj.as_ptr()) })
    }

    // With free-threading, threads can empty the POOL at any time, so this
    // function does not test anything meaningful
    #[cfg(not(any(pyo3_disable_reference_pool, Py_GIL_DISABLED)))]
    fn pool_dec_refs_contains(obj: &Py<PyAny>) -> bool {
        get_pool()
            .pending_decrefs
            .lock()
            .unwrap()
            .contains(&unsafe { NonNull::new_unchecked(obj.as_ptr()) })
    }

    #[test]
    fn test_pyobject_drop_attached_decreases_refcnt() {
        Python::attach(|py| {
            let obj = get_object(py);

            // Create a reference to drop while attached.
            let reference = obj.clone_ref(py);

            assert_eq!(obj._get_refcnt(py), 2);
            #[cfg(not(pyo3_disable_reference_pool))]
            assert!(pool_dec_refs_does_not_contain(&obj));

            // While attached, reference count will be decreased immediately.
            drop(reference);

            assert_eq!(obj._get_refcnt(py), 1);
            #[cfg(not(any(pyo3_disable_reference_pool)))]
            assert!(pool_dec_refs_does_not_contain(&obj));
        });
    }

    #[test]
    #[cfg(all(not(pyo3_disable_reference_pool), not(target_arch = "wasm32")))] // We are building wasm Python with pthreads disabled
    fn test_pyobject_drop_detached_doesnt_decrease_refcnt() {
        let obj = Python::attach(|py| {
            let obj = get_object(py);
            // Create a reference to drop while detached.
            let reference = obj.clone_ref(py);

            assert_eq!(obj._get_refcnt(py), 2);
            assert!(pool_dec_refs_does_not_contain(&obj));

            // Drop reference in a separate (detached) thread.
            std::thread::spawn(move || drop(reference)).join().unwrap();

            // The reference count should not have changed, it is remembered
            // to release later.
            assert_eq!(obj._get_refcnt(py), 2);
            #[cfg(not(Py_GIL_DISABLED))]
            assert!(pool_dec_refs_contains(&obj));
            obj
        });

        // On next attach, the reference is released
        #[allow(unused)]
        Python::attach(|py| {
            // With free-threading, another thread could still be processing
            // DECREFs after releasing the lock on the POOL, so the
            // refcnt could still be 2 when this assert happens
            #[cfg(not(Py_GIL_DISABLED))]
            assert_eq!(obj._get_refcnt(py), 1);
            assert!(pool_dec_refs_does_not_contain(&obj));
        });
    }

    #[test]
    fn test_attach_counts() {
        // Check `attach` and AttachGuard both increase counts correctly
        let get_attach_count = || ATTACH_COUNT.with(|c| c.get());

        assert_eq!(get_attach_count(), 0);
        Python::attach(|_| {
            assert_eq!(get_attach_count(), 1);

            let pool = unsafe { AttachGuard::assume() };
            assert_eq!(get_attach_count(), 2);

            let pool2 = unsafe { AttachGuard::assume() };
            assert_eq!(get_attach_count(), 3);

            drop(pool);
            assert_eq!(get_attach_count(), 2);

            Python::attach(|_| {
                // nested `attach` updates attach count
                assert_eq!(get_attach_count(), 3);
            });
            assert_eq!(get_attach_count(), 2);

            drop(pool2);
            assert_eq!(get_attach_count(), 1);
        });
        assert_eq!(get_attach_count(), 0);
    }

    #[test]
    fn test_detach() {
        assert!(!thread_is_attached());

        Python::attach(|py| {
            assert!(thread_is_attached());

            py.detach(move || {
                assert!(!thread_is_attached());

                Python::attach(|_| assert!(thread_is_attached()));

                assert!(!thread_is_attached());
            });

            assert!(thread_is_attached());
        });

        assert!(!thread_is_attached());
    }

    #[cfg(feature = "py-clone")]
    #[test]
    #[should_panic]
    fn test_detach_updates_refcounts() {
        Python::attach(|py| {
            // Make a simple object with 1 reference
            let obj = get_object(py);
            assert_eq!(obj._get_refcnt(py), 1);
            // Cloning the object when detached should panic
            py.detach(|| obj.clone());
        });
    }

    #[test]
    fn recursive_attach_ok() {
        Python::attach(|py| {
            let obj = Python::attach(|_| py.eval(c"object()", None, None).unwrap());
            assert_eq!(obj._get_refcnt(), 1);
        })
    }

    #[cfg(feature = "py-clone")]
    #[test]
    fn test_clone_attached() {
        Python::attach(|py| {
            let obj = get_object(py);
            let count = obj._get_refcnt(py);

            // Cloning when attached should increase reference count immediately
            #[expect(clippy::redundant_clone)]
            let c = obj.clone();
            assert_eq!(count + 1, c._get_refcnt(py));
        })
    }

    #[test]
    #[cfg(not(pyo3_disable_reference_pool))]
    fn test_detached_drop_is_collected_on_next_attach() {
        let obj = Python::attach(get_object);
        let (count, ptr) = Python::attach(|py| (obj._get_refcnt(py), obj.clone_ref(py).into_ptr()));

        // A decref registered while detached applies once an attach drains the pool.
        get_pool().register_decref(NonNull::new(ptr).unwrap());

        Python::attach(|py| {
            assert_eq!(count, obj._get_refcnt(py));
            drop(obj);
        });
    }

    #[test]
    #[cfg(not(pyo3_disable_reference_pool))]
    fn test_drop_deferred_references_does_not_deadlock() {
        // drop_deferred_references can run arbitrary Python code during Py_DECREF.
        // if the locking is implemented incorrectly, it will deadlock.

        use crate::ffi;

        Python::attach(|py| {
            let obj = get_object(py);

            unsafe extern "C" fn capsule_drop(capsule: *mut ffi::PyObject) {
                // This line will implicitly call drop_deferred_references
                // -> and so cause deadlock if drop_deferred_references is not handling recursion correctly.
                let pool = unsafe { AttachGuard::assume() };

                // Rebuild obj so that it can be dropped
                unsafe {
                    use crate::Bound;

                    Bound::from_owned_ptr(
                        pool.python(),
                        ffi::PyCapsule_GetPointer(capsule, core::ptr::null()) as _,
                    )
                };
            }

            let ptr = obj.into_ptr();

            let capsule =
                unsafe { ffi::PyCapsule_New(ptr as _, core::ptr::null(), Some(capsule_drop)) };

            get_pool().register_decref(NonNull::new(capsule).unwrap());

            // Updating the counts will call decref on the capsule, which calls capsule_drop
            get_pool().drop_deferred_references(py);
        })
    }

    #[test]
    #[cfg(not(pyo3_disable_reference_pool))]
    fn test_attach_guard_drop_deferred_references() {
        Python::attach(|py| {
            let obj = get_object(py);

            // For AttachGuard::attach

            get_pool().register_decref(NonNull::new(obj.clone_ref(py).into_ptr()).unwrap());
            #[cfg(not(Py_GIL_DISABLED))]
            assert!(pool_dec_refs_contains(&obj));
            let _guard = AttachGuard::attach();
            assert!(pool_dec_refs_does_not_contain(&obj));

            // For AttachGuard::assume

            get_pool().register_decref(NonNull::new(obj.clone_ref(py).into_ptr()).unwrap());
            #[cfg(not(Py_GIL_DISABLED))]
            assert!(pool_dec_refs_contains(&obj));
            let _guard2 = unsafe { AttachGuard::assume() };
            assert!(pool_dec_refs_does_not_contain(&obj));
        })
    }
}

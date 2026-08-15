// TODO https://github.com/PyO3/pyo3/issues/5487
#![allow(clippy::undocumented_unsafe_blocks)]

//! Interaction with attachment of the current thread to the Python interpreter.

#[cfg(pyo3_disable_reference_pool)]
use crate::impl_::panic::PanicTrap;
use crate::platform::prelude::*;
use crate::{ffi, Python};

use core::cell::Cell;
#[cfg(not(pyo3_disable_reference_pool))]
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
#[cfg_attr(pyo3_disable_reference_pool, allow(unused_imports))]
use core::{mem, ptr::NonNull};
#[cfg(not(pyo3_disable_reference_pool))]
use std::sync::{Mutex, OnceLock};

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
        // 只取一次 TLS 地址,进出共用。在 dylib 里每次 thread_local 访问都要走
        // 一次 tlv_get_addr(macOS)/__tls_get_addr —— 进 +1、出 -1 各取一次,
        // 实测占了整个 FFI 边界税(2.24ns / 每次调用)的大头。
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
        }
        decrement_attach_count();
    }
}

/// 这个线程当前附着的解释器,没有则 NULL。
///
/// 不能用 `PyInterpreterState_Get` —— 它在没有 thread state 时是 fatal,不能拿来"问"。
/// `PyGILState_GetThisThreadState` 在稳定 ABI 里,而且返回 NULL 而不是 fatal。
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
    // 3.9 的限定 API 没有 PyThreadState_GetInterpreter。那里退回"分不出解释器",
    // 池子会走保守路径 —— 见 `ReferencePool::drop_deferred_references_slow`。
    #[cfg(all(Py_LIMITED_API, not(Py_3_10)))]
    core::ptr::null_mut()
}

#[cfg(not(pyo3_disable_reference_pool))]
/// 每条待释放引用连同它属于的解释器。NULL 表示登记时分辨不出。
type PyObjVec = Vec<(*mut ffi::PyInterpreterState, NonNull<ffi::PyObject>)>;

#[cfg(not(pyo3_disable_reference_pool))]
/// Thread-safe storage for objects which were dec_ref while not attached.
struct ReferencePool {
    // Whether any decrefs are (or may be) pending. The `Mutex` performs
    // synchronization so we can use `Relaxed` ordering for all operations
    // on this flag.
    dirty: AtomicBool,
    pending_decrefs: Mutex<PyObjVec>,
    /// 见过的第一个解释器,以及是否见过第二个。
    ///
    /// 单解释器进程(绝大多数)必须保持原有行为:全部释放。只有真的出现了第二个
    /// 解释器,才需要按解释器分拣 —— 那时"分辨不出归属"的条目宁可泄漏也不能释放。
    first_interp: AtomicPtr<ffi::PyInterpreterState>,
    multiple_seen: AtomicBool,
}

#[cfg(not(pyo3_disable_reference_pool))]
impl ReferencePool {
    const fn new() -> Self {
        Self {
            dirty: AtomicBool::new(false),
            pending_decrefs: Mutex::new(Vec::new()),
            first_interp: AtomicPtr::new(core::ptr::null_mut()),
            multiple_seen: AtomicBool::new(false),
        }
    }

    /// 记下见过哪个解释器,并在见到第二个时翻开保守模式。
    fn note_interpreter(&self, interp: *mut ffi::PyInterpreterState) {
        if interp.is_null() || self.multiple_seen.load(Ordering::Relaxed) {
            return;
        }
        match self.first_interp.compare_exchange(
            core::ptr::null_mut(),
            interp,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {}
            Err(existing) if existing == interp => {}
            Err(_) => self.multiple_seen.store(true, Ordering::Relaxed),
        }
    }

    fn register_decref(&self, obj: NonNull<ffi::PyObject>) {
        let interp = current_interpreter_or_null();
        self.note_interpreter(interp);
        self.pending_decrefs.lock().unwrap().push((interp, obj));
        self.dirty.store(true, Ordering::Relaxed);
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

        let mut pending_decrefs = self.pending_decrefs.lock().unwrap();
        if pending_decrefs.is_empty() {
            // We don't set the dirty flag under the mutex so it's possible to reach
            // this case as a false positive. Returning early avoids a store on false
            // positives.
            return;
        }
        let decrefs = mem::take(&mut *pending_decrefs);
        drop(pending_decrefs);

        // 单解释器进程:和以前完全一样,全部释放。
        if !self.multiple_seen.load(Ordering::Relaxed) {
            for (_, ptr) in decrefs {
                // SAFETY: the thread is attached and there is only one interpreter.
                unsafe { ffi::Py_DECREF(ptr.as_ptr()) };
            }
            return;
        }

        // 多解释器:只释放属于【当前这个】解释器的。别人的放回去等它自己来收;
        // 分辨不出归属的也放回去 —— 在错误的解释器里释放是内存损坏,泄漏只是泄漏。
        //
        // 这是压测抓到的那个 abort:进程级的池子把 A 的对象交给 B 释放,
        // 指针不在 B 的 arena 里,libmalloc 当场 abort;而在 glibc 上它不 abort,
        // 只是静默损坏堆。
        let current = current_interpreter_or_null();
        let mut keep = Vec::new();
        for (interp, ptr) in decrefs {
            if !current.is_null() && interp == current {
                // SAFETY: the object belongs to the interpreter this thread is attached to.
                unsafe { ffi::Py_DECREF(ptr.as_ptr()) };
            } else {
                keep.push((interp, ptr));
            }
        }
        if !keep.is_empty() {
            self.pending_decrefs.lock().unwrap().extend(keep);
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(not(pyo3_disable_reference_pool))]
unsafe impl Send for ReferencePool {}

#[cfg(not(pyo3_disable_reference_pool))]
unsafe impl Sync for ReferencePool {}

#[cfg(not(pyo3_disable_reference_pool))]
static POOL: OnceLock<ReferencePool> = OnceLock::new();

#[cfg(not(pyo3_disable_reference_pool))]
fn get_pool() -> &'static ReferencePool {
    POOL.get_or_init(ReferencePool::new)
}

#[cfg_attr(pyo3_disable_reference_pool, inline(always))]
#[cfg_attr(pyo3_disable_reference_pool, allow(unused_variables))]
fn drop_deferred_references(py: Python<'_>) {
    #[cfg(not(pyo3_disable_reference_pool))]
    if let Some(pool) = POOL.get() {
        pool.drop_deferred_references(py);
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
            #[cfg(not(pyo3_disable_reference_pool))]
            if let Some(pool) = POOL.get() {
                pool.drop_deferred_references(Python::assume_attached());
            }
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
        get_pool().register_decref(obj);
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
            // 表里现在每条带着它属于的解释器,查法跟着改;断言("在不在表里")没变。
            .iter()
            .any(|(_, p)| *p == unsafe { NonNull::new_unchecked(obj.as_ptr()) })
    }

    // With free-threading, threads can empty the POOL at any time, so this
    // function does not test anything meaningful
    #[cfg(not(any(pyo3_disable_reference_pool, Py_GIL_DISABLED)))]
    fn pool_dec_refs_contains(obj: &Py<PyAny>) -> bool {
        get_pool()
            .pending_decrefs
            .lock()
            .unwrap()
            // 表里现在每条带着它属于的解释器,查法跟着改;断言("在不在表里")没变。
            .iter()
            .any(|(_, p)| *p == unsafe { NonNull::new_unchecked(obj.as_ptr()) })
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

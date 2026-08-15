// TODO https://github.com/PyO3/pyo3/issues/5487
#![allow(clippy::undocumented_unsafe_blocks)]

//! Implementation details of `#[pymodule]` which need to be accessible from proc-macro generated code.
#[allow(unused_imports, reason = "conditionally used")]
use crate::platform::prelude::*;
use crate::sync::PerInterpreterCell;
use core::{
    cell::UnsafeCell,
    ffi::CStr,
    ffi::{c_int, c_void},
    marker::PhantomData,
};

#[cfg(all(
    not(any(PyPy, GraalPy)),
    not(all(windows, Py_LIMITED_API, not(Py_3_10))),
))]

#[cfg(all(
    not(any(PyPy, GraalPy)),
    not(all(windows, Py_LIMITED_API, not(Py_3_10))),
    target_has_atomic = "64",
))]
#[cfg(all(
    not(any(PyPy, GraalPy)),
    not(all(windows, Py_LIMITED_API, not(Py_3_10))),
    not(target_has_atomic = "64"),
))]
use portable_atomic::AtomicI64;

#[cfg(not(any(PyPy, GraalPy)))]
#[cfg(all(windows, Py_LIMITED_API, not(Py_3_10)))]
use crate::exceptions::PyImportError;
use crate::ffi_ptr_ext::FfiPtrExt;
#[cfg(any(not(all(Py_LIMITED_API, Py_GIL_DISABLED)), Py_3_15))]
use crate::internal_tricks::array_ptr_as_mut;
use crate::prelude::PyTypeMethods;
use crate::{err::error_on_minusone, py_result_ext::PyResultExt};
use crate::{
    ffi,
    impl_::pyfunction::PyFunctionDef,
    types::{PyModule, PyModuleMethods},
    Bound, PyClass, PyResult, PyTypeInfo,
};
use crate::{
    sync::PyOnceLock,
    types::{any::PyAnyMethods, dict::PyDictMethods, PyDict},
    Py, PyAny, Python,
};

/// `Sync` wrapper of `ffi::PyModuleDef`.
pub struct ModuleDef {
    // wrapped in UnsafeCell so that Rust compiler treats this as interior mutability
    #[cfg(not(all(Py_LIMITED_API, Py_GIL_DISABLED)))]
    ffi_def: UnsafeCell<ffi::PyModuleDef>,
    name: &'static CStr,
    #[cfg(Py_3_15)]
    slots: &'static PyModuleSlots,
    /// abi3 且编译期 < 3.12 时,占位 slot 是否已按运行时版本改写过。
    #[cfg(all(Py_LIMITED_API, not(Py_3_12)))]
    slot_patched: core::sync::atomic::AtomicBool,
    /// Initialized module object, cached to avoid reinitialization.
    ///
    /// Per-interpreter. A `Py<PyModule>` belongs to the interpreter that created it, and caching
    /// one per process is what forced `make_module` to refuse every interpreter after the first.
    module: PerInterpreterCell<Py<PyModule>>,
}

unsafe impl Sync for ModuleDef {}

impl ModuleDef {
    /// Make new module definition with given module name.
    pub const fn new(
        name: &'static CStr,
        doc: &'static CStr,
        slots: &'static PrimaryModuleSlots,
        secondary_slots: &'static SecondaryModuleSlots,
    ) -> Self {
        // This is only used in PyO3 for append_to_inittab on Python 3.15 and newer.
        // There could also be other tools that need the legacy init hook.
        #[cfg(not(all(Py_LIMITED_API, Py_GIL_DISABLED)))]
        let ffi_def = UnsafeCell::new(ffi::PyModuleDef {
            m_base: ffi::PyModuleDef_HEAD_INIT,
            m_name: name.as_ptr(),
            m_doc: doc.as_ptr(),
            m_size: 0,
            m_methods: core::ptr::null_mut(),
            m_slots: array_ptr_as_mut({
                cfg_select! {
                    Py_3_15 => secondary_slots.0.get(),
                    _ => slots.0.get(),
                }
            }),
            m_traverse: None,
            m_clear: None,
            m_free: None,
        });

        #[cfg(any(not(Py_3_15), all(Py_LIMITED_API, Py_GIL_DISABLED)))]
        let _ = secondary_slots;
        #[cfg(all(Py_LIMITED_API, Py_GIL_DISABLED))]
        let _ = doc;

        ModuleDef {
            #[cfg(not(all(Py_LIMITED_API, Py_GIL_DISABLED)))]
            ffi_def,
            name,
            #[cfg(Py_3_15)]
            slots,
            #[cfg(all(Py_LIMITED_API, not(Py_3_12)))]
            slot_patched: core::sync::atomic::AtomicBool::new(false),
            module: PerInterpreterCell::new(),
        }
    }

    #[cfg(not(all(Py_LIMITED_API, Py_GIL_DISABLED)))]
    pub fn init_multi_phase(&'static self) -> *mut ffi::PyObject {
        // abi3 编译期不知道运行时是哪个 Python,而 `Py_mod_multiple_interpreters` 在 3.12
        // 以下会让 CPython 报 unknown slot ID。所以那种构建里先占一个位,到这里 —— CPython
        // 读 m_slots 之前的最后一刻 —— 按真实版本决定填上它还是抹成终止符。
        #[cfg(all(Py_LIMITED_API, not(Py_3_12)))]
        unsafe {
            self.patch_multiple_interpreters_slot();
        }
        unsafe { ffi::PyModuleDef_Init(self.ffi_def.get()) }
    }

    /// # Safety
    ///
    /// Must run before CPython reads `m_slots`, and only once.
    #[cfg(all(Py_LIMITED_API, not(Py_3_12)))]
    unsafe fn patch_multiple_interpreters_slot(&'static self) {
        use core::sync::atomic::Ordering;
        // 改写不是原子的(不支持时要把后面整段前移),所以只让一个线程做,且只做一次。
        if self
            .slot_patched
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        // `Py_Version` 是 3.11 才有的数据符号,abi3-py310 拿不到;`Py_GetVersion` 一直在
        // 限定 API 里,返回 "3.14.6 (main, ...)" 这种字符串,取头两段就够。
        let supported = {
            let v = ffi::Py_GetVersion();
            let mut major = 0i32;
            let mut minor = 0i32;
            let mut seen_dot = false;
            let mut i = 0isize;
            loop {
                let c = *v.offset(i) as u8;
                match c {
                    b'0'..=b'9' => {
                        let d = (c - b'0') as i32;
                        if seen_dot { minor = minor * 10 + d } else { major = major * 10 + d }
                    }
                    b'.' if !seen_dot => seen_dot = true,
                    _ => break,
                }
                i += 1;
            }
            major > 3 || (major == 3 && minor >= 12)
        };
        let slots = (*self.ffi_def.get()).m_slots;
        let mut i = 0;
        loop {
            let slot = &mut *slots.add(i);
            if slot.slot == 0 {
                return; // 没找到占位符 —— 这个构建没插它
            }
            if slot.slot == PLACEHOLDER_MULTIPLE_INTERPRETERS {
                if supported {
                    slot.slot = ffi::Py_mod_multiple_interpreters;
                    // 常量本身在 pyo3-ffi 里 cfg 到 Py_3_12,abi3-py310 编译期拿不到;
                    // 它的值是固定的 2(CPython moduleobject.h)。
                    slot.value = 2 as *mut core::ffi::c_void;
                } else {
                    // 3.12 之前:把占位符和它后面的整段前移一格,抹掉它
                    let mut j = i;
                    loop {
                        let next = *slots.add(j + 1);
                        *slots.add(j) = next;
                        if next.slot == 0 {
                            break;
                        }
                        j += 1;
                    }
                }
                return;
            }
            i += 1;
        }
    }

    /// Builds a module object directly. Used for [`#[pymodule]`][crate::pymodule] submodules.
    pub fn make_module(&'static self, py: Python<'_>) -> PyResult<Py<PyModule>> {
        // 子模块不经过 `PyInit_`/`init_multi_phase`,占位 slot 要在这里也改写一次,
        // 否则原样交给 CPython:SystemError: module ... uses unknown slot ID。
        #[cfg(all(Py_LIMITED_API, not(Py_3_12)))]
        unsafe {
            self.patch_multiple_interpreters_slot();
        }
        // The cached module object is now per-interpreter, so a second interpreter no longer
        // observes the first one's module and there is nothing to refuse. See pyo3#576.
        #[cfg(not(any(PyPy, GraalPy)))]
        {
            // `PerInterpreterCell` needs `PyInterpreterState_Get`, which is missing from
            // python3.dll for the Windows stable API on 3.9; there, fall back to refusing a
            // second initialization outright.
            #[cfg(all(windows, Py_LIMITED_API, not(Py_3_10)))]
            {
                // The Windows stable API before 3.10 cannot check the interpreter ID, so best that
                // can be done to guard against subinterpreters is fail if the module is initialized
                // twice
                if self.module.get(py).is_some() {
                    return Err(PyImportError::new_err(
                        "PyO3 modules compiled for the stable API on Windows targeting Python 3.9 may only be initialized once per interpreter process"
                    ));
                }
            }
        }

        // Make a dummy spec, needs a `name` attribute and that seems to be sufficient
        // for the loader system

        static SIMPLE_NAMESPACE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
        let simple_ns = SIMPLE_NAMESPACE.import(py, "types", "SimpleNamespace")?;

        let kwargs = PyDict::new(py);
        kwargs.set_item("name", self.name)?;
        let spec = simple_ns.call((), Some(&kwargs))?;

        self.module
            .get_or_try_init(py, || {
                // SAFETY: slots / def are static and fully initialized, spec is a valid object,
                // and these functions are known to create a valid module object on success
                let module: Bound<'_, PyModule> = unsafe {
                    cfg_select! {
                        Py_3_15 => ffi::PyModule_FromSlotsAndSpec(self.get_slots(), spec.as_ptr()),
                        not(Py_3_15) => ffi::PyModule_FromDefAndSpec(self.ffi_def.get(), spec.as_ptr()),
                    }.assume_owned_or_err(py)
                    .cast_into_unchecked()
                }?;

                // SAFETY: module is a known valid module object
                error_on_minusone(py, unsafe {
                    cfg_select! {
                        Py_3_15 => ffi::PyModule_Exec(module.as_ptr()),
                        not(Py_3_15) => ffi::PyModule_ExecDef(module.as_ptr(), self.ffi_def.get()),
                    }
                })?;

                Ok(module.unbind())
            })
            .map(|py_module| py_module.clone_ref(py))
    }

    #[cfg(Py_3_15)]
    pub fn get_slots(&'static self) -> *mut ffi::PySlot {
        array_ptr_as_mut(self.slots.0.get())
    }
}

/// Defines the `PyModExport_<name>` entry point used by Python 3.15 and newer.
///
/// This is wrapped in a `macro_rules!` so the proc-macro backend can emit a single
/// version-agnostic invocation; the body only expands on Python 3.15+, where
/// `ffi::PySlot` is defined.
#[cfg(Py_3_15)]
#[doc(hidden)]
#[macro_export]
macro_rules! __pyo3_pymodexport {
    ($symbol:literal, $def:path) => {
        #[doc(hidden)]
        #[export_name = $symbol]
        pub unsafe extern "C" fn __pyo3_export() -> *mut $crate::ffi::PySlot {
            $def.get_slots()
        }
    };
}

#[cfg(not(Py_3_15))]
#[doc(hidden)]
#[macro_export]
macro_rules! __pyo3_pymodexport {
    ($symbol:literal, $def:path) => {};
}

/// Defines the `PyInit_<name>` entry point used by Python 3.14 and older.
///
/// This is wrapped in a `macro_rules!` so the proc-macro backend can emit a single
/// version-agnostic invocation; the body only expands on Python 3.14 and older
#[cfg(not(all(Py_3_15, Py_LIMITED_API, Py_GIL_DISABLED)))]
#[doc(hidden)]
#[macro_export]
macro_rules! __pyo3_pyinit {
    ($symbol:literal, $def:path) => {
        #[doc(hidden)]
        #[export_name = $symbol]
        pub unsafe extern "C" fn __pyo3_init() -> *mut $crate::ffi::PyObject {
            $def.init_multi_phase()
        }
    };
}

#[cfg(all(Py_3_15, Py_LIMITED_API, Py_GIL_DISABLED))]
#[doc(hidden)]
#[macro_export]
macro_rules! __pyo3_pyinit {
    ($symbol:literal, $def:path) => {};
}

/// 占位 slot id。CPython 从不使用负数 slot id,所以它绝不会被当成真 slot 传出去 ——
/// [`ModuleDef::init_multi_phase`] 在 CPython 读到之前一定把它换掉或抹掉。
#[cfg(all(Py_LIMITED_API, not(Py_3_12)))]
const PLACEHOLDER_MULTIPLE_INTERPRETERS: c_int = -1;

/// Type of the exec slot used to initialise module contents
pub type ModuleExecSlot = unsafe extern "C" fn(*mut ffi::PyObject) -> c_int;

const MAX_SLOTS: usize =
    // Py_mod_exec
    1 +
    // Py_mod_multiple_interpreters —— abi3 构建在编译期不知道运行时是哪个 Python,
    // 所以即使 cfg 只到 3.10 也要把位置留出来,填不填由 `PyInit_` 时的版本决定。
    (cfg!(Py_3_12) || cfg!(all(Py_LIMITED_API, not(Py_3_12)))) as usize +
    // Py_mod_gil
    cfg!(Py_3_13) as usize +
    // Py_mod_name, Py_mod_doc, and Py_mod_abi
    3 * (cfg!(Py_3_15) as usize);
const MAX_SLOTS_WITH_TRAILING_NULL: usize = MAX_SLOTS + 1;

/// On Python 3.15+ we use `PySlot` system and `PyModule_FromSlotsAndSpec`
#[cfg(Py_3_15)]
pub type PrimaryModuleSlots = PyModuleSlots;
#[cfg(all(Py_3_15, not(all(Py_LIMITED_API, Py_GIL_DISABLED))))]
pub type SecondaryModuleSlots = PyModuleDefSlots;

/// On Python 3.14 and older the primary system is `ffi::PyModuleDef`.
#[cfg(not(Py_3_15))]
pub type PrimaryModuleSlots = PyModuleDefSlots;
#[cfg(not(all(Py_3_15, not(all(Py_LIMITED_API, Py_GIL_DISABLED)))))]
pub type SecondaryModuleSlots = ();

pub const fn secondary_slots(slots: &'static PrimaryModuleSlots) -> SecondaryModuleSlots {
    cfg_select! {
        // On Python 3.15+ we populate `PyModuleDefSlots` to point at primary slots
        // (as long as not using abi3t where `PyModuleDef` is opaque and we cannot know the layout)
        all(Py_3_15, not(all(Py_LIMITED_API, Py_GIL_DISABLED))) => PyModuleDefSlots(UnsafeCell::new([
            ffi::PyModuleDef_Slot {
                slot: ffi::Py_slot_subslots,
                value: slots.0.get().cast(),
            },
            // SAFETY: terminator of C-style array
            unsafe { core::mem::zeroed() },
        ])),
        // Older versions have no secondary slots
        _ => { let _ = slots; }
    }
}

/// Builder to create module slots. The size of the number of slots desired must
/// be known up front, and N needs to be at least one greater than the number of
/// actual slots pushed due to the need to have a zeroed element on the end.
pub struct PyModuleSlotsBuilder {
    // values (initially all zeroed)
    slots: PrimaryModuleSlots,
    // current length
    len: usize,
}

// note that macros cannot use conditional compilation,
// so all implementations below must be available in all
// Python versions
// By handling it here we can avoid conditional
// compilation within the macros; they can always emit
// e.g. a `.with_gil_used()` call.
impl PyModuleSlotsBuilder {
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            slots: cfg_select! {
                Py_3_15 => PyModuleSlots(UnsafeCell::new(
                    // SAFETY: `PySlot` is legal to be zeroed (terminates C-style array)
                    [unsafe { core::mem::zeroed::<ffi::PySlot>() }; MAX_SLOTS_WITH_TRAILING_NULL],
                )),
                _ => PyModuleDefSlots(UnsafeCell::new(
                    // SAFETY: `PyModuleDef_Slot` is legal to be zeroed (terminates C-style array)
                    [unsafe { core::mem::zeroed::<ffi::PyModuleDef_Slot>() }; MAX_SLOTS_WITH_TRAILING_NULL],
                ))
            },
            len: 0,
        }
    }

    pub const fn with_mod_exec(self, exec: ModuleExecSlot) -> Self {
        #[cfg(not(Py_3_15))]
        {
            self.push(ffi::Py_mod_exec, exec as *mut c_void)
        }
        #[cfg(Py_3_15)]
        {
            // safety: exce is not NULL
            self.push_value(unsafe { ffi::PySlot_FUNC(ffi::Py_mod_exec, exec as *mut c_void) })
        }
    }

    /// Declares that this module may be loaded into sub-interpreters that own their GIL.
    ///
    /// Without this slot CPython refuses to import the extension into such an interpreter unless
    /// the process opts out globally with `_imp._override_multi_interp_extensions_check(-1)`.
    /// PyO3 could not declare it while `#[pyclass]` type objects, `create_exception!` types and
    /// the module object itself were cached per *process*; each of those is now per-interpreter.
    pub const fn with_per_interpreter_gil(self) -> Self {
        #[cfg(all(Py_3_12, not(Py_3_15)))]
        {
            self.push(
                ffi::Py_mod_multiple_interpreters,
                ffi::Py_MOD_PER_INTERPRETER_GIL_SUPPORTED,
            )
        }
        #[cfg(Py_3_15)]
        {
            self.push_value(ffi::PySlot_DATA(
                ffi::Py_mod_multiple_interpreters,
                ffi::Py_MOD_PER_INTERPRETER_GIL_SUPPORTED,
            ))
        }
        // abi3 且编译期 < 3.12:占一个位,值先留空。`ModuleDef::init_multi_phase`
        // 在运行时按真实解释器版本决定是填上 slot 还是把它抹成终止符。
        #[cfg(all(Py_LIMITED_API, not(Py_3_12)))]
        {
            self.push(PLACEHOLDER_MULTIPLE_INTERPRETERS, core::ptr::null_mut())
        }
        #[cfg(all(not(Py_3_12), not(Py_LIMITED_API)))]
        {
            self
        }
    }

    pub const fn with_gil_used(self, gil_used: bool) -> Self {
        #[cfg(all(Py_3_13, not(Py_3_15)))]
        {
            self.push(
                ffi::Py_mod_gil,
                if gil_used {
                    ffi::Py_MOD_GIL_USED
                } else {
                    ffi::Py_MOD_GIL_NOT_USED
                },
            )
        }

        #[cfg(Py_3_15)]
        {
            self.push_value(ffi::PySlot_DATA(
                ffi::Py_mod_gil,
                if gil_used {
                    ffi::Py_MOD_GIL_USED
                } else {
                    ffi::Py_MOD_GIL_NOT_USED
                },
            ))
        }

        #[cfg(not(Py_3_13))]
        {
            // Silence unused variable warning
            let _ = gil_used;
            self
        }
    }

    pub const fn with_name(self, name: &'static CStr) -> Self {
        #[cfg(Py_3_15)]
        {
            self.push_value(ffi::PySlot_STATIC_DATA(
                ffi::Py_mod_name,
                name.as_ptr() as *mut c_void,
            ))
        }

        #[cfg(not(Py_3_15))]
        {
            // Silence unused variable warning
            let _ = name;
            self
        }
    }

    pub const fn with_abi_info(self) -> Self {
        #[cfg(Py_3_15)]
        {
            ffi::PyABIInfo_VAR!(ABI_INFO);
            self.push_value(ffi::PySlot_STATIC_DATA(
                ffi::Py_mod_abi,
                (&raw mut ABI_INFO).cast(),
            ))
        }

        #[cfg(not(Py_3_15))]
        {
            self
        }
    }

    pub const fn with_doc(self, doc: &'static CStr) -> Self {
        #[cfg(Py_3_15)]
        {
            self.push_value(ffi::PySlot_STATIC_DATA(
                ffi::Py_mod_doc,
                doc.as_ptr() as *mut c_void,
            ))
        }

        #[cfg(not(Py_3_15))]
        {
            // Silence unused variable warning
            let _ = doc;
            self
        }
    }

    pub const fn build(self) -> PrimaryModuleSlots {
        self.slots
    }

    #[cfg(not(Py_3_15))]
    const fn push(mut self, slot: c_int, value: *mut c_void) -> Self {
        // Required to guarantee there's still a zeroed element
        // at the end
        assert!(
            self.len < MAX_SLOTS,
            "Cannot add more than MAX_SLOTS slots to a PyModuleSlots",
        );
        self.slots.0.get_mut()[self.len] = ffi::PyModuleDef_Slot { slot, value };
        self.len += 1;
        self
    }

    #[cfg(Py_3_15)]
    const fn push_value(mut self, value: ffi::PySlot) -> Self {
        assert!(
            self.len < MAX_SLOTS,
            "Cannot add more than MAX_SLOTS slots to a PyModuleSlots",
        );
        self.slots.0.get_mut()[self.len] = value;
        self.len += 1;
        self
    }
}

/// Wrapper to safely store module slots, to be used in a `ModuleDef`.
pub struct PyModuleSlots(
    // necessarily empty before Python 3.15; PySlot doesn't exist
    #[cfg(Py_3_15)] UnsafeCell<[ffi::PySlot; MAX_SLOTS_WITH_TRAILING_NULL]>,
);

/// Slots to populate a `PyModuleDef`
/// Cannot create a `PyModuleDef` on abi3t due to lack of knowledge of object layout
#[cfg(not(all(Py_LIMITED_API, Py_GIL_DISABLED)))]
pub struct PyModuleDefSlots(
    UnsafeCell<
        [ffi::PyModuleDef_Slot; cfg_select! {
            // on Python 3.15+ only one slot for pointing at the primary slots, plus trailing null
            Py_3_15 => 2,
            _ => MAX_SLOTS_WITH_TRAILING_NULL
        }],
    >,
);

// It might be possible to avoid this with SyncUnsafeCell in the future
//
// SAFETY: the inner values are only accessed within a `ModuleDef`,
// used to call `PyModule_FromSlotsAndSpec`
unsafe impl Sync for PyModuleSlots {}
// SAFETY: the inner values are only accessed within a `ModuleDef`,
// which only uses them to build the `ffi::ModuleDef`.
#[cfg(not(all(Py_LIMITED_API, Py_GIL_DISABLED)))]
unsafe impl Sync for PyModuleDefSlots {}

/// Trait to add an element (class, function...) to a module.
///
/// Currently only implemented for classes.
pub trait PyAddToModule: crate::sealed::Sealed {
    fn add_to_module(&'static self, module: &Bound<'_, PyModule>) -> PyResult<()>;
}

/// For adding native types (non-pyclass) to a module.
pub struct AddTypeToModule<T>(PhantomData<T>);

impl<T> AddTypeToModule<T> {
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        AddTypeToModule(PhantomData)
    }
}

impl<T: PyTypeInfo> PyAddToModule for AddTypeToModule<T> {
    fn add_to_module(&'static self, module: &Bound<'_, PyModule>) -> PyResult<()> {
        let object = T::type_object(module.py());
        module.add(object.name()?, object)
    }
}

/// For adding a class to a module.
pub struct AddClassToModule<T>(PhantomData<T>);

impl<T> AddClassToModule<T> {
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        AddClassToModule(PhantomData)
    }
}

impl<T: PyClass> PyAddToModule for AddClassToModule<T> {
    fn add_to_module(&'static self, module: &Bound<'_, PyModule>) -> PyResult<()> {
        module.add_class::<T>()
    }
}

/// For adding a function to a module.
impl PyAddToModule for PyFunctionDef {
    fn add_to_module(&'static self, module: &Bound<'_, PyModule>) -> PyResult<()> {
        // safety: self is static
        module.add_function(self.create_py_c_function(module.py(), Some(module))?)
    }
}

/// For adding a module to a module.
impl PyAddToModule for ModuleDef {
    fn add_to_module(&'static self, module: &Bound<'_, PyModule>) -> PyResult<()> {
        module.add_submodule(self.make_module(module.py())?.bind(module.py()))
    }
}

#[cfg(test)]
mod tests {
    use alloc::borrow::Cow;
    use core::{ffi::c_int, ffi::CStr};

    use crate::impl_::trampoline;

    use super::*;

    unsafe extern "C" fn module_exec(_module: *mut ffi::PyObject) -> c_int {
        0
    }

    #[test]
    fn module_init() {
        unsafe extern "C" fn module_exec(module: *mut ffi::PyObject) -> c_int {
            unsafe {
                trampoline::module_exec(module, |m| {
                    m.add("SOME_CONSTANT", 42)?;
                    Ok(())
                })
            }
        }

        static NAME: &CStr = c"test_module";
        static DOC: &CStr = c"some doc";

        static SLOTS: PrimaryModuleSlots = PyModuleSlotsBuilder::new()
            .with_mod_exec(module_exec)
            .with_gil_used(false)
            .with_abi_info()
            .with_name(NAME)
            .with_doc(DOC)
            .build();

        static SECONDARY_SLOTS: SecondaryModuleSlots = secondary_slots(&SLOTS);

        static MODULE_DEF: ModuleDef = ModuleDef::new(NAME, DOC, &SLOTS, &SECONDARY_SLOTS);

        Python::attach(|py| {
            let module = MODULE_DEF.make_module(py).unwrap().into_bound(py);
            assert_eq!(
                module
                    .getattr("__name__")
                    .unwrap()
                    .extract::<Cow<'_, str>>()
                    .unwrap(),
                "test_module",
            );
            assert_eq!(
                module
                    .getattr("__doc__")
                    .unwrap()
                    .extract::<Cow<'_, str>>()
                    .unwrap(),
                "some doc",
            );
            assert_eq!(
                module
                    .getattr("SOME_CONSTANT")
                    .unwrap()
                    .extract::<u8>()
                    .unwrap(),
                42,
            );
        })
    }

    #[test]
    fn module_def_new() {
        // To get coverage for ModuleDef::new() need to create a non-static ModuleDef, however init
        // etc require static ModuleDef, so this test needs to be separated out.
        static NAME: &CStr = c"test_module";
        static DOC: &CStr = c"some doc";

        static SLOTS: PrimaryModuleSlots = PyModuleSlotsBuilder::new().build();
        static SECONDARY_SLOTS: SecondaryModuleSlots = secondary_slots(&SLOTS);

        let module_def: ModuleDef = ModuleDef::new(NAME, DOC, &SLOTS, &SECONDARY_SLOTS);

        #[cfg(not(all(Py_LIMITED_API, Py_GIL_DISABLED)))]
        unsafe {
            let expected_slots = cfg_select! {
                Py_3_15 => SECONDARY_SLOTS.0.get().cast(),
                _ => SLOTS.0.get().cast(),
            };
            assert_eq!((*module_def.ffi_def.get()).m_slots, expected_slots);
        }
        #[cfg(all(Py_3_15, not(all(Py_LIMITED_API, Py_GIL_DISABLED))))]
        unsafe {
            let secondary_slots = &*SECONDARY_SLOTS.0.get();
            assert_eq!(secondary_slots[0].slot, ffi::Py_slot_subslots);
            assert_eq!(secondary_slots[0].value, SLOTS.0.get().cast());
            assert!(secondary_slots[1] == ffi::PyModuleDef_Slot::default());
        }

        assert_eq!(module_def.name, NAME);
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn test_build_maximal_slots() {
        let mut builder = PyModuleSlotsBuilder::new()
            .with_mod_exec(module_exec)
            .with_name(c"test_module")
            .with_doc(c"some doc")
            .with_per_interpreter_gil()
            .with_gil_used(false)
            .with_abi_info();

        #[cfg(Py_3_15)]
        {
            let second_last = builder.slots.0.get_mut()[builder.len - 1];
            let last = builder.slots.0.get_mut()[builder.len];
            let zeroed = unsafe { core::mem::zeroed() };
            fn raw_bytes(inst: &ffi::PySlot) -> &[u8] {
                unsafe {
                    core::slice::from_raw_parts(
                        inst as *const ffi::PySlot as *const u8,
                        core::mem::size_of::<ffi::PySlot>(),
                    )
                }
            }
            let zeroed_bytes = raw_bytes(&zeroed);
            assert_eq!(raw_bytes(&last), zeroed_bytes);
            assert_ne!(raw_bytes(&second_last), zeroed_bytes);
        }
        #[cfg(not(Py_3_15))]
        {
            let second_last = builder.slots.0.get_mut()[builder.len - 1];
            let last = builder.slots.0.get_mut()[builder.len];
            let zeroed = ffi::PyModuleDef_Slot::default();
            assert!(last == zeroed);
            assert!(second_last != zeroed);
        }
        assert!(builder.len == MAX_SLOTS);

        let result = std::panic::catch_unwind(|| builder.with_mod_exec(module_exec).build());

        assert!(result.is_err());
    }

    #[test]
    #[should_panic]
    fn test_module_slots_builder_overflow() {
        let mut builder = PyModuleSlotsBuilder::new();
        for _ in 0..MAX_SLOTS + 1 {
            builder = builder.with_mod_exec(module_exec);
        }
    }
}

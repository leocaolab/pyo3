# Changelog — leocaolab/pyo3 (branch `subinterp-per-interpreter-state`)

Changes in this fork relative to upstream PyO3. Upstream's own changes are in
[`CHANGELOG.md`](CHANGELOG.md), which towncrier generates, and are not repeated here.

- **Base:** upstream `main` at `dfdbc46` (2026-08-12), crate version `0.29.2`. The version
  number is left unchanged. Releases of this fork are git tags, and consumers point
  `[patch.crates-io]` at a tag. A pre-release version such as `0.29.3-x` does not satisfy
  `pyo3 = "0.29"`: Cargo then drops the patch with only a warning and silently builds
  upstream (measured).
- **Supported Python:** 3.13 and 3.14 (the minimum for Pyronova and SkyTrade).
  3.12 was measured too; problems specific to 3.12 are out of scope.
- **Evidence:** every claim below is measured by a script in `subinterp-bench/`. The plan,
  the per-milestone status and the full numbers are in [`SUBINTERP-FIXES.md`](SUBINTERP-FIXES.md).

The goal is own-GIL sub-interpreters (PEP 684): one extension used by several
interpreters in one process, each running under its own GIL.

## [Unreleased] — 2026-09-22

### Added

- Modules declare `Py_mod_multiple_interpreters = Py_MOD_PER_INTERPRETER_GIL_SUPPORTED`,
  so they import into own-GIL sub-interpreters without
  `_imp._override_multi_interp_extensions_check`. On abi3 the slot is resolved at run time,
  because the compile-time minimum (e.g. abi3-py310) cannot know the interpreter it
  will run on. Upstream's
  `ImportError: PyO3 modules do not yet support subinterpreters` is gone.
- `pyo3::sync::PerInterpreterCell<T>`: one value per interpreter, freed when that
  interpreter is finalized. It backs every per-interpreter cache below.
- `pyo3::sync::InterpreterHandle`: `InterpreterHandle::current(py)` captures the
  interpreter an object belongs to, and `handle.attach(|py| ..)` attaches any thread to it.
  For callbacks from thread pools when an extension is shared by several interpreters.
- **Home interpreter for foreign threads** (#4). `Python::attach` on a thread with no
  Python thread state (rayon, tokio, `std::thread`) used to go through `PyGILState_Ensure`,
  which binds the **main** interpreter. It now attaches to the interpreter that executed
  this extension copy's module. If several interpreters loaded the same copy, it panics
  with an explanation (`Python::try_attach` returns `None`), and the same happens if that
  interpreter is gone. It never lands silently in main.

### Changed

- **Per-interpreter caches.** These hold one value per interpreter instead of one per
  process:
  - `#[pyclass]` type objects;
  - `create_exception!` and `import_exception!` types;
  - the module object;
  - Python-level classes cached by the conversion layer (`collections.abc.*`,
    `decimal.Decimal`, `fractions.Fraction`, `ipaddress.*`, `pathlib.Path`, `uuid.UUID`,
    `zoneinfo.ZoneInfo`, …);
  - `intern!` / `Interned` (#5);
  - enum-variant singletons and freelists.
- **`PyOnceLock<T>` holds one value per interpreter** (#6). Each interpreter initializes
  and reads its own value; with a single interpreter the behaviour is unchanged.
  - `get_mut`, `take` and `into_inner` act on the value of the interpreter the thread is
    attached to, and panic if the thread is not attached.
  - Dropping a heap `PyOnceLock` drops the current interpreter's value. Values written by
    other interpreters are leaked, never dropped from the wrong interpreter.
  - `get` costs 1.6 ns instead of 0.3 ns (macOS). This is because the value is looked up
    per interpreter.
- **One deferred-decref pool per interpreter** (#10). A `Py<T>` dropped while not
  attached is queued for its owner. The owner is fixed at enqueue time: the thread's
  thread state, else the home interpreter. The queue is drained only by an attach to that
  interpreter, and emptied when that interpreter is torn down. If the owner cannot be
  determined (a shared copy dropping on a foreign thread), it panics with an explanation.
  If the thread is already panicking, it prints the reason and leaks the object instead:
  a second panic would abort the process.
- **Interpreter teardown** releases that interpreter's cached values while attached,
  applies its queued decrefs, and runs `gc.collect` three times. `gc.collect` is looked up
  when the hook is registered, so nothing is imported during `Py_EndInterpreter` (#12).

### Fixed

- **Cross-interpreter decref (bug A).** Interpreter B applied interpreter A's queued
  decrefs. That freed A's objects into B's allocator: SIGABRT on macOS, silent heap
  corruption on glibc. Upstream with one shared `.so`: the wrong interpreter applied the
  decref 5/5 times, both on a foreign thread and inside `py.detach`. This fork: 0/5, on
  macOS and on Linux (`pool_probe.py`).
- **Wrong interpreter's objects (bug B).** With one polars shared by 4 interpreters,
  3 of them got another interpreter's `pl.Int64` (`map_elements(return_dtype=pl.Int64)`).
  Now 80/80 are correct, with polars unmodified (`polars_bugb_check.py`).
- **Crash when sub-interpreters are destroyed after polars work (bug C).** Before this
  fix, `writefile` with N=8 segfaulted 5 times in 10. It is now 10/10 clean across import,
  frame, compute and writefile, N = 2/4/8, shared and copies (`teardown_check.py`). The
  cause was the process-global `PyOnceLock` values, not the import in the teardown hook.
- **Deadlock when several interpreters import one extension at once.** The list of
  threads initializing a `#[pyclass]`'s `tp_dict` was process-wide, so one interpreter's
  finished init cleared another's in-flight entry. That thread then re-entered its own
  enum-variant `PyOnceLock` and waited on itself (polars `PyOperator`). The list is now
  per interpreter.
- Per-interpreter values are decref'd at teardown instead of deferred into a pool that
  nothing applied (was 2.6 MB retained per interpreter, without bound). The teardown path
  only uses limited-API calls, so abi3 builds compile.

### Performance

The numbers are on macOS and Python 3.14 unless noted.

- A `#[pyfunction]` call with nothing queued costs the same as upstream (10.2 ns vs
  10.9 ns). While another interpreter has decrefs queued it costs 13.1 ns
  (`attach_cost.py`).
- A foreign-thread attach costs 80 ns. Upstream takes 65 ns, but lands in the wrong
  interpreter.
- `intern!` lookup costs 2.5 ns vs 0.34 ns upstream.
- Importing polars and writing one CSV takes 1.02× as long as upstream, measured in the
  main interpreter with the same polars source.
- On Linux the per-read costs are higher: `intern!` 7.7 ns and `PyOnceLock::get` 3.4 ns,
  against 0.96 ns upstream. This has not been investigated.

### Known limitations

- On a `static`, `PyOnceLock::get` / `PerInterpreterCell::get` return `&'static T`, but
  that value is freed when its interpreter is finalized. Keeping the reference beyond
  that is a use-after-free (upstream instead used the wrong interpreter's object).
- An extension shared by several interpreters cannot call back into Python, or drop a
  `Py<T>`, on a thread pool without `InterpreterHandle`. Both fail loudly. The supported
  route is one copy of the extension per interpreter (Pyronova's `isolate`).
- On 3.12, in a sub-interpreter that falls back to the pure-Python `_pydatetime`, the
  datetime conversions use another interpreter's C-API table. This is out of scope: the
  minimum supported Python is 3.13, where all interpreters share one table.
- Creating and closing an interpreter that imports polars grows RSS by about 52 KB per
  interpreter lifetime. There is no upstream baseline, and PyO3's own share is 0.2 KB, so
  the cause is not attributed. It does not affect long-lived workers.
- Not proposed upstream yet (#14–#16).

# Per-interpreter PyO3: what still needs fixing, and how

Branch `subinterp-per-interpreter-state`, 2026-09-22. English companion to
`JOURNAL.md` (what is done) and `subinterp-bench/BUG-POOL.md` (bug A).

The branch already makes `#[pyclass]` type objects, `create_exception!` types
and module objects per interpreter, removes upstream's second-interpreter guard,
declares `Py_MOD_PER_INTERPRETER_GIL_SUPPORTED`, and adds `InterpreterHandle`.
This document is the plan for everything that is still wrong. Each workstream
maps to a GitHub milestone and its issues.

**The rule every fix enforces:** a Python object belongs to exactly one
interpreter. Code that keeps one past a call, or touches one from another
thread, must know which interpreter that is. When it cannot know, it must fail
loudly, never fall back to the main interpreter.

**Why it matters now:** the polars audit (`leocaolab/polars`, `subinterp`
branch, `SUBINTERP-AUDIT.md`) and the Pyronova design
(`pyre/docs/design/polars-in-subinterpreters.md`) depend on these fixes.
Phase 1 there (one polars copy per worker) needs **M1** only. Phase 2 (one
shared polars) needs M1–M4.

---

## Status of what is broken

| id | problem | where | severity |
|---|---|---|---|
| P3-foreign | `Python::attach` on a thread with no thread state calls `PyGILState_Ensure`, which binds the **main** interpreter | `src/internal/state.rs` (`do_attach_unchecked`) | **fixed (M1.3, `5fec073`)**: home interpreter for single-interpreter copies, loud failure for shared ones |
| P3-reattach | after `py.detach`, a nested `Python::attach` on the **same** thread also goes through `PyGILState_Ensure` | same | **measured correct** on 3.12.13, 3.13.15 and 3.14.7 (macOS) and 3.14.4 (Linux), both thread origins: not a bug (#2, #3) |
| P1 | `PyOnceLock` is one process-global `OnceCell` | `src/sync/once_lock.rs` | first interpreter's object handed to all (polars bug B; rust-numpy API table) |
| P2 | `intern!` / `Interned` hold a process-global `PyOnceLock<Py<PyString>>` | `src/sync.rs:237-246` | interned strings are **mortal** on 3.12–3.14 (measured), so their refcount is shared |
| A | process-global deferred-decref pool; drained by whichever interpreter attaches next | `src/internal/state.rs:265-388` | SIGABRT in 1–4 s at ≥4 interpreters with callbacks; silent heap corruption on Linux |
| A-dead | `73f61bb` "fix" that never executes (`note_interpreter`, `first_interp`, `multiple_seen`, the sorting branch) | `src/internal/state.rs:227-360` | dead code that claims to fix A |
| C | teardown runs `PyImport_ImportModule("gc")` inside `Py_EndInterpreter` | `src/sync/per_interpreter.rs:357` | segfault on destroy after parallel work (weak attribution) |
| D | ~60 KB leaked per interpreter lifetime; import 3.7× slower (legacy config A/B) | registry lifecycle | leak + start-up cost |
| audit | ~20 of 79 PyO3-internal cache sites never checked at runtime | `JOURNAL.md` "分支自身还欠的" | unknown |

---

## M1 — Foreign-thread and re-attach (P3)

### M1.1 Probe: where does an attach land?

Add to `subinterp-bench/probe` (upstream and fork builds, existing `build.sh`):

```rust
/// Interpreter id seen by a Python::attach made after py.detach on this thread.
#[pyfunction] fn attach_after_detach(py: Python<'_>) -> i64;
/// Interpreter id seen by a Python::attach on a fresh std::thread.
#[pyfunction] fn attach_on_new_thread(py: Python<'_>) -> i64;
/// Interpreter id of the caller (for comparison).
#[pyfunction] fn current_interp_id(py: Python<'_>) -> i64;
```

`subinterp-bench/attach_probe.py` runs them in N own-GIL sub-interpreters for
two thread origins:

- **Rust-style worker**: a thread whose first thread state is the
  sub-interpreter's (what Pyronova does);
- **Python thread**: a `threading.Thread` of the main interpreter that then runs
  the sub-interpreter (what `concurrent.interpreters` + `threading` does).

Output: a table `origin × call → correct / main / other / crash`, committed.
This decides how much of M1.3 is mandatory.

### M1.2 Remember the thread's own detached thread state

`SuspendAttach` (py.detach) saves the thread state it releases. Keep that
pointer in a thread-local "detached tstate" slot for the duration of the
detach. `do_attach_unchecked` checks the slot first: if set, it re-attaches
**that** thread state (`PyEval_RestoreThread`) instead of calling
`PyGILState_Ensure`. It is exact, costs one TLS read, and makes re-attach
independent of how CPython bound the gilstate slot.

### M1.3 Home interpreter per extension copy

Each extension `.so` has its own copy of PyO3's statics. Record, per copy:

```rust
static HOME: AtomicPtr<ffi::PyInterpreterState>;  // first interpreter to exec a module of this copy
static MULTI: AtomicBool;                          // a second, different interpreter exec'd one
```

set in the module exec path (`src/impl_/pymodule.rs:647-653`,
`init_multi_phase` `:118`, `make_module` `:197`).

Attach slow path (no thread state, no detached slot):

```
if MULTI:            Err(AttachError::AmbiguousInterpreter)  // Python::attach panics with a message
                                                            // naming the fix: use InterpreterHandle
else if HOME is null or HOME is the main interpreter:
                     PyGILState_Ensure (unchanged)
else:                attach to HOME via a fresh thread state
                     (same mechanism as InterpreterHandle::attach,
                      src/sync/interpreter_handle.rs:87-110)
```

A foreign attach therefore lands in the right interpreter (one copy per
interpreter), or fails loudly (a copy shared by several interpreters). It never
silently lands in main. `InterpreterHandle::attach` keeps working and is the
explicit tool for shared copies.

Thread-state cost: `PyThreadState_New` + delete per foreign attach. Measure
first. If needed, cache one thread state per (OS thread, HOME) in TLS,
invalidated by the per-interpreter teardown hook's generation bump
(`src/sync/per_interpreter.rs`, `GENERATION`).

### M1 status (2026-09-22)

| item | result |
|---|---|
| M1.1 probe (#2) | done. Re-attach after `py.detach`: correct in all 8 combinations. Fresh thread: MAIN in all 8, upstream and branch |
| M1.2 detached-tstate slot (#3) | **not needed**: re-attach is correct on 3.12, 3.13 and 3.14, so no code was added |
| M1.3 home interpreter (#4) | done (`5fec073`). Fresh thread: copies → correct, shared → loud, on 3.12 / 3.13 / 3.14 (macOS) and 3.14 (Linux). `matrix.py` unchanged on both OSes. 860 unit tests on both. Foreign attach 80 ns (upstream 65 ns, wrong interpreter). **polars (unpatched) write-path check:** copies 16/16 writes in the owning interpreter; shared → loud panic, turned into a crash by polars' own `unwrap` (`polars_write_check.py`) |

### M1 acceptance

- `attach_probe.py`: every row correct (fork), or `AmbiguousInterpreter` when
  a copy is shared; the upstream column documents today's behaviour.
- New Rust tests in `src/internal/state.rs` for the TLS slot and the HOME / MULTI
  transitions.
- `cargo test --lib --release` passes on macOS and Linux; `build.sh` +
  `matrix.py` 8/8.
- Rerun `POLARS.md`'s write-path check (custom file-like, 4 interpreters) with
  `InterpreterHandle` removed from polars: writes land in the owning
  interpreter via HOME, or fail loudly if polars is shared.

---

## M2 — Per-interpreter caches (P1, P2, audit)

### M2.1 `Interned` per interpreter

`Interned` is always a `static` (the `intern!` macro declares one). Back it with
`PerInterpreterCell<Py<PyString>>` (`src/sync/per_interpreter.rs:110`), which is
designed for statics. No API change.

### M2.2 `PyOnceLock` per interpreter

The first design (a fast-path `first` slot plus a per-interpreter hash map keyed
by a never-reused id) was dropped. It needed two lookup paths, an eviction list
purged on attach, and `get_mut`/`take` that only worked on one interpreter's
value. What was built instead: `PyOnceLock<T>` is `PerInterpreterCell<OnceCell<T>>`
(`src/sync/once_lock.rs`), and `PerInterpreterCell` learned to give its index back.

- **Reads** use `PerInterpreterCell`'s dense index into the current
  interpreter's registry (TLS-cached base, `GENERATION`-checked). One path for
  every interpreter.
- **Index reuse.** A dropped cell (`PyOnceLock::drop` → `PerInterpreterCell::release`,
  `src/sync/per_interpreter.rs`) clears its index from **every** live registry
  first (`REGISTRIES` list, each registry's `lock`), then pushes the index onto
  `FREE_INDICES`. So a cell that later reuses the index can never read an old
  value. Statics never drop, so they never release.
- **Whose value is dropped.** The current interpreter's value is dropped
  synchronously, which keeps `test_once_cell_drop` (a non-`'static` value
  dropped with its lock) passing. Values that other interpreters initialized
  are **leaked**. Dropping them from this thread would decref objects outside
  their interpreter, and deferring the drop would outlive a non-`'static` `T`.
  This only matters for a heap `PyOnceLock` that several interpreters wrote to,
  which no known user does.
- **`get_mut` / `take` / `into_inner`** (no `py`) act on the value of the
  interpreter the thread is attached to, and panic if the thread is not
  attached. Built on `PerInterpreterCell::get_mut`, which takes the pointer
  from the box and never casts `&T` to `&mut T`.
- Auto traits are the same as upstream (`PhantomData<OnceCell<T>>`).

**Known limit.** `get(&'static self)` on a `static` returns `&'static T`, but
that value is freed when its interpreter is finalized. PyO3's own statics
use the reference right away, so they are fine. Code that stores the `&'static T`
past its interpreter's lifetime is now a use-after-free, where upstream
used the wrong interpreter's object instead. Tying the return lifetime
to `'py` would fix this, but it is an API change, left for M5.

This fixes, with no downstream change: polars' 13 `PyOnceLock` statics
(`py_modules.rs:4-8`, `catalog/unity.rs:37-40`, `any_value.rs:545, 552`), and
rust-numpy's `PY_ARRAY_API` / `PY_UFUNC_API`.

### M2.3 Audit the remaining PyO3 cache sites

After M2.1 and M2.2, every `PyOnceLock`, `Interned` and `PerInterpreterCell` is per
interpreter. So the audit covers every process-level `static` in `src/`,
`pyo3-ffi/src` and `pyo3-macros-backend/src` that is **not** one of those. For each
one: does it hold a Python object or pointer, and if so, is that object the same
in every interpreter (immortal / static builtin) or not? Each Python-facing verdict
is **measured end to end** in `subinterp-bench/conv_probe.py`: N=4 own-GIL
sub-interpreters, one shared `.so` and one copy per interpreter, full API and abi3,
on 3.12.13, 3.13.15 and 3.14.7 (macOS).

| site | holds | verdict | evidence |
|---|---|---|---|
| every `PyOnceLock` static: 53 declarations in `src/` (builtin type caches in `types/*.rs`, `pathlib`/`ipaddress`/`decimal`/`uuid`/`zoneinfo`/`collections.abc` classes in conversions, `ImportedExceptionTypeObject`, `err_state`, `get_slot`, `coroutine/waker`) plus macro-generated enum `SINGLETON` and `FREELIST` | Python objects | per interpreter (M2.2) | `conv_probe.py`: `PathBuf` → own `pathlib.Path`, `Ipv4Addr` → own `IPv4Address`: fork 4/4 on every build and deploy; upstream shared .so 1/4. `cache_probe.py`: own `sys` 4/4 |
| `Interned` / `intern!` | `PyString` | per interpreter (M2.1) | `cache_probe.py` 4/4 |
| `LazyTypeObject` (`value`, `fully_initialized_type`, `initializing_threads`), `ModuleDef.module` | type / module objects, init bookkeeping | per interpreter (`PerInterpreterCell`) | `initializing_threads` fixed in M2.2 (deadlock) |
| **pyo3-ffi `PyDateTimeAPI_impl`** (`AtomicPtr<PyDateTime_CAPI>`), read by PyO3's datetime types, `from_timestamp`, `PyTzInfo::utc` | pointer to one interpreter's datetime C-API table | **was wrong on 3.12. Fixed:** PyO3 now keeps the table per interpreter (`ensure_datetime_api`, `src/types/datetime.rs`). The 3.12 cause is CPython's: C `_datetime` loads only in the first interpreter that imports it, and every other isolated sub-interpreter gets the pure-Python `_pydatetime` with its own heap types and no capsule. PyO3 then built the first interpreter's `timedelta` in every other interpreter. Now it raises `RuntimeError` saying why, with CPython's error as the cause | 3.12 fork before: `timedelta`/`utc` own object 1/4, 4 distinct `timedelta` types, `utc` mortal. After: the 1 interpreter with C `_datetime` is correct, the other 3 raise the explanatory error. 3.13/3.14: one shared, immortal table, 4/4 before and after. The ffi global is kept for direct FFI users, and on 3.12 it is still the first interpreter's |
| pyo3-ffi `static mut PyExc_*`, `Py*_Type` (CPython's exported static builtin types and exceptions) | CPython-owned static objects | shared by CPython by design, safe | immortal in every interpreter on 3.12, 3.13, 3.14 (`conv_probe.py`, last column) |
| `internal/state.rs` `POOL` (`ReferencePool`) | deferred decrefs from all interpreters | **wrong**. Owned by M3 (#9–#11) | `BUG-POOL.md` |
| `internal/home.rs` `STATE` | home interpreter pointer + flags | deliberately per `.so` copy (M1.3) | `attach_probe.py` |
| `sync/per_interpreter.rs` `NEXT_INDEX`, `FREE_INDICES`, `REGISTRIES`, `GENERATION`; thread-local `CACHE` | index bookkeeping, registry addresses. `CACHE` is tagged by interpreter + generation | no Python objects; process-wide by design | unit tests |
| thread-local `ATTACH_COUNT` | per-thread count | per thread, no Python objects | — |
| `conversions/std/num.rs` `DIGITS` (`OnceLock<bool>`) | the running CPython's `int` digit layout | plain data, same in every interpreter | — |
| `interpreter_lifecycle.rs` `START` (`Once`) | "Python initialized" | process-wide by nature | — |
| macro-generated `ITEMS`, `INTRINSIC_ITEMS`, `_PYO3_DEF` (`PyFunctionDef`), `SLOTS`/`SECONDARY_SLOTS`, introspection fragments, `PyMethodDef` statics | static C/Rust data (method tables, slot arrays) | no Python objects | — |

Result: apart from `POOL` (M3), there is no process-level PyO3 cache left that
hands one interpreter's object to another. The one new defect this audit found
(datetime on 3.12) is fixed. Direct FFI users still share pyo3-ffi's global
datetime table. That is pyo3-ffi's public API and it cannot be per interpreter
without a `Python` token, so it is documented here, not changed.

### M2.4 Correct the docs

`RETRO-subinterp.md` states that interned strings are "mostly immortal" on
3.12+. That is wrong (measured, `SUBINTERP-AUDIT.md`, "Measured facts"). Fix
it, and link the measurement.

### M2 status (2026-09-22)

| item | status |
|---|---|
| M2.1 `Interned` (#5) | done (`3c5d769`) |
| M2.2 `PyOnceLock` (#6) | done. `cache_probe.py`, N=4, 3.14 macOS: a `static PyOnceLock` holding `sys` gives each interpreter its own `sys` 4/4 with one shared `.so` (upstream 1/4); heap lock drop releases its value and a fresh lock is empty, 4/4; `get` 1.6 ns (upstream 0.3 ns). `attach_probe.py` unchanged. **polars bug B, polars unmodified** (`polars_bugb_check.py`, N=4 × 20 rounds of `map_elements(return_dtype=pl.Int64)`): shared polars went from 3/4 interpreters getting another interpreter's `Int64` to 80/80 correct; copies 80/80 before and after |
| found on the way | `LazyTypeObject.initializing_threads` was process-wide. A successful init `clear()`ed it, wiping another interpreter's in-flight entry. That thread lost its reentrancy guard, re-entered its own `#[pyclass]` enum-variant singleton `PyOnceLock`, and deadlocked (4 interpreters importing one shared polars at once, `PyOperator`). Hidden until M2.2 because the singleton used to be shared. Now per interpreter |
| M2.3 audit (#7) | done. Table in "M2.3" above. New defect found and fixed: datetime C-API table on 3.12 (wrong interpreter → explanatory error). Everything else is per interpreter, or shared by CPython and immortal (measured 3.12/3.13/3.14), or `POOL` (M3) |
| unit tests | 860, parallel and serial. The three `per_interpreter` teardown tests now tear down a real own-GIL sub-interpreter (`Py_EndInterpreter`) instead of the main interpreter's registry, which other parallel tests now depend on. Assertions unchanged; mutation-checked: removing `Registry::drop`'s attach guard still fails `registry_teardown_releases_its_values`. They need 3.12+ and the full API |

### M2 acceptance

- A test where two interpreters `intern!` the same text: different objects,
  each with a normal refcount.
- polars `map_elements` with a `return_dtype` in 2 interpreters (the bug-B
  repro): 0 wrong-interpreter classes with no polars change.
- rust-numpy: `to_numpy` in 2 interpreters that each have their own numpy copy:
  correct arrays.
- Per-read cost within budget (measured, recorded).

---

## M3 — Reference pool (A)

### M3.1 Remove the dead fix

Delete `note_interpreter`, `first_interp`, `multiple_seen`,
`current_interpreter_or_null`, and the sorting branch
(`src/internal/state.rs:227-360`). They never execute (`BUG-POOL.md` §4).
Clean first, then build.

### M3.2 Per-interpreter pools

Replace `static POOL: OnceLock<ReferencePool>` with one pool per interpreter.
`register_decref` must know the owner **when the object is queued**
(`BUG-POOL.md` §5). The owner comes from, in order:

1. the thread's detached thread state (M1.2), which covers drops during `py.detach`;
2. HOME, when the extension copy has exactly one interpreter (M1.3), which
   covers the copy route completely;
3. otherwise the owner is unknown (a shared copy, dropping on a foreign thread).

For case 3, M3.3 decides between:

| option | pro | con |
|---|---|---|
| `Py<T>` carries its interpreter (behind a cfg) | exact for every drop | `Py<T>` grows 8 → 16 bytes; layout change for every user |
| fail loudly (abort / panic with the drop site) | honest; no layout change | a shared copy must not drop `Py` off-thread |
| leak (never decref) | never corrupts | unbounded leak |

Recommendation: fail loudly by default, with the `Py<T>`-carries-interpreter
layout as an opt-in cfg. Benchmark it before choosing a default.

A pool is drained only by an attach to **its** interpreter, and emptied in
that interpreter's teardown hook.

### M3 status (2026-09-22)

| item | status |
|---|---|
| M3.1 dead code (#9) | done (`2a9c032`). The pool is byte-identical to upstream again, the base for M3.2 |
| M3.2 per-interpreter pools (#10) | done. One pool per interpreter in `POOLS`, keyed by interpreter (the main interpreter under a fixed key). The owner is fixed at enqueue (`owner_now`): the thread's thread state, else HOME (M1.3), else unknown. A pool is drained only by an attach to its own interpreter, and emptied by that interpreter's teardown hook (under `AssumeAttached`). Fast path: one global dirty-pool counter, so an idle attach costs what upstream's does |
| unknown owner | panics with an explanation, like `AttachError`. **While the thread is already panicking** (the drop is fallout of an attach that just failed loudly), it prints the reason and leaks the object instead: a second panic would abort, and that was measured turning `pool_soak.py` shared from a Python `PanicException` into exit 134 |
| M3.3 policy (#11) | open: fail-loudly is implemented; the `Py<T>`-carries-interpreter layout option is not prototyped |

Evidence (3.14.7 macOS):

- `pool_probe.py` (2 interpreters, 5 rounds per cell):

  | drop while not attached | upstream shared .so | fork shared .so | copies (both) |
  |---|---|---|---|
  | on a thread with no thread state | **decref applied by the other interpreter** 5/5 | loud 5/5 | queued, applied by the owner 5/5 |
  | inside `py.detach`, other interpreter attaches meanwhile | **applied by the other interpreter** 5/5 | queued until the owner re-attaches 5/5 | same 5/5 |
  | inside `py.detach`, nothing else running | owner, on return | owner, on return | owner, on return |

- `pool_soak.py` N=8 write, 90 s: copies 3/3 + 1/1 clean (≈0.95 M writes each, 0 wrong); pre-M3.2 copies 3/3 clean too. The pool is a static, so a copy was already its own pool. Shared: M1.3 fails the worker-thread attach first, so each interpreter gets a `PanicException` (8/8), with no abort. N=1 control: clean. N=8 collect copies: clean.
- `attach_cost.py` (ns per `#[pyfunction]` call): idle 10.2 (upstream 10.9); with another interpreter's pool dirty 13.1 (upstream 10.6, but upstream's number is what it costs to wrongly drain that pool on the first call).
- `leak.py` 300 rounds, before vs after M3.2: 0.2 vs 0.2–0.3 MB per thousand interpreters. Unchanged.
- polars bug B and the write check on the new build: unchanged (80/80; copies 16/16).
- Not yet run: ASAN on Linux.

### M3 acceptance

- `subinterp-bench/pool_soak.py` at N = 8, write mode: 10/10 clean on macOS
  (was 6/6 SIGABRT).
- The same under ASAN on Linux: 0 reports (glibc does not abort, so "no crash"
  is not evidence).
- N = 1 control unchanged; `leak.py` slope unchanged.

---

## M4 — Teardown and leak (C, D)

- **C:** resolve `gc.collect` at hook registration, while the interpreter is
  healthy, and store it in the registry; teardown calls the stored function and
  imports nothing (`src/sync/per_interpreter.rs:345-357`). Acceptance: the
  `POLARS.md` destroy-after-compute case (N = 2…8) 3/3 clean, or a probe-only
  repro if one exists.
- **D:** find the ~60 KB per interpreter and the 3.7× import cost (`leak.py`,
  legacy config, A/B against the parent commit). Acceptance: the slope is
  within 10% of upstream, and import time within 1.2× of upstream.

## M5 — Upstream readiness

- CI on the fork (public, so Actions are free): `cargo test --lib --release`,
  the abi3 build, `build.sh` + `matrix.py`, `attach_probe.py`, `leak.py`.
- The regression tests `JOURNAL.md` requires before talking to upstream.
- A comment on PyO3#3451 with the measurements and the design, split into
  reviewable pieces (`InterpreterHandle` first, since it is orthogonal).

## Order

M1 → M2 → M3 → M4 → M5, for three reasons. M1 is on the critical path of the
north star. M1.2 and M1.3 are also the ownership sources M3 needs. And M2 is
self-contained, with clear acceptance tests.

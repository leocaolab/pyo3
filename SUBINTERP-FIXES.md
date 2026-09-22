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
| M1.3 home interpreter (#4) | done (`5fec073`). Fresh thread: copies → correct, shared → loud, on 3.12 / 3.13 / 3.14 (macOS) and 3.14 (Linux). `matrix.py` unchanged on both OSes. 860 unit tests on both. Foreign attach 80 ns (upstream 65 ns, wrong interpreter). **Open:** polars write-path re-check |

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

`PerInterpreterCell` cannot back `PyOnceLock` directly. `PerInterpreterCell`
claims a global index forever, and `PyOnceLock` is also used as a field of
heap values created at runtime, so every instance would leak an index.

Design:

```
PyOnceLock<T> {
    first: OnceCell<(InterpId, T)>,       // fast path: the first interpreter to initialise
    others_id: AtomicU64,                 // 0 until a second interpreter uses this lock
}
per-interpreter registry: HashMap<u64 /* lock id */, Box<dyn Any>>
```

- `get` / `get_or_init` / `set` read `first` if its `InterpId` is the current
  interpreter's. Otherwise they claim `others_id` (a global, never reused
  counter) and use this interpreter's map.
- Values in the map are dropped with their interpreter (existing teardown
  hook). When a `PyOnceLock` is dropped, its id goes on a plain-data eviction
  list; each interpreter purges those ids on its next attach and at teardown.
  Ids are never reused, so a stale entry can never be read.
- `get_mut` / `take` / `into_inner` (no `py`) act on `first` only. They are
  documented as owner-interpreter operations and panic if `others_id != 0`.
- Cost: one interpreter-pointer compare on every read. Measure against
  upstream's atomic load (`JOURNAL.md` "单次调用开销").

This fixes, with no downstream change: polars' 13 `PyOnceLock` statics
(`py_modules.rs:4-8`, `catalog/unity.rs:37-40`, `any_value.rs:545, 552`), and
rust-numpy's `PY_ARRAY_API` / `PY_UFUNC_API`.

### M2.3 Audit the remaining PyO3 cache sites

Classify the ~20 unchecked sites (`JOURNAL.md`) with the executable rule from
`JOURNAL.md` §2: does it hold a PyObject → is it immortal → is it a heap type.
With M2.2 most become per-interpreter automatically. List the ones that don't.

### M2.4 Correct the docs

`RETRO-subinterp.md` states that interned strings are "mostly immortal" on
3.12+. That is wrong (measured, `SUBINTERP-AUDIT.md`, "Measured facts"). Fix
it, and link the measurement.

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

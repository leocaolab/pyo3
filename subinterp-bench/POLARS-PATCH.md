# 给 polars 的补丁:回调回正确的解释器

> 配合 [`POLARS.md`](POLARS.md) 的"写出到 Python file-like 会中止进程"一节读。
> 那一节讲**为什么**,这里讲**改哪儿**。
> 目标文件只有一个:`crates/polars-python/src/file.rs`(polars 1.43.2 / crates 0.55.1)

## 它修的是什么

polars 从 rayon worker 线程回调 Python 写数据。`Python::attach` 在一个 PyO3 从未
attach 过的线程上会走 `PyGILState_Ensure`,**而那个 API 按设计绑定主解释器**。于是
worker 线程在**主解释器**里改**子解释器**拥有的对象。

崩溃(`_PyBytes_Resize → realloc → POINTER_BEING_FREED_WAS_NOT_ALLOCATED`)只是表象;
不崩的时候它也是错的。

补丁让这个文件对象**记住它是在哪个解释器里被收到的**,worker 线程 attach 回那一个。

依赖:PyO3 侧的 `pyo3::sync::InterpreterHandle`(本分支提供)。

---

## 改动:12 行加,5 行改

### ① 结构体记住诞生地

```rust
 pub(crate) struct PyFileLikeObject {
     inner: Py<PyAny>,
+    /// 收到这个文件对象时所在的解释器。worker 线程必须 attach 回它,
+    /// 而不是靠 Python::attach 走 PyGILState_Ensure 落到主解释器。
+    interp: pyo3::sync::InterpreterHandle,
     expects_str: bool,
     has_flush: bool,
 }
```

### ② 两个构造器填上

```rust
 impl Clone for PyFileLikeObject {
     fn clone(&self) -> Self {
         Python::attach(|py| Self {
+            interp: pyo3::sync::InterpreterHandle::current(py),
             inner: self.inner.clone_ref(py),
             expects_str: self.expects_str,
             has_flush: self.has_flush,
         })
     }
 }

     pub(crate) fn new(object: Py<PyAny>, expects_str: bool, has_flush: bool) -> Self {
         PyFileLikeObject {
             inner: object,
+            // 这个函数是在已 attach 的上下文里被调的,拿到的就是拥有该文件对象的解释器。
+            interp: Python::attach(|py| pyo3::sync::InterpreterHandle::current(py)),
             expects_str,
             has_flush,
         }
     }
```

### ③ 五处方法里的回调换掉

```rust
-        Python::attach(|py| {
+        self.interp.attach(|py| {
```

| 位置 | 做什么 |
|---|---|
| `PyFileLikeObject::to_buffer` | 读 |
| `PyFileLikeObject::flush` | 刷 |
| `impl Read::read` | 读 |
| **`impl Write::write`** | **★ 就是它导致 abort** |
| `impl Seek::seek` | 定位 |

---

## 关键:捕获点和使用点是分开的

```
捕获   new() / clone()      在【收到文件对象的那个解释器】里
                            InterpreterHandle::current(py) 把它记下来
                                    ↓  handle 随结构体一起被 move 到 worker
使用   Write::write         在 rayon worker 线程,PyO3 从没 attach 过它
                            self.interp.attach(...) 为【那个】解释器建 tstate
```

**语义上零变化。** 单解释器下 `handle.attach` 走 fast path(当前解释器就是目标),
行为和原来的 `Python::attach` 完全一样 —— 有测试守着
(`interpreter_handle::tests::attaching_from_the_same_interpreter_is_a_no_op`)。

---

## ★ 三处故意没改 —— 这是没覆盖的面,不是确认安全的面

`file.rs` 里还有三个 `Python::attach` 在**自由函数**里(约第 151 / 415 / 449 行),
没有 `self`,拿不到 handle:

```rust
fn read_if_bytesio(...)      // 约 415
fn get_python_scan_source... // 约 449
// 以及约 151 处的辅助函数
```

它们跑在调用线程上,不涉及 worker 回调,**目前没测出问题**。但要真正提 PR,
这几处也该拿到 handle(从参数里的 `Bound<'_, PyAny>` 取 `InterpreterHandle::current(py)`)。

---

## 效果

**不开 `PYTHONMALLOC=malloc`**,自定义 file-like,4 个 own-GIL 子解释器:

| 写出方式 | patch 前 | patch 后 |
|---|---|---|
| `write_csv` | ★ 写在主解释器 0 → 中止 134 | 写在**解释器 1** → OK |
| `write_ndjson` | ★ 主解释器 0 → 中止 | 解释器 1 → OK |
| `write_parquet` | ★ 主解释器 0 → 中止 | 解释器 1 → OK |
| `write_ipc` | ★ 主解释器 0 → 中止 | 解释器 1 → OK |
| `write_json` | 解释器 1(本来就对) | 不变 |

worker 线程**仍然是 worker 线程**(探针仍报"别的线程"),只是它现在 attach 到了
正确的解释器。省掉了 `PYTHONMALLOC=malloc` 那 23% 的代价。

---

## 怎么验

`POLARS.md` 里那个记录"每次 `write` 发生在哪个解释器"的自定义 file-like:

```python
class P:
    def write(s, b):
        seen.add(interpreters.get_current().id)     # ← 判据在这一行
        ...
df.write_csv(P())
```

**判据不是"跑完没崩",是"写发生在哪个解释器"。** 只看崩不崩会被
`PYTHONMALLOC=malloc` 骗过去 —— 它让 abort 消失,而跨解释器写对象照旧。

---

## 上游怎么提

这个补丁依赖 PyO3 提供 `InterpreterHandle`,所以顺序是:

1. **PyO3**:`InterpreterHandle` 单独提 —— 它在单解释器下也完全正确,
   补的是"从外来线程回调时该 attach 到哪个解释器"这个一直缺的原语,
   和子解释器隔离那堆改动**正交**,可以独立评审
2. **polars**:上面这个补丁,外加把三处自由函数也带上

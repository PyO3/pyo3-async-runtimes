//! Integration tests for trio support.
//!
//! These mirror the asyncio integration tests in `tokio_asyncio/mod.rs`: every
//! probe goes through the public `pyo3_async_runtimes::tokio::*` conversion
//! API, exactly as a user would call it.
//!
//! The harness is `harness = false` because the crate's `testing::main()` runs
//! tests inside `tokio::run` → `asyncio.run`; trio tests need `trio.run` per
//! case instead. Filtering matches `testing::parse_args()`: pass a substring as
//! the first positional argument.

use std::ffi::CString;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(feature = "unstable-streams")]
use futures_util::stream::StreamExt;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::IntoPyObjectExt;
use pyo3_async_runtimes::generic::{self, ContextExt, JoinError, Runtime};
use pyo3_async_runtimes::{tokio as pyo3_tokio, RuntimeKind, TaskLocals};

// ---------------------------------------------------------------------------
// `NoSpawnRuntime` doesn't spawn — it immediately drops the future, to test the
// dropped-tx path. All other probes use the crate's real `tokio` runtime.
// ---------------------------------------------------------------------------

struct NeverJoinError;
impl JoinError for NeverJoinError {
    fn is_panic(&self) -> bool {
        false
    }
    fn into_panic(self) -> Box<dyn std::any::Any + Send + 'static> {
        unreachable!()
    }
}

struct NoSpawnRuntime;
impl Runtime for NoSpawnRuntime {
    type JoinError = NeverJoinError;
    type JoinHandle = std::future::Pending<Result<(), NeverJoinError>>;
    fn spawn<F>(_fut: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        std::future::pending()
    }
    fn spawn_blocking<F>(_f: F) -> Self::JoinHandle
    where
        F: FnOnce() + Send + 'static,
    {
        std::future::pending()
    }
}

impl ContextExt for NoSpawnRuntime {
    fn scope<F, R>(_locals: TaskLocals, fut: F) -> Pin<Box<dyn Future<Output = R> + Send>>
    where
        F: Future<Output = R> + Send + 'static,
    {
        Box::pin(fut)
    }
    fn get_task_locals() -> Option<TaskLocals> {
        None
    }
}

// ---------------------------------------------------------------------------
// Probe pyfunctions — all via the public conversion API.
// ---------------------------------------------------------------------------

#[pyfunction]
fn rust_sleep(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    pyo3_tokio::future_into_py(py, async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(42i64)
    })
}

#[pyfunction]
fn rust_never(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    pyo3_tokio::future_into_py(py, async move {
        std::future::pending::<()>().await;
        Ok(0i64)
    })
}

#[pyfunction]
fn rust_panic(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    pyo3_tokio::future_into_py::<_, ()>(py, async { panic!("this panic was intentional!") })
}

/// Convert `awaitable` to a Rust future, then expose that future back to
/// Python — round-tripping `tokio::into_future` ↔ `tokio::future_into_py`.
#[pyfunction]
fn roundtrip_awaitable<'py>(
    py: Python<'py>,
    awaitable: Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let fut = pyo3_tokio::into_future(awaitable)?;
    pyo3_tokio::future_into_py(py, fut)
}

#[pyfunction]
fn snapshot_locals(py: Python<'_>) -> PyResult<Py<PyAny>> {
    fn kind_str(kind: RuntimeKind) -> &'static str {
        match kind {
            RuntimeKind::Asyncio => "asyncio",
            RuntimeKind::Trio => "trio",
            _ => unreachable!(),
        }
    }
    let locals = TaskLocals::current(py)?;
    let d = PyDict::new(py);
    d.set_item("kind", kind_str(locals.kind()))?;
    d.set_item("token", locals.event_loop(py))?;
    d.set_item("context", locals.context(py))?;
    let copied = locals.copy_context(py)?;
    d.set_item("copied_kind", kind_str(copied.kind()))?;
    d.set_item("copied_token", copied.event_loop(py))?;
    d.set_item("copied_context", copied.context(py))?;
    d.into_py_any(py)
}

#[pyfunction]
fn local_probe(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    #[allow(deprecated)]
    pyo3_tokio::local_future_into_py(py, async { Ok(0i64) })
}

static CANCEL_PROBE_DROPPED: AtomicBool = AtomicBool::new(false);
static CANCEL_PROBE_COMPLETED: AtomicBool = AtomicBool::new(false);

struct DropGuard;
impl Drop for DropGuard {
    fn drop(&mut self) {
        CANCEL_PROBE_DROPPED.store(true, Ordering::Release);
    }
}

#[pyfunction]
fn cancel_probe(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    CANCEL_PROBE_DROPPED.store(false, Ordering::Release);
    CANCEL_PROBE_COMPLETED.store(false, Ordering::Release);
    let guard = DropGuard;
    pyo3_tokio::future_into_py(py, async move {
        let _guard = guard;
        std::future::pending::<()>().await;
        CANCEL_PROBE_COMPLETED.store(true, Ordering::Release);
        Ok(0i64)
    })
}

#[pyfunction]
fn cancel_probe_state() -> (bool, bool) {
    (
        CANCEL_PROBE_DROPPED.load(Ordering::Acquire),
        CANCEL_PROBE_COMPLETED.load(Ordering::Acquire),
    )
}

#[pyfunction]
fn tx_dropped_probe(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    generic::future_into_py_with_locals::<NoSpawnRuntime, _, _>(
        py,
        TaskLocals::current(py)?,
        async move { Ok(0i64) },
    )
}

#[cfg(feature = "unstable-streams")]
#[pyfunction]
fn stream_v1_probe<'py>(py: Python<'py>, gen: Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let stream = pyo3_tokio::into_stream_v1(gen)?;
    pyo3_tokio::future_into_py(py, async move {
        let mut items = Vec::new();
        futures_util::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            let v = Python::attach(|py| item?.bind(py).extract::<i64>())?;
            items.push(v);
        }
        Ok(items)
    })
}

#[cfg(feature = "unstable-streams")]
#[pyfunction]
fn stream_v2_probe<'py>(py: Python<'py>, gen: Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let stream = pyo3_tokio::into_stream_v2(gen)?;
    pyo3_tokio::future_into_py(py, async move {
        let items: Vec<Py<PyAny>> = stream.collect().await;
        Python::attach(|py| {
            items
                .into_iter()
                .map(|v| v.bind(py).extract::<i64>())
                .collect::<PyResult<Vec<i64>>>()
        })
    })
}

// ---------------------------------------------------------------------------
// Driver helpers
// ---------------------------------------------------------------------------

fn run_driver<'py, A>(py: Python<'py>, src: &str, args: A) -> PyResult<Py<PyAny>>
where
    A: pyo3::call::PyCallArgs<'py>,
{
    let module = PyModule::from_code(
        py,
        &CString::new(src).unwrap(),
        &CString::new("trio_test_driver.py").unwrap(),
        &CString::new("trio_test_driver").unwrap(),
    )?;
    module.getattr("drive")?.call1(args).map(Bound::unbind)
}

fn assert_ok_with<'py, A>(py: Python<'py>, src: &str, args: A)
where
    A: pyo3::call::PyCallArgs<'py>,
{
    let r: String = run_driver(py, src, args)
        .unwrap_or_else(|e| {
            e.print_and_set_sys_last_vars(py);
            panic!("driver failed")
        })
        .bind(py)
        .extract()
        .unwrap();
    assert_eq!(r, "ok");
}

const ASYNCIO_DRIVER: &str = r#"
import asyncio
async def main(f):
    return await f()
def drive(f):
    return asyncio.run(main(f))
"#;

const TRIO_DRIVER: &str = r#"
import trio
async def main(f):
    return await f()
def drive(f):
    return trio.run(main, f)
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn test_asyncio_roundtrip(py: Python<'_>) {
    let f = wrap_pyfunction!(rust_sleep, py).unwrap();
    let r: i64 = run_driver(py, ASYNCIO_DRIVER, (f,))
        .unwrap()
        .bind(py)
        .extract()
        .unwrap();
    assert_eq!(r, 42);
}

fn test_trio_roundtrip(py: Python<'_>) {
    let f = wrap_pyfunction!(rust_sleep, py).unwrap();
    let r: i64 = run_driver(py, TRIO_DRIVER, (f,))
        .unwrap()
        .bind(py)
        .extract()
        .unwrap();
    assert_eq!(r, 42);
}

fn test_trio_cancel_scope_propagates(py: Python<'_>) {
    let f = wrap_pyfunction!(rust_never, py).unwrap();
    let src = r#"
import time
import trio
async def main(f):
    start = time.monotonic()
    with trio.move_on_after(0.05):
        await f()
    return time.monotonic() - start
def drive(f):
    return trio.run(main, f)
"#;
    let elapsed: f64 = run_driver(py, src, (f,))
        .unwrap()
        .bind(py)
        .extract()
        .unwrap();
    assert!(
        elapsed < 1.0,
        "move_on_after did not propagate to RustCoroutine (elapsed {elapsed}s)"
    );
}

fn test_trio_contextvars(py: Python<'_>) {
    let f = wrap_pyfunction!(roundtrip_awaitable, py).unwrap();
    let src = r#"
import contextvars, trio
cx = contextvars.ContextVar("cx")
async def reader():
    return cx.get()
async def main(roundtrip):
    cx.set("foobar")
    return await roundtrip(reader())
def drive(roundtrip):
    return trio.run(main, roundtrip)
"#;
    let r: String = run_driver(py, src, (f,))
        .unwrap()
        .bind(py)
        .extract()
        .unwrap();
    assert_eq!(r, "foobar");
}

fn test_tasklocals_asyncio(py: Python<'_>) {
    let f = wrap_pyfunction!(snapshot_locals, py).unwrap();
    let src = r#"
import asyncio, contextvars
async def main(snap):
    d = snap()
    assert d["kind"] == "asyncio", d["kind"]
    assert d["token"] is asyncio.get_running_loop(), d["token"]
    assert d["context"] is None, d["context"]
    assert d["copied_kind"] == "asyncio"
    assert d["copied_token"] is asyncio.get_running_loop()
    assert isinstance(d["copied_context"], contextvars.Context), d["copied_context"]
    return "ok"
def drive(snap):
    return asyncio.run(main(snap))
"#;
    assert_ok_with(py, src, (f,));
}

fn test_tasklocals_trio(py: Python<'_>) {
    let f = wrap_pyfunction!(snapshot_locals, py).unwrap();
    let src = r#"
import trio, contextvars
async def main(snap):
    d = snap()
    assert d["kind"] == "trio", d["kind"]
    assert isinstance(d["token"], trio.lowlevel.TrioToken), d["token"]
    assert d["context"] is None
    assert d["copied_kind"] == "trio"
    assert isinstance(d["copied_token"], trio.lowlevel.TrioToken)
    assert isinstance(d["copied_context"], contextvars.Context)
    return "ok"
def drive(snap):
    return trio.run(main, snap)
"#;
    assert_ok_with(py, src, (f,));
}

fn test_tasklocals_unsupported_library(py: Python<'_>) {
    let f = wrap_pyfunction!(snapshot_locals, py).unwrap();
    let src = r#"
import sniffio
def drive(snap):
    sniffio.thread_local.name = "curio"
    try:
        try:
            snap()
        except RuntimeError as e:
            assert "unsupported Python async library: curio" in str(e), str(e)
            return "ok"
        raise AssertionError("expected RuntimeError")
    finally:
        sniffio.thread_local.name = None
"#;
    assert_ok_with(py, src, (f,));
}

fn test_tasklocals_no_loop(py: Python<'_>) {
    let f = wrap_pyfunction!(snapshot_locals, py).unwrap();
    let src = r#"
def drive(snap):
    try:
        snap()
    except RuntimeError as e:
        assert "no running event loop" in str(e), str(e)
        return "ok"
    raise AssertionError("expected RuntimeError")
"#;
    assert_ok_with(py, src, (f,));
}

fn test_into_future_asyncio_delegates(py: Python<'_>) {
    let f = wrap_pyfunction!(roundtrip_awaitable, py).unwrap();
    let src = r#"
import asyncio
async def aw():
    await asyncio.sleep(0)
    return 9
async def main(roundtrip):
    return await roundtrip(aw())
def drive(roundtrip):
    return asyncio.run(main(roundtrip))
"#;
    let r: i64 = run_driver(py, src, (f,))
        .unwrap()
        .bind(py)
        .extract()
        .unwrap();
    assert_eq!(r, 9);
}

fn test_into_future_trio_ok(py: Python<'_>) {
    let f = wrap_pyfunction!(roundtrip_awaitable, py).unwrap();
    let src = r#"
import trio
async def aw():
    await trio.sleep(0)
    return 7
async def main(roundtrip):
    return await roundtrip(aw())
def drive(roundtrip):
    return trio.run(main, roundtrip)
"#;
    let r: i64 = run_driver(py, src, (f,))
        .unwrap()
        .bind(py)
        .extract()
        .unwrap();
    assert_eq!(r, 7);
}

fn test_into_future_trio_error(py: Python<'_>) {
    let f = wrap_pyfunction!(roundtrip_awaitable, py).unwrap();
    let src = r#"
import trio
async def aw():
    raise ValueError("boom")
async def main(roundtrip):
    try:
        await roundtrip(aw())
    except ValueError as e:
        assert str(e) == "boom"
        return "ok"
    raise AssertionError("expected ValueError")
def drive(roundtrip):
    return trio.run(main, roundtrip)
"#;
    assert_ok_with(py, src, (f,));
}

fn test_into_future_trio_base_exception(py: Python<'_>) {
    let f = wrap_pyfunction!(roundtrip_awaitable, py).unwrap();
    let src = r#"
import trio
class MyBase(BaseException):
    pass
async def aw():
    raise MyBase("boom")
async def main(roundtrip):
    await roundtrip(aw())
    return "unreachable"
def find(exc, target):
    if isinstance(exc, target):
        return True
    for attr in ("__cause__", "__context__"):
        nxt = getattr(exc, attr, None)
        if nxt is not None and find(nxt, target):
            return True
    if isinstance(exc, BaseExceptionGroup):
        return any(find(e, target) for e in exc.exceptions)
    return False
def drive(roundtrip):
    try:
        r = trio.run(main, roundtrip)
    except BaseException as e:
        assert find(e, MyBase), f"MyBase not found in {e!r}"
        return "ok"
    raise AssertionError(f"expected trio.run to raise; got {r!r}")
"#;
    assert_ok_with(py, src, (f,));
}

fn test_future_into_py_dispatch_asyncio(py: Python<'_>) {
    let f = wrap_pyfunction!(rust_never, py).unwrap();
    let src = r#"
import asyncio
async def main(never):
    obj = never()
    assert asyncio.isfuture(obj), type(obj)
    obj.cancel()
    return "ok"
def drive(never):
    return asyncio.run(main(never))
"#;
    assert_ok_with(py, src, (f,));
}

fn test_future_into_py_dispatch_trio(py: Python<'_>) {
    let f = wrap_pyfunction!(rust_never, py).unwrap();
    let src = r#"
import trio, asyncio
async def main(never):
    obj = never()
    assert not asyncio.isfuture(obj), type(obj)
    assert type(obj).__name__ == "RustCoroutine", type(obj).__name__
    obj.close()
    return "ok"
def drive(never):
    return trio.run(main, never)
"#;
    assert_ok_with(py, src, (f,));
}

fn test_trio_panic(py: Python<'_>) {
    let f = wrap_pyfunction!(rust_panic, py).unwrap();
    let src = r#"
import trio
async def main(f):
    try:
        await f()
    except Exception as e:
        assert "this panic was intentional!" in str(e), str(e)
        assert "RustPanic" in type(e).__name__, type(e).__name__
        return "ok"
    raise AssertionError("expected RustPanic")
def drive(f):
    return trio.run(main, f)
"#;
    assert_ok_with(py, src, (f,));
}

fn test_trio_local_future_into_py_not_implemented(py: Python<'_>) {
    let f = wrap_pyfunction!(local_probe, py).unwrap();
    let src = r#"
import trio
async def main(probe):
    try:
        probe()
    except NotImplementedError as e:
        assert "local_future_into_py" in str(e), str(e)
        return "ok"
    raise AssertionError("expected NotImplementedError")
def drive(probe):
    return trio.run(main, probe)
"#;
    assert_ok_with(py, src, (f,));
}

fn test_future_into_py_close_cancels_rust(py: Python<'_>) {
    let probe = wrap_pyfunction!(cancel_probe, py).unwrap();
    let state = wrap_pyfunction!(cancel_probe_state, py).unwrap();
    let src = r#"
import trio
async def main(probe, state):
    c = probe()
    c.close()  # drops cancel_tx -> spawned select() resolves Left -> drops user fut
    for _ in range(200):
        dropped, completed = state()
        if dropped:
            break
        await trio.sleep(0.005)
    dropped, completed = state()
    assert dropped, "DropGuard never fired"
    assert not completed, "future ran to completion despite cancel"
    return "ok"
def drive(probe, state):
    return trio.run(main, probe, state)
"#;
    assert_ok_with(py, src, (probe, state));
}

fn test_future_into_py_tx_dropped_error(py: Python<'_>) {
    let probe = wrap_pyfunction!(tx_dropped_probe, py).unwrap();
    let src = r#"
import trio
async def main(probe):
    try:
        await probe()
    except RuntimeError as e:
        assert "Rust task was dropped before completion" in str(e), str(e)
        return "ok"
    raise AssertionError("expected RuntimeError")
def drive(probe):
    return trio.run(main, probe)
"#;
    assert_ok_with(py, src, (probe,));
}

fn test_trio_run_finished_error_swallowed(py: Python<'_>) {
    let f = wrap_pyfunction!(rust_sleep, py).unwrap();
    let src = r#"
import trio, io, sys, time
async def main(f):
    async def child():
        await f()
    async with trio.open_nursery() as n:
        n.start_soon(child)
        await trio.sleep(0)
        n.cancel_scope.cancel()
def drive(f):
    buf = io.StringIO()
    old = sys.stderr
    sys.stderr = buf
    try:
        trio.run(main, f)
        time.sleep(0.15)  # let the rust thread fire its wake against dead token
    finally:
        sys.stderr = old
    return "ok"
"#;
    assert_ok_with(py, src, (f,));
}

#[cfg(feature = "unstable-streams")]
fn test_trio_into_stream_v1(py: Python<'_>) {
    let probe = wrap_pyfunction!(stream_v1_probe, py).unwrap();
    let src = r#"
import trio
async def gen():
    for i in range(5):
        yield i
        await trio.sleep(0)
async def main(probe):
    items = await probe(gen())
    assert items == [0, 1, 2, 3, 4], items
    return "ok"
def drive(probe):
    return trio.run(main, probe)
"#;
    assert_ok_with(py, src, (probe,));
}

#[cfg(feature = "unstable-streams")]
fn test_trio_into_stream_v2(py: Python<'_>) {
    let probe = wrap_pyfunction!(stream_v2_probe, py).unwrap();
    let src = r#"
import trio
async def gen():
    for i in range(5):
        yield i
        await trio.sleep(0)
async def main(probe):
    items = await probe(gen())
    assert items == [0, 1, 2, 3, 4], items
    return "ok"
def drive(probe):
    return trio.run(main, probe)
"#;
    assert_ok_with(py, src, (probe,));
}

#[cfg(feature = "unstable-streams")]
fn test_trio_into_stream_v2_backpressure(py: Python<'_>) {
    let probe = wrap_pyfunction!(stream_v2_probe, py).unwrap();
    let src = r#"
import trio
async def gen():
    for i in range(50):
        yield i
async def main(probe):
    items = await probe(gen())
    assert items == list(range(50)), items
    return "ok"
def drive(probe):
    return trio.run(main, probe)
"#;
    assert_ok_with(py, src, (probe,));
}

#[cfg(feature = "unstable-streams")]
fn test_trio_into_stream_v2_gen_raises(py: Python<'_>) {
    let probe = wrap_pyfunction!(stream_v2_probe, py).unwrap();
    let src = r#"
import trio
async def gen():
    yield 1
    yield 2
    raise ValueError("boom")
async def main(probe):
    items = await probe(gen())
    assert items == [1, 2], items
    return "ok"
def drive(probe):
    return trio.run(main, probe)
"#;
    assert_ok_with(py, src, (probe,));
}

#[cfg(feature = "unstable-streams")]
fn test_trio_into_stream_v2_gen_raises_base_exception(py: Python<'_>) {
    let probe = wrap_pyfunction!(stream_v2_probe, py).unwrap();
    let src = r#"
import trio
async def gen():
    yield 1
    raise SystemExit(2)
async def main(probe):
    items = await probe(gen())
    assert items == [1], items
    return "ok"
def drive(probe):
    return trio.run(main, probe)
"#;
    assert_ok_with(py, src, (probe,));
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

type TestFn = fn(Python<'_>);

fn main() -> PyResult<()> {
    Python::initialize();

    let trio_available = Python::attach(|py| py.import("trio").is_ok());
    if !trio_available
        && std::env::var_os("CI").is_some()
        && std::env::var_os("PYO3_ASYNC_TEST_TRIO_OPTIONAL").is_none()
    {
        eprintln!("error: trio is not installed but CI is set; refusing to skip trio tests");
        std::process::exit(1);
    }
    let filter = std::env::args().nth(1);

    #[allow(unused_mut)]
    #[rustfmt::skip]
    let mut tests: Vec<(&str, TestFn, bool)> = vec![
        ("asyncio_roundtrip",                          test_asyncio_roundtrip,                          false),
        ("tasklocals_asyncio",                         test_tasklocals_asyncio,                         false),
        ("tasklocals_no_loop",                         test_tasklocals_no_loop,                         false),
        ("into_future_asyncio_delegates",              test_into_future_asyncio_delegates,              false),
        ("future_into_py_dispatch_asyncio",            test_future_into_py_dispatch_asyncio,            false),
        ("trio_roundtrip",                             test_trio_roundtrip,                             true),
        ("trio_cancel_scope_propagates",               test_trio_cancel_scope_propagates,               true),
        ("trio_contextvars",                           test_trio_contextvars,                           true),
        ("tasklocals_trio",                            test_tasklocals_trio,                            true),
        ("tasklocals_unsupported_library",             test_tasklocals_unsupported_library,             true),
        ("into_future_trio_ok",                        test_into_future_trio_ok,                        true),
        ("into_future_trio_error",                     test_into_future_trio_error,                     true),
        ("into_future_trio_base_exception",            test_into_future_trio_base_exception,            true),
        ("future_into_py_dispatch_trio",               test_future_into_py_dispatch_trio,               true),
        ("trio_panic",                                 test_trio_panic,                                 true),
        ("trio_local_future_into_py_not_implemented",  test_trio_local_future_into_py_not_implemented,  true),
        ("future_into_py_close_cancels_rust",          test_future_into_py_close_cancels_rust,          true),
        ("future_into_py_tx_dropped_error",            test_future_into_py_tx_dropped_error,            true),
        ("trio_run_finished_error_swallowed",          test_trio_run_finished_error_swallowed,          true),
    ];
    #[cfg(feature = "unstable-streams")]
    #[rustfmt::skip]
    tests.extend([
        ("trio_into_stream_v1",                           test_trio_into_stream_v1                           as TestFn, true),
        ("trio_into_stream_v2",                           test_trio_into_stream_v2                           as TestFn, true),
        ("trio_into_stream_v2_backpressure",              test_trio_into_stream_v2_backpressure              as TestFn, true),
        ("trio_into_stream_v2_gen_raises",                test_trio_into_stream_v2_gen_raises                as TestFn, true),
        ("trio_into_stream_v2_gen_raises_base_exception", test_trio_into_stream_v2_gen_raises_base_exception as TestFn, true),
    ]);

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    for (name, f, needs_trio) in tests {
        if let Some(ref filter) = filter {
            if !name.contains(filter.as_str()) {
                continue;
            }
        }
        if needs_trio && !trio_available {
            println!("test trio::{name} ... skipped (trio not installed)");
            skipped += 1;
            continue;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Python::attach(f)));
        match result {
            Ok(()) => {
                println!("test trio::{name} ... ok");
                passed += 1;
            }
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("<non-string panic>");
                println!("test trio::{name} ... FAILED: {msg}");
                failed += 1;
            }
        }
    }

    println!("\n{passed} passed; {failed} failed; {skipped} skipped");
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

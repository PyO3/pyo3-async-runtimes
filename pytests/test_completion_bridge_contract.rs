use std::{
    ffi::CString,
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
    time::Duration,
};

use pyo3::{prelude::*, types::PyAny};
use pyo3_async_runtimes::TaskLocals;

static CONVERSIONS: AtomicUsize = AtomicUsize::new(0);
static DROPS: AtomicUsize = AtomicUsize::new(0);

struct Value {
    panic: bool,
}

impl<'py> IntoPyObject<'py> for Value {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        CONVERSIONS.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "intentional result-conversion panic");
        Ok(42usize.into_pyobject(py)?.into_any())
    }
}

struct DropGuard;

impl Drop for DropGuard {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

struct PanicFuture {
    panic_on_drop: bool,
    pending: bool,
}

impl Future for PanicFuture {
    type Output = PyResult<usize>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        assert!(self.panic_on_drop, "intentional future-poll panic");
        if self.pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(42))
        }
    }
}

impl Drop for PanicFuture {
    fn drop(&mut self) {
        if self.panic_on_drop {
            DROPS.fetch_add(1, Ordering::SeqCst);
            panic!("intentional future-drop panic");
        }
    }
}

#[pyfunction]
fn panic_future<'py>(
    py: Python<'py>,
    event_loop: Bound<'py, PyAny>,
    panic_on_drop: bool,
    pending: bool,
) -> PyResult<Bound<'py, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py_with_locals(
        py,
        TaskLocals::new(event_loop),
        PanicFuture {
            panic_on_drop,
            pending,
        },
    )
}

#[pyfunction]
fn value<'py>(
    py: Python<'py>,
    event_loop: Bound<'py, PyAny>,
    delay_ms: u64,
    panic: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let guard = DropGuard;
    pyo3_async_runtimes::tokio::future_into_py_with_locals(
        py,
        TaskLocals::new(event_loop),
        async move {
            let _guard = guard;
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            Ok(Value { panic })
        },
    )
}

#[pyfunction]
fn counts() -> (usize, usize) {
    (
        CONVERSIONS.load(Ordering::SeqCst),
        DROPS.load(Ordering::SeqCst),
    )
}

#[pyfunction]
fn flush_deferred_references(py: Python<'_>) {
    py.detach(|| {
        std::thread::spawn(|| Python::attach(|_| ()))
            .join()
            .expect("reference-flush thread panicked");
    });
}

const TESTS: &str = r#"
import asyncio
import gc
import sys
import threading
import time
import weakref

class LoopProxy:
    def __init__(self, loop, reader_mode="normal"):
        self.loop = loop
        self.reader_mode = reader_mode
        self.thread = threading.get_ident()

    def __getattr__(self, name):
        if name == "_pyo3_async_runtimes_completion_bridge":
            raise AttributeError(name)
        return getattr(self.loop, name)

    def add_reader(self, fd, callback):
        if self.reader_mode == "unsupported":
            raise NotImplementedError
        if self.reader_mode == "thread_checked":
            assert threading.get_ident() == self.thread, "add_reader called off loop thread"
        self.loop.add_reader(fd, callback)

async def ordinary_and_batched(module):
    loop = asyncio.get_running_loop()
    values = await asyncio.wait_for(
        asyncio.gather(*(module.value(loop, 0, False) for _ in range(2000))),
        timeout=3,
    )
    assert values == [42] * 2000
    bridge_ref = getattr(loop, "_pyo3_async_runtimes_completion_bridge", None)
    if sys.platform == "win32":
        assert bridge_ref is None, "Windows must use the existing completion path"
    else:
        assert bridge_ref is not None and bridge_ref() is not None, "fast path not registered"

async def unsupported_reader(module):
    loop = LoopProxy(asyncio.get_running_loop(), "unsupported")
    assert await asyncio.wait_for(module.value(loop, 0, False), 1) == 42
    assert "_pyo3_async_runtimes_completion_bridge" not in loop.__dict__

async def cancel_drops_future(module):
    loop = asyncio.get_running_loop()
    before = module.counts()[1]
    future = module.value(loop, 60000, False)
    await asyncio.sleep(0.01)
    future.cancel()
    try:
        await future
    except asyncio.CancelledError:
        pass
    for _ in range(100):
        if module.counts()[1] > before:
            return
        await asyncio.sleep(0.001)
    raise AssertionError("cancelled Rust future was not dropped")

async def cancellation_races(module):
    loop = asyncio.get_running_loop()
    errors = []
    loop.set_exception_handler(lambda _, context: errors.append(context))
    for round in range(20):
        futures = [module.value(loop, i % 3, False) for i in range(100)]
        await asyncio.sleep((round % 3) / 1000)
        for future in futures[::2]:
            future.cancel()
        results = await asyncio.wait_for(
            asyncio.gather(*futures, return_exceptions=True), 2
        )
        assert all(value == 42 or isinstance(value, asyncio.CancelledError)
                   for value in results)
    await asyncio.sleep(0.01)
    assert not errors, errors

async def cancelled_result_is_not_converted(module):
    loop = asyncio.get_running_loop()
    before = module.counts()[0]
    future = module.value(loop, 10, False)
    time.sleep(0.03)
    future.cancel()
    await asyncio.sleep(0.03)
    assert module.counts()[0] == before, "cancelled result was converted to Python"

async def dropped_python_future_keeps_completion(module):
    loop = asyncio.get_running_loop()
    completed = []
    future = module.value(loop, 10, False)
    future.add_done_callback(lambda future: completed.append(future.result()))
    del future
    gc.collect()
    await asyncio.sleep(0.05)
    assert completed == [42], "dropping the Python reference discarded completion"

async def conversion_panic_resolves_future(module):
    loop = asyncio.get_running_loop()
    errors = []
    loop.set_exception_handler(lambda _, context: errors.append(context))
    future = module.value(loop, 0, True)
    others = [module.value(loop, 0, False) for _ in range(300)]
    done, pending = await asyncio.wait([future, *others], timeout=2)
    assert not pending, f"conversion panic stranded completions; loop errors: {errors}"
    error = future.exception()
    assert type(error).__name__ == "RustPanic", error
    assert "intentional result-conversion panic" in str(error)
    assert all(future.result() == 42 for future in others)
    assert not errors, errors

async def cross_thread_registration(module):
    loop = LoopProxy(asyncio.get_running_loop(), "thread_checked")
    future = await asyncio.to_thread(module.value, loop, 0, False)
    assert await asyncio.wait_for(future, 1) == 42
    assert "_pyo3_async_runtimes_completion_bridge" not in loop.__dict__

async def already_completed_result_is_not_converted(module):
    loop = asyncio.get_running_loop()
    before = module.counts()[0]
    future = module.value(loop, 10, False)
    future.set_result(99)
    await asyncio.sleep(0.05)
    assert future.result() == 99
    assert module.counts()[0] == before

async def future_panics_resolve_and_batch_continues(module):
    for panic_on_drop, message in [
        (False, "intentional future-poll panic"),
        (True, "intentional future-drop panic"),
    ]:
        future = module.panic_future(asyncio.get_running_loop(), panic_on_drop, False)
        other = module.value(asyncio.get_running_loop(), 0, False)
        results = await asyncio.wait_for(
            asyncio.gather(future, other, return_exceptions=True), 2
        )
        assert type(results[0]).__name__ == "RustPanic", results
        assert message in str(results[0]), results
        assert results[1] == 42, results

async def cancelled_future_drop_panic_does_not_break_bridge(module):
    before = module.counts()[1]
    future = module.panic_future(asyncio.get_running_loop(), True, True)
    await asyncio.sleep(0.01)
    future.cancel()
    try:
        await future
    except asyncio.CancelledError:
        pass
    for _ in range(100):
        if module.counts()[1] > before:
            break
        await asyncio.sleep(0.001)
    assert module.counts()[1] > before, "cancelled future was not dropped"
    assert await asyncio.wait_for(module.value(asyncio.get_running_loop(), 0, False), 1) == 42

def loop_close_cleanup(module, new_loop):
    loop = new_loop()
    future = module.value(loop, 50, False)
    bridge_ref = getattr(loop, "_pyo3_async_runtimes_completion_bridge")
    loop.close()
    assert bridge_ref() is None, "loop close kept the reader alive"
    del future
    loop_ref = weakref.ref(loop)
    del loop
    time.sleep(0.1)
    # Rust workers defer Python decrefs until a Python attachment.
    module.flush_deferred_references()
    gc.collect()
    assert loop_ref() is None, "completed Rust task kept the closed loop alive"

def unclosed_loop_cycle_cleanup(module, new_loop):
    loop = new_loop()
    future = module.value(loop, 10, False)
    loop_ref = weakref.ref(loop)
    future_ref = weakref.ref(future)
    del future
    del loop
    time.sleep(0.1)
    module.flush_deferred_references()
    gc.collect()
    assert future_ref() is None, "pending-future registry hid a reference cycle"
    assert loop_ref() is None, "pending-future registry kept an unreachable loop alive"

def main(module):
    tests = [
        ordinary_and_batched,
        unsupported_reader,
        cancel_drops_future,
        cancellation_races,
        dropped_python_future_keeps_completion,
        cross_thread_registration,
        future_panics_resolve_and_batch_continues,
        cancelled_future_drop_panic_does_not_break_bridge,
    ]
    if sys.platform == "win32":
        factories = [
            ("proactor", asyncio.ProactorEventLoop),
            ("selector", asyncio.SelectorEventLoop),
        ]
    else:
        tests.extend([
            cancelled_result_is_not_converted,
            conversion_panic_resolves_future,
            already_completed_result_is_not_converted,
        ])
        factories = [("asyncio", asyncio.new_event_loop)]
        try:
            import uvloop
        except ImportError:
            print("SKIP uvloop: not installed", flush=True)
        else:
            factories.append(("uvloop", uvloop.new_event_loop))
    failures = []
    for name, new_loop in factories:
        for test in tests:
            loop = new_loop()
            loop.set_debug(True)
            try:
                loop.run_until_complete(test(module))
            except BaseException as error:
                failures.append((name, test.__name__, repr(error)))
                print(f"FAIL {name}/{test.__name__}: {error!r}", flush=True)
            else:
                print(f"PASS {name}/{test.__name__}", flush=True)
            finally:
                loop.close()
        cleanup_tests = [] if sys.platform == "win32" else [
            loop_close_cleanup, unclosed_loop_cycle_cleanup
        ]
        for test in cleanup_tests:
            try:
                test(module, new_loop)
            except BaseException as error:
                failures.append((name, test.__name__, repr(error)))
                print(f"FAIL {name}/{test.__name__}: {error!r}", flush=True)
            else:
                print(f"PASS {name}/{test.__name__}", flush=True)

    class FallbackModule:
        counts = module.counts

        @staticmethod
        def value(loop, delay_ms, panic):
            return module.value(LoopProxy(loop, "unsupported"), delay_ms, panic)

        @staticmethod
        def panic_future(loop, panic_on_drop, pending):
            return module.panic_future(
                LoopProxy(loop, "unsupported"), panic_on_drop, pending
            )

    for test in [
        dropped_python_future_keeps_completion,
        cross_thread_registration,
        future_panics_resolve_and_batch_continues,
        cancelled_future_drop_panic_does_not_break_bridge,
    ]:
        loop = asyncio.new_event_loop()
        try:
            loop.run_until_complete(test(FallbackModule))
        except BaseException as error:
            failures.append(("fallback", test.__name__, repr(error)))
            print(f"BASELINE FAIL {test.__name__}: {error!r}", flush=True)
        else:
            print(f"BASELINE PASS {test.__name__}", flush=True)
        finally:
            loop.close()
    assert not failures, failures
"#;

fn main() -> PyResult<()> {
    Python::initialize();
    Python::attach(|py| {
        let extension = PyModule::new(py, "completion_contract")?;
        extension.add_function(wrap_pyfunction!(value, &extension)?)?;
        extension.add_function(wrap_pyfunction!(panic_future, &extension)?)?;
        extension.add_function(wrap_pyfunction!(counts, &extension)?)?;
        extension.add_function(wrap_pyfunction!(flush_deferred_references, &extension)?)?;
        let tests = PyModule::from_code(
            py,
            &CString::new(TESTS).unwrap(),
            &CString::new("completion_contract.py").unwrap(),
            &CString::new("completion_contract").unwrap(),
        )?;
        tests.call_method1("main", (&extension,))?;
        Ok(())
    })
}

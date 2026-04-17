//! `trio` support — Python-side park/wake primitives and a coroutine wrapper
//! that lets Rust futures be awaited under trio.
//!
//! Nothing here is needed for the asyncio path; this module is the
//! implementation detail behind [`RuntimeKind::Trio`](crate::RuntimeKind).
//!
//! `trio` and `sniffio` are imported lazily — if neither is installed the
//! module is inert and the rest of the crate behaves exactly as before.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_channel::oneshot;
use futures_util::future::{select, Either, FutureExt};
use futures_util::task::{waker_ref, ArcWake};
use pyo3::exceptions::{PyRuntimeError, PyStopIteration};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::IntoPyObjectExt;

use crate::err::RustPanic;
use crate::generic::{get_panic_message, ContextExt, Runtime};

// ---------------------------------------------------------------------------
// sniffio
// ---------------------------------------------------------------------------

static SNIFFIO_CURRENT: PyOnceLock<Option<Py<PyAny>>> = PyOnceLock::new();

/// Returns `Some("asyncio" | "trio" | ...)` if `sniffio` is importable and a
/// library is running, otherwise `None`.
pub(crate) fn sniff(py: Python<'_>) -> Option<String> {
    let current = SNIFFIO_CURRENT
        .get_or_init(py, || {
            py.import("sniffio")
                .and_then(|m| m.getattr(pyo3::intern!(py, "current_async_library")))
                .map(Bound::unbind)
                .ok()
        })
        .as_ref()?;
    current.bind(py).call0().ok()?.extract().ok()
}

// ---------------------------------------------------------------------------
// trio.lowlevel handles
// ---------------------------------------------------------------------------

static TRIO_LOWLEVEL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static TRIO_ABORT: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static TRIO_CURRENT_TASK: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static TRIO_CURRENT_TOKEN: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static TRIO_RESCHEDULE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static TRIO_WAIT_TASK_RESCHEDULED: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static TRIO_SPAWN_SYSTEM_TASK: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static FUNCTOOLS_PARTIAL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
static OUTCOME_OUTCOME: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

fn trio_lowlevel(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    TRIO_LOWLEVEL
        .get_or_try_init(py, || Ok(py.import("trio.lowlevel")?.into()))
        .map(|m| m.bind(py))
}

fn trio_attr<'py>(
    py: Python<'py>,
    cell: &'static PyOnceLock<Py<PyAny>>,
    name: &'static str,
) -> PyResult<&'py Bound<'py, PyAny>> {
    cell.get_or_try_init(py, || Ok(trio_lowlevel(py)?.getattr(name)?.unbind()))
        .map(|o| o.bind(py))
}

/// `trio.lowlevel.current_trio_token()` — the handle used to schedule callbacks
/// onto the trio run loop from any thread.
pub(crate) fn current_trio_token(py: Python<'_>) -> PyResult<Py<PyAny>> {
    trio_attr(py, &TRIO_CURRENT_TOKEN, "current_trio_token")?
        .call0()
        .map(Bound::unbind)
}

fn trio_reschedule<'py>(py: Python<'py>) -> PyResult<&'py Bound<'py, PyAny>> {
    trio_attr(py, &TRIO_RESCHEDULE, "reschedule")
}

pub(crate) fn trio_spawn_system_task<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
    trio_attr(py, &TRIO_SPAWN_SYSTEM_TASK, "spawn_system_task").cloned()
}

pub(crate) fn functools_partial<'py>(py: Python<'py>) -> PyResult<&'py Bound<'py, PyAny>> {
    FUNCTOOLS_PARTIAL
        .get_or_try_init(py, || {
            Ok(py
                .import("functools")?
                .getattr(pyo3::intern!(py, "partial"))?
                .unbind())
        })
        .map(|o| o.bind(py))
}

fn outcome_type<'py>(py: Python<'py>) -> PyResult<&'py Bound<'py, PyAny>> {
    OUTCOME_OUTCOME
        .get_or_try_init(py, || {
            Ok(py
                .import("outcome")?
                .getattr(pyo3::intern!(py, "Outcome"))?
                .unbind())
        })
        .map(|o| o.bind(py))
}

// ---------------------------------------------------------------------------
// TrioWaker + WakerCell
// ---------------------------------------------------------------------------

/// Abort callback passed to `wait_task_rescheduled`. Returns `Abort.FAILED` if
/// a Rust wake is already in flight (so that wake performs the single permitted
/// reschedule); otherwise claims the slot and returns `Abort.SUCCEEDED`.
#[pyclass]
struct TrioAbortFunc {
    woken: Arc<AtomicBool>,
}

#[pymethods]
impl TrioAbortFunc {
    fn __call__(&self, py: Python<'_>, _raise_cancel: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let abort = trio_attr(py, &TRIO_ABORT, "Abort")?;
        let variant = if self.woken.swap(true, Ordering::AcqRel) {
            "FAILED"
        } else {
            "SUCCEEDED"
        };
        abort.getattr(variant).map(Bound::unbind)
    }
}

/// Park/wake primitive for trio: parks via `wait_task_rescheduled`, wakes via
/// `TrioToken.run_sync_soon(reschedule, task)`.
///
/// Correctness: trio requires **exactly one** `reschedule` per
/// `wait_task_rescheduled` (an `Abort.SUCCEEDED` return counts as that one).
/// An `AtomicBool` guard ensures that a Rust-side wake racing with a trio
/// cancellation cannot produce a double-reschedule.
struct TrioWaker {
    task: Py<PyAny>,
    token: Py<PyAny>,
    woken: Arc<AtomicBool>,
}

impl TrioWaker {
    fn new(py: Python<'_>) -> PyResult<Self> {
        Ok(Self {
            task: trio_attr(py, &TRIO_CURRENT_TASK, "current_task")?
                .call0()?
                .unbind(),
            token: current_trio_token(py)?,
            // Starts "woken" (not armed) so a synchronous self-wake before the
            // first `yield_` cannot queue a spurious reschedule that would
            // later fire against an unrelated park.
            woken: Arc::new(AtomicBool::new(true)),
        })
    }

    fn yield_(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.woken.store(false, Ordering::Release);
        let abort = TrioAbortFunc {
            woken: self.woken.clone(),
        };
        // We extract the trap object from the `wait_task_rescheduled` coroutine
        // and discard the coroutine itself. This relies on that coroutine having
        // no `finally:`/cleanup body — true of trio's implementation today
        // (https://trio.readthedocs.io/en/stable/reference-lowlevel.html#trio.lowlevel.wait_task_rescheduled),
        // but an implementation detail rather than a stability guarantee.
        let result = trio_attr(py, &TRIO_WAIT_TASK_RESCHEDULED, "wait_task_rescheduled")
            .and_then(|f| f.call1((abort,)))
            .and_then(|c| c.call_method0(pyo3::intern!(py, "__await__")))
            .and_then(|i| i.call_method0(pyo3::intern!(py, "__next__")))
            .map(Bound::unbind);
        if result.is_err() {
            self.woken.store(true, Ordering::Release);
        }
        result
    }

    fn traverse(&self, visit: &pyo3::PyVisit<'_>) -> Result<(), pyo3::PyTraverseError> {
        visit.call(&self.task)?;
        visit.call(&self.token)
    }

    fn wake_threadsafe(&self, py: Python<'_>) {
        if self.woken.swap(true, Ordering::AcqRel) {
            return;
        }
        let reschedule = match trio_reschedule(py) {
            Ok(r) => r,
            Err(e) => {
                e.print_and_set_sys_last_vars(py);
                return;
            }
        };
        // `run_sync_soon` may raise `RunFinishedError` if `trio.run` already
        // exited; that is benign during shutdown.
        if let Err(e) = self.token.bind(py).call_method1(
            pyo3::intern!(py, "run_sync_soon"),
            (reschedule, self.task.bind(py)),
        ) {
            e.print_and_set_sys_last_vars(py);
        }
    }
}

/// `Arc`-able cell that adapts a `TrioWaker` into a `std::task::Waker` via
/// `ArcWake`.
pub(crate) struct WakerCell {
    inner: Mutex<Option<Arc<TrioWaker>>>,
    /// Set by `wake_by_ref` and consumed by `yield_`; lets a wake that arrives
    /// before the `TrioWaker` is installed (or between polls) trigger an
    /// immediate re-poll instead of being lost.
    pending_wake: AtomicBool,
}

impl WakerCell {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
            pending_wake: AtomicBool::new(false),
        })
    }

    /// Ensure a `TrioWaker` is installed, creating one if absent, then return
    /// the value to yield from `__next__`. Returns `None` if a wake was
    /// recorded while no waker was armed — the caller should re-poll instead
    /// of yielding.
    fn yield_(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        // Acquire (or lazily create) the waker. The mutex is never held across
        // a Python FFI call, avoiding GIL/mutex lock-order inversion.
        let waker: Arc<TrioWaker> = {
            let existing = self.inner.lock().unwrap().clone();
            match existing {
                Some(w) => w,
                None => {
                    // Defensive cross-check: this path is only reached when
                    // `RuntimeKind::Trio` was detected at TaskLocals
                    // construction time, but if the coroutine is then awaited
                    // under a different runtime, fail clearly rather than with
                    // an opaque `current_task()` error. `None` (sniffio absent)
                    // is treated as trio since detection already succeeded.
                    if !matches!(sniff(py).as_deref(), Some("trio") | None) {
                        return Err(PyRuntimeError::new_err(
                            "RustCoroutine awaited outside trio; use future_into_py with \
                             the running library's TaskLocals (or no explicit locals) instead",
                        ));
                    }
                    let w = Arc::new(TrioWaker::new(py)?);
                    *self.inner.lock().unwrap() = Some(w.clone());
                    w
                }
            }
        };
        if self.pending_wake.swap(false, Ordering::AcqRel) {
            return Ok(None);
        }
        let yielded = waker.yield_(py)?;
        // A wake_by_ref that lands while yield_ is arming (e.g., between the
        // swap above and TrioWaker setting woken=false) would otherwise be a
        // lost wake. Re-check and self-trigger so the just-armed park
        // resolves immediately instead of deadlocking.
        if self.pending_wake.swap(false, Ordering::AcqRel) {
            waker.wake_threadsafe(py);
        }
        Ok(Some(yielded))
    }

    fn clear(&self) {
        *self.inner.lock().unwrap() = None;
        self.pending_wake.store(false, Ordering::Release);
    }

    fn traverse(&self, visit: &pyo3::PyVisit<'_>) -> Result<(), pyo3::PyTraverseError> {
        // GC traverse must not block; on free-threaded builds GC may run
        // concurrently with `wake_by_ref`. If contended, skip — missed this
        // collection; the cycle will be caught on a later one.
        let waker = match self.inner.try_lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return Ok(()),
        };
        if let Some(w) = waker {
            w.traverse(visit)?;
        }
        Ok(())
    }
}

impl ArcWake for WakerCell {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.pending_wake.store(true, Ordering::Release);
        // Clone the waker out so the mutex is released before any Python call;
        // `Python::attach` may block on the GIL and must not happen while
        // holding the mutex.
        let waker = arc_self.inner.lock().unwrap().clone();
        if let Some(w) = waker {
            Python::attach(|py| w.wake_threadsafe(py));
        }
    }
}

// ---------------------------------------------------------------------------
// Coroutine pyclass
// ---------------------------------------------------------------------------

type BoxFut = Pin<Box<dyn Future<Output = PyResult<Py<PyAny>>> + Send>>;

/// Python awaitable backed by a Rust future. Parks the awaiting trio task via
/// `trio.lowlevel.wait_task_rescheduled` and wakes via `reschedule`.
///
/// This is distinct from `pyo3::coroutine::Coroutine` (the `experimental-async`
/// feature). That pyclass polls the user's future inline from `__next__`; this
/// one wraps a `oneshot::Receiver` whose sender is fed by the user's future
/// running on a separate Rust executor via `R::spawn`, so the only park/wake
/// state needed is the trio reschedule. Converging the two would mean teaching
/// pyo3-core's waker about trio, which is tracked upstream rather than here.
#[pyclass(name = "RustCoroutine")]
pub(crate) struct Coroutine {
    fut: Mutex<Option<BoxFut>>,
    waker_cell: Arc<WakerCell>,
    /// Dropping this signals the spawned Rust task (if any) to abort.
    cancel_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl Coroutine {
    fn with_cancel(fut: BoxFut, cancel_tx: oneshot::Sender<()>) -> Self {
        Self {
            fut: Mutex::new(Some(fut)),
            waker_cell: WakerCell::new(),
            cancel_tx: Mutex::new(Some(cancel_tx)),
        }
    }

    fn finish(&mut self) {
        *self.fut.get_mut().unwrap() = None;
        self.waker_cell.clear();
        *self.cancel_tx.get_mut().unwrap() = None;
    }

    fn poll(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let cell = self.waker_cell.clone();
        let std_waker = waker_ref(&cell);
        let mut cx = Context::from_waker(&std_waker);
        // The boxed `fut` is always `rx.await` for a oneshot receiver (see
        // `future_into_coroutine`), woken exactly once when the spawned Rust
        // task sends. The loop only iterates more than once if that wake
        // races in between `poll` and `yield_`, in which case the second poll
        // is `Ready`.
        loop {
            let fut_slot = self.fut.get_mut().unwrap();
            let fut = match fut_slot.as_mut() {
                Some(f) => f,
                None => {
                    return Err(PyRuntimeError::new_err(
                        "cannot reuse already awaited RustCoroutine",
                    ))
                }
            };
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(res) => {
                    self.finish();
                    return match res {
                        Ok(v) => Err(PyStopIteration::new_err(v)),
                        Err(e) if e.is_instance_of::<PyStopIteration>(py) => {
                            let wrapped = PyRuntimeError::new_err("coroutine raised StopIteration");
                            wrapped.set_cause(py, Some(e));
                            Err(wrapped)
                        }
                        Err(e) => Err(e),
                    };
                }
                Poll::Pending => match cell.yield_(py) {
                    Ok(Some(yielded)) => return Ok(yielded),
                    Ok(None) => continue,
                    Err(e) => {
                        self.finish();
                        return Err(e);
                    }
                },
            }
        }
    }
}

#[pymethods]
impl Coroutine {
    #[classattr]
    #[pyo3(name = "__name__")]
    fn name() -> &'static str {
        "RustCoroutine"
    }

    #[classattr]
    #[pyo3(name = "__qualname__")]
    fn qualname() -> &'static str {
        "RustCoroutine"
    }

    fn __await__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.poll(py)
    }

    /// trio's runner drives us via `coro.send(outcome)` (see
    /// `trio/_core/_run.py` — it uses `send` not `outcome.send(coro)` to
    /// work around CPython `throw()` bugs). After a successful abort the
    /// outcome is `Error(Cancelled)`; unwrap it so cancellation propagates
    /// instead of being silently dropped into a re-park busy loop.
    #[pyo3(signature = (value = None))]
    fn send(&mut self, py: Python<'_>, value: Option<Py<PyAny>>) -> PyResult<Py<PyAny>> {
        if let Some(v) = value {
            let v = v.bind(py);
            // trio hard-depends on `outcome`; surface ImportError loudly
            // rather than caching None and silently re-polling.
            let outcome_ty = outcome_type(py)?;
            if v.is_instance(outcome_ty).unwrap_or(false) {
                if let Err(e) = v.call_method0(pyo3::intern!(py, "unwrap")) {
                    self.finish();
                    return Err(e);
                }
                // Value(x).unwrap() returns x — discarded; trio only sends
                // Value(None) on resume or Error(Cancelled) on abort.
            }
        }
        self.poll(py)
    }

    /// Single-argument `throw(exc)` only — the 3-arg `throw(type, value, tb)`
    /// form is deprecated since CPython 3.12 and trio's runner never uses
    /// `throw()` (it sends `outcome.Error` via `send()` instead).
    fn throw(&mut self, exc: Bound<'_, PyAny>) -> PyResult<()> {
        let err = if let Ok(ty) = exc.cast::<pyo3::types::PyType>() {
            PyErr::from_type(ty.clone(), ())
        } else {
            PyErr::from_value(exc)
        };
        self.finish();
        Err(err)
    }

    fn close(&mut self) {
        self.finish();
    }

    fn __traverse__(&self, visit: pyo3::PyVisit<'_>) -> Result<(), pyo3::PyTraverseError> {
        self.waker_cell.traverse(&visit)
    }

    fn __clear__(&mut self) {
        self.finish();
    }
}

// ---------------------------------------------------------------------------
// into_future_with_locals — trio arm
// ---------------------------------------------------------------------------

static TRIO_RUNNER: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

const TRIO_RUNNER_SRC: &str = r#"
async def _runner(awaitable, completer):
    try:
        result = await awaitable
    except BaseException as exc:
        completer.set_error(exc)
    else:
        completer.set_result(result)
"#;

fn trio_runner(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    TRIO_RUNNER
        .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
            let module = PyModule::from_code(
                py,
                &std::ffi::CString::new(TRIO_RUNNER_SRC).unwrap(),
                &std::ffi::CString::new("pyo3_async_runtimes/_trio_runner.py").unwrap(),
                &std::ffi::CString::new("pyo3_async_runtimes_trio_runner").unwrap(),
            )?;
            Ok(module.getattr(pyo3::intern!(py, "_runner"))?.unbind())
        })
        .map(|o| o.bind(py))
}

/// Receives the outcome of a trio system task and forwards it to a Rust
/// oneshot channel.
#[pyclass]
struct OneshotSender {
    tx: Mutex<Option<oneshot::Sender<PyResult<Py<PyAny>>>>>,
}

#[pymethods]
impl OneshotSender {
    fn set_result(&self, value: Py<PyAny>) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(Ok(value));
        }
    }

    fn set_error(&self, exc: Bound<'_, PyAny>) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(Err(PyErr::from_value(exc)));
        }
    }
}

/// Schedule `awaitable` as a trio system task via `token.run_sync_soon`, in
/// the captured `context`, and send its outcome through `tx`.
pub(crate) fn schedule_awaitable(
    py: Python<'_>,
    token: &Bound<'_, PyAny>,
    context: &Bound<'_, PyAny>,
    awaitable: Bound<'_, PyAny>,
    tx: oneshot::Sender<PyResult<Py<PyAny>>>,
) -> PyResult<()> {
    let completer = Bound::new(
        py,
        OneshotSender {
            tx: Mutex::new(Some(tx)),
        },
    )?;
    let runner = trio_runner(py)?;
    let spawn = trio_spawn_system_task(py)?;
    // run_sync_soon only forwards positional args, so bind context= via
    // functools.partial. spawn_system_task(context=...) requires trio>=0.23.
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item(pyo3::intern!(py, "context"), context)?;
    let bound_spawn =
        functools_partial(py)?.call((spawn, runner, awaitable, completer), Some(&kwargs))?;
    token.call_method1(pyo3::intern!(py, "run_sync_soon"), (bound_spawn,))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// future_into_py_with_locals — trio arm
// ---------------------------------------------------------------------------

/// Spawn `fut` on `R` (with task-local propagation) and return a [`Coroutine`]
/// that awaits its result. Called from
/// [`generic::future_into_py_with_locals`](crate::generic::future_into_py_with_locals)
/// when `locals.kind() == Trio`.
#[allow(unused_must_use)] // R::spawn / R::spawn_blocking JoinHandles intentionally fire-and-forget
pub(crate) fn future_into_coroutine<R, F, T>(
    py: Python<'_>,
    locals: crate::TaskLocals,
    fut: F,
) -> PyResult<Bound<'_, PyAny>>
where
    R: Runtime + ContextExt,
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    R::spawn(async move {
        let scoped = R::scope(locals, async move {
            let fut = std::pin::pin!(fut);
            match select(cancel_rx, fut).await {
                Either::Left(_) => None,
                Either::Right((result, _)) => Some(result),
            }
        });
        match AssertUnwindSafe(scoped).catch_unwind().await {
            Err(payload) => {
                let msg = get_panic_message(&*payload).to_owned();
                // Same GIL rationale as the success path below: tx.send wakes
                // the Python-side receiver, which acquires the GIL.
                R::spawn_blocking(move || {
                    let _ = tx.send(Err(RustPanic::new_err(format!(
                        "rust future panicked: {msg}"
                    ))));
                });
            }
            Ok(None) => {}
            Ok(Some(result)) => {
                // Do not block a tokio worker thread on the GIL — same rationale
                // as `generic::future_into_py_with_locals`.
                R::spawn_blocking(move || {
                    let py_result = Python::attach(|py| result.and_then(|v| v.into_py_any(py)));
                    let _ = tx.send(py_result);
                });
            }
        }
    });
    let boxed: BoxFut = Box::pin(async move {
        rx.await.unwrap_or_else(|_| {
            Err(PyRuntimeError::new_err(
                "Rust task was dropped before completion",
            ))
        })
    });
    Coroutine::with_cancel(boxed, cancel_tx).into_bound_py_any(py)
}

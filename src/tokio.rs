//! <span class="module-item stab portability" style="display: inline; border-radius: 3px; padding: 2px; font-size: 80%; line-height: 1.2;"><code>tokio-runtime</code></span> PyO3 Asyncio functions specific to the tokio runtime
//!
//! Items marked with
//! <span
//!   class="module-item stab portability"
//!   style="display: inline; border-radius: 3px; padding: 2px; font-size: 80%; line-height: 1.2;"
//! ><code>unstable-streams</code></span>
//! >are only available when the `unstable-streams` Cargo feature is enabled:
//!
//! ```toml
//! [dependencies.pyo3-async-runtimes]
//! version = "0.24"
//! features = ["unstable-streams"]
//! ```

use std::cell::OnceCell;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::{future::Future, pin::Pin};

use ::tokio::{
    runtime::{Builder, Handle, Runtime},
    sync::Notify,
    task,
};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use pyo3::prelude::*;

use crate::{
    generic::{self, ContextExt, LocalContextExt, Runtime as GenericRuntime, SpawnLocalExt},
    TaskLocals,
};

/// <span class="module-item stab portability" style="display: inline; border-radius: 3px; padding: 2px; font-size: 80%; line-height: 1.2;"><code>attributes</code></span>
/// re-exports for macros
#[cfg(feature = "attributes")]
pub mod re_exports {
    /// re-export pending to be used in tokio macros without additional dependency
    pub use futures_util::future::pending;
    /// re-export tokio::runtime to build runtimes in tokio macros without additional dependency
    pub use tokio::runtime;
}

/// <span class="module-item stab portability" style="display: inline; border-radius: 3px; padding: 2px; font-size: 80%; line-height: 1.2;"><code>attributes</code></span>
#[cfg(feature = "attributes")]
pub use pyo3_async_runtimes_macros::tokio_main as main;

/// <span class="module-item stab portability" style="display: inline; border-radius: 3px; padding: 2px; font-size: 80%; line-height: 1.2;"><code>attributes</code></span>
/// <span class="module-item stab portability" style="display: inline; border-radius: 3px; padding: 2px; font-size: 80%; line-height: 1.2;"><code>testing</code></span>
/// Registers a `tokio` test with the `pyo3-asyncio` test harness
#[cfg(all(feature = "attributes", feature = "testing"))]
pub use pyo3_async_runtimes_macros::tokio_test as test;

// ============================================================================
// Runtime Wrapper - Enables graceful shutdown
// ============================================================================
//
// This wrapper manages the tokio runtime in a dedicated thread, allowing for
// proper shutdown coordination. The key insight is that the runtime must be
// owned (not borrowed) to call shutdown methods, but we need to provide
// shared access via static storage.
//
// Solution: Runtime lives in a dedicated thread, we only expose the Handle.
// When shutdown is requested, we signal the thread and join it, ensuring
// all tasks complete before returning.

/// Internal wrapper for tokio runtime with shutdown support.
///
/// The runtime is created in a dedicated thread and accessed via [`Handle`].
/// This enables graceful shutdown via [`request_shutdown`], which signals the
/// runtime thread and waits for it to complete.
struct RuntimeWrapper {
    /// Handle for spawning tasks (thread-safe, cloneable)
    handle: Handle,
    /// Thread running the actual runtime
    runtime_thread: Mutex<Option<JoinHandle<()>>>,
    /// Notifier to signal shutdown to the runtime thread
    shutdown_notifier: Arc<Notify>,
    /// Channel to send shutdown timeout to the runtime thread
    timeout_sender: Mutex<Option<std::sync::mpsc::SyncSender<u64>>>,
}

/// Default shutdown timeout in milliseconds.
const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 5000;

impl RuntimeWrapper {
    fn new() -> Self {
        let notify = Arc::new(Notify::new());
        let notify_clone = notify.clone();

        // Channel to send Handle back from runtime thread
        let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);

        // Channel to send shutdown timeout to runtime thread
        let (timeout_tx, timeout_rx) = std::sync::mpsc::sync_channel::<u64>(1);

        let runtime_thread = thread::Builder::new()
            .name("pyo3-tokio-runtime".into())
            .spawn(move || {
                let rt = Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("pyo3-async-runtimes: failed to build tokio runtime");

                // Send handle back to main thread
                let _ = handle_tx.send(rt.handle().clone());

                // Block until shutdown is signaled
                rt.block_on(notify_clone.notified());

                // Perform graceful shutdown with timeout
                let timeout_ms = timeout_rx.recv().unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS);
                rt.shutdown_timeout(std::time::Duration::from_millis(timeout_ms));
            })
            .expect("pyo3-async-runtimes: failed to spawn runtime thread");

        let handle = handle_rx
            .recv()
            .expect("pyo3-async-runtimes: failed to receive runtime handle");

        Self {
            handle,
            runtime_thread: Mutex::new(Some(runtime_thread)),
            shutdown_notifier: notify,
            timeout_sender: Mutex::new(Some(timeout_tx)),
        }
    }

    #[inline]
    fn handle(&self) -> &Handle {
        &self.handle
    }

    fn shutdown(&self, timeout_ms: u64) {
        // Signal the runtime thread to begin shutdown
        self.shutdown_notifier.notify_one();

        // Send timeout value to the runtime thread
        if let Some(sender) = self.timeout_sender.lock().unwrap().take() {
            let _ = sender.send(timeout_ms);
        }

        // Take the thread handle (so Drop won't try to join either)
        let thread = self.runtime_thread.lock().unwrap().take();

        // Wait for the runtime thread to complete, but only if we're not
        // currently inside the runtime (which would cause a deadlock).
        // If called from within a tokio task, we just signal shutdown and
        // let the runtime terminate when the process exits.
        if Handle::try_current().is_err() {
            if let Some(thread) = thread {
                let _ = thread.join();
            }
        }
        // If we ARE inside tokio, the thread handle is dropped without joining,
        // allowing the process to exit without waiting for the runtime thread.
    }
}

impl Drop for RuntimeWrapper {
    fn drop(&mut self) {
        self.shutdown_notifier.notify_one();

        if let Some(sender) = self.timeout_sender.lock().unwrap().take() {
            let _ = sender.send(DEFAULT_SHUTDOWN_TIMEOUT_MS);
        }

        if let Some(thread) = self.runtime_thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

// ============================================================================
// Static Storage
// ============================================================================

static TOKIO_BUILDER: Lazy<Mutex<Builder>> = Lazy::new(|| Mutex::new(multi_thread()));
/// Runtime wrapper stored in an Option to allow re-initialization after shutdown.
/// Uses Arc to allow safe sharing while supporting runtime replacement.
static RUNTIME_WRAPPER: RwLock<Option<Arc<RuntimeWrapper>>> = RwLock::new(None);
static TOKIO_RUNTIME: OnceLock<&'static Runtime> = OnceLock::new();

/// Pending runtime thread that needs cleanup.
/// Stored when request_shutdown_background is called, joined by join_pending_shutdown.
static PENDING_SHUTDOWN: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Get or create the runtime wrapper.
///
/// This function lazily initializes the runtime on first call and re-initializes
/// it after a shutdown. The returned Arc provides shared access to the runtime.
fn get_runtime_wrapper() -> Arc<RuntimeWrapper> {
    // Fast path: runtime exists
    {
        let guard = RUNTIME_WRAPPER.read();
        if let Some(ref wrapper) = *guard {
            return wrapper.clone();
        }
    }

    // Slow path: need to initialize (or re-initialize after shutdown)
    let mut guard = RUNTIME_WRAPPER.write();
    // Double-check after acquiring write lock
    if let Some(ref wrapper) = *guard {
        return wrapper.clone();
    }

    let wrapper = Arc::new(RuntimeWrapper::new());
    *guard = Some(wrapper.clone());
    wrapper
}

fn multi_thread() -> Builder {
    let mut builder = Builder::new_multi_thread();
    builder.enable_all();
    builder
}

// ============================================================================
// Generic Runtime Implementation
// ============================================================================

impl generic::JoinError for task::JoinError {
    fn is_panic(&self) -> bool {
        task::JoinError::is_panic(self)
    }
    fn into_panic(self) -> Box<dyn std::any::Any + Send + 'static> {
        task::JoinError::into_panic(self)
    }
}

struct TokioRuntime;

tokio::task_local! {
    static TASK_LOCALS: OnceCell<TaskLocals>;
}

impl GenericRuntime for TokioRuntime {
    type JoinError = task::JoinError;
    type JoinHandle = task::JoinHandle<()>;

    fn spawn<F>(fut: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        get_runtime_wrapper().handle().spawn(async move {
            fut.await;
        })
    }

    fn spawn_blocking<F>(f: F) -> Self::JoinHandle
    where
        F: FnOnce() + Send + 'static,
    {
        get_runtime_wrapper().handle().spawn_blocking(f)
    }
}

impl ContextExt for TokioRuntime {
    fn scope<F, R>(locals: TaskLocals, fut: F) -> Pin<Box<dyn Future<Output = R> + Send>>
    where
        F: Future<Output = R> + Send + 'static,
    {
        let cell = OnceCell::new();
        cell.set(locals).unwrap();

        Box::pin(TASK_LOCALS.scope(cell, fut))
    }

    fn get_task_locals() -> Option<TaskLocals> {
        TASK_LOCALS
            .try_with(|c| c.get().cloned())
            .unwrap_or_default()
    }
}

impl SpawnLocalExt for TokioRuntime {
    fn spawn_local<F>(fut: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + 'static,
    {
        tokio::task::spawn_local(fut)
    }
}

impl LocalContextExt for TokioRuntime {
    fn scope_local<F, R>(locals: TaskLocals, fut: F) -> Pin<Box<dyn Future<Output = R>>>
    where
        F: Future<Output = R> + 'static,
    {
        let cell = OnceCell::new();
        cell.set(locals).unwrap();

        Box::pin(TASK_LOCALS.scope(cell, fut))
    }
}

// ============================================================================
// Public API - Task Locals
// ============================================================================

/// Set the task local event loop for the given future
pub async fn scope<F, R>(locals: TaskLocals, fut: F) -> R
where
    F: Future<Output = R> + Send + 'static,
{
    TokioRuntime::scope(locals, fut).await
}

/// Set the task local event loop for the given !Send future
pub async fn scope_local<F, R>(locals: TaskLocals, fut: F) -> R
where
    F: Future<Output = R> + 'static,
{
    TokioRuntime::scope_local(locals, fut).await
}

/// Get the current event loop from either Python or Rust async task local context
///
/// This function first checks if the runtime has a task-local reference to the Python event loop.
/// If not, it calls [`get_running_loop`](`crate::get_running_loop`) to get the event loop
/// associated with the current OS thread.
pub fn get_current_loop(py: Python) -> PyResult<Bound<PyAny>> {
    generic::get_current_loop::<TokioRuntime>(py)
}

/// Either copy the task locals from the current task OR get the current running loop and
/// contextvars from Python.
pub fn get_current_locals(py: Python) -> PyResult<TaskLocals> {
    generic::get_current_locals::<TokioRuntime>(py)
}

// ============================================================================
// Public API - Runtime Initialization
// ============================================================================

/// Initialize the Tokio runtime with a custom builder.
///
/// Must be called before any async operations. Has no effect if the runtime
/// is already initialized.
pub fn init(builder: Builder) {
    *TOKIO_BUILDER.lock().unwrap() = builder
}

/// Initialize the Tokio runtime with an existing runtime.
///
/// This allows using an externally-managed runtime. Note that runtimes
/// initialized this way cannot be shut down via [`request_shutdown`].
///
/// Returns `Ok(())` if successful, `Err(())` if already initialized.
#[allow(clippy::result_unit_err)]
pub fn init_with_runtime(runtime: &'static Runtime) -> Result<(), ()> {
    TOKIO_RUNTIME.set(runtime).map_err(|_| ())
}

// ============================================================================
// Public API - Runtime Access
// ============================================================================

/// Get a handle to the tokio runtime.
///
/// This is the recommended way to interact with the runtime. The handle
/// can be used to spawn tasks and is compatible with [`request_shutdown`].
///
/// # Example
///
/// ```ignore
/// use pyo3_async_runtimes::tokio::get_handle;
///
/// get_handle().spawn(async {
///     // Your async code here
/// });
/// ```
pub fn get_handle() -> Handle {
    get_runtime_wrapper().handle().clone()
}

/// Get a reference to the tokio runtime.
///
/// # Deprecation Notice
///
/// This function is deprecated because the returned runtime cannot be
/// gracefully shut down. Use [`get_handle`] or the module-level [`spawn`]
/// and [`spawn_blocking`] functions instead.
///
/// If you need `block_on()`, consider restructuring your code to use
/// `future_into_py` or call this function with the understanding that
/// the runtime will not shut down cleanly.
#[deprecated(
    since = "0.28.0",
    note = "Use get_handle() or spawn/spawn_blocking functions. The returned runtime cannot be shut down gracefully."
)]
pub fn get_runtime() -> &'static Runtime {
    TOKIO_RUNTIME.get_or_init(|| {
        let rt = TOKIO_BUILDER
            .lock()
            .unwrap()
            .build()
            .expect("pyo3-async-runtimes: failed to build tokio runtime");
        Box::leak(Box::new(rt))
    })
}

// ============================================================================
// Public API - Task Spawning
// ============================================================================

/// Spawn a future on the tokio runtime.
///
/// This is equivalent to `get_handle().spawn(fut)` but more convenient.
/// Tasks spawned this way will be properly cleaned up when [`request_shutdown`]
/// is called.
///
/// # Example
///
/// ```ignore
/// use pyo3_async_runtimes::tokio;
///
/// let handle = tokio::spawn(async {
///     // Your async code here
///     42
/// });
/// ```
pub fn spawn<F>(fut: F) -> task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    get_runtime_wrapper().handle().spawn(fut)
}

/// Spawn a blocking task on the tokio runtime.
///
/// This is equivalent to `get_handle().spawn_blocking(f)` but more convenient.
///
/// # Example
///
/// ```ignore
/// use pyo3_async_runtimes::tokio;
///
/// let handle = tokio::spawn_blocking(|| {
///     // Your blocking code here
///     std::thread::sleep(std::time::Duration::from_secs(1));
///     42
/// });
/// ```
pub fn spawn_blocking<F, T>(f: F) -> task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    get_runtime_wrapper().handle().spawn_blocking(f)
}

// ============================================================================
// Public API - Shutdown
// ============================================================================

/// Request graceful shutdown of the tokio runtime.
///
/// This function signals the runtime to shut down and **blocks** until
/// shutdown is complete. All pending tasks will be given up to `timeout_ms`
/// milliseconds to complete before being forcibly terminated.
///
/// After shutdown completes, the runtime slot is cleared, allowing a new
/// runtime to be lazily initialized on the next operation. This enables
/// patterns where the runtime is shut down and later restarted.
///
/// # Arguments
///
/// * `timeout_ms` - Maximum time in milliseconds to wait for tasks to complete.
///   Use `0` for immediate shutdown, or `5000` (5 seconds) as a reasonable default.
///
/// # Returns
///
/// Returns `true` if shutdown was performed, `false` if no runtime was active.
///
/// # When to Call
///
/// Call this function before the Python event loop shuts down, typically at
/// the end of your `async def main()` or in an `atexit` handler:
///
/// ```python
/// import asyncio
/// from my_rust_module import cleanup_runtime
///
/// async def main():
///     # Your async operations here
///     await do_work()
///
///     # Cleanup before event loop shutdown
///     cleanup_runtime()
///
/// asyncio.run(main())
/// ```
///
/// # Thread Safety
///
/// This function is safe to call from any thread. It will block until the
/// runtime thread has fully terminated.
///
/// # Note
///
/// If called from within a tokio task, this function will only signal
/// shutdown but not block (to avoid deadlock). For async contexts, consider
/// using [`request_shutdown_background`] instead.
pub fn request_shutdown(timeout_ms: u64) -> bool {
    // Take the wrapper out of the Option, clearing the slot
    let wrapper = {
        let mut guard = RUNTIME_WRAPPER.write();
        guard.take()
    };

    if let Some(wrapper) = wrapper {
        // Shutdown the runtime (blocks until complete)
        wrapper.shutdown(timeout_ms);
        true
    } else {
        // No runtime was active
        false
    }
}

/// Request graceful shutdown of the tokio runtime in the background.
///
/// Unlike [`request_shutdown`], this function is safe to call from within
/// an async context running on the tokio runtime. It signals the runtime
/// to shut down and spawns a background thread to wait for completion,
/// allowing the calling async task to finish without blocking.
///
/// The runtime slot is cleared immediately, so subsequent operations will
/// lazily initialize a fresh runtime if needed.
///
/// # Arguments
///
/// * `timeout_ms` - Maximum time in milliseconds to wait for tasks to complete
///   after all spawned futures have finished.
///
/// # Returns
///
/// Returns `true` if shutdown was signaled, `false` if no runtime was active.
///
/// # When to Use
///
/// Use this function when you need to trigger shutdown from within an async
/// context (e.g., from a tokio task or within a `future_into_py` block):
///
/// ```rust,ignore
/// use pyo3_async_runtimes::tokio::{future_into_py, request_shutdown_background};
///
/// fn cleanup<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
///     future_into_py(py, async move {
///         // Do async cleanup...
///
///         // Safe to call from within async context
///         request_shutdown_background(5000);
///         Ok(())
///     })
/// }
/// ```
pub fn request_shutdown_background(timeout_ms: u64) -> bool {
    // Take the wrapper out of storage atomically.
    // This ensures new operations will create a fresh runtime.
    let wrapper = {
        let mut guard = RUNTIME_WRAPPER.write();
        guard.take()
    };

    if let Some(wrapper) = wrapper {
        // Signal the runtime thread to begin shutdown (non-blocking)
        wrapper.shutdown_notifier.notify_one();

        // Send timeout value to the runtime thread
        if let Some(sender) = wrapper.timeout_sender.lock().unwrap().take() {
            let _ = sender.send(timeout_ms);
        }

        // Store the thread handle for later joining via join_pending_shutdown()
        if let Some(thread) = wrapper.runtime_thread.lock().unwrap().take() {
            *PENDING_SHUTDOWN.lock().unwrap() = Some(thread);
        }

        // The wrapper is dropped here, releasing all Arc references.

        true
    } else {
        // No runtime was active
        false
    }
}

/// Join any pending runtime shutdown thread.
///
/// This function should be called from Python (via `asyncio.to_thread`) after
/// `request_shutdown_background` signals shutdown. It blocks until the runtime
/// thread completes, with the GIL released to allow other Python threads to run.
///
/// This is the key to proper cleanup: by calling this from Python's async context
/// (via `asyncio.to_thread`), we ensure the runtime thread is fully terminated
/// before Python's event loop closes.
///
/// # Arguments
///
/// * `py` - Python GIL token (will be released during blocking wait)
///
/// # Returns
///
/// Returns `true` if a thread was joined, `false` if no pending shutdown.
///
/// # Example
///
/// ```python
/// import asyncio
/// from etcd_client import _join_pending_shutdown
///
/// async def cleanup():
///     # ... signal shutdown ...
///     # Wait for runtime to fully terminate
///     await asyncio.to_thread(_join_pending_shutdown)
/// ```
pub fn join_pending_shutdown(py: Python<'_>) -> bool {
    let thread = PENDING_SHUTDOWN.lock().unwrap().take();

    if let Some(thread) = thread {
        // Release GIL while blocking on thread join
        #[allow(deprecated)] // py.allow_threads is deprecated but detach doesn't fit our use case
        py.allow_threads(|| {
            let _ = thread.join();
        });
        true
    } else {
        false
    }
}

// ============================================================================
// Public API - Future Conversion
// ============================================================================

/// Convert a Rust Future into a Python awaitable
///
/// If the `tokio-runtime` feature is enabled and the function is called from a Tokio thread,
/// the future is added to the Tokio runtime's task queue using `tokio::task::spawn`. Otherwise
/// it is added to the generic runtime.
///
/// # Arguments
/// * `py` - The current PyO3 GIL guard
/// * `fut` - The Rust future to be converted
///
/// # Examples
///
/// ```
/// use pyo3::prelude::*;
///
/// #[pyfunction]
/// fn sleep(py: Python) -> PyResult<Bound<PyAny>> {
///     pyo3_async_runtimes::tokio::future_into_py(py, async {
///         tokio::time::sleep(std::time::Duration::from_secs(1)).await;
///         Ok(())
///     })
/// }
/// ```
pub fn future_into_py<F, T>(py: Python, fut: F) -> PyResult<Bound<PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    generic::future_into_py::<TokioRuntime, _, _>(py, fut)
}

/// Convert a Rust Future into a Python awaitable with explicit task locals.
///
/// # Arguments
/// * `py` - The current PyO3 GIL guard
/// * `locals` - The task locals (event loop and context)
/// * `fut` - The Rust future to be converted
///
/// # Examples
///
/// ```
/// use pyo3::prelude::*;
///
/// #[pyfunction]
/// fn sleep(py: Python) -> PyResult<Bound<PyAny>> {
///     pyo3_async_runtimes::tokio::future_into_py_with_locals(
///         py,
///         pyo3_async_runtimes::tokio::get_current_locals(py)?,
///         async move {
///             tokio::time::sleep(std::time::Duration::from_secs(1)).await;
///             Ok(())
///         }
///     )
/// }
/// ```
pub fn future_into_py_with_locals<F, T>(
    py: Python,
    locals: TaskLocals,
    fut: F,
) -> PyResult<Bound<PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    generic::future_into_py_with_locals::<TokioRuntime, _, _>(py, locals, fut)
}

/// Convert a !Send Rust Future into a Python awaitable with explicit task locals.
///
/// # Arguments
/// * `py` - The current PyO3 GIL guard
/// * `locals` - The task locals (event loop and context)
/// * `fut` - The Rust future to be converted (does not need to be Send)
#[allow(deprecated)]
pub fn local_future_into_py_with_locals<F, T>(
    py: Python,
    locals: TaskLocals,
    fut: F,
) -> PyResult<Bound<PyAny>>
where
    F: Future<Output = PyResult<T>> + 'static,
    T: for<'py> IntoPyObject<'py>,
{
    generic::local_future_into_py_with_locals::<TokioRuntime, _, T>(py, locals, fut)
}

/// Convert a !Send Rust Future into a Python awaitable.
///
/// # Arguments
/// * `py` - The current PyO3 GIL guard
/// * `fut` - The Rust future to be converted (does not need to be Send)
#[allow(deprecated)]
pub fn local_future_into_py<F, T>(py: Python, fut: F) -> PyResult<Bound<PyAny>>
where
    F: Future<Output = PyResult<T>> + 'static,
    T: for<'py> IntoPyObject<'py>,
{
    generic::local_future_into_py::<TokioRuntime, _, T>(py, fut)
}

/// Convert a Python `awaitable` into a Rust Future
///
/// This function converts the `awaitable` into a Future which may be manually polled or await'd
/// in Rust.
///
/// # Arguments
/// * `awaitable` - The Python `awaitable` to be converted
///
/// # Examples
///
/// ```
/// use pyo3::prelude::*;
///
/// #[pyfunction]
/// fn await_coro(py: Python, coro: Bound<PyAny>) -> PyResult<()> {
///     pyo3_async_runtimes::tokio::get_handle().block_on(
///         pyo3_async_runtimes::tokio::into_future(coro)?
///     )?;
///     Ok(())
/// }
/// ```
pub fn into_future(
    awaitable: Bound<'_, PyAny>,
) -> PyResult<impl Future<Output = PyResult<Py<PyAny>>> + Send> {
    generic::into_future::<TokioRuntime>(awaitable)
}

/// Convert a Python `awaitable` into a Rust Future with `contextvars` support
///
/// This function converts the `awaitable` into a Future which may be manually polled or await'd
/// in Rust. It also sets the `contextvars` context for the future.
///
/// # Arguments
/// * `locals` - The task locals to use for the future
/// * `awaitable` - The Python `awaitable` to be converted
///
/// # Examples
///
/// ```
/// use pyo3::prelude::*;
///
/// #[pyfunction]
/// fn await_coro(py: Python, coro: Bound<PyAny>) -> PyResult<()> {
///     let locals = pyo3_async_runtimes::tokio::get_current_locals(py)?;
///     pyo3_async_runtimes::tokio::get_handle().block_on(
///         pyo3_async_runtimes::tokio::into_future_with_locals(&locals, coro)?
///     )?;
///     Ok(())
/// }
/// ```
pub fn into_future_with_locals(
    locals: &TaskLocals,
    awaitable: Bound<'_, PyAny>,
) -> PyResult<impl Future<Output = PyResult<Py<PyAny>>> + Send> {
    crate::into_future_with_locals(locals, awaitable)
}

/// Run the event loop until the given Future completes
///
/// The event loop runs until the given future is complete.
///
/// After this function returns, the event loop can be resumed with [`run_until_complete`]
///
/// # Arguments
/// * `event_loop` - The Python event loop that should run the future
/// * `fut` - The future to drive to completion
///
/// # Examples
///
/// ```
/// # use std::time::Duration;
/// #
/// # use pyo3::prelude::*;
/// #
/// # Python::initialize();
/// #
/// # Python::attach(|py| -> PyResult<()> {
/// # let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
/// pyo3_async_runtimes::tokio::run_until_complete(event_loop, async move {
///     tokio::time::sleep(Duration::from_secs(1)).await;
///     Ok(())
/// })?;
/// # Ok(())
/// # }).unwrap();
/// ```
pub fn run_until_complete<F, T>(event_loop: Bound<PyAny>, fut: F) -> PyResult<T>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: Send + Sync + 'static,
{
    generic::run_until_complete::<TokioRuntime, _, T>(&event_loop, fut)
}

/// Run the event loop until the given Future completes
///
/// # Arguments
/// * `py` - The current PyO3 GIL guard
/// * `fut` - The future to drive to completion
///
/// # Examples
///
/// ```no_run
/// # use std::time::Duration;
/// #
/// # use pyo3::prelude::*;
/// #
/// fn main() {
///     // call this or use pyo3 0.14 "auto-initialize" feature
///     Python::initialize();
///
///     Python::attach(|py| {
///         pyo3_async_runtimes::tokio::run(py, async move {
///             tokio::time::sleep(Duration::from_secs(1)).await;
///             Ok(())
///         })
///         .map_err(|e| {
///             e.print_and_set_sys_last_vars(py);
///         })
///         .unwrap();
///     })
/// }
/// ```
pub fn run<F, T>(py: Python, fut: F) -> PyResult<T>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: Send + Sync + 'static,
{
    generic::run::<TokioRuntime, _, _>(py, fut)
}

// ============================================================================
// Unstable Streams API
// ============================================================================

#[cfg(feature = "unstable-streams")]
pub mod stream {
    //! Utilities for working with Rust streams from Python

    use pyo3::{Py, PyAny, PyResult};

    use super::TokioRuntime;

    /// Convert an async generator into a stream
    ///
    /// **This API is marked as unstable** and is only available when the
    /// `unstable-streams` crate feature is enabled. This comes with no
    /// stability guarantees, and could be changed or removed at any time.
    ///
    /// # Arguments
    /// * `gen` - The Python async generator to be converted
    ///
    /// # Examples
    ///
    /// The returned stream does not implement `Unpin`, so it must be pinned before
    /// calling `next()`. Use `futures::pin_mut!` or `Box::pin()` as shown below:
    ///
    /// ```
    /// use pyo3::prelude::*;
    /// use futures::{pin_mut, StreamExt};
    ///
    /// #[pyfunction]
    /// fn print_values(gen: Bound<PyAny>) -> PyResult<()> {
    ///     pyo3_async_runtimes::tokio::get_handle().block_on(async move {
    ///         let stream = pyo3_async_runtimes::tokio::stream::into_stream(gen)?;
    ///         pin_mut!(stream);
    ///         while let Some(item) = stream.next().await {
    ///             println!("{:?}", item?);
    ///         }
    ///         Ok(())
    ///     })
    /// }
    /// ```
    pub fn into_stream(
        gen: pyo3::Bound<'_, PyAny>,
    ) -> PyResult<impl futures_util::Stream<Item = PyResult<Py<PyAny>>> + 'static> {
        crate::generic::into_stream_v1::<TokioRuntime>(gen)
    }

    /// Convert an async generator into a stream with `contextvars` support
    ///
    /// **This API is marked as unstable** and is only available when the
    /// `unstable-streams` crate feature is enabled. This comes with no
    /// stability guarantees, and could be changed or removed at any time.
    ///
    /// # Arguments
    /// * `locals` - The task locals to use for the stream
    /// * `gen` - The Python async generator to be converted
    ///
    /// # Examples
    ///
    /// The returned stream does not implement `Unpin`, so it must be pinned before
    /// calling `next()`. Use `futures::pin_mut!` or `Box::pin()` as shown below:
    ///
    /// ```
    /// use pyo3::prelude::*;
    /// use futures::{pin_mut, StreamExt};
    ///
    /// #[pyfunction]
    /// fn print_values(py: Python, gen: Bound<PyAny>) -> PyResult<()> {
    ///     let locals = pyo3_async_runtimes::tokio::get_current_locals(py)?;
    ///     pyo3_async_runtimes::tokio::get_handle().block_on(async move {
    ///         let stream = pyo3_async_runtimes::tokio::stream::into_stream_with_locals(
    ///             locals,
    ///             gen
    ///         )?;
    ///         pin_mut!(stream);
    ///         while let Some(item) = stream.next().await {
    ///             println!("{:?}", item?);
    ///         }
    ///         Ok(())
    ///     })
    /// }
    /// ```
    pub fn into_stream_with_locals(
        locals: crate::TaskLocals,
        gen: pyo3::Bound<'_, PyAny>,
    ) -> PyResult<impl futures_util::Stream<Item = PyResult<Py<PyAny>>> + 'static> {
        crate::generic::into_stream_with_locals_v1::<TokioRuntime>(locals, gen)
    }
}

// Module-level stream functions for backwards compatibility

/// Convert an async generator into a stream (v1 API).
///
/// See [`stream::into_stream`] for the preferred API.
#[cfg(feature = "unstable-streams")]
pub fn into_stream_v1(
    gen: pyo3::Bound<'_, pyo3::PyAny>,
) -> PyResult<impl futures_util::Stream<Item = PyResult<pyo3::Py<pyo3::PyAny>>> + 'static> {
    crate::generic::into_stream_v1::<TokioRuntime>(gen)
}

/// Convert an async generator into a stream with task locals (v1 API).
///
/// See [`stream::into_stream_with_locals`] for the preferred API.
#[cfg(feature = "unstable-streams")]
pub fn into_stream_with_locals_v1(
    locals: TaskLocals,
    gen: pyo3::Bound<'_, pyo3::PyAny>,
) -> PyResult<impl futures_util::Stream<Item = PyResult<pyo3::Py<pyo3::PyAny>>> + 'static> {
    crate::generic::into_stream_with_locals_v1::<TokioRuntime>(locals, gen)
}

/// Convert an async generator into a stream (v2 API).
///
/// This version yields items directly without wrapping in PyResult.
#[cfg(feature = "unstable-streams")]
pub fn into_stream_v2(
    gen: pyo3::Bound<'_, pyo3::PyAny>,
) -> PyResult<impl futures_util::Stream<Item = pyo3::Py<pyo3::PyAny>> + 'static> {
    crate::generic::into_stream_v2::<TokioRuntime>(gen)
}

/// Convert an async generator into a stream with task locals (v2 API).
///
/// This version yields items directly without wrapping in PyResult.
#[cfg(feature = "unstable-streams")]
pub fn into_stream_with_locals_v2(
    locals: TaskLocals,
    gen: pyo3::Bound<'_, pyo3::PyAny>,
) -> PyResult<impl futures_util::Stream<Item = pyo3::Py<pyo3::PyAny>> + 'static> {
    crate::generic::into_stream_with_locals_v2::<TokioRuntime>(locals, gen)
}

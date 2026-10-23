use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex},
};

use pyo3::{exceptions::PyOSError, prelude::*, PyTraverseError, PyVisit};

type Completion = Box<dyn for<'py> FnOnce(Python<'py>) -> PyResult<Py<PyAny>> + Send + 'static>;

struct QueueState {
    completions: VecDeque<(usize, Completion)>,
    signaled: bool,
    closed: bool,
}

pub(crate) struct BridgeCore {
    state: Mutex<QueueState>,
    writer: Mutex<UnixStream>,
}

impl BridgeCore {
    fn new(writer: UnixStream) -> Self {
        Self {
            state: Mutex::new(QueueState {
                completions: VecDeque::new(),
                signaled: false,
                closed: false,
            }),
            writer: Mutex::new(writer),
        }
    }

    pub(crate) fn send(&self, id: usize, completion: Completion) {
        let mut state = self.state.lock().expect("completion queue poisoned");
        if state.closed {
            drop(state);
            return;
        }
        state.completions.push_back((id, completion));

        if !state.signaled {
            state.signaled = true;
            if let Err(err) = self.signal() {
                if !matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::BrokenPipe
                ) {
                    eprintln!("failed to signal Python completion bridge: {err}");
                }
            }
        }
    }

    fn signal(&self) -> io::Result<()> {
        let mut writer = self.writer.lock().expect("completion socket poisoned");
        let written = loop {
            match writer.write(&[1]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if written == 1 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "completion socket wrote no wake-up byte",
            ))
        }
    }
}

#[pyclass(weakref)]
pub(crate) struct CompletionBridge {
    core: Arc<BridgeCore>,
    reader: Mutex<UnixStream>,
    // Keep future -> loop references visible to Python's cyclic GC.
    futures: Mutex<HashMap<usize, Py<PyAny>>>,
}

impl CompletionBridge {
    pub(crate) fn register(&self, future: &Bound<'_, PyAny>) -> (Arc<BridgeCore>, usize) {
        let id = future.as_ptr() as usize;
        self.futures
            .lock()
            .expect("completion futures poisoned")
            .insert(id, future.clone().unbind());
        (self.core.clone(), id)
    }
}

impl Drop for CompletionBridge {
    fn drop(&mut self) {
        let queued = {
            let mut state = self.core.state.lock().expect("completion queue poisoned");
            state.closed = true;
            std::mem::take(&mut state.completions)
        };
        drop(queued);
    }
}

#[pymethods]
impl CompletionBridge {
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        for future in self
            .futures
            .lock()
            .expect("completion futures poisoned")
            .values()
        {
            visit.call(future)?;
        }
        Ok(())
    }

    fn __clear__(&mut self) {
        self.futures
            .get_mut()
            .expect("completion futures poisoned")
            .clear();
    }

    #[getter]
    fn fd(&self) -> i32 {
        self.reader
            .lock()
            .expect("completion socket poisoned")
            .as_raw_fd()
    }

    fn __call__(&self, py: Python<'_>) -> PyResult<()> {
        // Consume wakeups before resetting signaled, or a concurrent send can be lost.
        let mut reader = self.reader.lock().expect("completion socket poisoned");
        let mut bytes = [0; 256];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(PyOSError::new_err(err.to_string())),
            }
        }
        drop(reader);

        const MAX_COMPLETIONS_PER_TURN: usize = 256;
        let completions = {
            let mut state = self.core.state.lock().expect("completion queue poisoned");
            let count = state.completions.len().min(MAX_COMPLETIONS_PER_TURN);
            let completions = state.completions.drain(..count).collect::<Vec<_>>();
            state.signaled = !state.completions.is_empty();

            if state.signaled {
                if let Err(err) = self.core.signal() {
                    if err.kind() != io::ErrorKind::WouldBlock {
                        return Err(PyOSError::new_err(err.to_string()));
                    }
                }
            }
            completions
        };

        for (id, completion) in completions {
            let future = self
                .futures
                .lock()
                .expect("completion futures poisoned")
                .remove(&id);
            let Some(future) = future else {
                continue;
            };
            let future = future.bind(py);
            let delivered = (|| {
                if future
                    .call_method0(pyo3::intern!(py, "done"))?
                    .is_truthy()?
                {
                    return Ok(());
                }
                let result =
                    catch_unwind(AssertUnwindSafe(|| completion(py))).unwrap_or_else(|panic| {
                        Err(crate::err::RustPanic::new_err(format!(
                            "rust result conversion panicked: {}",
                            crate::generic::get_panic_message(&*panic)
                        )))
                    });
                if future
                    .call_method0(pyo3::intern!(py, "done"))?
                    .is_truthy()?
                {
                    return Ok(());
                }
                match result {
                    Ok(value) => {
                        future.call_method1(pyo3::intern!(py, "set_result"), (value,))?;
                    }
                    Err(error) => {
                        future.call_method1(
                            pyo3::intern!(py, "set_exception"),
                            (error.into_value(py),),
                        )?;
                    }
                }
                Ok::<(), PyErr>(())
            })();
            if let Err(error) = delivered {
                crate::dump_err(py)(error);
            }
        }

        Ok(())
    }
}

pub(crate) fn for_loop(
    py: Python<'_>,
    event_loop: &Bound<'_, PyAny>,
) -> PyResult<Option<Py<CompletionBridge>>> {
    const ATTRIBUTE: &str = "_pyo3_async_runtimes_completion_bridge";

    let attribute = pyo3::intern!(py, ATTRIBUTE);
    match event_loop.getattr(attribute) {
        Ok(existing_ref) => {
            let existing = existing_ref.call0().map_err(|_| {
                PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                    "event-loop attribute {ATTRIBUTE} is already in use"
                ))
            })?;
            if existing.is_none() {
                event_loop.delattr(attribute)?;
            } else {
                let bridge = existing.extract::<Py<CompletionBridge>>().map_err(|_| {
                    PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                        "event-loop attribute {ATTRIBUTE} is already in use"
                    ))
                })?;
                return Ok(Some(bridge));
            }
        }
        Err(err) if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => {}
        Err(err) => return Err(err),
    }

    let add_reader = match event_loop.getattr(pyo3::intern!(py, "add_reader")) {
        Ok(add_reader) if add_reader.is_callable() => add_reader,
        Ok(_) => return Ok(None),
        Err(err) if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => {
            return Ok(None);
        }
        Err(err) => return Err(err),
    };

    let (reader, writer) = UnixStream::pair().map_err(|err| PyOSError::new_err(err.to_string()))?;
    reader
        .set_nonblocking(true)
        .and_then(|()| writer.set_nonblocking(true))
        .map_err(|err| PyOSError::new_err(err.to_string()))?;

    let core = Arc::new(BridgeCore::new(writer));
    let bridge = Py::new(
        py,
        CompletionBridge {
            core: core.clone(),
            reader: Mutex::new(reader),
            futures: Mutex::new(HashMap::new()),
        },
    )?;
    let fd = bridge
        .bind(py)
        .borrow()
        .reader
        .lock()
        .expect("completion socket poisoned")
        .as_raw_fd();
    let weak_bridge = py.import("weakref")?.call_method1("ref", (&bridge,))?;
    let empty_context = py.import("contextvars")?.call_method0("Context")?;

    if let Err(err) = empty_context.call_method1("run", (add_reader, fd, bridge.clone_ref(py))) {
        if err.is_instance_of::<pyo3::exceptions::PyNotImplementedError>(py) {
            return Ok(None);
        }
        return Err(err);
    }

    if let Err(err) = event_loop.setattr(attribute, weak_bridge) {
        event_loop.call_method1("remove_reader", (fd,))?;
        if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py)
            || err.is_instance_of::<pyo3::exceptions::PyTypeError>(py)
        {
            return Ok(None);
        }
        return Err(err);
    }

    Ok(Some(bridge))
}

pub(crate) fn for_future(
    py: Python<'_>,
    event_loop: &Bound<'_, PyAny>,
    future: &Bound<'_, PyAny>,
) -> PyResult<Option<(Arc<BridgeCore>, usize)>> {
    let owner = future.call_method0(pyo3::intern!(py, "get_loop"))?;
    match crate::get_running_loop(py) {
        Ok(running) if !running.is(&owner) => return Ok(None),
        Ok(_) => {}
        Err(error) if error.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py) => {
            if owner
                .call_method0(pyo3::intern!(py, "is_running"))?
                .is_truthy()?
            {
                return Ok(None);
            }
        }
        Err(error) => return Err(error),
    }
    Ok(for_loop(py, event_loop)?.map(|bridge| bridge.borrow(py).register(future)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_close_releases_registered_bridge() -> PyResult<()> {
        Python::initialize();
        Python::attach(|py| {
            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let bridge = for_loop(py, &event_loop)?.expect("selector loop supports add_reader");
            let _core = bridge.borrow(py).core.clone();
            drop(bridge);
            let bridge_ref = event_loop.getattr("_pyo3_async_runtimes_completion_bridge")?;
            assert!(!bridge_ref.call0()?.is_none());

            event_loop.call_method0("close")?;

            assert!(bridge_ref.call0()?.is_none());
            Ok(())
        })
    }
}

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
};

use pyo3::{exceptions::PyOSError, prelude::*};

pub(crate) type Completion = Box<dyn for<'py> FnOnce(Python<'py>) + Send + 'static>;

struct QueueState {
    completions: VecDeque<Completion>,
    signaled: bool,
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
            }),
            writer: Mutex::new(writer),
        }
    }

    pub(crate) fn send(&self, completion: Completion) {
        let mut state = self.state.lock().expect("completion queue poisoned");
        state.completions.push_back(completion);

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
        let written = self
            .writer
            .lock()
            .expect("completion socket poisoned")
            .write(&[1])?;
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
struct CompletionBridge {
    core: Arc<BridgeCore>,
    reader: Mutex<UnixStream>,
}

#[pymethods]
impl CompletionBridge {
    #[getter]
    fn fd(&self) -> i32 {
        self.reader
            .lock()
            .expect("completion socket poisoned")
            .as_raw_fd()
    }

    fn __call__(&self, py: Python<'_>) -> PyResult<()> {
        let mut reader = self.reader.lock().expect("completion socket poisoned");
        let mut bytes = [0; 256];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
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

        for completion in completions {
            completion(py);
        }

        Ok(())
    }
}

pub(crate) fn for_loop(
    py: Python<'_>,
    event_loop: &Bound<'_, PyAny>,
) -> PyResult<Option<Arc<BridgeCore>>> {
    const ATTRIBUTE: &str = "_pyo3_async_runtimes_completion_bridge";

    match event_loop.getattr(ATTRIBUTE) {
        Ok(existing_ref) => {
            let existing = existing_ref.call0().map_err(|_| {
                PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                    "event-loop attribute {ATTRIBUTE} is already in use"
                ))
            })?;
            if existing.is_none() {
                event_loop.delattr(ATTRIBUTE)?;
            } else {
                let bridge = existing
                    .extract::<PyRef<'_, CompletionBridge>>()
                    .map_err(|_| {
                        PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                            "event-loop attribute {ATTRIBUTE} is already in use"
                        ))
                    })?;
                return Ok(Some(bridge.core.clone()));
            }
        }
        Err(err) if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => {}
        Err(err) => return Err(err),
    }

    let add_reader = match event_loop.getattr("add_reader") {
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

    if let Err(err) = event_loop.setattr(ATTRIBUTE, weak_bridge) {
        event_loop.call_method1("remove_reader", (fd,))?;
        if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py)
            || err.is_instance_of::<pyo3::exceptions::PyTypeError>(py)
        {
            return Ok(None);
        }
        return Err(err);
    }

    Ok(Some(core))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_close_releases_registered_bridge() -> PyResult<()> {
        Python::initialize();
        Python::attach(|py| {
            let event_loop = py.import("asyncio")?.call_method0("new_event_loop")?;
            let _core = for_loop(py, &event_loop)?.expect("selector loop supports add_reader");
            let bridge_ref = event_loop.getattr("_pyo3_async_runtimes_completion_bridge")?;
            assert!(!bridge_ref.call0()?.is_none());

            event_loop.call_method0("close")?;

            assert!(bridge_ref.call0()?.is_none());
            Ok(())
        })
    }
}

use std::ffi::CString;
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use pyo3::{
    prelude::*,
    types::{IntoPyDict, PyType},
    wrap_pyfunction, wrap_pymodule,
};
use pyo3_async_runtimes::TaskLocals;

#[cfg(feature = "unstable-streams")]
use futures::{StreamExt, TryStreamExt};

use crate::common;

#[pyfunction]
fn sleep<'p>(py: Python<'p>, secs: Bound<'p, PyAny>) -> PyResult<Bound<'p, PyAny>> {
    let secs = secs.extract()?;

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        Ok(())
    })
}

#[pyo3_async_runtimes::tokio::test]
async fn test_future_into_py() -> PyResult<()> {
    let fut = Python::attach(|py| {
        let sleeper_mod = PyModule::new(py, "rust_sleeper")?;

        sleeper_mod.add_wrapped(wrap_pyfunction!(sleep))?;

        let test_mod = PyModule::from_code(
            py,
            &CString::new(common::TEST_MOD).unwrap(),
            &CString::new("test_future_into_py_mod.py").unwrap(),
            &CString::new("test_future_into_py_mod").unwrap(),
        )?;

        pyo3_async_runtimes::tokio::into_future(
            test_mod.call_method1("sleep_for_1s", (sleeper_mod.getattr("sleep")?,))?,
        )
    })?;

    fut.await?;

    Ok(())
}

#[pyo3_async_runtimes::tokio::test]
async fn test_async_sleep() -> PyResult<()> {
    let asyncio = Python::attach(|py| py.import("asyncio").map(Py::<PyAny>::from))?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    Python::attach(|py| {
        pyo3_async_runtimes::tokio::into_future(asyncio.bind(py).call_method1("sleep", (1.0,))?)
    })?
    .await?;

    Ok(())
}

#[pyo3_async_runtimes::tokio::test]
fn test_blocking_sleep() -> PyResult<()> {
    common::test_blocking_sleep()
}

#[pyo3_async_runtimes::tokio::test]
async fn test_into_future() -> PyResult<()> {
    common::test_into_future(Python::attach(|py| {
        pyo3_async_runtimes::tokio::get_current_loop(py)
            .unwrap()
            .into()
    }))
    .await
}

#[pyo3_async_runtimes::tokio::test]
async fn test_other_awaitables() -> PyResult<()> {
    common::test_other_awaitables(Python::attach(|py| {
        pyo3_async_runtimes::tokio::get_current_loop(py)
            .unwrap()
            .into()
    }))
    .await
}

#[pyo3_async_runtimes::tokio::test]
fn test_local_future_into_py(event_loop: Py<PyAny>) -> PyResult<()> {
    #[allow(deprecated)]
    let rt = pyo3_async_runtimes::tokio::get_runtime();
    tokio::task::LocalSet::new().block_on(rt, async {
        Python::attach(|py| {
            let non_send_secs = Rc::new(1);

            #[allow(deprecated)]
            let py_future = pyo3_async_runtimes::tokio::local_future_into_py_with_locals(
                py,
                TaskLocals::new(event_loop.bind(py).clone()),
                async move {
                    tokio::time::sleep(Duration::from_secs(*non_send_secs)).await;
                    Ok(())
                },
            )?;

            pyo3_async_runtimes::into_future_with_locals(
                &TaskLocals::new(event_loop.into_bound(py)),
                py_future,
            )
        })?
        .await?;

        Ok(())
    })
}

#[pyo3_async_runtimes::tokio::test]
async fn test_panic() -> PyResult<()> {
    let fut = Python::attach(|py| -> PyResult<_> {
        pyo3_async_runtimes::tokio::into_future(
            pyo3_async_runtimes::tokio::future_into_py::<_, ()>(py, async {
                panic!("this panic was intentional!")
            })?,
        )
    })?;

    match fut.await {
        Ok(_) => panic!("coroutine should panic"),
        Err(e) => Python::attach(|py| {
            if e.is_instance_of::<pyo3_async_runtimes::err::RustPanic>(py) {
                Ok(())
            } else {
                panic!("expected RustPanic err")
            }
        }),
    }
}

#[pyo3_async_runtimes::tokio::test]
async fn test_cancel() -> PyResult<()> {
    let completed = Arc::new(Mutex::new(false));

    let py_future = Python::attach(|py| -> PyResult<Py<PyAny>> {
        let completed = Arc::clone(&completed);
        Ok(pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            *completed.lock().unwrap() = true;

            Ok(())
        })?
        .into())
    })?;

    if let Err(e) = Python::attach(|py| -> PyResult<_> {
        py_future.bind(py).call_method0("cancel")?;
        pyo3_async_runtimes::tokio::into_future(py_future.into_bound(py))
    })?
    .await
    {
        Python::attach(|py| -> PyResult<()> {
            assert!(e.value(py).is_instance(
                py.import("asyncio")?
                    .getattr("CancelledError")?
                    .cast::<PyType>()
                    .unwrap()
            )?);
            Ok(())
        })?;
    } else {
        panic!("expected CancelledError");
    }

    tokio::time::sleep(Duration::from_secs(1)).await;
    if *completed.lock().unwrap() {
        panic!("future still completed")
    }

    Ok(())
}

#[pyo3_async_runtimes::tokio::test]
fn test_local_cancel(event_loop: Py<PyAny>) -> PyResult<()> {
    let locals = Python::attach(|py| -> PyResult<TaskLocals> {
        TaskLocals::new(event_loop.into_bound(py)).copy_context(py)
    })?;

    #[allow(deprecated)]
    let rt = pyo3_async_runtimes::tokio::get_runtime();
    tokio::task::LocalSet::new().block_on(
        rt,
        pyo3_async_runtimes::tokio::scope_local(locals, async {
            let completed = Arc::new(Mutex::new(false));
            let py_future = Python::attach(|py| -> PyResult<Py<PyAny>> {
                let completed = Arc::clone(&completed);

                #[allow(deprecated)]
                Ok(
                    pyo3_async_runtimes::tokio::local_future_into_py(py, async move {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        *completed.lock().unwrap() = true;
                        Ok(())
                    })?
                    .into(),
                )
            })?;

            if let Err(e) = Python::attach(|py| -> PyResult<_> {
                py_future.bind(py).call_method0("cancel")?;
                pyo3_async_runtimes::tokio::into_future(py_future.into_bound(py))
            })?
            .await
            {
                Python::attach(|py| -> PyResult<()> {
                    assert!(e.value(py).is_instance(
                        py.import("asyncio")?
                            .getattr("CancelledError")?
                            .cast::<PyType>()
                            .unwrap()
                    )?);
                    Ok(())
                })?;
            } else {
                panic!("expected CancelledError");
            }

            tokio::time::sleep(Duration::from_secs(1)).await;

            if *completed.lock().unwrap() {
                panic!("future still completed")
            }

            Ok(())
        }),
    )
}

/// This module is implemented in Rust.
#[pymodule]
fn test_mod(_py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    #![allow(deprecated)]
    #[pyfunction(name = "sleep")]
    fn sleep_(py: Python) -> PyResult<Bound<PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(())
        })
    }

    m.add_function(wrap_pyfunction!(sleep_, m)?)?;

    Ok(())
}

const TEST_CODE: &str = r#"
async def main():
    return await test_mod.sleep()

asyncio.new_event_loop().run_until_complete(main())
"#;

#[pyo3_async_runtimes::tokio::test]
fn test_multiple_asyncio_run() -> PyResult<()> {
    Python::attach(|py| {
        pyo3_async_runtimes::tokio::run(py, async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(())
        })?;
        pyo3_async_runtimes::tokio::run(py, async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(())
        })?;

        let d = [
            ("asyncio", py.import("asyncio")?.into()),
            ("test_mod", wrap_pymodule!(test_mod)(py)),
        ]
        .into_py_dict(py)?;

        py.run(&CString::new(TEST_CODE).unwrap(), Some(&d), None)?;
        py.run(&CString::new(TEST_CODE).unwrap(), Some(&d), None)?;
        Ok(())
    })
}

#[pymodule]
fn cvars_mod(_py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    #![allow(deprecated)]
    #[pyfunction]
    fn async_callback(py: Python, callback: Py<PyAny>) -> PyResult<Bound<PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            Python::attach(|py| {
                pyo3_async_runtimes::tokio::into_future(callback.bind(py).call0()?)
            })?
            .await?;

            Ok(())
        })
    }

    m.add_function(wrap_pyfunction!(async_callback, m)?)?;

    Ok(())
}

#[cfg(feature = "unstable-streams")]
const TOKIO_TEST_MOD: &str = r#"
import asyncio

async def gen():
    for i in range(10):
        await asyncio.sleep(0.1)
        yield i
"#;

#[cfg(feature = "unstable-streams")]
#[pyo3_async_runtimes::tokio::test]
async fn test_async_gen_v1() -> PyResult<()> {
    let stream = Python::attach(|py| {
        let test_mod = PyModule::from_code(
            py,
            &CString::new(TOKIO_TEST_MOD).unwrap(),
            &CString::new("test_rust_coroutine/tokio_test_mod.py").unwrap(),
            &CString::new("tokio_test_mod").unwrap(),
        )?;

        pyo3_async_runtimes::tokio::into_stream_v1(test_mod.call_method0("gen")?)
    })?;

    let vals = stream
        .map(|item| Python::attach(|py| -> PyResult<i32> { item?.bind(py).extract() }))
        .try_collect::<Vec<i32>>()
        .await?;

    assert_eq!((0..10).collect::<Vec<i32>>(), vals);

    Ok(())
}

#[cfg(feature = "unstable-streams")]
#[pyo3_async_runtimes::tokio::test]
async fn test_async_gen_v2() -> PyResult<()> {
    let stream = Python::attach(|py| {
        let test_mod = PyModule::from_code(
            py,
            &CString::new(TOKIO_TEST_MOD).unwrap(),
            &CString::new("test_rust_coroutine/tokio_test_mod.py").unwrap(),
            &CString::new("tokio_test_mod").unwrap(),
        )?;

        pyo3_async_runtimes::tokio::into_stream_v2(test_mod.call_method0("gen")?)
    })?;

    let vals = stream
        .map(|item| Python::attach(|py| -> PyResult<i32> { item.bind(py).extract() }))
        .try_collect::<Vec<i32>>()
        .await?;

    assert_eq!((0..10).collect::<Vec<i32>>(), vals);

    Ok(())
}

const CONTEXTVARS_CODE: &str = r#"
cx = contextvars.ContextVar("cx")

async def contextvars_test():
    assert cx.get() == "foobar"

async def main():
    cx.set("foobar")
    await cvars_mod.async_callback(contextvars_test)

asyncio.run(main())
"#;

#[pyo3_async_runtimes::tokio::test]
fn test_contextvars() -> PyResult<()> {
    Python::attach(|py| {
        let d = [
            ("asyncio", py.import("asyncio")?.into()),
            ("contextvars", py.import("contextvars")?.into()),
            ("cvars_mod", wrap_pymodule!(cvars_mod)(py)),
        ]
        .into_py_dict(py)?;

        py.run(&CString::new(CONTEXTVARS_CODE).unwrap(), Some(&d), None)?;
        py.run(&CString::new(CONTEXTVARS_CODE).unwrap(), Some(&d), None)?;
        Ok(())
    })
}

// Tests for the new shutdown API

#[pyo3_async_runtimes::tokio::test]
async fn test_spawn() -> PyResult<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();

    pyo3_async_runtimes::tokio::spawn(async move {
        tx.send(42).unwrap();
    });

    let result = rx.await.unwrap();
    assert_eq!(result, 42);

    Ok(())
}

#[pyo3_async_runtimes::tokio::test]
async fn test_spawn_blocking() -> PyResult<()> {
    let handle = pyo3_async_runtimes::tokio::spawn_blocking(|| {
        std::thread::sleep(Duration::from_millis(10));
        42
    });

    let result = handle.await.unwrap();
    assert_eq!(result, 42);

    Ok(())
}

#[pyo3_async_runtimes::tokio::test]
fn test_get_handle() -> PyResult<()> {
    let handle = pyo3_async_runtimes::tokio::get_handle();

    // The handle should be able to spawn tasks
    let (tx, rx) = std::sync::mpsc::channel();
    handle.spawn(async move {
        tx.send(42).unwrap();
    });

    let result = rx.recv().unwrap();
    assert_eq!(result, 42);

    Ok(())
}

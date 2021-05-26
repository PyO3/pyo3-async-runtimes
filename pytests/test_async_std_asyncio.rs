mod common;

use std::time::Duration;

use async_std::task;
use futures::prelude::*;
use pyo3::{prelude::*, wrap_pyfunction};

#[pyfunction]
fn sleep_for(py: Python, secs: &PyAny) -> PyResult<PyObject> {
    let secs = secs.extract()?;

    pyo3_asyncio::async_std::into_coroutine(py, async move {
        task::sleep(Duration::from_secs(secs)).await;
        Python::with_gil(|py| Ok(py.None()))
    })
}

#[pyo3_asyncio::async_std::test]
async fn test_into_coroutine() -> PyResult<()> {
    let fut = Python::with_gil(|py| {
        let sleeper_mod = PyModule::new(py, "rust_sleeper")?;

        sleeper_mod.add_wrapped(wrap_pyfunction!(sleep_for))?;

        let test_mod = PyModule::from_code(
            py,
            common::TEST_MOD,
            "test_rust_coroutine/test_mod.py",
            "test_mod",
        )?;

        pyo3_asyncio::into_future(
            test_mod.call_method1("sleep_for_1s", (sleeper_mod.getattr("sleep_for")?,))?,
        )
    })?;

    fut.await?;

    Ok(())
}

#[pyo3_asyncio::async_std::test]
async fn test_async_sleep() -> PyResult<()> {
    let asyncio =
        Python::with_gil(|py| py.import("asyncio").map(|asyncio| PyObject::from(asyncio)))?;

    task::sleep(Duration::from_secs(1)).await;

    Python::with_gil(|py| {
        pyo3_asyncio::into_future(asyncio.as_ref(py).call_method1("sleep", (1.0,))?)
    })?
    .await?;

    Ok(())
}

#[pyo3_asyncio::async_std::test]
fn test_blocking_sleep() -> PyResult<()> {
    common::test_blocking_sleep()
}

#[pyo3_asyncio::async_std::test]
async fn test_into_future() -> PyResult<()> {
    common::test_into_future().await
}

#[pyo3_asyncio::async_std::test]
async fn test_other_awaitables() -> PyResult<()> {
    common::test_other_awaitables().await
}

#[pyo3_asyncio::async_std::test]
fn test_init_twice() -> PyResult<()> {
    common::test_init_twice()
}

const ASYNC_STD_TEST_MOD: &str = r#"
import asyncio 

async def gen():
    for i in range(10):
        await asyncio.sleep(0.1)
        yield i        
"#;

#[pyo3_asyncio::async_std::test]
async fn test_async_gen_v1() -> PyResult<()> {
    let stream = Python::with_gil(|py| {
        let test_mod = PyModule::from_code(
            py,
            ASYNC_STD_TEST_MOD,
            "test_rust_coroutine/async_std_test_mod.py",
            "async_std_test_mod",
        )?;

        pyo3_asyncio::async_std::into_stream_v1(test_mod.call_method0("gen")?)
    })?;

    let vals = stream
        .map(|item| Python::with_gil(|py| -> PyResult<i32> { Ok(item?.as_ref(py).extract()?) }))
        .try_collect::<Vec<i32>>()
        .await?;

    assert_eq!((0..10).collect::<Vec<i32>>(), vals);

    Ok(())
}

#[pyo3_asyncio::tokio::test]
async fn test_async_gen_v2() -> PyResult<()> {
    let stream = Python::with_gil(|py| {
        let test_mod = PyModule::from_code(
            py,
            ASYNC_STD_TEST_MOD,
            "test_rust_coroutine/async_std_test_mod.py",
            "async_std_test_mod",
        )?;

        pyo3_asyncio::async_std::into_stream_v2(test_mod.call_method0("gen")?)
    })?;

    let vals = stream
        .map(|item| Python::with_gil(|py| -> PyResult<i32> { Ok(item.as_ref(py).extract()?) }))
        .try_collect::<Vec<i32>>()
        .await?;

    assert_eq!((0..10).collect::<Vec<i32>>(), vals);

    Ok(())
}

#[pyo3_asyncio::async_std::main]
async fn main() -> pyo3::PyResult<()> {
    pyo3_asyncio::testing::main().await
}

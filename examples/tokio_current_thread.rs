use pyo3::prelude::*;

#[pyo3_asyncio_0_21::tokio::main(flavor = "current_thread")]
async fn main() -> PyResult<()> {
    let fut = Python::with_gil(|py| {
        let asyncio = py.import_bound("asyncio")?;

        // convert asyncio.sleep into a Rust Future
        pyo3_asyncio_0_21::tokio::into_future(asyncio.call_method1("sleep", (1.into_py(py),))?)
    })?;

    println!("sleeping for 1s");
    fut.await?;
    println!("done");

    Ok(())
}

use pyo3::prelude::*;

#[pyo3_async_runtimes::tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() -> PyResult<()> {
    let fut = Python::attach(|py| {
        let asyncio = py.import("asyncio")?;

        // convert asyncio.sleep into a Rust Future
        pyo3_async_runtimes::tokio::into_future(asyncio.call_method1("sleep", (1,))?)
    })?;

    println!("sleeping for 1s");
    fut.await?;
    println!("done");

    Ok(())
}

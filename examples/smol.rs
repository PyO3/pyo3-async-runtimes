use pyo3::prelude::*;

#[pyo3_async_runtimes::smol::main]
async fn main() -> PyResult<()> {
    let fut = Python::attach(|py| {
        let asyncio = py.import("asyncio")?;

        // convert asyncio.sleep into a Rust Future
        pyo3_async_runtimes::smol::into_future(asyncio.call_method1("sleep", (1,))?)
    })?;

    println!("sleeping for 1s");
    fut.await?;
    println!("done");

    Ok(())
}

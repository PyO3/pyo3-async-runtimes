mod common;
mod tokio_asyncio;

use pyo3::prelude::*;

fn main() -> pyo3::PyResult<()> {
    pyo3::prepare_freethreaded_python();

    Python::with_gil(|py| {
        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();

        pyo3_async_runtimes::tokio::init(builder);
        std::thread::spawn(move || {
            pyo3_async_runtimes::tokio::get_runtime().block_on(futures::future::pending::<()>());
        });

        pyo3_async_runtimes::tokio::run(py, pyo3_async_runtimes::testing::main())
    })
}
